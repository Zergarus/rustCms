//! bxapi отдаёт поле «Блоки» JSON-массивом.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

async fn post_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let req = axum::http::Request::post(path)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    use tower::ServiceExt;
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[sqlx::test]
async fn blocks_field_is_returned_as_json_array(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    sqlx::query(
        "INSERT INTO collection_fields (collection_id, code, name, kind, sort, multiple)
         VALUES ($1, 'blocks', 'Блоки', 'blocks', 500, FALSE)",
    )
    .bind(content.collection_id)
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE collection_items
         SET field_values = field_values || jsonb_build_object('blocks', $2::jsonb)
         WHERE id = $1",
    )
    .bind(content.item_id)
    .bind(
        json!([
            { "type": "heading", "heading": "Заголовок", "level": 2 },
            { "type": "text", "text": "Текст" }
        ])
        .to_string(),
    )
    .execute(&db)
    .await
    .unwrap();

    let app = crate::app(crate::test_support::test_state(db.clone()));
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/iblock/news/element/{}", content.item_id),
        json!({ "select": ["blocks"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let blocks = &body["data"]["item"]["blocks"];
    assert_eq!(blocks.as_array().map(Vec::len), Some(2), "{body}");
    assert_eq!(blocks[0]["type"], "heading", "{body}");
    assert_eq!(blocks[1]["type"], "text", "{body}");
}
