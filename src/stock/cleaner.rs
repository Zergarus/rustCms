//! Автоснятие просроченных резервов неоплаченных заказов (агент Битрикса `ClearProductReservedQuantity`).

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::state::AppState;

/// Сколько резервов снимается за один проход.
const LIMIT: i64 = 100;

/// Снимает до 100 резервов со сроком `date_reserve_end <= now` у неоплаченных и неотменённых
/// заказов; счётчики возвращает триггер. При периоде 0 ничего не делает. Возвращает число снятых.
pub async fn clear_expired(db: &PgPool, now: DateTime<Utc>) -> sqlx::Result<usize> {
    let mut tx = db.begin().await?;
    if super::settings(&mut tx).await?.clear_period <= 0 {
        return Ok(0);
    }
    let found: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT r.id, r.item_id FROM basket_reservations r
         JOIN cart_items c ON c.id = r.cart_item_id
         JOIN orders o ON o.id = c.order_id
         WHERE r.date_reserve_end <= $1 AND r.quantity > 0 AND NOT o.paid AND NOT o.canceled
         ORDER BY r.id LIMIT $2",
    )
    .bind(now)
    .bind(LIMIT)
    .fetch_all(&mut *tx)
    .await?;
    if found.is_empty() {
        return Ok(0);
    }
    let mut items: Vec<i64> = found.iter().map(|(_, item)| *item).collect();
    items.sort_unstable();
    items.dedup();
    crate::catalog::load_locked(&mut tx, &items).await?;
    // Условия повторно под блокировкой: за время ожидания заказ могли оплатить
    let ids: Vec<i64> = found.iter().map(|(id, _)| *id).collect();
    let cleared = sqlx::query(
        "DELETE FROM basket_reservations r USING cart_items c, orders o
         WHERE r.id = ANY($1) AND c.id = r.cart_item_id AND o.id = c.order_id
           AND r.date_reserve_end <= $2 AND r.quantity > 0 AND NOT o.paid AND NOT o.canceled",
    )
    .bind(&ids)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected() as usize;
    tx.commit().await?;
    Ok(cleared)
}

/// Фоновая задача: раз в час, ошибки — в лог.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(3600));
        loop {
            timer.tick().await;
            match clear_expired(&state.db, Utc::now()).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("снято просроченных резервов: {n}"),
                Err(e) => tracing::error!("снятие просроченных резервов: {e:#}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{order_fixture, place_order_with};
    use chrono::Duration as Span;

    async fn set_option(db: &PgPool, name: &str, value: &str) {
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ('sale', $1, $2)
             ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(name)
        .bind(value)
        .execute(db)
        .await
        .unwrap();
    }

    async fn quantity(db: &PgPool, item: i64) -> f64 {
        sqlx::query_scalar("SELECT quantity::float8 FROM catalog_products WHERE item_id = $1")
            .bind(item)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn expire(db: &PgPool, order: i64, end: Option<DateTime<Utc>>) {
        sqlx::query(
            "UPDATE basket_reservations SET date_reserve_end = $2
             WHERE cart_item_id IN (SELECT id FROM cart_items WHERE order_id = $1)",
        )
        .bind(order)
        .bind(end)
        .execute(db)
        .await
        .unwrap();
    }

    async fn count(db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM basket_reservations")
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn clear_expired_rules(db: PgPool) {
        let f = order_fixture(&db).await;
        sqlx::query("DELETE FROM cart_items WHERE id = $1")
            .bind(f.item_id)
            .execute(&db)
            .await
            .unwrap();
        set_option(&db, "product_reserve_clear_period", "2").await;
        let mut orders = Vec::new();
        for _ in 0..5 {
            orders.push(place_order_with(&db, &f, None, None, 1.0).await);
        }
        assert_eq!(quantity(&db, f.product_id).await, 5.0);
        let now = Utc::now();
        let yesterday = Some(now - Span::days(1));
        for o in &orders[..3] {
            expire(&db, *o, yesterday).await;
        }
        expire(&db, orders[3], Some(now + Span::days(1))).await;
        expire(&db, orders[4], None).await;
        sqlx::query("UPDATE orders SET paid = TRUE WHERE id = $1")
            .bind(orders[1])
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE orders SET canceled = TRUE WHERE id = $1")
            .bind(orders[2])
            .execute(&db)
            .await
            .unwrap();

        assert_eq!(clear_expired(&db, now).await.unwrap(), 1);
        assert_eq!(count(&db).await, 4);
        assert_eq!(quantity(&db, f.product_id).await, 6.0);
        let left: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM basket_reservations r JOIN cart_items c ON c.id = r.cart_item_id
             WHERE c.order_id = $1",
        )
        .bind(orders[0])
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(left, 0);
    }

    #[sqlx::test]
    async fn clear_period_zero_noop(db: PgPool) {
        let f = order_fixture(&db).await;
        let o = place_order_with(&db, &f, None, None, 1.0).await;
        expire(&db, o, Some(Utc::now() - Span::days(1))).await;
        assert_eq!(clear_expired(&db, Utc::now()).await.unwrap(), 0);
        assert_eq!(count(&db).await, 1);
    }

    #[sqlx::test]
    async fn clear_limit_100(db: PgPool) {
        let f = order_fixture(&db).await;
        set_option(&db, "product_reserve_clear_period", "1").await;
        let o = place_order_with(&db, &f, None, None, 1.0).await;
        let cart: i64 = sqlx::query_scalar("SELECT id FROM cart_items WHERE order_id = $1")
            .bind(o)
            .fetch_one(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM basket_reservations")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO basket_reservations (cart_item_id, item_id, quantity, date_reserve_end)
             SELECT $1, $2, 0.01, now() - interval '1 day' FROM generate_series(1, 150)",
        )
        .bind(cart)
        .bind(f.product_id)
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(clear_expired(&db, Utc::now()).await.unwrap(), 100);
        assert_eq!(clear_expired(&db, Utc::now()).await.unwrap(), 50);
        assert_eq!(count(&db).await, 0);
    }
}
