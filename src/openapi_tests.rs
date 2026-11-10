//! OpenAPI публичного API.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sqlx::PgPool;
use tower::ServiceExt;

#[sqlx::test]
async fn openapi_document_is_served(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let req = Request::get("/openapi.json").body(Body::empty()).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(doc["openapi"], "3.0.3");
    assert!(doc["info"]["version"].is_string());
    for path in [
        "/api/v1/iblock/{apiCode}/element/list",
        "/api/v1/iblock/{apiCode}/search",
        "/api/v1/cart/items",
        "/api/v1/order/pay",
        "/api/v1/form/{code}",
    ] {
        assert!(doc["paths"].get(path).is_some(), "нет пути {path}");
    }
    assert_eq!(
        doc["components"]["schemas"]["Envelope"]["properties"]["status"]["enum"],
        serde_json::json!(["success", "error"])
    );
    assert!(doc["paths"]["/api/v1/cart/items"]["post"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "X-Bitrix-Csrf-Token"));
}
