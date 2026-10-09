//! Заказы пользователя для личного кабинета: страница заказов с позициями, отгрузками,
//! платежами и свойствами — пакетными запросами (`OrderHistoryReader` модуля bxapi).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

use super::repo::OrderItem;

#[derive(Debug, Clone, FromRow)]
pub struct UserShipment {
    pub order_id: i64,
    pub id: i64,
    pub delivery_id: Option<i64>,
    pub delivery_name: String,
    pub price: f64,
    pub tracking_number: String,
    pub allow_delivery: bool,
}

#[derive(Debug, Clone, FromRow)]
pub struct UserPayment {
    pub order_id: i64,
    pub id: i64,
    pub pay_system_id: Option<i64>,
    pub name: String,
    pub sum: f64,
    pub currency: String,
    pub paid: bool,
    pub paid_at: Option<DateTime<Utc>>,
}

/// Заказ пользователя со всем, что отдаётся в личный кабинет.
#[derive(Debug, Clone, FromRow)]
pub struct UserOrder {
    pub id: i64,
    pub account_number: String,
    pub status: String,
    pub status_name: String,
    pub created_at: DateTime<Utc>,
    pub price: f64,
    pub currency: String,
    pub canceled: bool,
    pub paid: bool,
    pub stock_deducted: bool,
    pub person_type_id: i64,
    pub user_comment: String,
    #[sqlx(skip)]
    pub shipments: Vec<UserShipment>,
    #[sqlx(skip)]
    pub payments: Vec<UserPayment>,
    #[sqlx(skip)]
    pub items: Vec<OrderItem>,
    /// (id свойства, код, название, значение)
    #[sqlx(skip)]
    pub properties: Vec<(i64, String, String, String)>,
}

const ORDER_COLS: &str =
    "o.id, COALESCE(NULLIF(o.account_number, ''), o.id::text) AS account_number,
    o.status, COALESCE(s.name, o.status) AS status_name, o.created_at, o.price::float8 AS price,
    o.currency, o.canceled, o.paid, o.stock_deducted, o.person_type_id, o.user_comment
    FROM orders o LEFT JOIN order_statuses s ON s.code = o.status";

/// Число заказов пользователя.
pub async fn count(db: &PgPool, user_id: i64) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT count(*) FROM orders WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(db)
        .await
}

/// Страница заказов пользователя, новые первыми.
pub async fn list(
    db: &PgPool,
    user_id: i64,
    limit: i64,
    offset: i64,
) -> sqlx::Result<Vec<UserOrder>> {
    let orders = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ORDER_COLS} WHERE o.user_id = $1 ORDER BY o.created_at DESC, o.id DESC
         LIMIT $2 OFFSET $3"
    )))
    .bind(user_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await?;
    fill(db, orders).await
}

/// Заказ пользователя; чужой или несуществующий — `None`.
pub async fn detail(db: &PgPool, id: i64, user_id: i64) -> sqlx::Result<Option<UserOrder>> {
    let orders = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ORDER_COLS} WHERE o.id = $1 AND o.user_id = $2"
    )))
    .bind(id)
    .bind(user_id)
    .fetch_all(db)
    .await?;
    Ok(fill(db, orders).await?.pop())
}

#[derive(FromRow)]
struct ItemRow {
    order_id: i64,
    #[sqlx(flatten)]
    item: OrderItem,
}

/// Позиции, отгрузки, платежи и свойства — по одному запросу на все заказы.
async fn fill(db: &PgPool, mut orders: Vec<UserOrder>) -> sqlx::Result<Vec<UserOrder>> {
    if orders.is_empty() {
        return Ok(orders);
    }
    let ids: Vec<i64> = orders.iter().map(|o| o.id).collect();
    let index: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();

    let items: Vec<ItemRow> = sqlx::query_as(
        "SELECT order_id, id, item_id AS product_id, store_id, quantity::float8 AS quantity,
                COALESCE(price, 0)::float8 AS price, name, custom_price, props
         FROM cart_items WHERE order_id = ANY($1) ORDER BY id",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    for r in items {
        orders[index[&r.order_id]].items.push(r.item);
    }

    let shipments: Vec<UserShipment> = sqlx::query_as(
        "SELECT order_id, id, delivery_id, delivery_name, price::float8 AS price,
                tracking_number, allow_delivery
         FROM shipments WHERE order_id = ANY($1) ORDER BY id",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    for s in shipments {
        orders[index[&s.order_id]].shipments.push(s);
    }

    let payments: Vec<UserPayment> = sqlx::query_as(
        "SELECT order_id, id, pay_system_id, name, sum::float8 AS sum, currency, paid, paid_at
         FROM payments WHERE order_id = ANY($1) ORDER BY id",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    for p in payments {
        orders[index[&p.order_id]].payments.push(p);
    }

    let properties: Vec<(i64, i64, String, String, String)> = sqlx::query_as(
        "SELECT order_id, property_id, code, name, value
         FROM order_property_values WHERE order_id = ANY($1) ORDER BY property_id",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    for (order_id, pid, code, name, value) in properties {
        orders[index[&order_id]]
            .properties
            .push((pid, code, name, value));
    }
    Ok(orders)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::test_support::{order_fixture, place_order};

    async fn user(db: &PgPool, login: &str) -> i64 {
        sqlx::query_scalar("INSERT INTO users (login, password_hash) VALUES ($1, '') RETURNING id")
            .bind(login)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn list_newest_first_with_paging(db: PgPool) {
        let f = order_fixture(&db).await;
        let u = user(&db, "u").await;
        let other = user(&db, "other").await;
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(place_order(&db, &f, Some(u)).await);
        }
        place_order(&db, &f, Some(other)).await;
        // Первый заказ — самый новый по дате, несмотря на меньший id
        sqlx::query("UPDATE orders SET created_at = now() + interval '1 hour' WHERE id = $1")
            .bind(ids[0])
            .execute(&db)
            .await
            .unwrap();
        let page: Vec<i64> = list(&db, u, 2, 0)
            .await
            .unwrap()
            .iter()
            .map(|o| o.id)
            .collect();
        assert_eq!(page, vec![ids[0], ids[2]]);
        let rest = list(&db, u, 2, 2).await.unwrap();
        assert_eq!(rest.iter().map(|o| o.id).collect::<Vec<_>>(), vec![ids[1]]);
        assert_eq!(count(&db, u).await.unwrap(), 3);
        let o = &rest[0];
        assert_eq!(
            (o.items.len(), o.shipments.len(), o.payments.len()),
            (1, 1, 1)
        );
        assert_eq!(o.shipments[0].delivery_name, "Самовывоз");
        assert_eq!(o.payments[0].name, "QR");
        assert_eq!(o.status_name, "Принят");
    }

    #[sqlx::test]
    async fn detail_of_other_user_is_none(db: PgPool) {
        let f = order_fixture(&db).await;
        let u = user(&db, "u").await;
        let other = user(&db, "other").await;
        let id = place_order(&db, &f, Some(u)).await;
        let guest = place_order(&db, &f, None).await;
        assert_eq!(detail(&db, id, u).await.unwrap().map(|o| o.id), Some(id));
        assert!(detail(&db, id, other).await.unwrap().is_none());
        assert!(detail(&db, guest, u).await.unwrap().is_none());
        assert!(detail(&db, id + 1000, u).await.unwrap().is_none());
    }
}
