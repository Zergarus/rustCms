//! Общий rate-limit публичного API.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::PgPool;
use tower::ServiceExt;

fn with_limit(db: PgPool, limit: u32) -> Router {
    let mut state = crate::test_support::test_state(db);
    let mut config = (*state.config).clone();
    config.rate_limit_per_min = limit;
    state.config = Arc::new(config);
    crate::app(state)
}

async fn get(app: &Router, path: &str, ip: &str) -> StatusCode {
    let req = Request::get(path)
        .header("x-forwarded-for", ip)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

#[sqlx::test]
async fn over_limit_gets_429(db: PgPool) {
    let app = with_limit(db.clone(), 1);
    assert_eq!(
        get(&app, "/api/v1/auth/session", "203.0.113.11").await,
        StatusCode::OK
    );
    assert_eq!(
        get(&app, "/api/v1/auth/session", "203.0.113.11").await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[sqlx::test]
async fn different_ips_have_separate_budgets(db: PgPool) {
    let app = with_limit(db.clone(), 1);
    assert_eq!(
        get(&app, "/api/v1/auth/session", "203.0.113.21").await,
        StatusCode::OK
    );
    // Другой адрес — свой бюджет.
    assert_eq!(
        get(&app, "/api/v1/auth/session", "198.51.100.2").await,
        StatusCode::OK
    );
}

#[sqlx::test]
async fn zero_limit_disables(db: PgPool) {
    let app = with_limit(db.clone(), 0);
    for _ in 0..3 {
        assert_eq!(
            get(&app, "/api/v1/auth/session", "192.0.2.3").await,
            StatusCode::OK
        );
    }
}
