//! Поле «Блоки» в админке: сохранение и control в карточке.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

async fn blocks_field(db: &PgPool, collection_id: i64) {
    sqlx::query(
        "INSERT INTO collection_fields (collection_id, code, name, kind, sort, multiple)
         VALUES ($1, 'blocks', 'Блоки', 'blocks', 500, FALSE)",
    )
    .bind(collection_id)
    .execute(db)
    .await
    .unwrap();
}

async fn post_multipart(
    app: &axum::Router,
    path: &str,
    cookie: &str,
    fields: &[(&str, &str)],
) -> (StatusCode, String) {
    let boundary = "XBOUNDARY";
    let mut body = String::new();
    for (name, value) in fields {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    let req = axum::http::Request::post(path)
        .header(
            axum::http::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(axum::http::header::COOKIE, cookie)
        .header(axum::http::header::HOST, "127.0.0.1:3000")
        .header(axum::http::header::ORIGIN, "http://127.0.0.1:3000")
        .body(axum::body::Body::from(body))
        .unwrap();
    use tower::ServiceExt;
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[sqlx::test]
async fn item_form_renders_blocks_control(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    blocks_field(&db, content.collection_id).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;

    let (status, _, page) = crate::test_support::get_raw(
        &app,
        &format!("/admin/items/{}", content.item_id),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let page = String::from_utf8_lossy(&page);
    assert!(page.contains("blocks-input"), "{page}");
    assert!(page.contains("data-collection="), "{page}");
}

#[sqlx::test]
async fn blocks_are_saved_and_empty_dropped(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    blocks_field(&db, content.collection_id).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;
    let blocks = json!([
        { "type": "heading", "heading": "Привет" },
        { "type": "text", "text": "" }
    ])
    .to_string();
    let section = content.section_id.to_string();
    let fields = [
        ("name", "Первая новость"),
        ("code", "pervaya"),
        ("active", "on"),
        ("sort", "500"),
        ("section_id", section.as_str()),
        ("prop_blocks", blocks.as_str()),
    ];
    let (status, _) = post_multipart(
        &app,
        &format!("/admin/items/{}", content.item_id),
        &cookie,
        &fields,
    )
    .await;
    assert!(status.is_redirection(), "{status}");

    let stored: Value = sqlx::query_scalar(
        "SELECT field_values->'blocks' FROM collection_items WHERE id = $1",
    )
    .bind(content.item_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(stored, json!([{ "type": "heading", "heading": "Привет", "level": 2 }]));
}

#[sqlx::test]
async fn broken_json_is_field_error(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    blocks_field(&db, content.collection_id).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;
    let section = content.section_id.to_string();
    let fields = [
        ("name", "Первая новость"),
        ("code", "pervaya"),
        ("active", "on"),
        ("sort", "500"),
        ("section_id", section.as_str()),
        ("prop_blocks", "{oops"),
    ];
    let (status, page) = post_multipart(
        &app,
        &format!("/admin/items/{}", content.item_id),
        &cookie,
        &fields,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("JSON-массив"), "{page}");

    let stored: Option<Value> = sqlx::query_scalar(
        "SELECT field_values->'blocks' FROM collection_items WHERE id = $1",
    )
    .bind(content.item_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(stored, None, "значение не сохранилось");
}
