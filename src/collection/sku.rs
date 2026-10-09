//! Торговые предложения: тесты триггеров пересчёта типа и доступности товара.

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    async fn item(db: &PgPool, collection: i64, code: &str, product: Option<i64>) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name, product_id)
             VALUES ($1, $2, $2, $3) RETURNING id",
        )
        .bind(collection)
        .bind(code)
        .bind(product)
        .fetch_one(db)
        .await
        .unwrap()
    }

    /// Коллекция товаров и коллекция предложений.
    async fn collections(db: &PgPool) -> (i64, i64) {
        let products: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name, is_catalog) VALUES ('catalog', 'Каталог', TRUE) RETURNING id",
        )
        .fetch_one(db)
        .await
        .unwrap();
        let offers: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name, is_catalog, product_collection_id)
             VALUES ('offers', 'Предложения', TRUE, $1) RETURNING id",
        )
        .bind(products)
        .fetch_one(db)
        .await
        .unwrap();
        (products, offers)
    }

    async fn stock(db: &PgPool, id: i64, quantity: i32, trace: bool, zero: bool) {
        sqlx::query(
            "INSERT INTO catalog_products (item_id, quantity, quantity_trace, can_buy_zero)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(quantity)
        .bind(trace)
        .bind(zero)
        .execute(db)
        .await
        .unwrap();
    }

    async fn state(db: &PgPool, id: i64) -> Option<(i16, bool)> {
        sqlx::query_as("SELECT type, available FROM catalog_products WHERE item_id = $1")
            .bind(id)
            .fetch_optional(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn type_follows_offers(db: PgPool) {
        let (products, offers) = collections(&db).await;
        let product = item(&db, products, "tovar", None).await;
        let offer = item(&db, offers, "offer", Some(product)).await;
        stock(&db, offer, 5, true, false).await;
        assert_eq!(state(&db, product).await, Some((3, true)));
        assert_eq!(state(&db, offer).await.unwrap().0, 4);

        sqlx::query("DELETE FROM collection_items WHERE id = $1")
            .bind(offer)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, product).await, Some((6, false)));
    }

    #[sqlx::test]
    async fn availability_follows_offer_stock(db: PgPool) {
        let (products, offers) = collections(&db).await;
        let product = item(&db, products, "tovar", None).await;
        let offer = item(&db, offers, "offer", Some(product)).await;
        stock(&db, offer, 5, true, false).await;
        assert_eq!(state(&db, product).await, Some((3, true)));

        sqlx::query("UPDATE catalog_products SET quantity = 0 WHERE item_id = $1")
            .bind(offer)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, product).await, Some((3, false)));

        sqlx::query("UPDATE catalog_products SET can_buy_zero = TRUE WHERE item_id = $1")
            .bind(offer)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, product).await, Some((3, true)));

        sqlx::query("UPDATE collection_items SET active = FALSE WHERE id = $1")
            .bind(offer)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, product).await, Some((3, false)));
    }

    #[sqlx::test]
    async fn moving_offer_refreshes_both_products(db: PgPool) {
        let (products, offers) = collections(&db).await;
        let a = item(&db, products, "a", None).await;
        let b = item(&db, products, "b", None).await;
        let offer = item(&db, offers, "offer", Some(a)).await;
        stock(&db, offer, 5, true, false).await;
        assert_eq!(state(&db, a).await, Some((3, true)));

        sqlx::query("UPDATE collection_items SET product_id = $1 WHERE id = $2")
            .bind(b)
            .bind(offer)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, a).await, Some((6, false)));
        assert_eq!(state(&db, b).await, Some((3, true)));
    }

    #[sqlx::test]
    async fn product_delete_cascades_offers(db: PgPool) {
        let (products, offers) = collections(&db).await;
        let product = item(&db, products, "tovar", None).await;
        let offer = item(&db, offers, "offer", Some(product)).await;
        stock(&db, offer, 5, true, false).await;

        sqlx::query("DELETE FROM collection_items WHERE id = $1")
            .bind(product)
            .execute(&db)
            .await
            .unwrap();
        let left: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM collection_items WHERE id IN ($1, $2))
                  + (SELECT count(*) FROM catalog_products WHERE item_id IN ($1, $2))",
        )
        .bind(product)
        .bind(offer)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(left, 0);
    }
}
