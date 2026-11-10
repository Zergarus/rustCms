//! CORS с credentials: явные origin'ы не должны ронять приложение.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sqlx::PgPool;
use tower::ServiceExt;

#[sqlx::test]
async fn explicit_origins_are_allowed(db: PgPool) {
    let mut state = crate::test_support::test_state(db.clone());
    let mut config = (*state.config).clone();
    config.cors_origins = vec!["https://shop.example.ru".into()];
    state.config = Arc::new(config);
    let app = crate::app(state);

    let req = Request::get("/api/health")
        .header(header::ORIGIN, "https://shop.example.ru")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some("https://shop.example.ru")
    );
    assert_eq!(
        res.headers()
            .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );
}

#[sqlx::test]
async fn preflight_allows_csrf_header(db: PgPool) {
    let mut state = crate::test_support::test_state(db.clone());
    let mut config = (*state.config).clone();
    config.cors_origins = vec!["https://shop.example.ru".into()];
    state.config = Arc::new(config);
    let app = crate::app(state);

    let req = Request::builder()
        .method("OPTIONS")
        .uri("/api/v1/cart/items")
        .header(header::ORIGIN, "https://shop.example.ru")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .header(
            header::ACCESS_CONTROL_REQUEST_HEADERS,
            "content-type,x-bitrix-csrf-token",
        )
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert!(res.status().is_success(), "{}", res.status());
}
