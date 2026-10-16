//! Приём документов из 1С (`import`): документ «Заказ товара» меняет найденный заказ или
//! создаёт новый (`CSaleOrderLoader::updateOrderWithoutShipmentsPayments` /
//! `addOrderWithoutShipmentsPayments` Битрикса). Каждая группа документов (`Контейнер` или
//! одиночный документ) — своя транзакция с меткой обмена; ошибка документа откатывает группу.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{anyhow, bail};
use serde::Serialize;
use sqlx::{PgConnection, PgPool};

use super::parse::{AgentIn, DocumentIn, ItemIn, ItemKind, Operation, SaleFile};
use super::settings::{CONTRAGENT_CODES, MapEntry, MapSource, SaleSettings};
use super::{apply_docs, mark_exchange};
use crate::bxapi::project::OrderImportHook;
use crate::sale::repo::{
    recalc_totals_in, set_canceled_in, set_status_in, stock_lines, traced_locked,
};
use crate::sale::stock::{self, StockLine};

/// Реквизит документа со статусом заказа (`CC_BSC1_1C_STATUS_ID`).
const TRAIT_STATUS: &str = "Статуса заказа ИД";
/// Реквизит документа с отменой (`CC_BSC1_CANCEL`).
const TRAIT_CANCEL: &str = "Отменен";
/// Комментарий истории статусов при смене из 1С.
pub(super) const HISTORY_COMMENT: &str = "1С";

/// Итог приёма; `notify` — письма о смене статуса (событие, заказ), отправляются после коммита.
#[derive(Debug, Clone, Default, PartialEq, Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SaleStats {
    pub orders_created: u32,
    pub orders_updated: u32,
    pub payments_created: u32,
    pub payments_updated: u32,
    pub shipments_created: u32,
    pub shipments_updated: u32,
    pub skipped: u32,
    pub errors: Vec<String>,
    #[serde(skip)]
    pub notify: Vec<(String, i64)>,
}

impl SaleStats {
    fn merge(&mut self, o: SaleStats) {
        self.orders_created += o.orders_created;
        self.orders_updated += o.orders_updated;
        self.payments_created += o.payments_created;
        self.payments_updated += o.payments_updated;
        self.shipments_created += o.shipments_created;
        self.shipments_updated += o.shipments_updated;
        self.skipped += o.skipped;
        self.errors.extend(o.errors);
        self.notify.extend(o.notify);
    }
}

async fn order_by(
    conn: &mut PgConnection,
    sql: &'static str,
    key: &str,
) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(key)
        .fetch_optional(&mut *conn)
        .await
}

/// Заказ по номеру документа 1С (`orders.id_1c`), новейший.
pub(super) async fn by_id_1c(conn: &mut PgConnection, key: &str) -> sqlx::Result<Option<i64>> {
    let key = key.trim();
    if key.is_empty() {
        return Ok(None);
    }
    order_by(
        conn,
        "SELECT id FROM orders WHERE id_1c = $1 ORDER BY id DESC LIMIT 1",
        key,
    )
    .await
}

async fn find_plain(conn: &mut PgConnection, key: &str) -> sqlx::Result<Option<i64>> {
    if let Ok(id) = key.parse::<i64>()
        && let Some(id) = sqlx::query_scalar("SELECT id FROM orders WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?
    {
        return Ok(Some(id));
    }
    if let Some(id) = by_id_1c(conn, key).await? {
        return Ok(Some(id));
    }
    order_by(
        conn,
        "SELECT id FROM orders WHERE account_number = $1 ORDER BY id DESC LIMIT 1",
        key,
    )
    .await
}

/// Заказ по Ид или номеру документа (`getOrderIdByDocument`): id, затем `id_1c`, затем номер;
/// номер с префиксом магазина — то же без префикса.
pub async fn find_order(
    conn: &mut PgConnection,
    doc_id_or_number: &str,
    prefix: &str,
) -> sqlx::Result<Option<i64>> {
    let key = doc_id_or_number.trim();
    if key.is_empty() {
        return Ok(None);
    }
    if let Some(id) = find_plain(conn, key).await? {
        return Ok(Some(id));
    }
    match key.strip_prefix(prefix) {
        Some(rest) if !prefix.is_empty() && !rest.is_empty() => {
            if let Ok(id) = rest.parse::<i64>()
                && let Some(id) = sqlx::query_scalar("SELECT id FROM orders WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&mut *conn)
                    .await?
            {
                return Ok(Some(id));
            }
            order_by(
                conn,
                "SELECT id FROM orders WHERE account_number = $1 ORDER BY id DESC LIMIT 1",
                rest,
            )
            .await
        }
        _ => Ok(None),
    }
}

/// Документ новее сохранённой версии: числа сравниваются как числа, иначе как строки;
/// пустая версия (своя или документа) не мешает (`checkVersion1C`).
pub(super) fn is_newer(doc: &str, stored: &str) -> bool {
    let (d, s) = (doc.trim(), stored.trim());
    if d.is_empty() || s.is_empty() {
        return true;
    }
    match (d.parse::<f64>(), s.parse::<f64>()) {
        (Ok(a), Ok(b)) => a > b,
        _ => d > s,
    }
}

pub(super) struct Ctx<'a> {
    pub(super) db: &'a PgPool,
    pub(super) s: &'a SaleSettings,
    map: &'a [MapEntry],
    agents: &'a [AgentIn],
    hook: Option<&'a dyn OrderImportHook>,
}

/// Применяет группы файла начиная с `start`; после группы, во время которой наступил
/// `deadline`, останавливается и возвращает индекс следующей группы (`None` — всё).
pub async fn apply_file(
    db: &PgPool,
    s: &SaleSettings,
    map: &[MapEntry],
    file: &SaleFile,
    hook: Option<&dyn OrderImportHook>,
    start: usize,
    deadline: Option<Instant>,
) -> (SaleStats, Option<usize>) {
    let ctx = Ctx {
        db,
        s,
        map,
        agents: &file.agents,
        hook,
    };
    let mut stats = SaleStats::default();
    for (i, group) in file.groups.iter().enumerate().skip(start) {
        match apply_group(&ctx, group).await {
            Ok(local) => stats.merge(local),
            Err((doc, e)) => {
                let id = if doc.id.is_empty() {
                    &doc.id_1c
                } else {
                    &doc.id
                };
                stats.errors.push(format!("Документ {id}: {e:#}"));
            }
        }
        if i + 1 < file.groups.len() && deadline.is_some_and(|d| Instant::now() >= d) {
            return (stats, Some(i + 1));
        }
    }
    (stats, None)
}

async fn apply_group<'g>(
    ctx: &Ctx<'_>,
    group: &'g [DocumentIn],
) -> Result<SaleStats, (&'g DocumentIn, anyhow::Error)> {
    let first = group.first();
    let fail = |doc: &'g DocumentIn| move |e: anyhow::Error| (doc, e);
    let Some(first) = first else {
        return Ok(SaleStats::default());
    };
    let db_fail = |e: sqlx::Error| (first, anyhow!(e));
    let mut tx = ctx.db.begin().await.map_err(db_fail)?;
    mark_exchange(&mut tx).await.map_err(db_fail)?;
    let mut local = SaleStats::default();
    for doc in group {
        apply_document(ctx, &mut tx, doc, &mut local)
            .await
            .map_err(fail(doc))?;
    }
    tx.commit().await.map_err(db_fail)?;
    Ok(local)
}

async fn apply_document(
    ctx: &Ctx<'_>,
    tx: &mut PgConnection,
    doc: &DocumentIn,
    stats: &mut SaleStats,
) -> anyhow::Result<()> {
    let order_id = match &doc.operation {
        Operation::Order => apply_order(ctx, tx, doc, stats).await?,
        Operation::Payment(op) => apply_docs::apply_payment(ctx, tx, doc, *op, stats).await?,
        Operation::Shipment => apply_docs::apply_shipment(ctx, tx, doc, stats).await?,
        Operation::Unknown(name) => bail!("неизвестная хозяйственная операция «{name}»"),
    };
    if let (Some(order_id), Some(hook)) = (order_id, ctx.hook) {
        hook.after_document(tx, order_id, doc).await?;
    }
    Ok(())
}

/// Документ заказа; `None` — пропущен (версия не новее).
async fn apply_order(
    ctx: &Ctx<'_>,
    tx: &mut PgConnection,
    doc: &DocumentIn,
    stats: &mut SaleStats,
) -> anyhow::Result<Option<i64>> {
    let prefix = &ctx.s.number_prefix;
    // `getDocumentId`: Номер — как номер заказа сайта; Ид и Номер1С — только по `id_1c`
    let mut found = find_order(tx, &doc.number, prefix).await?;
    for key in [&doc.id, &doc.id_1c] {
        if found.is_none() {
            found = by_id_1c(tx, key).await?;
        }
    }
    let Some(id) = found else {
        if !ctx.s.import_new_orders {
            bail!("заказ не найден");
        }
        let id = create_order(ctx, tx, doc, stats).await?;
        apply_flags(ctx, tx, id, doc, stats).await?;
        stats.orders_created += 1;
        return Ok(Some(id));
    };

    let version: String =
        sqlx::query_scalar("SELECT version_1c FROM orders WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if !is_newer(&doc.version, &version) {
        stats.skipped += 1;
        return Ok(None);
    }
    if ctx.s.update_basket_quantity && doc.items.iter().any(|i| i.kind == ItemKind::Product) {
        update_basket(ctx, tx, id, doc).await?;
    }
    sqlx::query("UPDATE orders SET id_1c = $2, version_1c = $3 WHERE id = $1")
        .bind(id)
        .bind(&doc.id_1c)
        .bind(&doc.version)
        .execute(&mut *tx)
        .await?;
    apply_flags(ctx, tx, id, doc, stats).await?;
    stats.orders_updated += 1;
    Ok(Some(id))
}

/// Отмена (`ПометкаУдаления` или «Отменен») и статус из 1С (при опции).
async fn apply_flags(
    ctx: &Ctx<'_>,
    tx: &mut PgConnection,
    id: i64,
    doc: &DocumentIn,
    stats: &mut SaleStats,
) -> anyhow::Result<()> {
    let cancel = doc.traits.get(TRAIT_CANCEL).map(|v| v.trim() == "true");
    if doc.deleted || cancel == Some(true) {
        set_canceled_in(tx, id, true, "").await?;
    } else if cancel == Some(false) {
        set_canceled_in(tx, id, false, "").await?;
    }
    let code = doc.traits.get(TRAIT_STATUS).map_or("", |v| v.trim());
    if ctx.s.change_status_from_1c && !code.is_empty() {
        set_status_1c(tx, id, code, stats).await?;
    }
    Ok(())
}

/// Статус из 1С: история с комментарием «1С», письмо — если статус шлёт письма и сменился.
pub(super) async fn set_status_1c(
    tx: &mut PgConnection,
    id: i64,
    code: &str,
    stats: &mut SaleStats,
) -> anyhow::Result<()> {
    let notify: Option<bool> =
        sqlx::query_scalar("SELECT notify FROM order_statuses WHERE code = $1")
            .bind(code)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(notify) = notify else {
        bail!("статус {code} не найден");
    };
    if set_status_in(tx, id, code, None, HISTORY_COMMENT).await? && notify {
        stats
            .notify
            .push((format!("SALE_STATUS_CHANGED_{code}"), id));
    }
    Ok(())
}

/// Товар каталога по Ид из 1С (предложение `X#X` — по `X`): id и название.
async fn product_by_xml_id(tx: &mut PgConnection, xml_id: &str) -> anyhow::Result<(i64, String)> {
    let found: Option<(i64, String)> = sqlx::query_as(
        "SELECT i.id, i.name FROM collection_items i JOIN catalog_products p ON p.item_id = i.id
         WHERE i.xml_id <> '' AND (i.xml_id = $1 OR i.xml_id = $1 || '#' || $1)
         ORDER BY (i.xml_id = $1) DESC, i.id LIMIT 1",
    )
    .bind(xml_id)
    .fetch_optional(&mut *tx)
    .await?;
    found.ok_or_else(|| anyhow!("товар {xml_id} не найден"))
}

/// `normalizeExternalCode`: `X#X` → `X`.
fn normalize_xml_id(xml_id: &str) -> &str {
    match xml_id.split_once('#') {
        Some((product, offer)) if product == offer => product,
        _ => xml_id,
    }
}

pub(super) fn delivery_line(doc: &DocumentIn) -> Option<&ItemIn> {
    doc.items.iter().find(|i| i.kind == ItemKind::Service)
}

pub(super) fn round(v: f64, digits: i32) -> f64 {
    let k = 10f64.powi(digits);
    (v * k).round() / k
}

pub(super) fn same(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// `item_id` и `store_id` выбираются для сверки позиций, но пока нигде не читаются.
#[derive(sqlx::FromRow)]
#[allow(dead_code)]
struct BasketRow {
    id: i64,
    item_id: i64,
    store_id: Option<i64>,
    quantity: f64,
    price: f64,
    xml_id: String,
}

/// Правка позиций по строкам-товарам документа: строки сопоставляются по Ид товара, новые
/// добавляются, отсутствующие в документе удаляются.
#[derive(Default)]
pub(super) struct BasketPlan {
    /// Строка, количество, цена, изменилось ли количество.
    updates: Vec<(i64, f64, f64, bool)>,
    inserts: Vec<(i64, String, f64, f64)>,
    removed: Vec<i64>,
}

impl BasketPlan {
    pub(super) fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.inserts.is_empty() && self.removed.is_empty()
    }

    /// Расходится состав или количество (не только цены).
    pub(super) fn quantity_differs(&self) -> bool {
        !self.inserts.is_empty() || !self.removed.is_empty() || self.updates.iter().any(|u| u.3)
    }
}

pub(super) async fn plan_basket(
    tx: &mut PgConnection,
    id: i64,
    doc: &DocumentIn,
) -> anyhow::Result<BasketPlan> {
    let rows: Vec<BasketRow> = sqlx::query_as(
        "SELECT c.id, c.item_id, c.store_id, c.quantity::float8 AS quantity,
                COALESCE(c.price, 0)::float8 AS price, COALESCE(i.xml_id, '') AS xml_id
         FROM cart_items c LEFT JOIN collection_items i ON i.id = c.item_id
         WHERE c.order_id = $1 ORDER BY c.id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let mut matched = vec![false; rows.len()];
    let mut plan = BasketPlan::default();
    for it in doc.items.iter().filter(|i| i.kind == ItemKind::Product) {
        let pos = rows.iter().enumerate().position(|(n, r)| {
            !matched[n]
                && !r.xml_id.is_empty()
                && (r.xml_id == it.xml_id || normalize_xml_id(&r.xml_id) == it.xml_id)
        });
        match pos {
            Some(n) => {
                matched[n] = true;
                let r = &rows[n];
                // Точность столбцов: количество — 3 знака, цена — 2
                let quantity = if it.quantity > 0.0 {
                    round(it.quantity, 3)
                } else {
                    r.quantity
                };
                let price = round(it.price, 2);
                if !same(quantity, r.quantity) || !same(price, r.price) {
                    plan.updates
                        .push((r.id, quantity, price, !same(quantity, r.quantity)));
                }
            }
            None => {
                let (product_id, name) = product_by_xml_id(tx, &it.xml_id).await?;
                if it.quantity > 0.0 {
                    plan.inserts.push((
                        product_id,
                        name,
                        round(it.quantity, 3),
                        round(it.price, 2),
                    ));
                }
            }
        }
    }
    plan.removed = rows
        .iter()
        .zip(&matched)
        .filter(|(_, m)| !**m)
        .map(|(r, _)| r.id)
        .collect();
    Ok(plan)
}

/// Применяет план; остатки — через `stock::plan`, если заказ их списал. Суммы не пересчитывает.
pub(super) async fn apply_basket(
    ctx: &Ctx<'_>,
    tx: &mut PgConnection,
    id: i64,
    plan: &BasketPlan,
) -> anyhow::Result<()> {
    let deducted: bool = sqlx::query_scalar("SELECT stock_deducted FROM orders WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let before = stock_lines(tx, id).await?;
    sqlx::query("DELETE FROM cart_items WHERE id = ANY($1)")
        .bind(&plan.removed)
        .execute(&mut *tx)
        .await?;
    for (line, quantity, price, _) in &plan.updates {
        sqlx::query(
            "UPDATE cart_items SET quantity = $2, price = $3, custom_price = TRUE, updated_at = now()
             WHERE id = $1",
        )
        .bind(line)
        .bind(quantity)
        .bind(price)
        .execute(&mut *tx)
        .await?;
    }
    for (product_id, name, quantity, price) in &plan.inserts {
        insert_line(ctx, tx, id, *product_id, name, *quantity, *price).await?;
    }
    if deducted {
        let after = stock_lines(tx, id).await?;
        let all: Vec<StockLine> = before.iter().chain(&after).cloned().collect();
        let traced = traced_locked(tx, &all).await?;
        stock::apply(tx, &stock::plan(&before, &after, &traced)).await?;
    }
    Ok(())
}

/// Оплачен или есть отгруженная отгрузка — состав не правится.
pub(super) async fn is_final(tx: &mut PgConnection, id: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT paid OR EXISTS (SELECT 1 FROM shipments WHERE order_id = o.id AND deducted)
         FROM orders o WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
}

pub(super) const FINAL_ORDER: &str = "заказ в финальном состоянии, состав не изменён";

/// Состав по документу (`updateOrderWithoutShipmentsPayments`); строка-услуга — доставка.
async fn update_basket(
    ctx: &Ctx<'_>,
    tx: &mut PgConnection,
    id: i64,
    doc: &DocumentIn,
) -> anyhow::Result<()> {
    let plan = plan_basket(tx, id, doc).await?;
    let current_delivery: f64 =
        sqlx::query_scalar("SELECT delivery_price::float8 FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let delivery = delivery_line(doc)
        .map(|d| round(d.sum, 2))
        .filter(|p| !same(*p, current_delivery));
    if plan.is_empty() && delivery.is_none() {
        return Ok(());
    }
    if is_final(tx, id).await? {
        bail!(FINAL_ORDER);
    }
    apply_basket(ctx, tx, id, &plan).await?;
    recalc_totals_in(tx, id, delivery).await?;
    Ok(())
}

async fn insert_line(
    ctx: &Ctx<'_>,
    tx: &mut PgConnection,
    order_id: i64,
    product_id: i64,
    name: &str,
    quantity: f64,
    price: f64,
) -> sqlx::Result<()> {
    let props = crate::collection::sku::basket_props(ctx.db, product_id).await?;
    sqlx::query(
        "INSERT INTO cart_items (item_id, quantity, name, price, currency, order_id, custom_price, props)
         SELECT $1, $2, $3, $4, currency, id, TRUE, $5 FROM orders WHERE id = $6",
    )
    .bind(product_id)
    .bind(quantity)
    .bind(name)
    .bind(price)
    .bind(sqlx::types::Json(&props))
    .bind(order_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Покупатель по Ид контрагента (`users.xml_id`), иначе новый пользователь без пароля.
async fn buyer(tx: &mut PgConnection, agent: &AgentIn) -> anyhow::Result<i64> {
    if let Some(id) = sqlx::query_scalar("SELECT id FROM users WHERE xml_id = $1")
        .bind(&agent.id)
        .fetch_optional(&mut *tx)
        .await?
    {
        return Ok(id);
    }
    let field = |key: &str| agent.fields.get(key).map_or("", |v| v.trim());
    let email = field("EMAIL");
    let mut name = field("NAME").to_string();
    if name.is_empty() && field("SURNAME").is_empty() {
        name = [field("AGENT_NAME"), field("CONTACT_PERSON")]
            .into_iter()
            .find(|v| !v.is_empty())
            .unwrap_or_default()
            .to_string();
    }
    let base = if email.is_empty() {
        format!("buyer_{}", hex::encode(rand::random::<[u8; 4]>()))
    } else {
        email.to_string()
    };
    let mut login = base.clone();
    let mut n = 1;
    while sqlx::query_scalar::<_, i64>("SELECT id FROM users WHERE login = $1")
        .bind(&login)
        .fetch_optional(&mut *tx)
        .await?
        .is_some()
    {
        n += 1;
        login = format!("{base}_{n}");
    }
    Ok(sqlx::query_scalar(
        "INSERT INTO users (login, email, password_hash, name, last_name, second_name, phone, xml_id)
         VALUES ($1, $2, '', $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(&login)
    .bind(Some(email).filter(|e| !e.is_empty()))
    .bind(&name)
    .bind(field("SURNAME"))
    .bind(field("SECOND_NAME"))
    .bind(field("PHONE"))
    .bind(&agent.id)
    .fetch_one(&mut *tx)
    .await?)
}

/// Новый заказ из 1С (`external`): покупатель, тип плательщика по домену контрагента, позиции,
/// свойства по соответствию реквизитов, отгрузка службой по умолчанию (при опции).
async fn create_order(
    ctx: &Ctx<'_>,
    tx: &mut PgConnection,
    doc: &DocumentIn,
    stats: &mut SaleStats,
) -> anyhow::Result<i64> {
    let agent = doc
        .agent_id
        .as_deref()
        .and_then(|id| ctx.agents.iter().find(|a| a.id == id))
        .ok_or_else(|| anyhow!("контрагент не найден"))?;
    let products: Vec<&ItemIn> = doc
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::Product && i.quantity > 0.0)
        .collect();
    if products.is_empty() {
        bail!("в документе нет товаров");
    }
    let person_type: i64 = sqlx::query_scalar(
        "SELECT id FROM person_types WHERE active AND domain = $1 ORDER BY sort, id LIMIT 1",
    )
    .bind(agent.domain.code())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| anyhow!("тип плательщика не найден"))?;
    let mut lines = Vec::new();
    for it in &products {
        let (product_id, name) = product_by_xml_id(tx, &it.xml_id).await?;
        lines.push((product_id, name, it.quantity, it.price));
    }
    let status: String = sqlx::query_scalar(
        "SELECT code FROM order_statuses ORDER BY (code = 'N') DESC, sort, code LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| anyhow!("нет статусов заказа"))?;
    let user_id = buyer(tx, agent).await?;
    let delivery = delivery_line(doc).map_or(0.0, |d| d.sum);

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO orders (user_id, person_type_id, status, delivery_price, stock_deducted,
                             id_1c, version_1c, external, updated_1c)
         VALUES ($1, $2, $3, $4, TRUE, $5, $6, TRUE, TRUE) RETURNING id",
    )
    .bind(user_id)
    .bind(person_type)
    .bind(&status)
    .bind(delivery)
    .bind(&doc.id_1c)
    .bind(&doc.version)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE orders SET account_number = id::text WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO order_status_history (order_id, status, comment) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(&status)
        .bind(HISTORY_COMMENT)
        .execute(&mut *tx)
        .await?;
    for (product_id, name, quantity, price) in &lines {
        insert_line(ctx, tx, id, *product_id, name, *quantity, *price).await?;
    }
    let after = stock_lines(tx, id).await?;
    let traced = traced_locked(tx, &after).await?;
    stock::apply(tx, &stock::plan(&[], &after, &traced)).await?;

    write_properties(ctx, tx, id, person_type, agent).await?;

    if ctx.s.new_order_new_shipment {
        let name: String = match ctx.s.default_delivery {
            Some(d) => sqlx::query_scalar("SELECT name FROM deliveries WHERE id = $1")
                .bind(d)
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or_default(),
            None => String::new(),
        };
        sqlx::query(
            "INSERT INTO shipments (order_id, delivery_id, delivery_name, price) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(ctx.s.default_delivery)
        .bind(&name)
        .bind(delivery)
        .execute(&mut *tx)
        .await?;
        stats.shipments_created += 1;
    }
    recalc_totals_in(tx, id, None).await?;
    Ok(id)
}

/// Свойства заказа из полей контрагента — соответствие реквизитов в обратную сторону:
/// строки «свойство» типа плательщика (или общие), чьё поле есть у контрагента.
async fn write_properties(
    ctx: &Ctx<'_>,
    tx: &mut PgConnection,
    order_id: i64,
    person_type: i64,
    agent: &AgentIn,
) -> sqlx::Result<()> {
    let mut by_code: HashMap<&str, &MapEntry> = HashMap::new();
    for e in ctx.map.iter().filter(|e| e.source == MapSource::Property) {
        if e.person_type_id == person_type
            || (e.person_type_id == 0 && !by_code.contains_key(e.code.as_str()))
        {
            by_code.insert(&e.code, e);
        }
    }
    let mut entries: Vec<&MapEntry> = by_code.into_values().collect();
    entries.sort_by(|a, b| a.code.cmp(&b.code));
    for e in entries {
        let Some((_, key, _)) = CONTRAGENT_CODES.iter().find(|(code, ..)| *code == e.code) else {
            continue;
        };
        let Some(value) = agent.fields.get(*key).filter(|v| !v.is_empty()) else {
            continue;
        };
        // Части местоположения (`<id>_CITY`…) обратно не разбираются
        let Ok(property_id) = e.value.trim().parse::<i64>() else {
            continue;
        };
        sqlx::query(
            "INSERT INTO order_property_values (order_id, property_id, code, name, value)
             SELECT $1, id, code, name, $3 FROM order_properties
             WHERE id = $2 AND person_type_id = $4
             ON CONFLICT (order_id, property_id) DO NOTHING",
        )
        .bind(order_id)
        .bind(property_id)
        .bind(value)
        .bind(person_type)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::bxapi::project::BoxFuture;
    use crate::exchange::sale::document::PayOperation;
    use crate::exchange::sale::parse::{AgentIn, ItemIn, ItemKind, Operation};
    use crate::exchange::sale::settings::{Domain, MapSource};
    use crate::test_support::{Fixture, amounts, order_fixture, place_order};

    fn doc(id: &str, version: &str, traits: &[(&str, &str)]) -> DocumentIn {
        DocumentIn {
            id: id.into(),
            number: id.into(),
            id_1c: format!("ЗК-{id}"),
            version: version.into(),
            date: String::new(),
            operation: Operation::Order,
            base: String::new(),
            deleted: false,
            sum: None,
            agent_id: None,
            items: Vec::new(),
            traits: traits
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn item(xml_id: &str, price: f64, quantity: f64) -> ItemIn {
        ItemIn {
            xml_id: xml_id.into(),
            name: xml_id.into(),
            price,
            quantity,
            sum: price * quantity,
            kind: ItemKind::Product,
        }
    }

    fn delivery(price: f64) -> ItemIn {
        ItemIn {
            kind: ItemKind::Service,
            ..item("ORDER_DELIVERY", price, 1.0)
        }
    }

    fn file(docs: Vec<DocumentIn>) -> SaleFile {
        SaleFile {
            agents: Vec::new(),
            groups: docs.into_iter().map(|d| vec![d]).collect(),
        }
    }

    async fn run(db: &PgPool, s: &SaleSettings, f: &SaleFile) -> SaleStats {
        let (stats, next) = apply_file(db, s, &[], f, None, 0, None).await;
        assert_eq!(next, None);
        stats
    }

    /// Товар фикстуры с Ид для 1С.
    async fn fixture(db: &PgPool) -> Fixture {
        let f = order_fixture(db).await;
        sqlx::query("UPDATE collection_items SET xml_id = 'tovar-1c' WHERE id = $1")
            .bind(f.product_id)
            .execute(db)
            .await
            .unwrap();
        f
    }

    /// Ещё товар каталога (остаток 10, без складов) с Ид для 1С.
    async fn product(db: &PgPool, xml_id: &str) -> i64 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name, xml_id)
             SELECT id, $1, 'Товар ' || $1, $1 FROM collections WHERE code = 'catalog' RETURNING id",
        )
        .bind(xml_id)
        .fetch_one(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO catalog_products (item_id, quantity, quantity_trace, can_buy_zero)
             VALUES ($1, 10, TRUE, FALSE)",
        )
        .bind(id)
        .execute(db)
        .await
        .unwrap();
        id
    }

    async fn quantity(db: &PgPool, product_id: i64) -> f64 {
        sqlx::query_scalar("SELECT quantity::float8 FROM catalog_products WHERE item_id = $1")
            .bind(product_id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn status(db: &PgPool, id: i64) -> (String, String, bool, bool) {
        sqlx::query_as("SELECT status, id_1c, updated_1c, canceled FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn statuses(db: &PgPool) {
        sqlx::query(
            "INSERT INTO order_statuses (code, name, notify) VALUES ('F', 'Выполнен', TRUE),
                 ('P', 'Собирается', FALSE)",
        )
        .execute(db)
        .await
        .unwrap();
    }

    async fn orders_count(db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM orders")
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn find_order_by_id_1c_and_number(db: PgPool) {
        let f = fixture(&db).await;
        let a = place_order(&db, &f, None).await;
        let b = place_order(&db, &f, None).await;
        sqlx::query("UPDATE orders SET id_1c = 'ЗК-7', account_number = 'A-' || id WHERE id = $1")
            .bind(b)
            .execute(&db)
            .await
            .unwrap();
        let mut conn = db.acquire().await.unwrap();
        let mut find =
            async |key: &str, prefix: &str| find_order(&mut conn, key, prefix).await.unwrap();
        assert_eq!(find(&a.to_string(), "").await, Some(a));
        assert_eq!(find("ЗК-7", "").await, Some(b));
        assert_eq!(find(&format!("A-{b}"), "").await, Some(b));
        assert_eq!(find(&format!("SHOP{a}"), "SHOP").await, Some(a));
        assert_eq!(find(&format!("SHOPA-{b}"), "SHOP").await, Some(b));
        assert_eq!(find("нет", "").await, None);
        assert_eq!(find("", "").await, None);
    }

    #[sqlx::test]
    async fn status_changes_only_with_option(db: PgPool) {
        let f = fixture(&db).await;
        statuses(&db).await;
        let id = place_order(&db, &f, None).await;
        let key = id.to_string();

        let off = SaleSettings::default();
        let stats = run(
            &db,
            &off,
            &file(vec![doc(&key, "1", &[("Статуса заказа ИД", "P")])]),
        )
        .await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!(stats.orders_updated, 1);
        assert_eq!(
            status(&db, id).await,
            ("N".into(), format!("ЗК-{id}"), true, false)
        );

        let on = SaleSettings {
            change_status_from_1c: true,
            ..SaleSettings::default()
        };
        let stats = run(
            &db,
            &on,
            &file(vec![doc(&key, "2", &[("Статуса заказа ИД", "P")])]),
        )
        .await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!(
            status(&db, id).await,
            ("P".into(), format!("ЗК-{id}"), true, false)
        );
        let history: (String, String, Option<i64>) = sqlx::query_as(
            "SELECT status, comment, user_id FROM order_status_history WHERE order_id = $1
             ORDER BY id DESC LIMIT 1",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(history, ("P".into(), "1С".into(), None));
        // Статус без письма
        assert!(stats.notify.is_empty());

        // Неизвестный статус — ошибка документа
        let stats = run(
            &db,
            &on,
            &file(vec![doc(&key, "3", &[("Статуса заказа ИД", "Q")])]),
        )
        .await;
        assert_eq!(
            stats.errors,
            vec![format!("Документ {id}: статус Q не найден")]
        );
    }

    #[sqlx::test]
    async fn status_change_sends_mail_once(db: PgPool) {
        let f = fixture(&db).await;
        statuses(&db).await;
        let id = place_order(&db, &f, None).await;
        let s = SaleSettings {
            change_status_from_1c: true,
            ..SaleSettings::default()
        };
        let f1 = file(vec![doc(
            &id.to_string(),
            "1",
            &[("Статуса заказа ИД", "F")],
        )]);
        let stats = run(&db, &s, &f1).await;
        assert_eq!(
            stats.notify,
            vec![("SALE_STATUS_CHANGED_F".to_string(), id)]
        );
        // Повтор того же файла: версия не новее — документ пропущен, письма нет
        let stats = run(&db, &s, &f1).await;
        assert!(stats.notify.is_empty());
        assert_eq!(stats.skipped, 1);
        // И без версии статус уже тот же — письма нет
        let f2 = file(vec![doc(
            &id.to_string(),
            "",
            &[("Статуса заказа ИД", "F")],
        )]);
        let stats = run(&db, &s, &f2).await;
        assert!(stats.notify.is_empty());
        assert_eq!(stats.orders_updated, 1);
    }

    #[sqlx::test]
    async fn cancel_and_uncancel_restore_stock(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let s = SaleSettings::default();
        assert_eq!(amounts(&db, f.product_id).await, (10.0, 5.0));
        let stats = run(
            &db,
            &s,
            &file(vec![doc(&id.to_string(), "1", &[("Отменен", "true")])]),
        )
        .await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert!(status(&db, id).await.3);
        assert_eq!(amounts(&db, f.product_id).await, (11.0, 5.0));
        // Документ без реквизита «Отменен» отмену не снимает
        run(&db, &s, &file(vec![doc(&id.to_string(), "2", &[])])).await;
        assert!(status(&db, id).await.3);
        run(
            &db,
            &s,
            &file(vec![doc(&id.to_string(), "3", &[("Отменен", "false")])]),
        )
        .await;
        assert!(!status(&db, id).await.3);
        assert_eq!(amounts(&db, f.product_id).await, (10.0, 5.0));
    }

    #[sqlx::test]
    async fn deletion_mark_cancels(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let mut d = doc(&id.to_string(), "1", &[("Отменен", "false")]);
        d.deleted = true;
        let stats = run(&db, &SaleSettings::default(), &file(vec![d])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert!(status(&db, id).await.3);
        assert_eq!(amounts(&db, f.product_id).await, (11.0, 5.0));
    }

    #[sqlx::test]
    async fn older_version_is_skipped(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        sqlx::query("UPDATE orders SET version_1c = '5' WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let s = SaleSettings::default();
        for version in ["4", "5"] {
            let stats = run(
                &db,
                &s,
                &file(vec![doc(&id.to_string(), version, &[("Отменен", "true")])]),
            )
            .await;
            assert_eq!((stats.skipped, stats.orders_updated), (1, 0));
            assert_eq!(
                status(&db, id).await,
                ("N".into(), String::new(), false, false)
            );
        }
        // Числа сравниваются как числа: 10 новее 5
        let stats = run(
            &db,
            &s,
            &file(vec![doc(&id.to_string(), "10", &[("Отменен", "true")])]),
        )
        .await;
        assert_eq!((stats.skipped, stats.orders_updated), (0, 1));
        assert!(status(&db, id).await.3);
        let version: String = sqlx::query_scalar("SELECT version_1c FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(version, "10");
    }

    async fn lines(db: &PgPool, id: i64) -> Vec<(i64, f64, f64, bool)> {
        sqlx::query_as(
            "SELECT item_id, quantity::float8, price::float8, custom_price FROM cart_items
             WHERE order_id = $1 ORDER BY item_id",
        )
        .bind(id)
        .fetch_all(db)
        .await
        .unwrap()
    }

    async fn totals(db: &PgPool, id: i64) -> (f64, f64, f64, f64, f64) {
        sqlx::query_as(
            "SELECT goods_price::float8, delivery_price::float8, price::float8,
                    (SELECT sum::float8 FROM payments WHERE order_id = o.id),
                    (SELECT price::float8 FROM shipments WHERE order_id = o.id)
             FROM orders o WHERE id = $1",
        )
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
    }

    /// Заказ из товара фикстуры (1 × 100) и второго товара (1 × 50, остаток не списывался).
    async fn two_line_order(db: &PgPool, f: &Fixture) -> (i64, i64) {
        let id = place_order(db, f, None).await;
        let second = product(db, "p2-1c").await;
        sqlx::query(
            "INSERT INTO cart_items (item_id, quantity, name, price, order_id) VALUES ($1, 1, 'p2', 50, $2)",
        )
        .bind(second)
        .bind(id)
        .execute(db)
        .await
        .unwrap();
        (id, second)
    }

    #[sqlx::test]
    async fn basket_update_with_option(db: PgPool) {
        let f = fixture(&db).await;
        let (id, second) = two_line_order(&db, &f).await;
        let third = product(&db, "p3-1c").await;
        let mut d = doc(&id.to_string(), "1", &[]);
        d.items = vec![
            item("tovar-1c", 90.0, 3.0),
            item("p3-1c", 30.0, 2.0),
            delivery(250.0),
            ItemIn {
                kind: ItemKind::Unknown,
                ..item("skidka", 1.0, 1.0)
            },
        ];
        let stats = run(&db, &SaleSettings::default(), &file(vec![d])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!(
            lines(&db, id).await,
            vec![(f.product_id, 3.0, 90.0, true), (third, 2.0, 30.0, true)]
        );
        assert_eq!(totals(&db, id).await, (330.0, 250.0, 580.0, 580.0, 250.0));
        // Остатки: +2 товара фикстуры, второй вернулся, третий списан
        assert_eq!(quantity(&db, f.product_id).await, 8.0);
        assert_eq!(quantity(&db, second).await, 11.0);
        assert_eq!(quantity(&db, third).await, 8.0);
        let name: String = sqlx::query_scalar("SELECT name FROM cart_items WHERE item_id = $1")
            .bind(third)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(name, "Товар p3-1c");
        assert!(status(&db, id).await.2);
    }

    #[sqlx::test]
    async fn basket_kept_without_option_and_for_final_order(db: PgPool) {
        let f = fixture(&db).await;
        let (id, second) = two_line_order(&db, &f).await;
        let mut d = doc(&id.to_string(), "1", &[]);
        d.items = vec![item("tovar-1c", 90.0, 3.0)];
        let off = SaleSettings {
            update_basket_quantity: false,
            ..SaleSettings::default()
        };
        let stats = run(&db, &off, &file(vec![d.clone()])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!(stats.orders_updated, 1);
        assert_eq!(
            lines(&db, id).await,
            vec![
                (f.product_id, 1.0, 100.0, false),
                (second, 1.0, 50.0, false)
            ]
        );

        // Оплаченный заказ по составу не правится: документ откатывается целиком
        sqlx::query("UPDATE orders SET paid = TRUE, id_1c = '' WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        d.version = "2".into();
        let stats = run(&db, &SaleSettings::default(), &file(vec![d.clone()])).await;
        assert_eq!(
            stats.errors,
            vec![format!(
                "Документ {id}: заказ в финальном состоянии, состав не изменён"
            )]
        );
        assert_eq!(stats.orders_updated, 0);
        assert_eq!(status(&db, id).await.1, "");
        assert_eq!(lines(&db, id).await.len(), 2);

        // Отгруженный — тоже; тот же состав — не ошибка
        sqlx::query("UPDATE orders SET paid = FALSE WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE shipments SET deducted = TRUE WHERE order_id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let stats = run(&db, &SaleSettings::default(), &file(vec![d])).await;
        assert_eq!(stats.errors.len(), 1);
        let mut same = doc(&id.to_string(), "3", &[]);
        same.items = vec![item("tovar-1c", 100.0, 1.0), item("p2-1c", 50.0, 1.0)];
        let stats = run(&db, &SaleSettings::default(), &file(vec![same])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
    }

    fn agent(id: &str, domain: Domain, fields: &[(&str, &str)]) -> AgentIn {
        AgentIn {
            id: id.into(),
            domain,
            fields: fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[sqlx::test]
    async fn new_order_with_new_and_existing_agent(db: PgPool) {
        let f = fixture(&db).await;
        let company: i64 = sqlx::query_scalar(
            "INSERT INTO person_types (name, domain, sort) VALUES ('Юрлицо', 'E', 200) RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        // Неактивный тип того же домена не выбирается
        sqlx::query("INSERT INTO person_types (name, domain, sort, active) VALUES ('Старый', 'E', 1, FALSE)")
            .execute(&db)
            .await
            .unwrap();
        let existing: i64 = sqlx::query_scalar(
            "INSERT INTO users (login, password_hash, xml_id) VALUES ('romashka', 'x', 'org-77') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let surname: i64 = sqlx::query_scalar(
            "INSERT INTO order_properties (person_type_id, code, name) VALUES ($1, 'LAST', 'Фамилия') RETURNING id",
        )
        .bind(f.person_type_id)
        .fetch_one(&db)
        .await
        .unwrap();
        let delivery_id: i64 =
            sqlx::query_scalar("INSERT INTO deliveries (name) VALUES ('Курьер') RETURNING id")
                .fetch_one(&db)
                .await
                .unwrap();
        let map = vec![
            MapEntry {
                person_type_id: f.person_type_id,
                code: "BUYER_PERSON_NAME_LAST".into(),
                name: String::new(),
                source: MapSource::Property,
                value: surname.to_string(),
            },
            // Поле, которого нет у контрагента, не пишется
            MapEntry {
                person_type_id: f.person_type_id,
                code: "BUYER_PERSON_ZIP".into(),
                name: String::new(),
                source: MapSource::Property,
                value: "999".into(),
            },
        ];
        let s = SaleSettings {
            default_delivery: Some(delivery_id),
            ..SaleSettings::default()
        };
        let mut org = doc("ord-1c-501", "1", &[]);
        org.agent_id = Some("org-77".into());
        org.items = vec![item("tovar-1c", 1000.0, 3.0), delivery(300.0)];
        let mut fiz = doc("ord-1c-502", "1", &[]);
        fiz.agent_id = Some("fiz-12".into());
        fiz.items = vec![item("tovar-1c", 500.0, 1.0)];
        let sale = SaleFile {
            agents: vec![
                agent("org-77", Domain::Entity, &[("INN", "7701234567")]),
                agent(
                    "fiz-12",
                    Domain::Individual,
                    &[
                        ("SURNAME", "Сидоров"),
                        ("NAME", "Алексей"),
                        ("EMAIL", "sidorov@example.com"),
                        ("PHONE", "+7 900 111-22-33"),
                    ],
                ),
            ],
            groups: vec![vec![org], vec![fiz]],
        };
        let (stats, _) = apply_file(&db, &s, &map, &sale, None, 0, None).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!((stats.orders_created, stats.shipments_created), (2, 2));

        type Row = (
            i64,
            Option<i64>,
            i64,
            bool,
            bool,
            String,
            String,
            f64,
            f64,
            String,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, user_id, person_type_id, external, updated_1c, id_1c, version_1c,
                    price::float8, delivery_price::float8, status
             FROM orders ORDER BY id",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        let (org_id, org_user, org_type, external, marked, id_1c, version, price, dp, st) =
            rows[0].clone();
        assert_eq!(org_user, Some(existing));
        assert_eq!(org_type, company);
        assert!(external && marked);
        assert_eq!((id_1c.as_str(), version.as_str()), ("ЗК-ord-1c-501", "1"));
        assert_eq!((price, dp, st.as_str()), (3300.0, 300.0, "N"));
        assert_eq!(
            lines(&db, org_id).await,
            vec![(f.product_id, 3.0, 1000.0, true)]
        );
        let shipment: (Option<i64>, String, f64) = sqlx::query_as(
            "SELECT delivery_id, delivery_name, price::float8 FROM shipments WHERE order_id = $1",
        )
        .bind(org_id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(shipment, (Some(delivery_id), "Курьер".into(), 300.0));
        // Остаток списан: 3 + 1
        assert_eq!(quantity(&db, f.product_id).await, 6.0);

        let (fiz_id, fiz_user, fiz_type, ..) = rows[1].clone();
        assert_eq!(fiz_type, f.person_type_id);
        let user: (String, Option<String>, String, String, String, String) = sqlx::query_as(
            "SELECT login, email, password_hash, name, last_name, xml_id FROM users WHERE id = $1",
        )
        .bind(fiz_user.unwrap())
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(
            user,
            (
                "sidorov@example.com".into(),
                Some("sidorov@example.com".into()),
                String::new(),
                "Алексей".into(),
                "Сидоров".into(),
                "fiz-12".into()
            )
        );
        let props: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT property_id, code, value FROM order_property_values WHERE order_id = $1",
        )
        .bind(fiz_id)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(props, vec![(surname, "LAST".into(), "Сидоров".into())]);

        // Повтор файла: заказы найдены по Номер1С, дублей нет
        let (stats, _) = apply_file(&db, &s, &map, &sale, None, 0, None).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!((stats.orders_created, stats.skipped), (0, 2));
        assert_eq!(orders_count(&db).await, 2);

        // Без опции отгрузки и без типа плательщика нужного домена
        sqlx::query("UPDATE person_types SET active = FALSE WHERE domain = 'E'")
            .execute(&db)
            .await
            .unwrap();
        let no_ship = SaleSettings {
            new_order_new_shipment: false,
            ..s.clone()
        };
        let mut again = sale.clone();
        again.groups[0][0].id = "ord-1c-601".into();
        again.groups[0][0].id_1c = "ЗК-601".into();
        again.groups[1][0].id = "ord-1c-602".into();
        again.groups[1][0].id_1c = "ЗК-602".into();
        let (stats, _) = apply_file(&db, &no_ship, &map, &again, None, 0, None).await;
        assert_eq!(
            stats.errors,
            vec!["Документ ord-1c-601: тип плательщика не найден".to_string()]
        );
        assert_eq!((stats.orders_created, stats.shipments_created), (1, 0));
    }

    #[sqlx::test]
    async fn new_order_refused_when_disabled(db: PgPool) {
        let f = fixture(&db).await;
        place_order(&db, &f, None).await;
        let s = SaleSettings {
            import_new_orders: false,
            ..SaleSettings::default()
        };
        let mut d = doc("ord-1c-9", "1", &[]);
        d.items = vec![item("tovar-1c", 10.0, 1.0)];
        let stats = run(&db, &s, &file(vec![d])).await;
        assert_eq!(
            stats.errors,
            vec!["Документ ord-1c-9: заказ не найден".to_string()]
        );
        assert_eq!(orders_count(&db).await, 1);
    }

    #[sqlx::test]
    async fn unknown_product_fails_only_its_document(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let first = doc(&id.to_string(), "1", &[("Отменен", "true")]);
        let mut second = doc(&id.to_string(), "2", &[]);
        second.items = vec![item("tovar-1c", 100.0, 1.0), item("chuzhoy", 5.0, 1.0)];
        let stats = run(&db, &SaleSettings::default(), &file(vec![first, second])).await;
        assert_eq!(
            stats.errors,
            vec![format!("Документ {id}: товар chuzhoy не найден")]
        );
        assert_eq!(stats.orders_updated, 1);
        assert!(status(&db, id).await.3);
        assert_eq!(lines(&db, id).await.len(), 1);

        // Документы одного контейнера — одна транзакция: ошибка откатывает и первый
        let other = place_order(&db, &f, None).await;
        let ok = doc(&other.to_string(), "1", &[("Отменен", "true")]);
        let mut bad = doc(&id.to_string(), "3", &[]);
        bad.items = vec![item("chuzhoy", 5.0, 1.0)];
        let container = SaleFile {
            agents: Vec::new(),
            groups: vec![vec![ok, bad]],
        };
        let stats = run(&db, &SaleSettings::default(), &container).await;
        assert_eq!(stats.errors.len(), 1);
        assert_eq!(stats.orders_updated, 0);
        assert!(!status(&db, other).await.3);

        // Оплата и неизвестная операция — ошибка документа
        let mut pay = doc(&id.to_string(), "4", &[]);
        pay.operation = Operation::Unknown("Возврат товара".into());
        let stats = run(&db, &SaleSettings::default(), &file(vec![pay])).await;
        assert_eq!(
            stats.errors,
            vec![format!(
                "Документ {id}: неизвестная хозяйственная операция «Возврат товара»"
            )]
        );
    }

    struct Hook {
        seen: Mutex<Vec<i64>>,
        fail: bool,
    }

    impl OrderImportHook for Hook {
        fn after_document<'a>(
            &'a self,
            conn: &'a mut PgConnection,
            order_id: i64,
            doc: &'a DocumentIn,
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.seen.lock().unwrap().push(order_id);
                sqlx::query("UPDATE orders SET manager_comment = $2 WHERE id = $1")
                    .bind(order_id)
                    .bind(&doc.id_1c)
                    .execute(&mut *conn)
                    .await?;
                anyhow::ensure!(!self.fail, "проект против");
                Ok(())
            })
        }
    }

    #[sqlx::test]
    async fn import_hook_runs_in_transaction(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let s = SaleSettings::default();
        let ok = Arc::new(Hook {
            seen: Mutex::default(),
            fail: false,
        });
        let sale = file(vec![doc(&id.to_string(), "1", &[])]);
        let (stats, _) = apply_file(&db, &s, &[], &sale, Some(ok.as_ref()), 0, None).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!(*ok.seen.lock().unwrap(), vec![id]);
        let comment: String =
            sqlx::query_scalar("SELECT manager_comment FROM orders WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(comment, format!("ЗК-{id}"));

        let bad = Hook {
            seen: Mutex::default(),
            fail: true,
        };
        let sale = file(vec![doc(&id.to_string(), "2", &[("Отменен", "true")])]);
        let (stats, _) = apply_file(&db, &s, &[], &sale, Some(&bad), 0, None).await;
        assert_eq!(stats.errors, vec![format!("Документ {id}: проект против")]);
        assert_eq!(stats.orders_updated, 0);
        assert!(!status(&db, id).await.3);
    }

    #[sqlx::test]
    async fn stops_after_deadline(db: PgPool) {
        let f = fixture(&db).await;
        let a = place_order(&db, &f, None).await;
        let b = place_order(&db, &f, None).await;
        let sale = file(vec![
            doc(&a.to_string(), "1", &[("Отменен", "true")]),
            doc(&b.to_string(), "1", &[("Отменен", "true")]),
        ]);
        let s = SaleSettings::default();
        let (stats, next) = apply_file(&db, &s, &[], &sale, None, 0, Some(Instant::now())).await;
        assert_eq!((stats.orders_updated, next), (1, Some(1)));
        assert!(status(&db, a).await.3 && !status(&db, b).await.3);
        let (stats, next) = apply_file(&db, &s, &[], &sale, None, 1, Some(Instant::now())).await;
        assert_eq!((stats.orders_updated, next), (1, None));
        assert!(status(&db, b).await.3);
    }

    #[sqlx::test]
    async fn lookup_follows_document_number_then_id_1c(db: PgPool) {
        let f = fixture(&db).await;
        let a = place_order(&db, &f, None).await;
        let b = place_order(&db, &f, None).await;
        let c = place_order(&db, &f, None).await;
        sqlx::query("UPDATE orders SET account_number = 'A-' || id WHERE id = $1")
            .bind(a)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE orders SET id_1c = 'guid-c' WHERE id = $1")
            .bind(c)
            .execute(&db)
            .await
            .unwrap();
        let s = SaleSettings {
            number_prefix: "SHOP".into(),
            import_new_orders: false,
            ..SaleSettings::default()
        };
        let cancel = |id: &str, number: &str, id_1c: &str| DocumentIn {
            number: number.into(),
            id_1c: id_1c.into(),
            ..doc(id, "", &[("Отменен", "true")])
        };

        // (а) Ид — GUID из 1С, Номер — префикс + номер заказа
        let stats = run(
            &db,
            &s,
            &file(vec![cancel("guid-a", &format!("SHOPA-{a}"), "ЗК-1")]),
        )
        .await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert!(status(&db, a).await.3);

        // (б) Номер1С, совпадающий с id другого заказа, этот заказ не находит
        let stats = run(
            &db,
            &s,
            &file(vec![cancel("guid-x", "нет", &b.to_string())]),
        )
        .await;
        assert_eq!(
            stats.errors,
            vec!["Документ guid-x: заказ не найден".to_string()]
        );
        // Ид-число — тоже только по id_1c
        let stats = run(&db, &s, &file(vec![cancel(&b.to_string(), "нет", "ЗК-2")])).await;
        assert_eq!(stats.errors.len(), 1);
        assert!(!status(&db, b).await.3);

        // (в) Ид совпадает с сохранённым id_1c
        let stats = run(&db, &s, &file(vec![cancel("guid-c", "нет", "ЗК-3")])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert!(status(&db, c).await.3);
    }

    #[sqlx::test]
    async fn line_price_compared_with_cents(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        sqlx::query("UPDATE orders SET paid = TRUE WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let mut d = doc(&id.to_string(), "1", &[]);
        d.items = vec![item("tovar-1c", 100.004, 1.0)];
        let stats = run(&db, &SaleSettings::default(), &file(vec![d])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!(stats.orders_updated, 1);
        assert_eq!(
            lines(&db, id).await,
            vec![(f.product_id, 1.0, 100.0, false)]
        );
    }

    // --- Оплаты и отгрузки ---

    fn pay_doc(number: &str, base: i64, version: &str, op: PayOperation) -> DocumentIn {
        DocumentIn {
            id: format!("pay-guid-{number}"),
            number: number.into(),
            id_1c: format!("ПП-{number}"),
            operation: Operation::Payment(op),
            base: base.to_string(),
            ..doc(number, version, &[])
        }
    }

    fn ship_doc(number: &str, base: i64, version: &str) -> DocumentIn {
        DocumentIn {
            id: format!("ship-guid-{number}"),
            number: number.into(),
            id_1c: format!("РН-{number}"),
            operation: Operation::Shipment,
            base: base.to_string(),
            ..doc(number, version, &[])
        }
    }

    fn with(mut d: DocumentIn, traits: &[(&str, &str)]) -> DocumentIn {
        for (k, v) in traits {
            d.traits.insert(k.to_string(), v.to_string());
        }
        d
    }

    /// Первая (единственная) оплата и отгрузка заказа фикстуры.
    async fn first_payment(db: &PgPool, order: i64) -> i64 {
        sqlx::query_scalar("SELECT min(id) FROM payments WHERE order_id = $1")
            .bind(order)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn first_shipment(db: &PgPool, order: i64) -> i64 {
        sqlx::query_scalar("SELECT min(id) FROM shipments WHERE order_id = $1")
            .bind(order)
            .fetch_one(db)
            .await
            .unwrap()
    }

    type PaymentRow = (
        bool,
        String,
        Option<chrono::NaiveDate>,
        bool,
        String,
        String,
        String,
        bool,
    );

    async fn payment(db: &PgPool, id: i64) -> PaymentRow {
        sqlx::query_as(
            "SELECT paid, pay_voucher_num, pay_voucher_date, is_return, return_comment, id_1c,
                    version_1c, updated_1c
             FROM payments WHERE id = $1",
        )
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn order_paid(db: &PgPool, id: i64) -> bool {
        sqlx::query_scalar("SELECT paid FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn count(db: &PgPool, table: &str, order: i64) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE order_id = $1"
        )))
        .bind(order)
        .fetch_one(db)
        .await
        .unwrap()
    }

    fn date(s: &str) -> Option<chrono::NaiveDate> {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
    }

    #[sqlx::test]
    async fn payment_paid_with_voucher(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let p1 = first_payment(&db, id).await;
        let p2: i64 = sqlx::query_scalar(
            "INSERT INTO payments (order_id, name, sum) VALUES ($1, 'Остаток', 0) RETURNING id",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        let s = SaleSettings::default();

        let d = with(
            pay_doc(&p1.to_string(), id, "1", PayOperation::Cashless),
            &[
                ("Оплачен", "true"),
                ("Дата оплаты по 1С", "2026-10-14T10:20:30"),
                ("Номер оплаты по 1С", "000123"),
            ],
        );
        let stats = run(&db, &s, &file(vec![d])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!(stats.payments_updated, 1);
        assert_eq!(
            payment(&db, p1).await,
            (
                true,
                "000123".into(),
                date("2026-10-14"),
                false,
                String::new(),
                format!("ПП-{p1}"),
                "1".into(),
                true
            )
        );
        // Вторая оплата не тронута, заказ ещё не оплачен
        assert!(!payment(&db, p2).await.0);
        assert!(!order_paid(&db, id).await);
        assert!(stats.notify.is_empty());
        assert!(status(&db, id).await.2);

        // Вторая — по Ид, сохранённому в id_1c; теперь оплачены все — заказ оплачен
        sqlx::query("UPDATE payments SET id_1c = 'guid-p2' WHERE id = $1")
            .bind(p2)
            .execute(&db)
            .await
            .unwrap();
        let d = DocumentIn {
            number: String::new(),
            id: "guid-p2".into(),
            id_1c: "guid-p2".into(),
            ..with(
                pay_doc("x", id, "1", PayOperation::Cash),
                &[("Оплачен", "true")],
            )
        };
        let stats = run(&db, &s, &file(vec![d])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert!(payment(&db, p2).await.0);
        assert!(order_paid(&db, id).await);
        assert_eq!(stats.notify, vec![("SALE_ORDER_PAID".to_string(), id)]);

        // Номер чужой оплаты не находит оплату этого заказа; без создания — ошибка
        let other = place_order(&db, &f, None).await;
        let no_new = SaleSettings {
            import_new_payments: false,
            ..SaleSettings::default()
        };
        let d = pay_doc(
            &first_payment(&db, other).await.to_string(),
            id,
            "1",
            PayOperation::Cash,
        );
        let key = d.id.clone();
        let stats = run(&db, &no_new, &file(vec![d])).await;
        assert_eq!(
            stats.errors,
            vec![format!("Документ {key}: оплата не найдена")]
        );

        // Основание не найдено — заказ не найден
        let d = pay_doc("chuzhaya", 999_999, "1", PayOperation::Cash);
        let key = d.id.clone();
        let stats = run(&db, &s, &file(vec![d])).await;
        assert_eq!(
            stats.errors,
            vec![format!("Документ {key}: заказ не найден")]
        );
    }

    #[sqlx::test]
    async fn payment_return_unpays(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let p = first_payment(&db, id).await;
        let s = SaleSettings::default();
        let paid = with(
            pay_doc(&p.to_string(), id, "1", PayOperation::Cashless),
            &[("Оплачен", "true")],
        );
        run(&db, &s, &file(vec![paid])).await;
        assert!(order_paid(&db, id).await);

        let ret = with(
            pay_doc(&p.to_string(), id, "2", PayOperation::Cashless),
            &[
                ("Оплачен", "true"),
                ("ПризнакВозврата", "true"),
                ("Причина к Возврату", "Брак"),
            ],
        );
        let stats = run(&db, &s, &file(vec![ret])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        let row = payment(&db, p).await;
        assert_eq!((row.0, row.3, row.4.as_str()), (false, true, "Брак"));
        assert!(!order_paid(&db, id).await);

        // «Отменен» тоже снимает оплату
        let again = with(
            pay_doc(&p.to_string(), id, "3", PayOperation::Cashless),
            &[("Оплачен", "true")],
        );
        run(&db, &s, &file(vec![again])).await;
        assert!(payment(&db, p).await.0 && order_paid(&db, id).await);
        let cancel = with(
            pay_doc(&p.to_string(), id, "4", PayOperation::Cashless),
            &[("Оплачен", "true"), ("Отменен", "true")],
        );
        let stats = run(&db, &s, &file(vec![cancel])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert!(!payment(&db, p).await.0 && !order_paid(&db, id).await);
    }

    #[sqlx::test]
    async fn new_payment_uses_default_pay_system_by_operation(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let mut ps = Vec::new();
        for name in ["Наличные", "Безнал", "Эквайринг", "Оплачено в 1С"]
        {
            let ps_id: i64 =
                sqlx::query_scalar("INSERT INTO pay_systems (name) VALUES ($1) RETURNING id")
                    .bind(name)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            ps.push(ps_id);
        }
        let s = SaleSettings {
            default_ps_cash: Some(ps[0]),
            default_ps_cashless: Some(ps[1]),
            default_ps_acquiring: Some(ps[2]),
            default_ps_order_paid: Some(ps[3]),
            ..SaleSettings::default()
        };
        let new = |number: &str, op, sum: f64, paid: bool| DocumentIn {
            sum: Some(sum),
            ..with(
                pay_doc(number, id, "1", op),
                &[("Оплачен", if paid { "true" } else { "false" })],
            )
        };
        let sale = file(vec![
            new("1С-01", PayOperation::Cash, 10.0, false),
            new("1С-02", PayOperation::Cashless, 20.0, true),
            new("1С-03", PayOperation::Acquiring, 30.0, false),
            // Оплата всей суммы заказа
            new("1С-04", PayOperation::Cash, 100.0, true),
        ]);
        let stats = run(&db, &s, &sale).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!((stats.payments_created, stats.payments_updated), (4, 0));
        type Row = (Option<i64>, String, f64, bool, String, String, bool);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT pay_system_id, name, sum::float8, paid, id_1c, version_1c, updated_1c
             FROM payments WHERE order_id = $1 AND id_1c <> '' ORDER BY id",
        )
        .bind(id)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    Some(ps[0]),
                    "Наличные".into(),
                    10.0,
                    false,
                    "ПП-1С-01".into(),
                    "1".into(),
                    true
                ),
                (
                    Some(ps[1]),
                    "Безнал".into(),
                    20.0,
                    true,
                    "ПП-1С-02".into(),
                    "1".into(),
                    true
                ),
                (
                    Some(ps[2]),
                    "Эквайринг".into(),
                    30.0,
                    false,
                    "ПП-1С-03".into(),
                    "1".into(),
                    true
                ),
                (
                    Some(ps[3]),
                    "Оплачено в 1С".into(),
                    100.0,
                    true,
                    "ПП-1С-04".into(),
                    "1".into(),
                    true
                ),
            ]
        );
        // Исходная оплата заказа не оплачена — заказ не оплачен
        assert!(!order_paid(&db, id).await);

        // Без настройки «Создавать новые оплаты» — ошибка
        let off = SaleSettings {
            import_new_payments: false,
            ..s.clone()
        };
        let stats = run(
            &db,
            &off,
            &file(vec![new("1С-05", PayOperation::Cash, 1.0, false)]),
        )
        .await;
        assert_eq!(
            stats.errors,
            vec!["Документ pay-guid-1С-05: оплата не найдена".to_string()]
        );
        assert_eq!(count(&db, "payments", id).await, 5);
    }

    #[sqlx::test]
    async fn payment_deletion_rules(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let p1 = first_payment(&db, id).await;
        let p2: i64 = sqlx::query_scalar(
            "INSERT INTO payments (order_id, name, sum, paid, paid_at) VALUES ($1, 'Оплачена', 5, TRUE, now())
             RETURNING id",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        let s = SaleSettings::default();
        let del = |p: i64| DocumentIn {
            deleted: true,
            ..pay_doc(&p.to_string(), id, "1", PayOperation::Cash)
        };

        // Неоплаченная удаляется; остались только оплаченные — заказ оплачен
        let stats = run(&db, &s, &file(vec![del(p1)])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!(stats.payments_updated, 1);
        assert_eq!(count(&db, "payments", id).await, 1);
        assert!(order_paid(&db, id).await);

        // Оплаченная — ошибка, остаётся
        let stats = run(&db, &s, &file(vec![del(p2)])).await;
        assert_eq!(
            stats.errors,
            vec![format!(
                "Документ pay-guid-{p2}: оплата проведена, удаление невозможно"
            )]
        );
        assert_eq!(count(&db, "payments", id).await, 1);

        // Не найдена — ошибка, а не новая оплата
        let stats = run(&db, &s, &file(vec![del(999_999)])).await;
        assert_eq!(
            stats.errors,
            vec!["Документ pay-guid-999999: оплата не найдена".to_string()]
        );
        assert_eq!(count(&db, "payments", id).await, 1);

        // Оплаты отменённого заказа и заказа в статусе F не меняются
        sqlx::query("UPDATE orders SET canceled = TRUE WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let d = pay_doc(&p2.to_string(), id, "2", PayOperation::Cash);
        let stats = run(&db, &s, &file(vec![d.clone()])).await;
        assert_eq!(
            stats.errors,
            vec![format!("Документ pay-guid-{p2}: заказ отменён")]
        );
        statuses(&db).await;
        sqlx::query("UPDATE orders SET canceled = FALSE, status = 'F' WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let stats = run(&db, &s, &file(vec![d])).await;
        assert_eq!(
            stats.errors,
            vec![format!("Документ pay-guid-{p2}: заказ в финальном статусе")]
        );
    }

    type ShipmentRow = (
        bool,
        bool,
        String,
        Option<chrono::NaiveDate>,
        String,
        String,
        String,
        bool,
        f64,
    );

    async fn shipment(db: &PgPool, id: i64) -> ShipmentRow {
        sqlx::query_as(
            "SELECT deducted, allow_delivery, delivery_doc_num, delivery_doc_date, tracking_number,
                    id_1c, version_1c, updated_1c, price::float8
             FROM shipments WHERE id = $1",
        )
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn shipped_status(db: &PgPool) {
        sqlx::query(
            "INSERT INTO order_statuses (code, name, notify) VALUES ('S', 'Отгружен', TRUE)",
        )
        .execute(db)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn shipment_deducted_tracking_and_final_status(db: PgPool) {
        let f = fixture(&db).await;
        shipped_status(&db).await;
        let id = place_order(&db, &f, None).await;
        let sh = first_shipment(&db, id).await;
        let s = SaleSettings {
            final_status_on_delivery: Some("S".into()),
            ..SaleSettings::default()
        };

        // Только разрешение доставки: не отгружена, статус прежний
        let allow = with(
            ship_doc(&sh.to_string(), id, "1"),
            &[("Доставка разрешена", "true")],
        );
        let stats = run(&db, &s, &file(vec![allow])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!(stats.shipments_updated, 1);
        let row = shipment(&db, sh).await;
        assert_eq!((row.0, row.1), (false, true));
        assert_eq!(status(&db, id).await.0, "N");

        let d = with(
            ship_doc(&sh.to_string(), id, "2"),
            &[
                ("Дата отгрузки по 1С", "2026-10-15T09:00:00"),
                ("Номер отгрузки по 1С", "РН-0077"),
                ("Идентификатор отправления", "TRACK-42"),
            ],
        );
        let stats = run(&db, &s, &file(vec![d])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!(
            shipment(&db, sh).await,
            (
                true,
                true,
                "РН-0077".into(),
                date("2026-10-15"),
                "TRACK-42".into(),
                format!("РН-{sh}"),
                "2".into(),
                true,
                0.0
            )
        );
        let (st, _, marked, _) = status(&db, id).await;
        assert_eq!((st.as_str(), marked), ("S", true));
        let comment: String = sqlx::query_scalar(
            "SELECT comment FROM order_status_history WHERE order_id = $1 ORDER BY id DESC LIMIT 1",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(comment, "1С");
        assert_eq!(
            stats.notify,
            vec![("SALE_STATUS_CHANGED_S".to_string(), id)]
        );
        // Отгрузка остатки не списывает
        assert_eq!(amounts(&db, f.product_id).await, (10.0, 5.0));

        // Удаление отгруженной — ошибка
        let del = DocumentIn {
            deleted: true,
            ..ship_doc(&sh.to_string(), id, "3")
        };
        let stats = run(&db, &s, &file(vec![del])).await;
        assert_eq!(
            stats.errors,
            vec![format!(
                "Документ ship-guid-{sh}: отгрузка проведена, удаление невозможно"
            )]
        );
        assert_eq!(count(&db, "shipments", id).await, 1);

        // Неотгруженная удаляется
        let other = place_order(&db, &f, None).await;
        let osh = first_shipment(&db, other).await;
        let del = DocumentIn {
            deleted: true,
            ..ship_doc(&osh.to_string(), other, "1")
        };
        let stats = run(&db, &s, &file(vec![del])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!(count(&db, "shipments", other).await, 0);
    }

    #[sqlx::test]
    async fn shipment_deducted_by_flag_with_document_date(db: PgPool) {
        let f = fixture(&db).await;
        shipped_status(&db).await;
        let id = place_order(&db, &f, None).await;
        let sh = first_shipment(&db, id).await;
        let s = SaleSettings {
            final_status_on_delivery: Some("S".into()),
            ..SaleSettings::default()
        };
        let d = DocumentIn {
            date: "2026-10-13".into(),
            ..with(ship_doc(&sh.to_string(), id, "1"), &[("Отгружен", "true")])
        };
        let stats = run(&db, &s, &file(vec![d])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        let row = shipment(&db, sh).await;
        assert_eq!((row.0, row.1, row.3), (true, true, date("2026-10-13")));
        assert_eq!(status(&db, id).await.0, "S");
        assert_eq!(
            stats.notify,
            vec![("SALE_STATUS_CHANGED_S".to_string(), id)]
        );
        assert_eq!(amounts(&db, f.product_id).await, (10.0, 5.0));

        // Дата реквизита важнее даты документа
        let other = place_order(&db, &f, None).await;
        let osh = first_shipment(&db, other).await;
        let d = DocumentIn {
            date: "2026-10-13".into(),
            ..with(
                ship_doc(&osh.to_string(), other, "1"),
                &[
                    ("Отгружен", "true"),
                    ("Дата отгрузки по 1С", "2026-10-14T08:00:00"),
                ],
            )
        };
        run(&db, &s, &file(vec![d])).await;
        assert_eq!(shipment(&db, osh).await.3, date("2026-10-14"));
    }

    #[sqlx::test]
    async fn shipment_price_difference_is_ignored(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let sh = first_shipment(&db, id).await;
        sqlx::query("UPDATE orders SET paid = TRUE WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        for s in [
            SaleSettings::default(),
            SaleSettings {
                update_basket_quantity: false,
                ..SaleSettings::default()
            },
        ] {
            let mut d = with(ship_doc(&sh.to_string(), id, ""), &[("Отгружен", "true")]);
            d.items = vec![item("tovar-1c", 90.0, 1.0)];
            let stats = run(&db, &s, &file(vec![d])).await;
            assert_eq!(stats.errors, Vec::<String>::new());
            assert!(shipment(&db, sh).await.0);
            assert_eq!(
                lines(&db, id).await,
                vec![(f.product_id, 1.0, 100.0, false)]
            );
        }
    }

    #[sqlx::test]
    async fn shipment_quantity_mismatch(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let sh = first_shipment(&db, id).await;
        let mut d = ship_doc(&sh.to_string(), id, "1");
        d.items = vec![item("tovar-1c", 100.0, 3.0), delivery(250.0)];

        // Без опции — ошибка, ничего не изменилось
        let off = SaleSettings {
            update_basket_quantity: false,
            ..SaleSettings::default()
        };
        let stats = run(&db, &off, &file(vec![d.clone()])).await;
        assert_eq!(
            stats.errors,
            vec![format!(
                "Документ ship-guid-{sh}: количество в отгрузке не совпадает с заказом"
            )]
        );
        assert_eq!(
            lines(&db, id).await,
            vec![(f.product_id, 1.0, 100.0, false)]
        );
        assert_eq!(shipment(&db, sh).await.6, "");

        // Совпадающее количество без опции — не ошибка
        let mut same = ship_doc(&sh.to_string(), id, "1");
        same.items = vec![item("tovar-1c", 100.0, 1.0)];
        let stats = run(&db, &off, &file(vec![same])).await;
        assert_eq!(stats.errors, Vec::<String>::new());

        // С опцией — правка позиций по правилам заказа: остатки и суммы, доставка — цена отгрузки
        d.version = "2".into();
        let stats = run(&db, &SaleSettings::default(), &file(vec![d])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!(lines(&db, id).await, vec![(f.product_id, 3.0, 100.0, true)]);
        assert_eq!(quantity(&db, f.product_id).await, 8.0);
        assert_eq!(shipment(&db, sh).await.8, 250.0);
        assert_eq!(totals(&db, id).await, (300.0, 250.0, 550.0, 550.0, 250.0));

        // Оплаченный заказ по составу не правится
        sqlx::query("UPDATE orders SET paid = TRUE WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let mut d = ship_doc(&sh.to_string(), id, "3");
        d.items = vec![item("tovar-1c", 100.0, 1.0)];
        let stats = run(&db, &SaleSettings::default(), &file(vec![d])).await;
        assert_eq!(
            stats.errors,
            vec![format!(
                "Документ ship-guid-{sh}: заказ в финальном состоянии, состав не изменён"
            )]
        );
        assert_eq!(lines(&db, id).await, vec![(f.product_id, 3.0, 100.0, true)]);
    }

    #[sqlx::test]
    async fn new_shipment_and_disabled_creation(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let courier: i64 =
            sqlx::query_scalar("INSERT INTO deliveries (name) VALUES ('Курьер') RETURNING id")
                .fetch_one(&db)
                .await
                .unwrap();
        let s = SaleSettings {
            default_delivery: Some(courier),
            ..SaleSettings::default()
        };
        let mut d = with(
            ship_doc("1С-77", id, "1"),
            &[
                ("Дата отгрузки по 1С", "2026-10-15"),
                ("Идентификатор отправления", "T-1"),
            ],
        );
        // Отгрузка не единственная — позиции с заказом не сверяются
        d.items = vec![item("tovar-1c", 100.0, 5.0), delivery(300.0)];
        let stats = run(&db, &s, &file(vec![d])).await;
        assert_eq!(stats.errors, Vec::<String>::new());
        assert_eq!((stats.shipments_created, stats.shipments_updated), (1, 0));
        let row: (Option<i64>, String, f64, bool, String, String) = sqlx::query_as(
            "SELECT delivery_id, delivery_name, price::float8, deducted, tracking_number, id_1c
             FROM shipments WHERE order_id = $1 AND id_1c <> ''",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(
            row,
            (
                Some(courier),
                "Курьер".into(),
                300.0,
                true,
                "T-1".into(),
                "РН-1С-77".into()
            )
        );
        assert_eq!(
            lines(&db, id).await,
            vec![(f.product_id, 1.0, 100.0, false)]
        );
        // Доставка заказа — сумма отгрузок
        assert_eq!(
            sqlx::query_scalar::<_, f64>("SELECT delivery_price::float8 FROM orders WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap(),
            300.0
        );

        let off = SaleSettings {
            import_new_shipments: false,
            ..s.clone()
        };
        let stats = run(&db, &off, &file(vec![ship_doc("1С-78", id, "1")])).await;
        assert_eq!(
            stats.errors,
            vec!["Документ ship-guid-1С-78: отгрузка не найдена".to_string()]
        );
        assert_eq!(count(&db, "shipments", id).await, 2);
    }

    #[sqlx::test]
    async fn container_is_one_transaction(db: PgPool) {
        let f = fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let p: i64 = sqlx::query_scalar(
            "INSERT INTO payments (order_id, name, sum, paid) VALUES ($1, 'Оплачена', 5, TRUE) RETURNING id",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        let mut order = doc(&id.to_string(), "1", &[]);
        order.items = vec![item("tovar-1c", 100.0, 2.0)];
        let bad = DocumentIn {
            deleted: true,
            ..pay_doc(&p.to_string(), id, "1", PayOperation::Cash)
        };
        let sale = SaleFile {
            agents: Vec::new(),
            groups: vec![vec![order, bad]],
        };
        let stats = run(&db, &SaleSettings::default(), &sale).await;
        assert_eq!(
            stats.errors,
            vec![format!(
                "Документ pay-guid-{p}: оплата проведена, удаление невозможно"
            )]
        );
        assert_eq!((stats.orders_updated, stats.payments_updated), (0, 0));
        // Документ заказа откатился вместе с оплатой: ни Номер1С, ни состава, ни остатков
        assert_eq!(status(&db, id).await.1, "");
        assert_eq!(
            lines(&db, id).await,
            vec![(f.product_id, 1.0, 100.0, false)]
        );
        assert_eq!(amounts(&db, f.product_id).await, (10.0, 5.0));
    }

    #[sqlx::test]
    async fn repeat_file_is_noop(db: PgPool) {
        let f = fixture(&db).await;
        shipped_status(&db).await;
        let id = place_order(&db, &f, None).await;
        let p = first_payment(&db, id).await;
        let sh = first_shipment(&db, id).await;
        let s = SaleSettings {
            final_status_on_delivery: Some("S".into()),
            ..SaleSettings::default()
        };
        let mut order = doc(&id.to_string(), "1", &[]);
        order.items = vec![item("tovar-1c", 100.0, 2.0)];
        let mut ship = with(
            ship_doc(&sh.to_string(), id, "1"),
            &[
                ("Дата отгрузки по 1С", "2026-10-15"),
                ("Идентификатор отправления", "T-9"),
            ],
        );
        ship.items = vec![item("tovar-1c", 100.0, 2.0)];
        let sale = SaleFile {
            agents: Vec::new(),
            groups: vec![
                vec![order],
                vec![with(
                    pay_doc(&p.to_string(), id, "1", PayOperation::Cashless),
                    &[("Оплачен", "true")],
                )],
                vec![DocumentIn {
                    sum: Some(50.0),
                    ..with(
                        pay_doc("1С-П", id, "1", PayOperation::Cash),
                        &[("Оплачен", "true")],
                    )
                }],
                vec![ship],
                vec![with(
                    ship_doc("1С-Р", id, "1"),
                    &[("Дата отгрузки по 1С", "2026-10-16")],
                )],
            ],
        };
        let first = run(&db, &s, &sale).await;
        assert_eq!(first.errors, Vec::<String>::new());
        assert_eq!(
            (
                first.orders_updated,
                first.payments_updated,
                first.payments_created,
                first.shipments_updated,
                first.shipments_created
            ),
            (1, 1, 1, 1, 1)
        );
        assert_eq!(first.notify.len(), 2, "{:?}", first.notify);
        let snapshot = async || {
            (
                count(&db, "payments", id).await,
                count(&db, "shipments", id).await,
                count(&db, "order_status_history", id).await,
                lines(&db, id).await,
                quantity(&db, f.product_id).await,
                totals_all(&db, id).await,
            )
        };
        let before = snapshot().await;

        let second = run(&db, &s, &sale).await;
        assert_eq!(second.errors, Vec::<String>::new());
        assert_eq!(
            SaleStats {
                skipped: 0,
                ..second.clone()
            },
            SaleStats::default()
        );
        assert_eq!(second.skipped, 5);
        assert_eq!(snapshot().await, before);
    }

    async fn totals_all(db: &PgPool, id: i64) -> (f64, f64, bool, String) {
        sqlx::query_as(
            "SELECT price::float8, delivery_price::float8, paid, status FROM orders WHERE id = $1",
        )
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
    }
}
