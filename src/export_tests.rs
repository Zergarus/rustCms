//! Экспорт: JSON и CSV.

use sqlx::PgPool;

#[sqlx::test]
async fn export_writes_json_and_csv(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    sqlx::query(
        "UPDATE collection_items SET name = 'Новость \"в кавычках\"; с точкой' WHERE id = $1",
    )
    .bind(content.item_id)
    .execute(&db)
    .await
    .unwrap();
    let dir = std::env::temp_dir().join(format!("cms-export-{}", rand::random::<u64>()));

    let count = crate::export::export(&db, &dir).await.unwrap();
    assert_eq!(count, 1);
    let json = tokio::fs::read_to_string(dir.join("collections.json"))
        .await
        .unwrap();
    assert!(json.contains("\"news\""), "{json}");
    assert!(json.contains("Новость"), "{json}");

    let csv = tokio::fs::read_to_string(dir.join("news.csv"))
        .await
        .unwrap();
    assert!(csv.starts_with("id;code;name;active;"), "{csv}");
    assert!(
        csv.contains("Новость \"\"в кавычках\"\"; с точкой"),
        "{csv}"
    );

    tokio::fs::remove_dir_all(&dir).await.ok();
}

#[test]
fn csv_field_escapes_quotes() {
    assert_eq!(crate::export::csv_field("a\"b"), "\"a\"\"b\"");
    assert_eq!(crate::export::csv_field("a;b"), "\"a;b\"");
}
