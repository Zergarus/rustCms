//! Заказ в БД: создание при оформлении, чтение для писем и админки, действия менеджера.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgConnection, PgPool};

use super::stock::{StockChange, apply};

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
    pub element_id: i64,
    pub store_id: Option<i64>,
    pub quantity: f64,
    pub price: f64,
    pub name: String,
    pub custom_price: bool,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Shipment {
    pub delivery_id: Option<i64>,
    pub delivery_name: String,
    pub price: f64,
    pub store_id: Option<i64>,
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
    /// (id позиции корзины, цена, название)
    pub items: Vec<(i64, f64, String)>,
    /// Списание остатков.
    pub stock: Vec<StockChange>,
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Записывает заказ в транзакции вызывающего; позиции корзины становятся позициями заказа.
pub async fn create(tx: &mut PgConnection, o: &NewOrder) -> sqlx::Result<i64> {
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
    for (item_id, price, name) in &o.items {
        sqlx::query(
            "UPDATE cart_items SET order_id = $2, price = $3, name = $4, currency = $5, updated_at = now()
             WHERE id = $1 AND order_id IS NULL",
        )
        .bind(item_id)
        .bind(id)
        .bind(price)
        .bind(name)
        .bind(&o.currency)
        .execute(&mut *tx)
        .await?;
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
        "SELECT id, element_id, store_id, quantity::float8 AS quantity,
                COALESCE(price, 0)::float8 AS price, name, custom_price
         FROM cart_items WHERE order_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    order.shipment = sqlx::query_as(
        "SELECT delivery_id, delivery_name, price::float8 AS price, store_id
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
