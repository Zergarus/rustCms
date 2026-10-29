//! Штрихкоды товара: хранение (`catalog_barcodes`) и поиск товара по коду.

use sqlx::PgConnection;

use super::StockError;

/// Штрихкод товара: общий (`store_id`/`order_id` пусты) или единица на складе/в заказе.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct BarcodeRow {
    pub barcode: String,
    pub store_id: Option<i64>,
    pub order_id: Option<i64>,
}

fn reject(message: impl Into<String>) -> StockError {
    StockError::Rejected(vec![message.into()])
}

/// Заменяет общие штрихкоды товара. Пустые и повторы отбрасываются; код другого товара — отказ.
pub async fn set_common(
    tx: &mut PgConnection,
    item_id: i64,
    codes: &[String],
    user: Option<i64>,
) -> Result<(), StockError> {
    let mut clean: Vec<&str> = Vec::new();
    for c in codes {
        let c = c.trim();
        if !c.is_empty() && !clean.contains(&c) {
            clean.push(c);
        }
    }
    let name: String = sqlx::query_scalar("SELECT name FROM collection_items WHERE id = $1")
        .bind(item_id)
        .fetch_one(&mut *tx)
        .await?;
    for code in &clean {
        let other: Option<i64> = sqlx::query_scalar(
            "SELECT item_id FROM catalog_barcodes WHERE barcode = $1 AND item_id <> $2",
        )
        .bind(code)
        .bind(item_id)
        .fetch_optional(&mut *tx)
        .await?;
        if other.is_some() {
            return Err(reject(format!(
                "Штрихкод {code}, введенный для товара {name}, уже существует."
            )));
        }
    }
    sqlx::query(
        "DELETE FROM catalog_barcodes
         WHERE item_id = $1 AND store_id IS NULL AND order_id IS NULL AND barcode <> ALL($2)",
    )
    .bind(item_id)
    .bind(&clean)
    .execute(&mut *tx)
    .await?;
    for code in &clean {
        // Код мог уже быть у этого товара единицей на складе — его не трогаем
        sqlx::query(
            "INSERT INTO catalog_barcodes (item_id, barcode, created_by, modified_by)
             VALUES ($1, $2, $3, $3) ON CONFLICT (barcode) DO NOTHING",
        )
        .bind(item_id)
        .bind(code)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Флажок «поштучный учёт штрихкодов»; снять нельзя, пока есть штрихкоды единиц на складах.
pub async fn set_multi(tx: &mut PgConnection, item_id: i64, on: bool) -> Result<(), StockError> {
    if !on {
        let has: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM catalog_barcodes WHERE item_id = $1 AND store_id IS NOT NULL)",
        )
        .bind(item_id)
        .fetch_one(&mut *tx)
        .await?;
        if has {
            return Err(reject("У товара есть штрихкоды единиц на складах"));
        }
    }
    sqlx::query("UPDATE catalog_products SET barcode_multi = $2 WHERE item_id = $1")
        .bind(item_id)
        .bind(on)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Штрихкоды товара по возрастанию кода.
pub async fn list(tx: &mut PgConnection, item_id: i64) -> sqlx::Result<Vec<BarcodeRow>> {
    sqlx::query_as(
        "SELECT barcode, store_id, order_id FROM catalog_barcodes
         WHERE item_id = $1 ORDER BY barcode",
    )
    .bind(item_id)
    .fetch_all(tx)
    .await
}

/// Товар по штрихкоду: `catalog_barcodes`, затем поле `cml2_bar_code`; при нескольких — меньший id.
pub async fn find_item(db: &mut PgConnection, code: &str) -> sqlx::Result<Option<i64>> {
    let code = code.trim();
    if code.is_empty() {
        return Ok(None);
    }
    let found: Option<i64> =
        sqlx::query_scalar("SELECT min(item_id) FROM catalog_barcodes WHERE barcode = $1")
            .bind(code)
            .fetch_one(&mut *db)
            .await?;
    if found.is_some() {
        return Ok(found);
    }
    sqlx::query_scalar(
        "SELECT min(id) FROM collection_items WHERE field_values ->> 'cml2_bar_code' = $1",
    )
    .bind(code)
    .fetch_one(db)
    .await
}

/// Похоже на штрихкод: 8–14 цифр (EAN-8…GTIN-14).
pub fn looks_like_barcode(q: &str) -> bool {
    let q = q.trim();
    (8..=14).contains(&q.len()) && q.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::order_fixture;
    use sqlx::PgPool;

    async fn second_item(db: &PgPool, first: i64) -> i64 {
        let collection: i64 =
            sqlx::query_scalar("SELECT collection_id FROM collection_items WHERE id = $1")
                .bind(first)
                .fetch_one(db)
                .await
                .unwrap();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name)
             VALUES ($1, 'vtoroy', 'Второй') RETURNING id",
        )
        .bind(collection)
        .fetch_one(db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO catalog_products (item_id) VALUES ($1)")
            .bind(id)
            .execute(db)
            .await
            .unwrap();
        id
    }

    fn codes(rows: &[BarcodeRow]) -> Vec<&str> {
        rows.iter().map(|r| r.barcode.as_str()).collect()
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[sqlx::test]
    async fn set_common_replaces(db: PgPool) {
        let f = order_fixture(&db).await;
        let mut c = db.acquire().await.unwrap();
        set_common(&mut c, f.product_id, &strings(&["111", "222"]), None)
            .await
            .unwrap();
        set_common(
            &mut c,
            f.product_id,
            &strings(&[" 222 ", "333", "", "333"]),
            None,
        )
        .await
        .unwrap();
        let rows = list(&mut c, f.product_id).await.unwrap();
        assert_eq!(codes(&rows), ["222", "333"]);
    }

    #[sqlx::test]
    async fn set_common_rejects_foreign(db: PgPool) {
        let f = order_fixture(&db).await;
        let other = second_item(&db, f.product_id).await;
        let mut c = db.acquire().await.unwrap();
        set_common(&mut c, other, &strings(&["999"]), None)
            .await
            .unwrap();
        match set_common(&mut c, f.product_id, &strings(&["999"]), None).await {
            Err(StockError::Rejected(m)) => assert_eq!(
                m,
                ["Штрихкод 999, введенный для товара Товар, уже существует."]
            ),
            other => panic!("ожидался отказ, получено {other:?}"),
        }
    }

    #[sqlx::test]
    async fn set_multi_blocked_by_store_barcodes(db: PgPool) {
        let f = order_fixture(&db).await;
        let mut c = db.acquire().await.unwrap();
        set_multi(&mut c, f.product_id, true).await.unwrap();
        sqlx::query(
            "INSERT INTO catalog_barcodes (item_id, barcode, store_id) VALUES ($1, 'U1', 1)",
        )
        .bind(f.product_id)
        .execute(&mut *c)
        .await
        .unwrap();
        match set_multi(&mut c, f.product_id, false).await {
            Err(StockError::Rejected(m)) => {
                assert_eq!(m, ["У товара есть штрихкоды единиц на складах"])
            }
            other => panic!("ожидался отказ, получено {other:?}"),
        }
        let on: bool =
            sqlx::query_scalar("SELECT barcode_multi FROM catalog_products WHERE item_id = $1")
                .bind(f.product_id)
                .fetch_one(&mut *c)
                .await
                .unwrap();
        assert!(on);
    }

    #[sqlx::test]
    async fn find_item_by_barcode_then_field(db: PgPool) {
        let f = order_fixture(&db).await;
        let other = second_item(&db, f.product_id).await;
        let mut c = db.acquire().await.unwrap();
        sqlx::query("UPDATE collection_items SET field_values = $2 WHERE id = $1")
            .bind(f.product_id)
            .bind(serde_json::json!({"cml2_bar_code": "4600000000017"}))
            .execute(&mut *c)
            .await
            .unwrap();
        assert_eq!(
            find_item(&mut c, " 4600000000017 ").await.unwrap(),
            Some(f.product_id)
        );
        assert_eq!(find_item(&mut c, "0000").await.unwrap(), None);
        // Код в обоих местах у разных товаров — берётся из catalog_barcodes
        set_common(&mut c, other, &strings(&["4600000000017"]), None)
            .await
            .unwrap();
        assert_eq!(
            find_item(&mut c, "4600000000017").await.unwrap(),
            Some(other)
        );
    }

    #[test]
    fn looks_like_barcode_cases() {
        assert!(looks_like_barcode("4601234567890"));
        assert!(!looks_like_barcode("1044"));
        assert!(looks_like_barcode("12345678"));
        assert!(!looks_like_barcode("abc12345678"));
    }
}
