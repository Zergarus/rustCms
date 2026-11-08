use crate::test_support::{get_raw, test_state};
use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;

/// Фикстура: catalog (3 активных — из них один с пустым кодом и один с заглавными
/// в коде, 1 неактивная, 1 с будущей публикацией), info (2), schemes (1).
/// Коллекция `contacts` отсутствует вовсе.
async fn fixture(db: &PgPool) -> PgPool {
    for q in [
        "INSERT INTO collections(code,name,api_enabled) VALUES ('catalog','Каталог',TRUE)",
        "INSERT INTO collections(code,name,api_enabled) VALUES ('info','Статьи',TRUE)",
        "INSERT INTO collections(code,name,api_enabled) VALUES ('schemes','Схемы',TRUE)",
    ] {
        sqlx::query(q).execute(db).await.unwrap();
    }
    for (collection, code, active, published) in [
        ("catalog", "catalog1", true, None),
        ("catalog", "CATALOG2", true, None), // путь должен прийти в нижнем регистре
        ("catalog", "", true, None),         // пустой код — путь по id
        ("catalog", "catalog4", false, None), // неактивная
        ("catalog", "catalog5", true, Some("2999-01-01")), // будущая публикация
        ("info", "info1", true, None),
        ("info", "info2", true, None),
        ("schemes", "schemes1", true, None),
    ] {
        sqlx::query(
            "INSERT INTO collection_items(collection_id,code,name,active,published_at)
             SELECT id,$1,$2,$3,$4::timestamptz FROM collections WHERE code=$5",
        )
        .bind(code)
        .bind(format!("Запись {code}"))
        .bind(active)
        .bind(published)
        .bind(collection)
        .execute(db)
        .await
        .unwrap();
    }
    db.clone()
}

#[sqlx::test]
async fn sitemap_index_counts_and_order(db: PgPool) {
    let db = fixture(&db).await;
    let app = crate::app(test_state(db));
    let (status, _, body) = get_raw(&app, "/api/v1/seo/sitemap", "").await;

    assert_eq!(status, StatusCode::OK, "{body:?}");
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "success");
    assert_eq!(json["errors"], serde_json::json!([]));

    let items = json["data"]["items"].as_array().unwrap();
    let types: Vec<&str> = items
        .iter()
        .map(|i| i["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["products", "catalog", "news", "schemes", "contacts"]);

    let count = |t: &str| {
        items
            .iter()
            .find(|i| i["type"] == t)
            .unwrap()["count"]
            .as_i64()
            .unwrap()
    };
    assert_eq!(count("products"), 3);
    assert_eq!(count("catalog"), 0);
    assert_eq!(count("news"), 2);
    assert_eq!(count("schemes"), 1);
    assert_eq!(count("contacts"), 0);

    for t in ["catalog", "contacts"] {
        assert!(
            items.iter().find(|i| i["type"] == t).unwrap()["lastmod"].is_null(),
            "{t}"
        );
    }
    let lastmod = items[0]["lastmod"].as_str().unwrap();
    assert!(lastmod.ends_with("+00:00"), "{lastmod}");
}
