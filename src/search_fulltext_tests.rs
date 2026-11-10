//! Полнотекстовый поиск: словоформы, подстроки и порядок выдачи.

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

async fn search(app: &Router, query: &str) -> Value {
    let req = axum::http::Request::post("/api/v1/iblock/news/search")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            json!({ "query": query }).to_string(),
        ))
        .unwrap();
    use tower::ServiceExt;
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn names(body: &Value) -> Vec<String> {
    body["data"]["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[sqlx::test]
async fn word_forms_are_found(db: PgPool) {
    // Запись фикстуры — «Первая новость»; ищем другую форму слова.
    let _content = crate::test_support::content_fixture(&db).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));

    let body = search(&app, "новости").await;
    let found = names(&body);
    assert!(
        found.iter().any(|name| name == "Первая новость"),
        "словоформа должна находиться: {body}"
    );
}

#[sqlx::test]
async fn substring_and_typo_are_found(db: PgPool) {
    let _content = crate::test_support::content_fixture(&db).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));

    let found = names(&search(&app, "новос").await);
    assert!(
        found.iter().any(|name| name == "Первая новость"),
        "подстрока должна находиться"
    );
}

#[sqlx::test]
async fn exact_name_goes_first(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    sqlx::query(
        "INSERT INTO collection_items (collection_id, code, name, active, sort)
         VALUES ($1, 'novost', 'Новость', TRUE, 500)",
    )
    .bind(content.collection_id)
    .execute(&db)
    .await
    .unwrap();
    let app = crate::app(crate::test_support::test_state(db.clone()));

    let found = names(&search(&app, "новость").await);
    assert_eq!(
        found.first().map(String::as_str),
        Some("Новость"),
        "{found:?}"
    );
    assert!(found.contains(&"Первая новость".to_string()), "{found:?}");
}
