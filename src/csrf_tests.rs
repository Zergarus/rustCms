//! CSRF: токен выдаётся cookie и проверяется на изменяющих запросах bxapi.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

fn with_csrf(db: PgPool) -> Router {
    let mut state = crate::test_support::test_state(db);
    let mut config = (*state.config).clone();
    config.csrf_enforce = true;
    state.config = Arc::new(config);
    crate::app(state)
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Забирает токен и cookie из `/base/csrf/token`.
async fn token(app: &Router) -> (String, String) {
    let req = Request::get("/api/v1/base/csrf/token")
        .body(Body::empty())
        .unwrap();
    let (status, headers, json) = send(app, req).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let token = json["data"]["token"].as_str().unwrap().to_string();
    let set_cookie = headers
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        set_cookie.contains(&format!("{}={token}", crate::bxapi::CSRF_COOKIE)),
        "cookie должна совпадать с токеном: {set_cookie}"
    );
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Lax"), "{set_cookie}");
    let cookie = format!("{}={token}", crate::bxapi::CSRF_COOKIE);
    (token, cookie)
}

#[sqlx::test]
async fn post_without_token_is_rejected(db: PgPool) {
    let app = with_csrf(db.clone());
    let req = Request::post("/api/v1/auth/logout")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _, json) = send(&app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["errors"][0]["code"], "invalid_csrf");
}

#[sqlx::test]
async fn post_with_token_only_in_cookie_is_rejected(db: PgPool) {
    let app = with_csrf(db.clone());
    let (_, cookie) = token(&app).await;
    let req = Request::post("/api/v1/auth/logout")
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    let (status, _, json) = send(&app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
}

#[sqlx::test]
async fn post_with_matching_token_passes(db: PgPool) {
    let app = with_csrf(db.clone());
    let (token, cookie) = token(&app).await;
    let req = Request::post("/api/v1/auth/logout")
        .header(header::COOKIE, cookie)
        .header(crate::bxapi::CSRF_HEADER, token)
        .body(Body::empty())
        .unwrap();
    let (status, _, json) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "success");
}

#[sqlx::test]
async fn get_requests_do_not_need_token(db: PgPool) {
    let app = with_csrf(db.clone());
    let req = Request::get("/api/v1/auth/session")
        .body(Body::empty())
        .unwrap();
    let (status, _, json) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK, "{json}");
}

/// Login отдаёт токен и cookie, и этого токена хватает следующему POST.
#[sqlx::test]
async fn login_token_is_accepted_afterwards(db: PgPool) {
    crate::test_support::user_cookie(&db, "buyer", false).await;
    let app = with_csrf(db.clone());
    let (token, cookie) = token(&app).await;

    let req = Request::post("/api/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, cookie)
        .header(crate::bxapi::CSRF_HEADER, token)
        .body(Body::from(
            r#"{"login":"buyer","password":"password123","remember":false}"#,
        ))
        .unwrap();
    let (status, headers, json) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let fresh = json["data"]["csrfToken"].as_str().unwrap().to_string();
    let set_cookie: Vec<String> = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_string)
        .collect();
    assert!(
        set_cookie
            .iter()
            .any(|value| value.contains(&format!("{}={fresh}", crate::bxapi::CSRF_COOKIE))),
        "cookie должна обновиться вместе с токеном: {set_cookie:?}"
    );

    let req = Request::post("/api/v1/auth/logout")
        .header(
            header::COOKIE,
            format!("{}={fresh}", crate::bxapi::CSRF_COOKIE),
        )
        .header(crate::bxapi::CSRF_HEADER, fresh)
        .body(Body::empty())
        .unwrap();
    let (status, _, json) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK, "{json}");
}
