//! Версии записи: снимок, история и восстановление.

use axum::http::StatusCode;
use sqlx::PgPool;

use crate::versions;

#[sqlx::test]
async fn snapshot_history_and_restore(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    let state = crate::test_support::test_state(db.clone());
    let cookie = crate::test_support::admin_cookie(&db).await;
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE login = 'smoke'")
        .fetch_one(&db)
        .await
        .unwrap();

    // Первая версия — исходное имя.
    versions::snapshot(&db, content.item_id, "create", user_id, "smoke").await;
    sqlx::query(
        "UPDATE collection_items SET name = 'Новое имя', field_values = '{\"x\":1}' WHERE id = $1",
    )
    .bind(content.item_id)
    .execute(&db)
    .await
    .unwrap();
    versions::snapshot(&db, content.item_id, "update", user_id, "smoke").await;

    let history = versions::history(&db, content.item_id).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].action, "update");
    assert_eq!(history[1].action, "create");

    // Восстанавливаем первую версию через админский маршрут.
    let app = crate::app(state);
    let (status, _, _) = crate::test_support::post_form_raw(
        &app,
        &format!(
            "/admin/items/{}/revisions/{}/restore",
            content.item_id, history[1].id
        ),
        &cookie,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "восстановление версии");
    let (name, xml_id, values): (String, String, sqlx::types::Json<serde_json::Value>) =
        sqlx::query_as("SELECT name, xml_id, field_values FROM collection_items WHERE id = $1")
            .bind(content.item_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(name, "Первая новость");
    assert_eq!(xml_id, "", "xml_id тоже восстанавливается");
    assert!(values.0.get("color").is_some(), "{values:?}");

    // Восстановление — новая версия в истории и запись в журнале действий.
    let history = versions::history(&db, content.item_id).await.unwrap();
    assert_eq!(history[0].action, "restore");
    let audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE entity = 'item' AND action = 'restore'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(audit, 1);

    // Страница истории доступна владельцу прав на коллекцию.
    let (status, _, page) = crate::test_support::get_raw(
        &app,
        &format!("/admin/items/{}/history", content.item_id),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let page = String::from_utf8_lossy(&page);
    assert!(page.contains("История"), "{page}");
    assert!(page.contains("restore"), "{page}");
}

#[sqlx::test]
async fn item_update_writes_revision(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;

    let body = [
        ("name", "Запись изменена"),
        ("code", "news-item"),
        ("active", "on"),
        ("sort", "500"),
        ("section", &content.section_id.to_string()),
    ];
    let (status, _) = post_multipart(
        &app,
        &format!("/admin/items/{}", content.item_id),
        &cookie,
        &body,
    )
    .await;
    assert!(status.is_redirection(), "{status}");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM item_revisions WHERE item_id = $1")
        .bind(content.item_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 1, "сохранение записи пишет версию");
    let name: String = sqlx::query_scalar(
        "SELECT data->>'name' FROM item_revisions WHERE item_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(content.item_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(name, "Запись изменена");
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

/// Старый код занят другой записью — понятная ошибка, а не 500.
#[sqlx::test]
async fn restore_rejects_taken_code(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE login = 'smoke'")
        .fetch_one(&db)
        .await
        .unwrap();

    // Снимок с кодом pervaya, затем код записи меняется.
    versions::snapshot(&db, content.item_id, "create", user_id, "smoke").await;
    sqlx::query("UPDATE collection_items SET code = 'changed' WHERE id = $1")
        .bind(content.item_id)
        .execute(&db)
        .await
        .unwrap();
    // Код pervaya теперь занят другой записью.
    sqlx::query(
        "INSERT INTO collection_items (collection_id, code, name) VALUES ($1, 'pervaya', 'Другая')",
    )
    .bind(content.collection_id)
    .execute(&db)
    .await
    .unwrap();

    let revision: i64 =
        sqlx::query_scalar("SELECT id FROM item_revisions WHERE item_id = $1 ORDER BY id LIMIT 1")
            .bind(content.item_id)
            .fetch_one(&db)
            .await
            .unwrap();
    let (status, _, _) = crate::test_support::post_form_raw(
        &app,
        &format!(
            "/admin/items/{}/revisions/{revision}/restore",
            content.item_id
        ),
        &cookie,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "занятый код");
    let code: String = sqlx::query_scalar("SELECT code FROM collection_items WHERE id = $1")
        .bind(content.item_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(code, "changed", "запись не изменилась");
}
