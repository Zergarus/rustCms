use std::sync::{
    Arc, Mutex,
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
    fail_status: bool,
    seen: Mutex<Vec<Value>>,
}

impl Stub {
    fn new(paid: bool) -> Arc<Self> {
        Arc::new(Self {
            starts: AtomicUsize::new(0),
            paid: Arc::new(AtomicBool::new(paid)),
            fail_status: false,
            seen: Mutex::new(Vec::new()),
        })
    }
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
                external_id: Some("ext-1".into()),
            })
        })
    }

    fn status<'a>(
        &'a self,
        _state: &'a crate::state::AppState,
        payment: &'a PaymentRef,
    ) -> BoxFuture<'a, Result<PaymentState, crate::bxapi::BxError>> {
        self.seen.lock().unwrap().push(payment.data.clone());
        let paid = self.paid.load(Ordering::SeqCst);
        let fail = self.fail_status;
        Box::pin(async move {
            if fail {
                return Err(crate::bxapi::BxError::new("provider_down", "down"));
            }
            Ok(PaymentState { paid, notice: None })
        })
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

async fn token(state: &crate::state::AppState, order_id: i64) -> String {
    payment::new_checkout_token(state, order_id).await.unwrap()
}

fn cookie(token: &str) -> String {
    format!("{}={token}", payment::CHECKOUT_COOKIE)
}

#[sqlx::test]
async fn pay_starts_once_and_reuses(db: PgPool) {
    pay_system(&db).await;
    let order_id = order(&db).await;
    let stub = Stub::new(false);
    let mut state = crate::test_support::test_state(db.clone());
    let token = token(&state, order_id).await;
    state.project = Arc::new(project_with(stub.clone()));
    let app = crate::app(state);
    let cookie = cookie(&token);

    let (status, json) = post(&app, "/api/v1/order/pay", json!({"id": order_id}), &cookie).await;
    assert_eq!(status, StatusCode::OK, "{json}");
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
    let external: String = sqlx::query_scalar("SELECT external_id FROM payments WHERE id = $1")
        .bind(payment_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(external, "ext-1");

    let (_, again) = post(&app, "/api/v1/order/pay", json!({"id": order_id}), &cookie).await;
    assert_eq!(again["data"]["payment"], json["data"]["payment"]);
    assert_eq!(stub.starts.load(Ordering::SeqCst), 1);
}

#[sqlx::test]
async fn concurrent_pay_calls_provider_once(db: PgPool) {
    pay_system(&db).await;
    let order_id = order(&db).await;
    let stub = Stub::new(false);
    let mut state = crate::test_support::test_state(db.clone());
    let token = token(&state, order_id).await;
    state.project = Arc::new(project_with(stub.clone()));
    let app = crate::app(state);
    let cookie = cookie(&token);

    let (a, b) = tokio::join!(
        post(&app, "/api/v1/order/pay", json!({"id": order_id}), &cookie),
        post(&app, "/api/v1/order/pay", json!({"id": order_id}), &cookie),
    );
    assert_eq!(a.0, StatusCode::OK, "{}", a.1);
    assert_eq!(b.0, StatusCode::OK, "{}", b.1);
    assert_eq!(
        stub.starts.load(Ordering::SeqCst),
        1,
        "провайдер вызван один раз"
    );
    let payments: i64 = sqlx::query_scalar("SELECT count(*) FROM payments WHERE order_id = $1")
        .bind(order_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(payments, 1);
}

#[sqlx::test]
async fn channel_marks_order_paid_with_notice(db: PgPool) {
    pay_system(&db).await;
    let order_id = order(&db).await;
    let stub = Stub::new(true);
    let mut state = crate::test_support::test_state(db.clone());
    let token = token(&state, order_id).await;
    state.project = Arc::new(project_with(stub.clone()));
    let app = crate::app(state);
    let cookie = cookie(&token);
    // Провайдеру нужен запущенный платёж: сначала pay.
    post(&app, "/api/v1/order/pay", json!({"id": order_id}), &cookie).await;

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
    assert!(
        json["data"]["state"]["notice"]["title"].is_string(),
        "фронт останавливает опрос только при paid && notice: {json}"
    );

    // Статус видит данные, которые вернул start.
    let seen = stub.seen.lock().unwrap().clone();
    assert!(
        seen.iter().any(|data| data["qrPayload"].is_string()),
        "{seen:?}"
    );

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

    // Повторный опрос не дублирует историю.
    get(
        &app,
        &format!("/api/v1/order/{order_id}/payment-channel"),
        &cookie,
    )
    .await;
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
async fn provider_failure_does_not_break_polling(db: PgPool) {
    pay_system(&db).await;
    let order_id = order(&db).await;
    let mut stub = Stub::new(false);
    Arc::get_mut(&mut stub).unwrap().fail_status = true;
    let mut state = crate::test_support::test_state(db.clone());
    let token = token(&state, order_id).await;
    state.project = Arc::new(project_with(stub));
    let app = crate::app(state);

    let (status, json) = get(
        &app,
        &format!("/api/v1/order/{order_id}/payment-channel"),
        &cookie(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["state"]["paid"], false);
}

#[sqlx::test]
async fn foreign_and_missing_tokens_cannot_pay(db: PgPool) {
    pay_system(&db).await;
    let fixture = crate::test_support::order_fixture(&db).await;
    let first = crate::test_support::place_order(&db, &fixture, None).await;
    let second = crate::test_support::place_order(&db, &fixture, None).await;
    let stub = Stub::new(false);
    let mut state = crate::test_support::test_state(db.clone());
    let foreign = token(&state, second).await;
    state.project = Arc::new(project_with(stub));
    let app = crate::app(state);

    // Токен от другого заказа доступа не даёт.
    let (_, json) = post(
        &app,
        "/api/v1/order/pay",
        json!({"id": first}),
        &cookie(&foreign),
    )
    .await;
    assert_eq!(json["errors"][0]["code"], "order_not_found");

    // Голый id в cookie больше не работает.
    let (_, json) = post(
        &app,
        "/api/v1/order/pay",
        json!({"id": first}),
        &format!("{}={first}", payment::CHECKOUT_COOKIE),
    )
    .await;
    assert_eq!(json["errors"][0]["code"], "order_not_found");
}

#[sqlx::test]
async fn canceled_order_is_rejected(db: PgPool) {
    pay_system(&db).await;
    let order_id = order(&db).await;
    let stub = Stub::new(false);
    let mut state = crate::test_support::test_state(db.clone());
    let token = token(&state, order_id).await;
    state.project = Arc::new(project_with(stub));
    let app = crate::app(state);
    sqlx::query("UPDATE orders SET canceled = TRUE WHERE id = $1")
        .bind(order_id)
        .execute(&db)
        .await
        .unwrap();

    let (status, json) = post(
        &app,
        "/api/v1/order/pay",
        json!({"id": order_id}),
        &cookie(&token),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["errors"][0]["code"], "payment_forbidden");
}

#[sqlx::test]
async fn without_provider_payment_is_unavailable(db: PgPool) {
    pay_system(&db).await;
    let order_id = order(&db).await;
    let mut state = crate::test_support::test_state(db.clone());
    let token = token(&state, order_id).await;
    state.project = Arc::new(Project::default());
    let app = crate::app(state);

    let (_, json) = post(
        &app,
        "/api/v1/order/pay",
        json!({"id": order_id}),
        &cookie(&token),
    )
    .await;
    assert_eq!(json["errors"][0]["code"], "payment_provider_missing");
}

#[sqlx::test]
async fn checkout_cookie_keeps_recent_tokens(db: PgPool) {
    let state = crate::test_support::test_state(db);
    let mut jar = axum_extra::extract::cookie::CookieJar::new();
    jar = jar.add(axum_extra::extract::cookie::Cookie::new(
        payment::CHECKOUT_COOKIE,
        "aaa,bbb",
    ));
    assert_eq!(payment::checkout_tokens(&jar), ["aaa", "bbb"]);

    let cookie = payment::checkout_cookie(&state, &jar, "ccc");
    assert_eq!(cookie.value(), "ccc,aaa,bbb");
    let cookie = payment::checkout_cookie(&state, &jar, "aaa");
    assert_eq!(cookie.value(), "aaa,bbb");
}
