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
        PaymentWebhook, Project,
    },
    payment,
};

struct Stub {
    starts: AtomicUsize,
    paid: Arc<AtomicBool>,
    amount: Option<f64>,
    fail_status: bool,
    webhook_payment: i64,
    webhook_ok: bool,
    seen: Mutex<Vec<Value>>,
}

impl Stub {
    fn new(paid: bool) -> Arc<Self> {
        Arc::new(Self {
            starts: AtomicUsize::new(0),
            paid: Arc::new(AtomicBool::new(paid)),
            amount: None,
            fail_status: false,
            webhook_payment: 0,
            webhook_ok: true,
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
        let amount = self.amount;
        let fail = self.fail_status;
        Box::pin(async move {
            if fail {
                return Err(crate::bxapi::BxError::new("provider_down", "down"));
            }
            Ok(PaymentState {
                paid,
                amount,
                notice: None,
            })
        })
    }

    fn webhook<'a>(
        &'a self,
        _state: &'a crate::state::AppState,
        _payload: &'a Value,
        _headers: &'a axum::http::HeaderMap,
    ) -> BoxFuture<'a, Result<PaymentWebhook, crate::bxapi::BxError>> {
        let ok = self.webhook_ok;
        let payment_id = self.webhook_payment;
        Box::pin(async move {
            if !ok {
                return Err(crate::bxapi::BxError::with_status(
                    axum::http::StatusCode::FORBIDDEN,
                    "bad_signature",
                    "bad signature",
                ));
            }
            Ok(PaymentWebhook { payment_id })
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

async fn payment_of(db: &PgPool, order_id: i64) -> (i64, f64) {
    sqlx::query_as("SELECT id, sum::float8 FROM payments WHERE order_id = $1")
        .bind(order_id)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Заказ, платёж и приложение с настроенным провайдером.
async fn setup(db: &PgPool, paid: bool, amount: Option<f64>) -> (Router, i64, i64) {
    pay_system(db).await;
    let order_id = order(db).await;
    let (payment_id, _sum) = payment_of(db, order_id).await;
    let mut stub = Stub::new(paid);
    {
        let inner = Arc::get_mut(&mut stub).unwrap();
        inner.amount = amount;
        inner.webhook_payment = payment_id;
    }
    let mut state = crate::test_support::test_state(db.clone());
    state.project = Arc::new(project_with(stub));
    (crate::app(state), order_id, payment_id)
}

async fn webhook(app: &Router, code: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::post(format!("/payment/webhook/{code}"))
        .header(header::CONTENT_TYPE, "application/json")
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

#[sqlx::test]
async fn webhook_confirms_payment_once(db: PgPool) {
    let (app, order_id, _) = setup(&db, true, Some(100.0)).await;

    let (status, json) = webhook(&app, "qr", json!({"event": "paid"})).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["success"], true);

    let paid: bool = sqlx::query_scalar("SELECT paid FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(paid);

    // Повтор уведомления идемпотентен: успех и без второй записи истории.
    let (again, body) = webhook(&app, "qr", json!({"event": "paid"})).await;
    assert_eq!(again, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["success"], true);
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
async fn webhook_body_is_not_proof_of_payment(db: PgPool) {
    // Провайдер говорит «не оплачено», хотя в теле пришло paid:true.
    let (app, order_id, _) = setup(&db, false, None).await;
    let (status, json) = webhook(&app, "qr", json!({"paid": true})).await;
    assert_eq!(status, StatusCode::OK, "{json}");

    let paid: bool = sqlx::query_scalar("SELECT paid FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(!paid, "тело уведомления не подтверждает оплату");
}

#[sqlx::test]
async fn webhook_amount_mismatch_is_rejected(db: PgPool) {
    let (app, order_id, _) = setup(&db, true, Some(1.0)).await;
    webhook(&app, "qr", json!({"paid": true})).await;

    let paid: bool = sqlx::query_scalar("SELECT paid FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(!paid, "чужая сумма не закрывает заказ");
}

#[sqlx::test]
async fn webhook_rejects_unknown_and_bad_signature(db: PgPool) {
    let (app, _, _) = setup(&db, true, Some(100.0)).await;

    let (status, json) = webhook(&app, "nope", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    assert_eq!(json["errors"][0]["code"], "payment_webhook_unknown");

    // Подпись проверяет провайдер проекта; ошибка не раскрывает деталей.
    let mut stub = Stub::new(true);
    Arc::get_mut(&mut stub).unwrap().webhook_ok = false;
    let mut state = crate::test_support::test_state(db.clone());
    state.project = Arc::new(project_with(stub));
    let strict = crate::app(state);
    let (status, json) = webhook(&strict, "qr", json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["errors"][0]["code"], "payment_webhook_rejected");
}

#[sqlx::test]
async fn reconcile_confirms_forgotten_payment(db: PgPool) {
    pay_system(&db).await;
    let order_id = order(&db).await;
    let (payment_id, _) = payment_of(&db, order_id).await;
    let mut stub = Stub::new(true);
    {
        let inner = Arc::get_mut(&mut stub).unwrap();
        inner.amount = Some(100.0);
        inner.webhook_payment = payment_id;
    }
    let mut state = crate::test_support::test_state(db.clone());
    let token = token(&state, order_id).await;
    state.project = Arc::new(project_with(stub));
    // Платёж запущен (как в жизни), но статус не опрашивали — «закрыли вкладку».
    let app = crate::app(state.clone());
    post(
        &app,
        "/api/v1/order/pay",
        json!({"id": order_id}),
        &cookie(&token),
    )
    .await;

    let confirmed = payment::reconcile(&state).await.unwrap();
    assert_eq!(confirmed, 1);
    let paid: bool = sqlx::query_scalar("SELECT paid FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(paid);
}

#[sqlx::test]
async fn webhook_provider_failure_is_retryable(db: PgPool) {
    pay_system(&db).await;
    let order_id = order(&db).await;
    let (payment_id, _) = payment_of(&db, order_id).await;
    let mut stub = Stub::new(true);
    {
        let inner = Arc::get_mut(&mut stub).unwrap();
        inner.fail_status = true;
        inner.webhook_payment = payment_id;
    }
    let mut state = crate::test_support::test_state(db.clone());
    state.project = Arc::new(project_with(stub));
    let app = crate::app(state);

    let (status, json) = webhook(&app, "qr", json!({})).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "провайдер должен повторить уведомление: {json}"
    );
    assert_eq!(json["errors"][0]["code"], "payment_provider_failed");
}

#[sqlx::test]
async fn webhook_from_foreign_pay_system_is_ignored(db: PgPool) {
    pay_system(&db).await;
    // Вторая система со своим провайдером.
    sqlx::query(
        "INSERT INTO pay_systems (id, code, name, api_type, active)
         VALUES (2, 'other', 'Other', 'redirect', TRUE) ON CONFLICT (id) DO NOTHING",
    )
    .execute(&db)
    .await
    .unwrap();
    let order_id = order(&db).await;
    let (payment_id, _) = payment_of(&db, order_id).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT pay_system_id FROM payments WHERE id = $1")
            .bind(payment_id)
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );

    let mut own = Stub::new(true);
    {
        let inner = Arc::get_mut(&mut own).unwrap();
        inner.amount = Some(100.0);
        inner.webhook_payment = payment_id;
    }
    let mut other = Stub::new(true);
    {
        let inner = Arc::get_mut(&mut other).unwrap();
        inner.amount = Some(100.0);
        inner.webhook_payment = payment_id; // тот же платёж, но чужой провайдер
    }
    let mut project = project_with(own);
    project.payment_extensions.push(PaymentExtension {
        pay_system_id: 2,
        provider: other,
    });
    let mut state = crate::test_support::test_state(db.clone());
    state.project = Arc::new(project);
    let app = crate::app(state);

    let (status, json) = webhook(&app, "other", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let paid: bool = sqlx::query_scalar("SELECT paid FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(!paid, "чужая платёжная система не подтверждает платёж");
}

#[sqlx::test]
async fn channel_does_not_trust_mismatched_amount(db: PgPool) {
    pay_system(&db).await;
    let order_id = order(&db).await;
    let (payment_id, _) = payment_of(&db, order_id).await;
    let mut stub = Stub::new(true);
    {
        let inner = Arc::get_mut(&mut stub).unwrap();
        inner.amount = Some(1.0);
        inner.webhook_payment = payment_id;
    }
    let mut state = crate::test_support::test_state(db.clone());
    let token = token(&state, order_id).await;
    state.project = Arc::new(project_with(stub));
    let app = crate::app(state);

    let (_, json) = get(
        &app,
        &format!("/api/v1/order/{order_id}/payment-channel"),
        &cookie(&token),
    )
    .await;
    assert_eq!(json["data"]["state"]["paid"], false);
}

#[sqlx::test]
async fn reconcile_skips_payments_never_started(db: PgPool) {
    pay_system(&db).await;
    let _order = order(&db).await; // платёж создан, но pay не вызывали
    let mut stub = Stub::new(true);
    Arc::get_mut(&mut stub).unwrap().amount = Some(100.0);
    let mut state = crate::test_support::test_state(db.clone());
    state.project = Arc::new(project_with(stub));

    assert_eq!(payment::reconcile(&state).await.unwrap(), 0);
}

#[sqlx::test]
async fn webhook_for_canceled_order_is_logged_and_not_paid(db: PgPool) {
    let (app, order_id, _) = setup(&db, true, Some(100.0)).await;
    sqlx::query("UPDATE orders SET canceled = TRUE WHERE id = $1")
        .bind(order_id)
        .execute(&db)
        .await
        .unwrap();

    let (status, json) = webhook(&app, "qr", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let paid: bool = sqlx::query_scalar("SELECT paid FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(!paid);
}
