//! Расширения проекта: свои маршруты и повтор заказа.

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode, header},
    routing::get,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

#[sqlx::test]
async fn project_routes_are_mounted(db: PgPool) {
    let project = crate::bxapi::project::Project {
        routes: Some(
            Router::new()
                .route("/project/ping", get(|| async { Json(json!({"ok": true})) }))
                .route("/1c/api/ping", get(|| async { Json(json!({"api": true})) })),
        ),
        ..crate::bxapi::project::Project::default()
    };
    let mut state = crate::test_support::test_state(db.clone());
    state.project = Arc::new(project);
    let app = crate::app(state);

    for path in ["/project/ping", "/1c/api/ping"] {
        let (status, _, body) = crate::test_support::get_raw(&app, path, "").await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(String::from_utf8_lossy(&body).contains("true"), "{path}");
    }
}

async fn repeat(app: &Router, order_id: i64, cookie: &str) -> (StatusCode, Value) {
    let req = Request::post(format!("/api/v1/profile/orders/{order_id}/repeat"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
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
async fn repeat_order_returns_items_to_cart(db: PgPool) {
    let fixture = crate::test_support::order_fixture(&db).await;
    let (user_id, cookie) = crate::test_support::site_cookie(&db, "buyer").await;
    let order_id = crate::test_support::place_order(&db, &fixture, Some(user_id)).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));

    let (status, json) = repeat(&app, order_id, &cookie).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["orderId"], order_id);
    assert_eq!(json["data"]["added"], 1, "{json}");
    assert_eq!(json["data"]["skipped"], 0, "{json}");

    let cart: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cart_items WHERE order_id IS NULL
         AND buyer_id = (SELECT id FROM buyers WHERE user_id = $1)",
    )
    .bind(user_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(cart, 1);

    // Чужой заказ недоступен.
    let (_, other) = crate::test_support::site_cookie(&db, "other").await;
    let (status, json) = repeat(&app, order_id, &other).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    assert_eq!(json["errors"][0]["code"], "order_not_found");
}

#[sqlx::test]
async fn repeat_skips_unavailable_product(db: PgPool) {
    let fixture = crate::test_support::order_fixture(&db).await;
    let (user_id, cookie) = crate::test_support::site_cookie(&db, "buyer").await;
    let order_id = crate::test_support::place_order(&db, &fixture, Some(user_id)).await;
    sqlx::query("UPDATE collection_items SET active = FALSE WHERE id = $1")
        .bind(fixture.product_id)
        .execute(&db)
        .await
        .unwrap();
    let app = crate::app(crate::test_support::test_state(db.clone()));

    let (status, json) = repeat(&app, order_id, &cookie).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["added"], 0, "{json}");
    assert_eq!(json["data"]["skipped"], 1, "{json}");
}
