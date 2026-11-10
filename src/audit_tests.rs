//! Журнал действий: запись событий и страница просмотра.

use axum::http::StatusCode;
use sqlx::PgPool;

async fn audit_rows(db: &PgPool, entity: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE entity = $1")
        .bind(entity)
        .fetch_one(db)
        .await
        .unwrap()
}

#[sqlx::test]
async fn settings_save_is_logged(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;
    let body = "site_name=Сайт&server_name=example.ru&email_from=&smtp_host=&smtp_port=&smtp_user=&smtp_password=&smtp_secure=";
    let (status, _, _) =
        crate::test_support::post_form_raw(&app, "/admin/settings", &cookie, body).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (login, action): (String, String) =
        sqlx::query_as("SELECT user_login, action FROM audit_log WHERE entity = 'settings'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(action, "settings");
    assert_eq!(login, "smoke");
}

#[sqlx::test]
async fn item_delete_is_logged(db: PgPool) {
    let content = crate::test_support::content_fixture(&db).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;

    let (status, _, _) = crate::test_support::post_form_raw(
        &app,
        &format!("/admin/items/{}/delete", content.item_id),
        &cookie,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "удаление записи");

    let (entity_id, summary): (String, String) = sqlx::query_as(
        "SELECT entity_id, summary FROM audit_log WHERE entity = 'item' AND action = 'delete'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(entity_id, content.item_id.to_string());
    assert!(summary.contains("удалена запись"), "{summary}");
}

#[sqlx::test]
async fn order_paid_is_logged(db: PgPool) {
    let fixture = crate::test_support::order_fixture(&db).await;
    let order_id = crate::test_support::place_order(&db, &fixture, None).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;

    let (status, _, _) = crate::test_support::post_form_raw(
        &app,
        &format!("/admin/shop/orders/{order_id}/paid"),
        &cookie,
        "value=Y&reason=",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "оплата заказа");

    let (action, entity_id): (String, String) = sqlx::query_as(
        "SELECT action, entity_id FROM audit_log WHERE entity = 'order' AND action = 'paid'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(action, "paid");
    assert_eq!(entity_id, order_id.to_string());
}

#[sqlx::test]
async fn audit_page_requires_super_and_shows_entries(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;
    // Создаём запись журнала, затем смотрим страницу.
    let body = "site_name=Сайт&server_name=&email_from=&smtp_host=&smtp_port=&smtp_user=&smtp_password=&smtp_secure=";
    crate::test_support::post_form_raw(&app, "/admin/settings", &cookie, body).await;
    let (status, _, page) = crate::test_support::get_raw(&app, "/admin/audit", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let page = String::from_utf8_lossy(&page);
    assert!(page.contains("Журнал действий"), "{page}");
    assert!(page.contains("smoke"), "{page}");

    let editor = crate::test_support::permitted_cookie(&db, "editor", &[]).await;
    let (status, _, _) = crate::test_support::get_raw(&app, "/admin/audit", &editor).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(audit_rows(&db, "settings").await, 1);
}
