//! SEO как вкладка карточки записи: рендер и сохранение вместе с записью.

use axum::http::StatusCode;
use sqlx::PgPool;

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
async fn item_page_has_seo_tab(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;
    let (status, _, page) =
        crate::test_support::get_raw(&app, &format!("/admin/items/{}", content.item_id), &cookie)
            .await;
    assert_eq!(status, StatusCode::OK);
    let page = String::from_utf8_lossy(&page);
    assert!(page.contains(r#"data-tab="seo""#), "{page}");
    assert!(page.contains(r#"name="seo_title""#), "{page}");
}

#[sqlx::test]
async fn seo_is_saved_with_item(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;
    let section = content.section_id.to_string();
    let fields = [
        ("name", "Первая новость"),
        ("code", "pervaya"),
        ("active", "on"),
        ("sort", "500"),
        ("section_id", section.as_str()),
        ("seo_title", "Заголовок для поиска"),
        ("seo_robots", "noindex,follow"),
    ];
    let (status, _) = post_multipart(
        &app,
        &format!("/admin/items/{}", content.item_id),
        &cookie,
        &fields,
    )
    .await;
    assert!(status.is_redirection(), "{status}");

    let (title, robots): (String, String) =
        sqlx::query_as("SELECT title, robots FROM seo_settings WHERE item_id = $1")
            .bind(content.item_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(title, "Заголовок для поиска");
    assert_eq!(robots, "noindex,follow");
}
