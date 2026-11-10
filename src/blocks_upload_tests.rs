//! Загрузка картинок для поля «Блоки».

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;

const PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 252, 207, 192, 80, 15, 0, 4,
    133, 1, 128, 132, 169, 140, 33, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

async fn upload(
    app: &axum::Router,
    cookie: &str,
    collection: Option<i64>,
    name: &str,
    content_type: &str,
    data: &[u8],
) -> (StatusCode, Value) {
    let boundary = "XBOUNDARY";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let path = match collection {
        Some(id) => format!("/admin/blocks/upload?collection={id}"),
        None => "/admin/blocks/upload".to_string(),
    };
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[sqlx::test]
async fn png_upload_returns_path(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;

    let (status, body) = upload(
        &app,
        &cookie,
        Some(content.collection_id),
        "pic.png",
        "image/png",
        PNG,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let path = body["data"]["path"].as_str().unwrap_or_default();
    assert!(path.starts_with("/upload/blocks/"), "{body}");
    assert!(body["data"]["id"].as_i64().unwrap_or(0) > 0, "{body}");
}

#[sqlx::test]
async fn text_file_is_rejected(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM files")
        .fetch_one(&db)
        .await
        .unwrap();

    let (status, body) = upload(
        &app,
        &cookie,
        Some(content.collection_id),
        "note.png",
        "image/png",
        b"not an image",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM files")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(before, after, "файл не должен сохраняться");
}

#[sqlx::test]
async fn collection_is_required(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;
    let (status, _) = upload(&app, &cookie, None, "pic.png", "image/png", PNG).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn foreign_collection_is_forbidden(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::permitted_cookie(&db, "manager", &[]).await;
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM files")
        .fetch_one(&db)
        .await
        .unwrap();

    let (status, _) = upload(
        &app,
        &cookie,
        Some(content.collection_id),
        "pic.png",
        "image/png",
        PNG,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM files")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(before, after);
}
