use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::{
    bxapi::project::{
        BoxFuture, PaymentExtension, PaymentProvider, PaymentRef, PaymentStarted, PaymentState,
        Project,
    },
    payment,
};

struct Stub {
    starts: AtomicUsize,
    paid: Arc<AtomicBool>,
}

impl PaymentProvider for Stub {
    fn start<'a>(
        &'a self,
        _state: &'a crate::state::AppState,
        payment: &'a PaymentRef,
    ) -> BoxFuture<'a, Result<PaymentStarted, crate::bxapi::BxError>> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(PaymentStarted {
                url: Some("https://pay.example/1".into()),
                data: json!({ "qrPayload": format!("pay:{}", payment.payment_id) }),
            })
        })
    }

    fn status<'a>(
        &'a self,
        _state: &'a crate::state::AppState,
        _payment: &'a PaymentRef,
    ) -> BoxFuture<'a, Result<PaymentState, crate::bxapi::BxError>> {
        let paid = self.paid.load(Ordering::SeqCst);
        Box::pin(async move { Ok(PaymentState { paid, notice: None }) })
    }
}

fn project_with(stub: Arc<Stub>) -> Project {
    let mut project = Project::default();
    project.payment_extensions.push(PaymentExtension {
        pay_system_id: 1,
        provider: stub,
    });
    project
}

async fn post(app: &Router, path: &str, body: Value, cookie: &str) -> (StatusCode, Value) {
    let req = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, cookie)
        .body(Body::from(body.to_string()))
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

async fn get(app: &Router, path: &str, cookie: &str) -> (StatusCode, Value) {
    let req = Request::get(path)
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

async fn pay_system(db: &PgPool) {
    sqlx::query(
        "INSERT INTO pay_systems (id, code, name, api_type, active)
         VALUES (1, 'qr', 'QR', 'qr', TRUE)
         ON CONFLICT (id) DO UPDATE SET api_type = 'qr'",
    )
    .execute(db)
    .await
    .unwrap();
}

async fn order(db: &PgPool) -> i64 {
    let fixture = crate::test_support::order_fixture(db).await;
    crate::test_support::place_order(db, &fixture, None).await
}

#[sqlx::test]
async fn pay_starts_once_and_reuses(db: PgPool) {
    let order_id = order(&db).await;
    let stub = Arc::new(Stub {
        starts: AtomicUsize::new(0),
        paid: Arc::new(AtomicBool::new(false)),
    });
    let mut state = crate::test_support::test_state(db.clone());
    state.project = Arc::new(project_with(stub.clone()));
    let app = crate::app(state);
    let cookie = format!("{}={order_id}", payment::CHECKOUT_COOKIE);

    let (status, json) = post(&app, "/api/v1/order/pay", json!({"id": order_id}), &cookie).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "success");
    assert_eq!(json["data"]["success"], true);
    assert_eq!(
        json["data"]["payment"]["paymentUrl"],
        "https://pay.example/1"
    );
    let payment_id: i64 = sqlx::query_scalar("SELECT id FROM payments WHERE order_id = $1")
        .bind(order_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(
        json["data"]["payment"]["qrPayload"],
        format!("pay:{payment_id}")
    );

    // Повторный запрос не создаёт второй платёж и не зовёт провайдера снова.
    let (_, again) = post(&app, "/api/v1/order/pay", json!({"id": order_id}), &cookie).await;
    assert_eq!(again["data"]["payment"], json["data"]["payment"]);
    assert_eq!(stub.starts.load(Ordering::SeqCst), 1);
    let payments: i64 = sqlx::query_scalar("SELECT count(*) FROM payments WHERE order_id = $1")
        .bind(order_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(payments, 1);
}

#[sqlx::test]
async fn channel_marks_order_paid(db: PgPool) {
    pay_system(&db).await;
    let order_id = order(&db).await;
    let stub = Arc::new(Stub {
        starts: AtomicUsize::new(0),
        paid: Arc::new(AtomicBool::new(true)),
    });
    let mut state = crate::test_support::test_state(db.clone());
    state.project = Arc::new(project_with(stub));
    let app = crate::app(state);
    let cookie = format!("{}={order_id}", payment::CHECKOUT_COOKIE);

    let (status, json) = get(
        &app,
        &format!("/api/v1/order/{order_id}/payment-channel"),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["pull"], Value::Null);
    assert_eq!(json["data"]["state"]["paid"], true);
    assert_eq!(json["data"]["state"]["paymentType"], "qr");

    let paid: bool = sqlx::query_scalar("SELECT paid FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(paid);
    let history: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM order_status_history
         WHERE order_id = $1 AND comment LIKE 'Оплата получена%'",
    )
    .bind(order_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(history, 1);
}

#[sqlx::test]
async fn stranger_and_canceled_are_rejected(db: PgPool) {
    let order_id = order(&db).await;
    let stub = Arc::new(Stub {
        starts: AtomicUsize::new(0),
        paid: Arc::new(AtomicBool::new(false)),
    });
    let mut state = crate::test_support::test_state(db.clone());
    state.project = Arc::new(project_with(stub));
    let app = crate::app(state);

    let (_, json) = post(&app, "/api/v1/order/pay", json!({"id": order_id}), "").await;
    assert_eq!(json["errors"][0]["code"], "order_not_found");

    sqlx::query("UPDATE orders SET canceled = TRUE WHERE id = $1")
        .bind(order_id)
        .execute(&db)
        .await
        .unwrap();
    let cookie = format!("{}={order_id}", payment::CHECKOUT_COOKIE);
    let (status, json) = post(&app, "/api/v1/order/pay", json!({"id": order_id}), &cookie).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["errors"][0]["code"], "payment_forbidden");
}

#[sqlx::test]
async fn without_provider_payment_is_unavailable(db: PgPool) {
    let order_id = order(&db).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = format!("{}={order_id}", payment::CHECKOUT_COOKIE);

    let (_, json) = post(&app, "/api/v1/order/pay", json!({"id": order_id}), &cookie).await;
    assert_eq!(json["errors"][0]["code"], "payment_provider_missing");
}

#[sqlx::test]
async fn checkout_cookie_keeps_recent_orders(db: PgPool) {
    let state = crate::test_support::test_state(db);
    let mut jar = axum_extra::extract::cookie::CookieJar::new();
    jar = jar.add(axum_extra::extract::cookie::Cookie::new(
        payment::CHECKOUT_COOKIE,
        "5,7",
    ));
    assert_eq!(payment::checkout_ids(&jar), vec![5, 7]);

    // Новый заказ идёт первым, прежние сохраняются, дубли убираются.
    let cookie = payment::checkout_cookie(&state, &jar, 9);
    assert_eq!(cookie.value(), "9,5,7");
    let cookie = payment::checkout_cookie(&state, &jar, 5);
    assert_eq!(cookie.value(), "5,7");
}
