//! Обмен заказами с 1С (`type=sale`).

// Метку и настройки подключают следующие задачи плана обмена заказами.
#![allow(dead_code)]

pub mod document;
pub mod settings;

use sqlx::PgConnection;

/// Помечает текущую транзакцию как изменения из 1С: триггер `orders_mark_1c` ставит заказам
/// `updated_1c = TRUE` вместо сброса. Действует до конца транзакции.
pub async fn mark_exchange(conn: &mut PgConnection) -> sqlx::Result<()> {
    sqlx::query("SET LOCAL cms.exchange_1c = 'on'")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use sqlx::PgPool;

    use super::*;
    use crate::test_support::{order_fixture, place_order};

    async fn flag(db: &PgPool, id: i64) -> (bool, DateTime<Utc>) {
        sqlx::query_as("SELECT updated_1c, updated_at FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn site_change_resets_updated_1c(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;

        // Изменение из обмена: метка ставится
        let mut tx = db.begin().await.unwrap();
        mark_exchange(&mut tx).await.unwrap();
        sqlx::query("UPDATE orders SET manager_comment = 'из 1С' WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let (marked, at) = flag(&db, id).await;
        assert!(marked);

        // Изменение на сайте: метка сбрасывается, время растёт
        sqlx::query("UPDATE orders SET manager_comment = 'x' WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let (marked, later) = flag(&db, id).await;
        assert!(!marked);
        assert!(later > at);

        // Метка живёт только в своей транзакции
        let mut tx = db.begin().await.unwrap();
        mark_exchange(&mut tx).await.unwrap();
        sqlx::query("UPDATE orders SET manager_comment = 'y' WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(flag(&db, id).await.0);

        // Новая оплата на сайте — тоже изменение заказа
        let (_, before) = flag(&db, id).await;
        sqlx::query("INSERT INTO payments (order_id, name) VALUES ($1, 'Наличные')")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let (marked, after) = flag(&db, id).await;
        assert!(!marked);
        assert!(after > before);

        // Правка свойства, позиции и отгрузки двигают время заказа
        for sql in [
            "INSERT INTO order_property_values (order_id, property_id, value) VALUES ($1, 1, 'v')",
            "UPDATE cart_items SET quantity = 2 WHERE order_id = $1",
            "UPDATE shipments SET tracking_number = 't' WHERE order_id = $1",
        ] {
            let (_, before) = flag(&db, id).await;
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(id)
                .execute(&db)
                .await
                .unwrap();
            assert!(flag(&db, id).await.1 > before, "{sql}");
        }

        // Повтор того же значения — не изменение
        let (_, before) = flag(&db, id).await;
        sqlx::query("UPDATE orders SET manager_comment = manager_comment WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(flag(&db, id).await.1, before);
    }

    #[sqlx::test]
    async fn removed_shipments_are_logged(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let shipment: i64 = sqlx::query_scalar(
            "UPDATE shipments SET id_1c = 'ship-1c' WHERE order_id = $1 RETURNING id",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("DELETE FROM shipments WHERE id = $1")
            .bind(shipment)
            .execute(&db)
            .await
            .unwrap();
        let removed: Vec<(i64, i64, String)> =
            sqlx::query_as("SELECT order_id, shipment_id, id_1c FROM shipments_removed")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(removed, [(id, shipment, "ship-1c".to_string())]);

        // Удаление заказа целиком уносит и журнал, и не спотыкается о каскад
        let other = place_order(&db, &f, None).await;
        sqlx::query("DELETE FROM orders WHERE id = ANY($1)")
            .bind(vec![id, other])
            .execute(&db)
            .await
            .unwrap();
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM shipments_removed")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }
}
