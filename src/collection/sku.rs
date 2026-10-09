//! Торговые предложения: связь предложения с товаром и тесты триггеров пересчёта типа
//! и доступности товара.

use serde_json::{Map, Value};

use super::{Collection, Field};

/// Код системного поля связи, которое создаёт `create_offer_collection`.
#[allow(dead_code)] // подключается в админке (следующая задача SKU)
pub const LINK_CODE: &str = "cml2_link";

/// SQL-выражение `field_values` записи `alias` (алиас или имя таблицы `collection_items`).
/// У предложения (`product_id` не пуст) добавлен ключ кода поля `sku_field_id` её
/// коллекции со значением `product_id` — родительского товара, не позиции корзины.
/// В самой БД значение связи в JSONB не хранится.
pub fn field_values_sql(alias: &str) -> String {
    format!(
        "(CASE WHEN {alias}.product_id IS NULL THEN {alias}.field_values \
         ELSE {alias}.field_values || COALESCE((SELECT jsonb_build_object(f.code, {alias}.product_id) \
         FROM collections c JOIN collection_fields f ON f.id = c.sku_field_id \
         WHERE c.id = {alias}.collection_id), '{{}}'::jsonb) END)"
    )
}

/// Вынимает из `values` значение поля связи коллекции предложений и возвращает id
/// родительского товара (число или строка с числом, у множественного — первый).
pub fn take_link(
    collection: &Collection,
    fields: &[Field],
    values: &mut Map<String, Value>,
) -> Option<i64> {
    let id = collection.sku_field_id?;
    let code = &fields.iter().find(|f| f.id == id)?.code;
    let value = values.remove(code)?;
    let value = match value {
        Value::Array(items) => items.into_iter().next()?,
        other => other,
    };
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use crate::collection::repo;

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
        assert_eq!(state(&db, offer).await.unwrap().0, 4);
    }

    #[sqlx::test]
    async fn offer_type_follows_link(db: PgPool) {
        let (products, offers) = collections(&db).await;
        let product = item(&db, products, "tovar", None).await;
        let rec = item(&db, offers, "rec", None).await;
        stock(&db, rec, 5, true, false).await;
        assert_eq!(state(&db, rec).await.unwrap().0, 1);

        sqlx::query("UPDATE collection_items SET product_id = $1 WHERE id = $2")
            .bind(product)
            .bind(rec)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, rec).await.unwrap().0, 4);
        assert_eq!(state(&db, product).await, Some((3, true)));

        sqlx::query("UPDATE collection_items SET product_id = NULL WHERE id = $1")
            .bind(rec)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, rec).await.unwrap().0, 1);
        assert_eq!(state(&db, product).await, Some((6, false)));
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

    /// Каталог с коллекцией предложений, созданной через `create_offer_collection`.
    async fn linked(db: &PgPool) -> (i64, crate::collection::Collection, i64, i64) {
        let products: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name, is_catalog) VALUES ('catalog', 'Каталог', TRUE) RETURNING id",
        )
        .fetch_one(db)
        .await
        .unwrap();
        let product = item(db, products, "tovar", None).await;
        let parent = repo::get_collection(db, products).await.unwrap().unwrap();
        let offers = repo::create_offer_collection(db, &parent).await.unwrap();
        let offer = item(db, offers.id, "offer", Some(product)).await;
        (products, offers, product, offer)
    }

    #[sqlx::test]
    async fn link_reads_as_field(db: PgPool) {
        let (products, offers, product, offer) = linked(&db).await;
        assert_eq!(offers.code, "catalog_offers");
        assert_eq!(offers.product_collection_id, Some(products));
        assert!(offers.is_catalog);
        let fields = repo::list_fields(&db, offers.id).await.unwrap();
        let link = fields.iter().find(|f| f.code == "cml2_link").unwrap();
        assert_eq!(link.kind, "element");
        assert_eq!(link.link_collection_id, Some(products));
        assert_eq!(offers.sku_field_id, Some(link.id));

        let read = repo::get_item(&db, offer).await.unwrap().unwrap();
        assert_eq!(read.field_values["cml2_link"], json!(product));
        assert_eq!(read.product_id, Some(product));
        let (listed, _) = repo::list_items(&db, offers.id, None, 10, 0).await.unwrap();
        assert_eq!(listed[0].field_values["cml2_link"], json!(product));
        // у товара ключа нет, в самой БД значение не дублируется
        let plain = repo::get_item(&db, product).await.unwrap().unwrap();
        assert!(plain.field_values.get("cml2_link").is_none());
        let stored: bool = sqlx::query_scalar(
            "SELECT field_values ? 'cml2_link' FROM collection_items WHERE id = $1",
        )
        .bind(offer)
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(!stored);
    }

    #[sqlx::test]
    async fn take_link_removes_value(db: PgPool) {
        let (_, offers, product, _) = linked(&db).await;
        let fields = repo::list_fields(&db, offers.id).await.unwrap();
        let mut values = serde_json::Map::new();
        values.insert("cml2_link".into(), json!(product));
        values.insert("other".into(), json!(1));
        assert_eq!(
            super::take_link(&offers, &fields, &mut values),
            Some(product)
        );
        assert!(!values.contains_key("cml2_link"));
        assert!(values.contains_key("other"));
        let mut text = serde_json::Map::new();
        text.insert("cml2_link".into(), json!(product.to_string()));
        assert_eq!(super::take_link(&offers, &fields, &mut text), Some(product));
        assert_eq!(super::take_link(&offers, &fields, &mut text), None);
    }

    #[sqlx::test]
    async fn create_and_update_write_product_id(db: PgPool) {
        let (_, offers, product, _) = linked(&db).await;
        let other = item(&db, offers.product_collection_id.unwrap(), "drugoy", None).await;
        let mut input = crate::collection::ItemInput {
            section_id: None,
            code: "novoe".into(),
            xml_id: String::new(),
            name: "Новое".into(),
            active: true,
            sort: 500,
            preview_text: String::new(),
            detail_text: String::new(),
            preview_picture_id: None,
            detail_picture_id: None,
            published_at: None,
            field_values: serde_json::Map::new(),
            product_id: Some(product),
        };
        let id = repo::create_item(&db, offers.id, &input).await.unwrap();
        assert_eq!(
            repo::get_item(&db, id).await.unwrap().unwrap().product_id,
            Some(product)
        );
        input.product_id = Some(other);
        repo::update_item(&db, id, &input).await.unwrap();
        assert_eq!(
            repo::get_item(&db, id).await.unwrap().unwrap().product_id,
            Some(other)
        );
    }

    #[sqlx::test]
    async fn link_and_unlink_offers(db: PgPool) {
        let (products, offers) = collections(&db).await;
        // существующая пара из collections() без поля связи: link_offers его создаёт
        repo::link_offers(&db, products, offers).await.unwrap();
        let c = repo::get_collection(&db, offers).await.unwrap().unwrap();
        assert!(c.sku_field_id.is_some());
        repo::link_offers(&db, products, offers).await.unwrap();
        let fields = repo::list_fields(&db, offers).await.unwrap();
        assert_eq!(fields.iter().filter(|f| f.code == "cml2_link").count(), 1);

        let product = item(&db, products, "tovar", None).await;
        item(&db, offers, "offer", Some(product)).await;
        assert!(matches!(
            repo::unlink_offers(&db, products).await,
            Err(repo::UnlinkError::HasOffers(1))
        ));
        sqlx::query("DELETE FROM collection_items WHERE collection_id = $1")
            .bind(offers)
            .execute(&db)
            .await
            .unwrap();
        repo::unlink_offers(&db, products).await.unwrap();
        let c = repo::get_collection(&db, offers).await.unwrap().unwrap();
        assert_eq!((c.product_collection_id, c.sku_field_id), (None, None));
        assert!(repo::list_fields(&db, offers).await.unwrap().is_empty());
    }
}
