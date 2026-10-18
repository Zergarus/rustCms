//! Приём документов оплат и отгрузок из 1С (ветки `pay_system_*_operation` и
//! `shipment_operation` `CSaleOrderLoader` Битрикса). Работает в транзакции группы из
//! [`super::apply`].

use anyhow::bail;
use chrono::NaiveDate;
use sqlx::PgConnection;

use super::apply::{
    Ctx, FINAL_ORDER, SaleStats, apply_basket, delivery_line, doc_error, find_order, is_final,
    is_newer, plan_basket, round, same, set_status_1c,
};
use super::document::PayOperation;
use super::parse::{DocumentIn, ItemKind};
use crate::sale::repo::{recalc_totals_in, sync_paid_in};

/// Реквизиты документов оплаты и отгрузки (`lang/ru/component.php` компонента обмена).
const TRAIT_PAID: &str = "Оплачен";
const TRAIT_PAID_DATE: &str = "Дата оплаты по 1С";
const TRAIT_PAID_NUM: &str = "Номер оплаты по 1С";
const TRAIT_RETURN: &str = "ПризнакВозврата";
const TRAIT_RETURN_REASON: &str = "Причина к Возврату";
const TRAIT_CANCEL: &str = "Отменен";
const TRAIT_DELIVERY_DATE: &str = "Дата отгрузки по 1С";
const TRAIT_DEDUCTED: &str = "Отгружен";
const TRAIT_DELIVERY_NUM: &str = "Номер отгрузки по 1С";
const TRAIT_TRACKING: &str = "Идентификатор отправления";
const TRAIT_ALLOW_DELIVERY: &str = "Доставка разрешена";

/// Финальный статус Битрикса: оплаты и отгрузки такого заказа не меняются.
const STATUS_FINAL: &str = "F";

fn trait_of<'a>(doc: &'a DocumentIn, name: &str) -> &'a str {
    doc.traits.get(name).map_or("", |v| v.trim())
}

/// Дата из реквизита: `ГГГГ-ММ-ДД[Tчч:мм:сс]` или `ДД.ММ.ГГГГ`; иначе — нет.
fn parse_date(v: &str) -> Option<NaiveDate> {
    let day = v.split(['T', ' ']).next().unwrap_or_default();
    NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(day, "%d.%m.%Y"))
        .ok()
}

#[derive(Clone, Copy)]
enum Child {
    Payment,
    Shipment,
}

impl Child {
    fn by_id(self) -> &'static str {
        match self {
            Child::Payment => "SELECT id, order_id FROM payments WHERE id = $1 AND order_id = $2",
            Child::Shipment => "SELECT id, order_id FROM shipments WHERE id = $1 AND order_id = $2",
        }
    }

    fn by_id_1c(self) -> &'static str {
        match self {
            Child::Payment => {
                "SELECT id, order_id FROM payments
                 WHERE id_1c = $1 AND ($2::bigint IS NULL OR order_id = $2) ORDER BY id DESC LIMIT 1"
            }
            Child::Shipment => {
                "SELECT id, order_id FROM shipments
                 WHERE id_1c = $1 AND ($2::bigint IS NULL OR order_id = $2) ORDER BY id DESC LIMIT 1"
            }
        }
    }
}

/// Оплата или отгрузка документа (`getDocumentId`): Номер — наш id в заказе основания, затем
/// Номер, Ид и Номер1С — по `id_1c` (в заказе основания; без него — по всем, заказ берётся
/// у найденной). Возвращает (id, заказ).
async fn find_child(
    tx: &mut PgConnection,
    kind: Child,
    order: Option<i64>,
    doc: &DocumentIn,
) -> sqlx::Result<Option<(i64, i64)>> {
    if let (Some(order), Ok(id)) = (order, doc.number.trim().parse::<i64>())
        && let Some(found) = sqlx::query_as(sqlx::AssertSqlSafe(kind.by_id()))
            .bind(id)
            .bind(order)
            .fetch_optional(&mut *tx)
            .await?
    {
        return Ok(Some(found));
    }
    for key in [&doc.number, &doc.id, &doc.id_1c] {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        if let Some(found) = sqlx::query_as(sqlx::AssertSqlSafe(kind.by_id_1c()))
            .bind(key)
            .bind(order)
            .fetch_optional(&mut *tx)
            .await?
        {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// Заказ документа: по `Основание` (правила документа заказа), иначе — заказ найденной оплаты
/// или отгрузки. `None` у второго — не найдена.
async fn locate(
    ctx: &Ctx<'_>,
    tx: &mut PgConnection,
    kind: Child,
    doc: &DocumentIn,
) -> anyhow::Result<(i64, Option<i64>)> {
    let order = find_order(tx, &doc.base, &ctx.s.number_prefix).await?;
    let found = find_child(tx, kind, order, doc).await?;
    let Some(order) = order.or(found.map(|f| f.1)) else {
        bail!("заказ не найден");
    };
    Ok((order, found.map(|f| f.0)))
}

#[derive(sqlx::FromRow)]
struct PaymentState {
    id: i64,
    paid: bool,
    version_1c: String,
}

/// Документ оплаты; `None` — пропущен (версия не новее).
pub(super) async fn apply_payment(
    ctx: &Ctx<'_>,
    tx: &mut PgConnection,
    doc: &DocumentIn,
    op: PayOperation,
    stats: &mut SaleStats,
) -> anyhow::Result<Option<i64>> {
    let (order_id, found) = locate(ctx, tx, Child::Payment, doc).await?;
    // Строка заказа — раньше строки оплаты, как на сайте (`set_paid_in`): без взаимной блокировки
    let (canceled, status): (bool, String) =
        sqlx::query_as("SELECT canceled, status FROM orders WHERE id = $1 FOR UPDATE")
            .bind(order_id)
            .fetch_one(&mut *tx)
            .await?;
    let current = match found {
        Some(id) => {
            let p: PaymentState = sqlx::query_as(
                "SELECT id, paid, version_1c FROM payments WHERE id = $1 FOR UPDATE",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if !is_newer(&doc.version, &p.version_1c) {
                stats.skipped += 1;
                return Ok(None);
            }
            Some(p)
        }
        None => None,
    };
    if canceled {
        bail!("заказ отменён");
    }
    if status == STATUS_FINAL {
        bail!("заказ в финальном статусе");
    }

    let cancel = trait_of(doc, TRAIT_CANCEL) == "true";
    let is_return = !cancel && trait_of(doc, TRAIT_RETURN) == "true";
    let reason = trait_of(doc, TRAIT_RETURN_REASON);
    let voucher_date = parse_date(trait_of(doc, TRAIT_PAID_DATE));
    let voucher_num = trait_of(doc, TRAIT_PAID_NUM);
    let paid_flag = trait_of(doc, TRAIT_PAID) == "true";
    let sum = doc.sum.map(|v| round(v, 2));

    match current {
        Some(p) if doc.deleted => {
            if p.paid {
                bail!("оплата проведена, удаление невозможно");
            }
            sqlx::query("DELETE FROM payments WHERE id = $1")
                .bind(p.id)
                .execute(&mut *tx)
                .await?;
            stats.payments_updated += 1;
        }
        Some(p) => {
            // «Отменен» и «ПризнакВозврата» снимают оплату, «Оплачен» — ставит
            let paid = !cancel && !is_return && (p.paid || paid_flag);
            sqlx::query(
                "UPDATE payments SET
                     sum = CASE WHEN NOT paid AND $2::float8 IS NOT NULL THEN $2 ELSE sum END,
                     pay_voucher_date = COALESCE($3, pay_voucher_date),
                     pay_voucher_num = CASE WHEN $4 <> '' THEN $4 ELSE pay_voucher_num END,
                     is_return = is_return OR $5,
                     return_comment = CASE WHEN $5 THEN $6 ELSE return_comment END,
                     paid_at = CASE WHEN $7 THEN CASE WHEN paid THEN paid_at ELSE now() END END,
                     paid = $7,
                     id_1c = $8, version_1c = $9, updated_1c = TRUE
                 WHERE id = $1",
            )
            .bind(p.id)
            .bind(sum)
            .bind(voucher_date)
            .bind(voucher_num)
            .bind(is_return)
            .bind(reason)
            .bind(paid)
            .bind(&doc.id_1c)
            .bind(&doc.version)
            .execute(&mut *tx)
            .await?;
            stats.payments_updated += 1;
        }
        None if doc.deleted || !ctx.s.import_new_payments => bail!("оплата не найдена"),
        None => {
            create_payment(
                ctx,
                tx,
                order_id,
                doc,
                op,
                sum,
                paid_flag && !cancel && !is_return,
            )
            .await?;
            stats.payments_created += 1;
        }
    }
    if sync_paid_in(tx, order_id).await? == Some(true) {
        stats.notify.push(("SALE_ORDER_PAID".into(), order_id));
    }
    Ok(Some(order_id))
}

/// Новая оплата из 1С (`addPaymentFromDocumentByOrder`): платёжная система по умолчанию для
/// операции, при оплате всего остатка заказа — «для оплаченного заказа».
async fn create_payment(
    ctx: &Ctx<'_>,
    tx: &mut PgConnection,
    order_id: i64,
    doc: &DocumentIn,
    op: PayOperation,
    sum: Option<f64>,
    paid: bool,
) -> anyhow::Result<()> {
    let (price, currency, paid_sum): (f64, String, f64) = sqlx::query_as(
        "SELECT price::float8, currency,
                COALESCE((SELECT sum(sum) FROM payments WHERE order_id = o.id AND paid), 0)::float8
         FROM orders o WHERE id = $1",
    )
    .bind(order_id)
    .fetch_one(&mut *tx)
    .await?;
    let rest = round((price - paid_sum).max(0.0), 2);
    let sum = sum.unwrap_or(rest);
    let s = ctx.s;
    let by_operation = match op {
        PayOperation::Cash => s.default_ps_cash,
        PayOperation::Cashless => s.default_ps_cashless,
        PayOperation::Acquiring => s.default_ps_acquiring,
    };
    let whole = paid && (sum > rest || same(sum, rest));
    let pay_system = if whole {
        s.default_ps_order_paid.or(by_operation)
    } else {
        by_operation
    };
    let name: String = match pay_system {
        Some(id) => sqlx::query_scalar("SELECT name FROM pay_systems WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or_default(),
        None => String::new(),
    };
    let is_return = trait_of(doc, TRAIT_CANCEL) != "true" && trait_of(doc, TRAIT_RETURN) == "true";
    sqlx::query(
        "INSERT INTO payments (order_id, pay_system_id, name, sum, currency, paid, paid_at,
                               pay_voucher_num, pay_voucher_date, is_return, return_comment,
                               id_1c, version_1c, updated_1c)
         VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $6 THEN now() END, $7, $8, $9, $10, $11, $12, TRUE)",
    )
    .bind(order_id)
    .bind(pay_system)
    .bind(&name)
    .bind(sum)
    .bind(&currency)
    .bind(paid)
    .bind(trait_of(doc, TRAIT_PAID_NUM))
    .bind(parse_date(trait_of(doc, TRAIT_PAID_DATE)))
    .bind(is_return)
    .bind(if is_return {
        trait_of(doc, TRAIT_RETURN_REASON)
    } else {
        ""
    })
    .bind(&doc.id_1c)
    .bind(&doc.version)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct ShipmentState {
    id: i64,
    deducted: bool,
    version_1c: String,
    price: f64,
}

/// Документ отгрузки; `None` — пропущен (версия не новее).
pub(super) async fn apply_shipment(
    ctx: &Ctx<'_>,
    tx: &mut PgConnection,
    doc: &DocumentIn,
    stats: &mut SaleStats,
) -> anyhow::Result<Option<i64>> {
    let (order_id, found) = locate(ctx, tx, Child::Shipment, doc).await?;
    // Строка заказа — раньше строки отгрузки, как на сайте
    let status: String = sqlx::query_scalar("SELECT status FROM orders WHERE id = $1 FOR UPDATE")
        .bind(order_id)
        .fetch_one(&mut *tx)
        .await?;
    let current = match found {
        Some(id) => {
            let sh: ShipmentState = sqlx::query_as(
                "SELECT id, deducted, version_1c, price::float8 AS price FROM shipments
                 WHERE id = $1 FOR UPDATE",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if !is_newer(&doc.version, &sh.version_1c) {
                stats.skipped += 1;
                return Ok(None);
            }
            Some(sh)
        }
        None => None,
    };
    if status == STATUS_FINAL {
        bail!("заказ в финальном статусе");
    }

    if doc.deleted {
        let Some(sh) = current else {
            bail!("отгрузка не найдена");
        };
        if sh.deducted {
            bail!("отгрузка проведена, удаление невозможно");
        }
        sqlx::query("DELETE FROM shipments WHERE id = $1")
            .bind(sh.id)
            .execute(&mut *tx)
            .await?;
        sync_delivery(tx, order_id).await?;
        stats.shipments_updated += 1;
        return Ok(Some(order_id));
    }

    let current = match current {
        Some(sh) => {
            stats.shipments_updated += 1;
            sh
        }
        None if !ctx.s.import_new_shipments => bail!("отгрузка не найдена"),
        None => {
            stats.shipments_created += 1;
            create_shipment(ctx, tx, order_id).await?
        }
    };

    let mut totals = false;
    // Состав финального заказа расходится: ошибка в итог, отгрузка применяется без версии
    let mut kept = false;
    // Позиций у отгрузки нет: сверяются с заказом, только если отгрузка у него единственная
    let single: bool = sqlx::query_scalar("SELECT count(*) = 1 FROM shipments WHERE order_id = $1")
        .bind(order_id)
        .fetch_one(&mut *tx)
        .await?;
    if single && doc.items.iter().any(|i| i.kind == ItemKind::Product) {
        // Только расхождение количеств; разница в ценах отгрузку не касается
        let plan = plan_basket(tx, order_id, doc).await?;
        if plan.quantity_differs() {
            if !ctx.s.update_basket_quantity {
                bail!("количество в отгрузке не совпадает с заказом");
            }
            if is_final(tx, order_id).await? {
                stats.errors.push(doc_error(doc, &FINAL_ORDER));
                kept = true;
            } else {
                apply_basket(ctx, tx, order_id, &plan).await?;
                totals = true;
            }
        }
    }

    // Строка-услуга — стоимость доставки неотгруженной отгрузки
    let price = delivery_line(doc)
        .map(|d| round(d.sum, 2))
        .filter(|p| !current.deducted && !same(*p, current.price));
    totals |= price.is_some();
    // Отгружена: «Отгружен» = true (`Ship()`) или задана «Дата отгрузки по 1С»
    let ship_date = trait_of(doc, TRAIT_DELIVERY_DATE);
    let ship = trait_of(doc, TRAIT_DEDUCTED) == "true" || !ship_date.is_empty();
    let doc_date = parse_date(ship_date).or_else(|| parse_date(&doc.date));
    let deducted = trait_of(doc, TRAIT_CANCEL) != "true" && (current.deducted || ship);
    let allow = match trait_of(doc, TRAIT_ALLOW_DELIVERY) {
        _ if ship => Some(true),
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    };
    sqlx::query(
        "UPDATE shipments SET
             price = COALESCE($2, price),
             base_price = COALESCE($2, base_price),
             deducted_at = CASE WHEN $3 THEN CASE WHEN deducted THEN deducted_at ELSE now() END END,
             deducted = $3,
             allow_delivery_at = CASE WHEN COALESCE($4, allow_delivery)
                 THEN CASE WHEN allow_delivery THEN allow_delivery_at ELSE now() END END,
             allow_delivery = COALESCE($4, allow_delivery),
             delivery_doc_date = CASE WHEN $5 THEN COALESCE($6, delivery_doc_date) ELSE delivery_doc_date END,
             delivery_doc_num = CASE WHEN $7 <> '' THEN $7 ELSE delivery_doc_num END,
             tracking_number = CASE WHEN $8 <> '' THEN $8 ELSE tracking_number END,
             id_1c = $9, version_1c = CASE WHEN $11 THEN version_1c ELSE $10 END,
             updated_1c = TRUE
         WHERE id = $1",
    )
    .bind(current.id)
    .bind(price)
    .bind(deducted)
    .bind(allow)
    .bind(ship)
    .bind(doc_date)
    .bind(trait_of(doc, TRAIT_DELIVERY_NUM))
    .bind(trait_of(doc, TRAIT_TRACKING))
    .bind(&doc.id_1c)
    .bind(&doc.version)
    .bind(kept)
    .execute(&mut *tx)
    .await?;
    if totals {
        sync_delivery(tx, order_id).await?;
    }

    // Заказ отгружен целиком — «Статус при отгрузке» (кроме финального F)
    if let Some(code) = ctx.s.final_status_on_delivery.as_deref().map(str::trim)
        && !code.is_empty()
    {
        let (status, shipped): (String, bool) = sqlx::query_as(
            "SELECT status, EXISTS (SELECT 1 FROM shipments WHERE order_id = o.id)
                    AND NOT EXISTS (SELECT 1 FROM shipments WHERE order_id = o.id AND NOT deducted)
             FROM orders o WHERE id = $1",
        )
        .bind(order_id)
        .fetch_one(&mut *tx)
        .await?;
        if shipped && status != STATUS_FINAL && status != code {
            set_status_1c(tx, order_id, code, stats).await?;
        }
    }
    Ok(Some(order_id))
}

/// Новая отгрузка службой по умолчанию (`addShipmentFromDocumentByOrder`).
async fn create_shipment(
    ctx: &Ctx<'_>,
    tx: &mut PgConnection,
    order_id: i64,
) -> anyhow::Result<ShipmentState> {
    let name: String = match ctx.s.default_delivery {
        Some(d) => sqlx::query_scalar("SELECT name FROM deliveries WHERE id = $1")
            .bind(d)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or_default(),
        None => String::new(),
    };
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO shipments (order_id, delivery_id, delivery_name, price, base_price, updated_1c)
         VALUES ($1, $2, $3, 0, 0, TRUE) RETURNING id",
    )
    .bind(order_id)
    .bind(ctx.s.default_delivery)
    .bind(&name)
    .fetch_one(&mut *tx)
    .await?;
    Ok(ShipmentState {
        id,
        deducted: false,
        version_1c: String::new(),
        price: 0.0,
    })
}

/// Доставка заказа — сумма отгрузок; суммы заказа пересчитываются.
async fn sync_delivery(tx: &mut PgConnection, order_id: i64) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE orders SET delivery_price =
             COALESCE((SELECT sum(price) FROM shipments WHERE order_id = $1), 0)
         WHERE id = $1",
    )
    .bind(order_id)
    .execute(&mut *tx)
    .await?;
    recalc_totals_in(tx, order_id, None).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_from_traits() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 14);
        assert_eq!(parse_date("2026-10-14T10:20:30"), d);
        assert_eq!(parse_date("2026-10-14"), d);
        assert_eq!(parse_date("14.10.2026 0:00:00"), d);
        assert_eq!(parse_date(""), None);
        assert_eq!(parse_date("вчера"), None);
    }
}
