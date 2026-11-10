//! Метрики Prometheus.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::PgPool;
use tower::ServiceExt;

async fn get(app: &axum::Router, path: &str) -> (StatusCode, String) {
    let req = Request::get(path).body(Body::empty()).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[sqlx::test]
async fn metrics_report_requests_and_db(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let (status, _) = get(&app, "/api/v1/auth/session").await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get(&app, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("cms_requests_total{method=\"GET\",status=\"200\"}"),
        "{body}"
    );
    assert!(body.contains("cms_db_up 1"), "{body}");
    assert!(body.contains("cms_build_info{version="), "{body}");
}
