//! Заказ в БД: создание при оформлении, чтение для писем и админки, действия менеджера.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::{PgConnection, PgPool};

use super::stock::{self, StockChange, StockLine, apply};
use crate::{catalog, collection::sku::ItemProp};

/// Значение свойства заказа (флаги — из справочника свойств, если он ещё есть).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OrderPropertyValue {
    pub property_id: i64,
    pub code: String,
    pub name: String,
    pub value: String,
    pub is_email: bool,
    pub is_phone: bool,
    pub is_payer: bool,
}

/// Позиция заказа.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OrderItem {
    pub id: i64,
    pub product_id: i64,
    pub store_id: Option<i64>,
    pub quantity: f64,
    pub price: f64,
    pub name: String,
    pub custom_price: bool,
    /// Свойства позиции (у предложения — объём, цвет и т.п.), снимок на момент добавления.
    #[sqlx(json)]
    pub props: Vec<ItemProp>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Shipment {
    pub id: i64,
    pub delivery_id: Option<i64>,
    pub delivery_name: String,
    pub price: f64,
    pub store_id: Option<i64>,
    pub tracking_number: String,
    pub allow_delivery: bool,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Payment {
    pub pay_system_id: Option<i64>,
    pub name: String,
    pub sum: f64,
    pub paid: bool,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct HistoryEntry {
    pub status: String,
    pub status_name: String,
    pub user_login: Option<String>,
    pub comment: String,
    pub created_at: DateTime<Utc>,
}

/// Заказ целиком.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OrderView {
    pub id: i64,
    pub account_number: String,
    pub user_id: Option<i64>,
    pub user_login: String,
    pub user_email: String,
    pub user_name: String,
    pub person_type_id: i64,
    pub status: String,
    pub status_name: String,
    pub goods_price: f64,
    pub delivery_price: f64,
    pub price: f64,
    pub currency: String,
    pub user_comment: String,
    pub manager_comment: String,
    pub canceled: bool,
    pub canceled_at: Option<DateTime<Utc>>,
    pub cancel_reason: String,
    pub paid: bool,
    pub paid_at: Option<DateTime<Utc>>,
    pub stock_deducted: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[sqlx(skip)]
    pub properties: Vec<OrderPropertyValue>,
    #[sqlx(skip)]
    pub items: Vec<OrderItem>,
    #[sqlx(skip)]
    pub shipment: Option<Shipment>,
    #[sqlx(skip)]
    pub payment: Option<Payment>,
    #[sqlx(skip)]
    pub history: Vec<HistoryEntry>,
}

/// Заказ к записи при оформлении.
pub struct NewOrder {
    pub user_id: Option<i64>,
    /// Покупатель, чьи позиции корзины становятся позициями заказа.
    pub buyer_id: i64,
    pub person_type_id: i64,
    pub status: String,
    pub currency: String,
    pub goods_price: f64,
    pub delivery_price: f64,
    pub user_comment: String,
    /// (id свойства, код, название, значение)
    pub properties: Vec<(i64, String, String, String)>,
    /// (id службы, название, цена, склад самовывоза)
    pub delivery: (i64, String, f64, Option<i64>),
    /// (id платёжки, название)
    pub payment: (i64, String),
    /// (id позиции корзины, количество по снимку, цена, название)
    pub items: Vec<(i64, f64, f64, String)>,
    /// Списание остатков.
    pub stock: Vec<StockChange>,
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[derive(Debug)]
pub enum CreateError {
    /// Позиция уже оформлена другим запросом или изменилась после снимка.
    BasketChanged,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for CreateError {
    fn from(e: sqlx::Error) -> Self {
        CreateError::Db(e)
    }
}

/// Записывает заказ в транзакции вызывающего; позиции корзины становятся позициями заказа.
/// Позиция должна быть ещё в корзине этого покупателя и с тем же количеством — иначе
/// [`CreateError::BasketChanged`] (транзакцию вызывающий откатывает).
pub async fn create(tx: &mut PgConnection, o: &NewOrder) -> Result<i64, CreateError> {
    let total = round2(o.goods_price + o.delivery_price);
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO orders (user_id, person_type_id, status, goods_price, delivery_price, price,
                             currency, user_comment, stock_deducted)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, TRUE) RETURNING id",
    )
    .bind(o.user_id)
    .bind(o.person_type_id)
    .bind(&o.status)
    .bind(o.goods_price)
    .bind(o.delivery_price)
    .bind(total)
    .bind(&o.currency)
    .bind(&o.user_comment)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE orders SET account_number = id::text WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for (property_id, code, name, value) in &o.properties {
        sqlx::query(
            "INSERT INTO order_property_values (order_id, property_id, code, name, value)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(property_id)
        .bind(code)
        .bind(name)
        .bind(value)
        .execute(&mut *tx)
        .await?;
    }
    let (delivery_id, delivery_name, delivery_price, store_id) = &o.delivery;
    sqlx::query(
        "INSERT INTO shipments (order_id, delivery_id, delivery_name, price, store_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(delivery_id)
    .bind(delivery_name)
    .bind(delivery_price)
    .bind(store_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO payments (order_id, pay_system_id, name, sum, currency) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(o.payment.0)
    .bind(&o.payment.1)
    .bind(total)
    .bind(&o.currency)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO order_status_history (order_id, status) VALUES ($1, $2)")
        .bind(id)
        .bind(&o.status)
        .execute(&mut *tx)
        .await?;
    for (item_id, quantity, price, name) in &o.items {
        let moved = sqlx::query(
            "UPDATE cart_items SET order_id = $2, price = $3, name = $4, currency = $5, updated_at = now()
             WHERE id = $1 AND order_id IS NULL AND buyer_id = $6 AND quantity = $7",
        )
        .bind(item_id)
        .bind(id)
        .bind(price)
        .bind(name)
        .bind(&o.currency)
        .bind(o.buyer_id)
        .bind(quantity)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if moved != 1 {
            return Err(CreateError::BasketChanged);
        }
    }
    apply(tx, &o.stock).await?;
    Ok(id)
}

/// Заказ со свойствами, позициями, отгрузкой, оплатой и историей.
pub async fn load(db: &PgPool, id: i64) -> sqlx::Result<Option<OrderView>> {
    let Some(mut order) = sqlx::query_as::<_, OrderView>(
        "SELECT o.id, COALESCE(o.account_number, o.id::text) AS account_number, o.user_id,
                COALESCE(u.login, '') AS user_login, COALESCE(u.email, '') AS user_email,
                COALESCE(trim(concat_ws(' ', u.last_name, u.name, u.second_name)), '') AS user_name,
                o.person_type_id, o.status, COALESCE(s.name, o.status) AS status_name,
                o.goods_price::float8 AS goods_price, o.delivery_price::float8 AS delivery_price,
                o.price::float8 AS price, o.currency, o.user_comment, o.manager_comment,
                o.canceled, o.canceled_at, o.cancel_reason, o.paid, o.paid_at, o.stock_deducted,
                o.created_at, o.updated_at
         FROM orders o
         LEFT JOIN users u ON u.id = o.user_id
         LEFT JOIN order_statuses s ON s.code = o.status
         WHERE o.id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?
    else {
        return Ok(None);
    };
    order.properties = sqlx::query_as(
        "SELECT v.property_id, v.code, v.name, v.value,
                COALESCE(p.is_email, FALSE) AS is_email, COALESCE(p.is_phone, FALSE) AS is_phone,
                COALESCE(p.is_payer, FALSE) AS is_payer
         FROM order_property_values v LEFT JOIN order_properties p ON p.id = v.property_id
         WHERE v.order_id = $1 ORDER BY COALESCE(p.sort, 0), v.property_id",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    order.items = sqlx::query_as(
        "SELECT id, item_id AS product_id, store_id, quantity::float8 AS quantity,
                COALESCE(price, 0)::float8 AS price, name, custom_price, props
         FROM cart_items WHERE order_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    order.shipment = sqlx::query_as(
        "SELECT id, delivery_id, delivery_name, price::float8 AS price, store_id,
                tracking_number, allow_delivery
         FROM shipments WHERE order_id = $1 ORDER BY id LIMIT 1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    order.payment = sqlx::query_as(
        "SELECT pay_system_id, name, sum::float8 AS sum, paid
         FROM payments WHERE order_id = $1 ORDER BY id LIMIT 1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    order.history = sqlx::query_as(
        "SELECT h.status, COALESCE(s.name, h.status) AS status_name, u.login AS user_login,
                h.comment, h.created_at
         FROM order_status_history h
         LEFT JOIN order_statuses s ON s.code = h.status
         LEFT JOIN users u ON u.id = h.user_id
         WHERE h.order_id = $1 ORDER BY h.created_at, h.id",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    Ok(Some(order))
}

/// Значения свойств-местоположений из ввода, которые есть в базе (по коду или id).
pub async fn known_locations(
    db: &PgPool,
    input: &Map<String, Value>,
) -> sqlx::Result<HashSet<String>> {
    let values: Vec<String> = input
        .values()
        .filter_map(|v| match v {
            Value::String(s) => Some(s.trim().to_string()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .filter(|v| !v.is_empty())
        .collect();
    let found: Vec<String> = sqlx::query_scalar(
        "SELECT code FROM locations WHERE code = ANY($1)
         UNION SELECT id::text FROM locations WHERE id::text = ANY($1)",
    )
    .bind(&values)
    .fetch_all(db)
    .await?;
    Ok(found.into_iter().collect())
}

/// Меняет статус и пишет историю. `false` — статус уже такой.
pub async fn set_status(
    db: &PgPool,
    order_id: i64,
    status: &str,
    admin_id: Option<i64>,
    comment: &str,
) -> sqlx::Result<bool> {
    let mut tx = db.begin().await?;
    let changed = sqlx::query(
        "UPDATE orders SET status = $2, updated_at = now() WHERE id = $1 AND status <> $2",
    )
    .bind(order_id)
    .bind(status)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if changed {
        sqlx::query(
            "INSERT INTO order_status_history (order_id, status, user_id, comment) VALUES ($1, $2, $3, $4)",
        )
        .bind(order_id)
        .bind(status)
        .bind(admin_id)
        .bind(comment)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Отметка оплаты заказа и его оплаты. `false` — уже так.
pub async fn set_paid(db: &PgPool, order_id: i64, paid: bool) -> sqlx::Result<bool> {
    let mut tx = db.begin().await?;
    let changed = sqlx::query(
        "UPDATE orders SET paid = $2, paid_at = CASE WHEN $2 THEN now() END, updated_at = now()
         WHERE id = $1 AND paid <> $2",
    )
    .bind(order_id)
    .bind(paid)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if changed {
        sqlx::query(
            "UPDATE payments SET paid = $2, paid_at = CASE WHEN $2 THEN now() END WHERE order_id = $1",
        )
        .bind(order_id)
        .bind(paid)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Позиции заказа для расчёта остатков.
pub async fn stock_lines(tx: &mut PgConnection, order_id: i64) -> sqlx::Result<Vec<StockLine>> {
    let rows: Vec<(i64, Option<i64>, f64)> = sqlx::query_as(
        "SELECT item_id, store_id, quantity::float8 FROM cart_items WHERE order_id = $1 ORDER BY id",
    )
    .bind(order_id)
    .fetch_all(&mut *tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(product_id, store_id, quantity)| StockLine {
            product_id,
            store_id,
            quantity,
        })
        .collect())
}

/// Товары с учётом количества среди позиций (строки товаров блокируются).
pub async fn traced_locked(
    tx: &mut PgConnection,
    lines: &[StockLine],
) -> sqlx::Result<HashSet<i64>> {
    let ids: Vec<i64> = lines.iter().map(|l| l.product_id).collect();
    Ok(catalog::load_locked(tx, &ids)
        .await?
        .into_iter()
        .filter(|(_, i)| i.quantity_trace)
        .map(|(id, _)| id)
        .collect())
}

/// Отмена (возврат остатков) или снятие отмены (повторное списание). `false` — уже так;
/// `stock_deducted` не даёт вернуть или списать дважды.
pub async fn set_canceled(
    db: &PgPool,
    order_id: i64,
    canceled: bool,
    reason: &str,
) -> sqlx::Result<bool> {
    let mut tx = db.begin().await?;
    let Some((was, deducted)): Option<(bool, bool)> =
        sqlx::query_as("SELECT canceled, stock_deducted FROM orders WHERE id = $1 FOR UPDATE")
            .bind(order_id)
            .fetch_optional(&mut *tx)
            .await?
    else {
        return Ok(false);
    };
    if was == canceled {
        return Ok(false);
    }
    let lines = stock_lines(&mut tx, order_id).await?;
    let traced = traced_locked(&mut tx, &lines).await?;
    let deducted = if canceled && deducted {
        apply(&mut tx, &stock::plan(&lines, &[], &traced)).await?;
        false
    } else if !canceled && !deducted {
        apply(&mut tx, &stock::plan(&[], &lines, &traced)).await?;
        true
    } else {
        deducted
    };
    sqlx::query(
        "UPDATE orders SET canceled = $2, canceled_at = CASE WHEN $2 THEN now() END,
             cancel_reason = CASE WHEN $2 THEN $3 ELSE '' END, stock_deducted = $4, updated_at = now()
         WHERE id = $1",
    )
    .bind(order_id)
    .bind(canceled)
    .bind(reason)
    .bind(deducted)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Заменяет значения свойств и комментарий менеджера.
/// Id файлов из значения файлового свойства («5,7»); мусор, повторы и неположительные — мимо.
pub fn file_ids(raw: &str) -> Vec<i64> {
    let mut out = Vec::new();
    for id in raw.split(',').filter_map(|p| p.trim().parse::<i64>().ok()) {
        if id > 0 && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Записывает значение свойства (код и название — из справочника) или удаляет пустое.
async fn write_property_value(
    tx: &mut PgConnection,
    order_id: i64,
    prop: (i64, &str, &str),
    value: &str,
) -> sqlx::Result<()> {
    if value.is_empty() {
        sqlx::query("DELETE FROM order_property_values WHERE order_id = $1 AND property_id = $2")
            .bind(order_id)
            .bind(prop.0)
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query(
            "INSERT INTO order_property_values (order_id, property_id, code, name, value)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (order_id, property_id) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(order_id)
        .bind(prop.0)
        .bind(prop.1)
        .bind(prop.2)
        .bind(value)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE orders SET updated_at = now() WHERE id = $1")
        .bind(order_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

async fn locked_value(
    tx: &mut PgConnection,
    order_id: i64,
    property_id: i64,
) -> sqlx::Result<String> {
    // Блокировка заказа: два одновременных добавления файлов не теряют друг друга
    sqlx::query("SELECT id FROM orders WHERE id = $1 FOR UPDATE")
        .bind(order_id)
        .execute(&mut *tx)
        .await?;
    Ok(sqlx::query_scalar(
        "SELECT value FROM order_property_values WHERE order_id = $1 AND property_id = $2",
    )
    .bind(order_id)
    .bind(property_id)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or_default())
}

fn join_ids(ids: &[i64]) -> String {
    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

/// Файлы в файловое свойство: множественному — дописать, одиночному — заменить первым.
pub async fn add_property_files(
    db: &PgPool,
    order_id: i64,
    prop: &super::OrderProperty,
    ids: &[i64],
) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    let current = file_ids(&locked_value(&mut tx, order_id, prop.id).await?);
    let next: Vec<i64> = if prop.multiple {
        let mut all = current;
        all.extend(ids.iter().filter(|id| **id > 0));
        file_ids(&join_ids(&all))
    } else {
        ids.iter().copied().filter(|id| *id > 0).take(1).collect()
    };
    if next.is_empty() {
        return Ok(());
    }
    write_property_value(
        &mut tx,
        order_id,
        (prop.id, &prop.code, &prop.name),
        &join_ids(&next),
    )
    .await?;
    tx.commit().await
}

/// Убирает файл из значения свойства; пустое значение удаляется.
pub async fn remove_property_file(
    db: &PgPool,
    order_id: i64,
    property_id: i64,
    file_id: i64,
) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    let current = file_ids(&locked_value(&mut tx, order_id, property_id).await?);
    let next: Vec<i64> = current.into_iter().filter(|id| *id != file_id).collect();
    write_property_value(&mut tx, order_id, (property_id, "", ""), &join_ids(&next)).await?;
    tx.commit().await
}

/// Трек-номер и разрешение доставки первой отгрузки; `false` — у заказа нет отгрузки.
pub async fn set_shipment(
    db: &PgPool,
    order_id: i64,
    tracking: &str,
    allow: bool,
) -> sqlx::Result<bool> {
    let mut tx = db.begin().await?;
    let done = sqlx::query(
        "UPDATE shipments SET tracking_number = $2, allow_delivery = $3
         WHERE id = (SELECT min(id) FROM shipments WHERE order_id = $1)",
    )
    .bind(order_id)
    .bind(tracking.trim())
    .bind(allow)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if done {
        sqlx::query("UPDATE orders SET updated_at = now() WHERE id = $1")
            .bind(order_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(done)
}

pub async fn update_properties(
    db: &PgPool,
    order_id: i64,
    values: &[(i64, String, String, String)],
    keep: &[i64],
    manager_comment: &str,
) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    // Значения из `keep` (файлы) правятся отдельно и здесь не трогаются
    sqlx::query(
        "DELETE FROM order_property_values WHERE order_id = $1 AND NOT (property_id = ANY($2))",
    )
    .bind(order_id)
    .bind(keep)
    .execute(&mut *tx)
    .await?;
    for (property_id, code, name, value) in values {
        sqlx::query(
            "INSERT INTO order_property_values (order_id, property_id, code, name, value)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(order_id)
        .bind(property_id)
        .bind(code)
        .bind(name)
        .bind(value)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE orders SET manager_comment = $2, updated_at = now() WHERE id = $1")
        .bind(order_id)
        .bind(manager_comment)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        sale::stock::{StockLine, plan},
        test_support::{amounts, order_fixture, place_order},
    };

    fn new_order(f: &crate::test_support::Fixture, quantity: f64) -> NewOrder {
        let lines = [StockLine {
            product_id: f.product_id,
            store_id: Some(1),
            quantity,
        }];
        NewOrder {
            user_id: None,
            buyer_id: f.buyer_id,
            person_type_id: f.person_type_id,
            status: "N".into(),
            currency: "RUB".into(),
            goods_price: 100.0 * quantity,
            delivery_price: 0.0,
            user_comment: String::new(),
            properties: Vec::new(),
            delivery: (1, "Самовывоз".into(), 0.0, Some(1)),
            payment: (1, "QR".into()),
            items: vec![(f.item_id, quantity, 100.0, "Товар".into())],
            stock: plan(&[], &lines, &[f.product_id].into_iter().collect()),
        }
    }

    #[sqlx::test]
    async fn repeated_create_with_same_items_is_rejected(db: PgPool) {
        let f = order_fixture(&db).await;
        let mut tx = db.begin().await.unwrap();
        create(&mut tx, &new_order(&f, 1.0)).await.unwrap();
        tx.commit().await.unwrap();
        // Второй запрос с теми же позициями (двойной клик) — позиции уже в заказе
        let mut tx = db.begin().await.unwrap();
        let second = create(&mut tx, &new_order(&f, 1.0)).await;
        assert!(matches!(second, Err(CreateError::BasketChanged)));
        drop(tx);
        let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM orders")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(orders, 1);
        assert_eq!(amounts(&db, f.product_id).await, (9.0, 4.0));
    }

    #[sqlx::test]
    async fn create_rejects_changed_quantity(db: PgPool) {
        let f = order_fixture(&db).await;
        // Количество в корзине поменяли после снимка: снимок — 2 шт., в БД — 1 шт.
        let mut tx = db.begin().await.unwrap();
        let result = create(&mut tx, &new_order(&f, 2.0)).await;
        assert!(matches!(result, Err(CreateError::BasketChanged)));
    }

    #[test]
    fn file_ids_parse() {
        assert_eq!(file_ids("5,7"), vec![5, 7]);
        assert_eq!(file_ids(" 5 , abc,,7,5,-1,0"), vec![5, 7]);
        assert!(file_ids("").is_empty());
    }

    #[sqlx::test]
    async fn shipment_tracking_saved(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        assert!(set_shipment(&db, id, " 80012345 ", true).await.unwrap());
        let s = load(&db, id).await.unwrap().unwrap().shipment.unwrap();
        assert_eq!(
            (s.tracking_number.as_str(), s.allow_delivery),
            ("80012345", true)
        );
        assert!(!set_shipment(&db, id + 1000, "x", false).await.unwrap());
    }

    async fn file_prop(
        db: &PgPool,
        f: &crate::test_support::Fixture,
        code: &str,
        multiple: bool,
    ) -> crate::sale::OrderProperty {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO order_properties (person_type_id, code, name, kind, multiple)
             VALUES ($1, $2, $2, 'file', $3) RETURNING id",
        )
        .bind(f.person_type_id)
        .bind(code)
        .bind(multiple)
        .fetch_one(db)
        .await
        .unwrap();
        crate::sale::load_settings(db)
            .await
            .unwrap()
            .properties
            .into_iter()
            .find(|p| p.id == id)
            .unwrap()
    }

    async fn value(db: &PgPool, order_id: i64, property_id: i64) -> Option<String> {
        sqlx::query_scalar(
            "SELECT value FROM order_property_values WHERE order_id = $1 AND property_id = $2",
        )
        .bind(order_id)
        .bind(property_id)
        .fetch_optional(db)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn property_files_add_replace_remove(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let many = file_prop(&db, &f, "WAYBILL", true).await;
        let one = file_prop(&db, &f, "INVOICE", false).await;
        add_property_files(&db, id, &many, &[1]).await.unwrap();
        add_property_files(&db, id, &many, &[2]).await.unwrap();
        assert_eq!(value(&db, id, many.id).await.as_deref(), Some("1,2"));
        remove_property_file(&db, id, many.id, 1).await.unwrap();
        assert_eq!(value(&db, id, many.id).await.as_deref(), Some("2"));
        add_property_files(&db, id, &one, &[1]).await.unwrap();
        add_property_files(&db, id, &one, &[2, 3]).await.unwrap();
        assert_eq!(value(&db, id, one.id).await.as_deref(), Some("2"));
        let name: String = sqlx::query_scalar(
            "SELECT name FROM order_property_values WHERE order_id = $1 AND property_id = $2",
        )
        .bind(id)
        .bind(one.id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(name, "INVOICE");
    }

    #[sqlx::test]
    async fn update_properties_keeps_files(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let many = file_prop(&db, &f, "WAYBILL", true).await;
        add_property_files(&db, id, &many, &[1]).await.unwrap();
        let rows = vec![(
            500,
            "FIO".to_string(),
            "ФИО".to_string(),
            "Иван".to_string(),
        )];
        update_properties(&db, id, &rows, &[many.id], "")
            .await
            .unwrap();
        assert_eq!(value(&db, id, many.id).await.as_deref(), Some("1"));
        assert_eq!(value(&db, id, 500).await.as_deref(), Some("Иван"));
    }
}
