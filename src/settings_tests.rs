use axum::http::StatusCode;
use sqlx::PgPool;

use crate::settings;

fn form(pairs: &[(&str, &str)]) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

async fn save(app: &axum::Router, cookie: &str, pairs: &[(&str, &str)]) -> StatusCode {
    let (status, _, _) =
        crate::test_support::post_form_raw(app, "/admin/settings", cookie, &form(pairs)).await;
    status
}

#[sqlx::test]
async fn settings_roundtrip_and_smtp(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;

    let (status, _, page) = crate::test_support::get_raw(&app, "/admin/settings", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let page = String::from_utf8_lossy(&page);
    assert!(page.contains("Настройки сайта"), "{page}");

    let status = save(
        &app,
        &cookie,
        &[
            ("site_name", "ТрансОпт"),
            ("server_name", "example.ru"),
            ("email_from", "no-reply@example.ru"),
            ("smtp_host", "smtp.example.ru"),
            ("smtp_port", "465"),
            ("smtp_user", "postbox"),
            ("smtp_password", "secret"),
            ("smtp_secure", "tls"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let loaded = settings::load(&db).await.unwrap();
    assert_eq!(loaded.site_name, "ТрансОпт");
    assert_eq!(loaded.server_name, "example.ru");
    assert_eq!(loaded.email_from, "no-reply@example.ru");
    assert_eq!(loaded.smtp_host, "smtp.example.ru");

    let smtp = settings::smtp(&db).await.unwrap().unwrap();
    assert_eq!(smtp.port, 465);
    assert_eq!(smtp.user, "postbox");
    assert_eq!(smtp.password, "secret");
    assert_eq!(smtp.secure, "tls");

    // Пароль не отдаётся в форму.
    let (_, _, page) = crate::test_support::get_raw(&app, "/admin/settings", &cookie).await;
    assert!(!String::from_utf8_lossy(&page).contains("secret"));

    // Пустой пароль оставляет прежний.
    let status = save(
        &app,
        &cookie,
        &[
            ("site_name", "ТрансОпт"),
            ("smtp_host", "smtp.example.ru"),
            ("smtp_port", "465"),
            ("smtp_password", ""),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let smtp = settings::smtp(&db).await.unwrap().unwrap();
    assert_eq!(smtp.password, "secret");
}

#[sqlx::test]
async fn settings_invalid_port_is_not_saved(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;
    let status = save(
        &app,
        &cookie,
        &[("smtp_host", "smtp.example.ru"), ("smtp_port", "99999")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "форма с ошибкой отдаётся страницей");

    let loaded = settings::load(&db).await.unwrap();
    assert!(loaded.smtp_host.is_empty(), "ничего не сохранено");
}

#[sqlx::test]
async fn settings_require_super(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::permitted_cookie(&db, "manager", &[]).await;

    let (status, _, _) = crate::test_support::get_raw(&app, "/admin/settings", &cookie).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let status = save(&app, &cookie, &[("site_name", "Взлом")]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn smtp_none_without_host(db: PgPool) {
    assert!(settings::smtp(&db).await.unwrap().is_none());
}

#[sqlx::test]
async fn invalid_sender_is_not_saved(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;
    let status = save(&app, &cookie, &[("email_from", "no-reply@")]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(settings::load(&db).await.unwrap().email_from.is_empty());
}
