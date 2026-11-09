use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

async fn post(app: &Router, path: &str, body: Value, cookie: &str) -> (StatusCode, Value) {
    let mut req = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, cookie);
    if !cookie.is_empty() {
        req = req.header(header::ORIGIN, "http://127.0.0.1:3000");
    }
    let req = req.body(Body::from(body.to_string())).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

fn register_body(email: &str) -> Value {
    json!({
        "name": "Иван",
        "email": email,
        "phone": "+7 900 000-00-00",
        "password": "secret1",
        "passwordConfirm": "secret1",
        "personalData": true,
        "smartCaptchaToken": "ignored",
    })
}

/// Кладёт в токен известный код, как будто он пришёл письмом.
async fn set_token(db: &PgPool, user_id: i64, kind: &str, code: &str) {
    sqlx::query(
        "INSERT INTO auth_tokens (user_id, kind, token_hash, expires_at)
         VALUES ($1, $2, $3, now() + interval '1 hour')
         ON CONFLICT (kind, user_id) DO UPDATE SET token_hash = EXCLUDED.token_hash",
    )
    .bind(user_id)
    .bind(kind)
    .bind(crate::bxapi::auth::token_hash(code))
    .execute(db)
    .await
    .unwrap();
}

#[sqlx::test]
async fn register_creates_pending_user(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let (status, json) = post(
        &app,
        "/api/v1/auth/register",
        register_body("Ivan@Example.RU"),
        "",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["status"], "success");
    assert!(json["data"]["user"].is_null());
    assert_eq!(json["data"]["pending"], true);
    assert!(json["data"]["csrfToken"].as_str().unwrap().len() > 10);

    let user: (String, bool, String) =
        sqlx::query_as("SELECT login, active, phone FROM users WHERE login = 'ivan@example.ru'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(user.0, "ivan@example.ru");
    assert!(!user.1, "до подтверждения пользователь неактивен");
    assert_eq!(user.2, "+7 900 000-00-00");

    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_tokens WHERE kind = 'confirm'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(tokens, 1);
}

#[sqlx::test]
async fn register_validation_codes(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));

    let cases = [
        (
            json!({"name": "И", "email": "bad", "password": "secret1", "passwordConfirm": "secret1", "personalData": true}),
            "invalid_email",
        ),
        (
            json!({"name": "И", "email": "a@b.ru", "password": "123", "passwordConfirm": "123", "personalData": true}),
            "weak_password",
        ),
        (
            json!({"name": "И", "email": "a@b.ru", "password": "secret1", "passwordConfirm": "secret2", "personalData": true}),
            "password_mismatch",
        ),
        (
            json!({"name": "И", "email": "a@b.ru", "password": "secret1", "passwordConfirm": "secret1", "personalData": false}),
            "personal_data_required",
        ),
        (
            json!({"email": "a@b.ru", "password": "secret1", "passwordConfirm": "secret1", "personalData": true}),
            "name_required",
        ),
    ];
    for (body, code) in cases {
        let (status, json) = post(&app, "/api/v1/auth/register", body, "").await;
        assert_eq!(status, StatusCode::OK, "{code}");
        assert_eq!(json["status"], "error", "{code}");
        assert_eq!(json["errors"][0]["code"], code, "{json}");
    }

    let (_, _) = post(
        &app,
        "/api/v1/auth/register",
        register_body("dup@example.ru"),
        "",
    )
    .await;
    let (_, json) = post(
        &app,
        "/api/v1/auth/register",
        register_body("DUP@example.ru"),
        "",
    )
    .await;
    assert_eq!(json["errors"][0]["code"], "email_exists");
}

#[sqlx::test]
async fn confirm_activates_and_signs_in(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let (_, _) = post(
        &app,
        "/api/v1/auth/register",
        register_body("p@example.ru"),
        "",
    )
    .await;
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE login = 'p@example.ru'")
        .fetch_one(&db)
        .await
        .unwrap();
    set_token(&db, user_id, "confirm", "code-123").await;

    let (status, json) = post(
        &app,
        "/api/v1/auth/confirm-registration",
        json!({"login": "p@example.ru", "confirmCode": "wrong"}),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["errors"][0]["code"], "invalid_confirm_code");

    let (status, json) = post(
        &app,
        "/api/v1/auth/confirm-registration",
        json!({"login": "P@example.ru", "confirmCode": "code-123"}),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"]["user"]["login"], "p@example.ru");

    let active: bool = sqlx::query_scalar("SELECT active FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(active);
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM site_sessions WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(sessions, 1);
    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_tokens WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(tokens, 0, "код подтверждения одноразовый");
}

#[sqlx::test]
async fn forgot_is_opaque_and_creates_token(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    post(
        &app,
        "/api/v1/auth/register",
        register_body("f@example.ru"),
        "",
    )
    .await;

    let (status, json) = post(
        &app,
        "/api/v1/auth/forgot-password",
        json!({"login": "nobody@example.ru"}),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"]["sent"], true);
    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_tokens WHERE kind = 'reset'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(tokens, 0);

    let (_, json) = post(
        &app,
        "/api/v1/auth/forgot-password",
        json!({"login": "f@example.ru"}),
        "",
    )
    .await;
    assert_eq!(json["data"]["sent"], true);
    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_tokens WHERE kind = 'reset'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(tokens, 1);
}

#[sqlx::test]
async fn reset_changes_password_and_drops_sessions(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    post(
        &app,
        "/api/v1/auth/register",
        register_body("r@example.ru"),
        "",
    )
    .await;
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE login = 'r@example.ru'")
        .fetch_one(&db)
        .await
        .unwrap();
    // Старая сессия и reset-токен из письма.
    sqlx::query(
        "INSERT INTO site_sessions (token_hash, user_id, expires_at)
         VALUES ($1, $2, now() + interval '1 day')",
    )
    .bind(crate::bxapi::auth::token_hash("old-cookie"))
    .bind(user_id)
    .execute(&db)
    .await
    .unwrap();
    set_token(&db, user_id, "reset", "word-123").await;

    let (status, json) = post(
        &app,
        "/api/v1/auth/reset-password",
        json!({"login": "r@example.ru", "checkword": "word-123", "password": "newpass1", "passwordConfirm": "newpass1"}),
        "CMS_SID=old-cookie",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"]["authorized"], true);
    assert_eq!(json["data"]["changed"], true);

    let old: i64 = sqlx::query_scalar("SELECT count(*) FROM site_sessions WHERE token_hash = $1")
        .bind(crate::bxapi::auth::token_hash("old-cookie"))
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(old, 0, "прежние сессии погашены");

    let (status, json) = post(
        &app,
        "/api/v1/auth/login",
        json!({"login": "r@example.ru", "password": "newpass1"}),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["status"], "success", "вход новым паролем: {json}");
}
