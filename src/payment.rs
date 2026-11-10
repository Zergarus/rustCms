//! Платежи заказа: доступ, провайдер проекта, отметка оплаты.
//!
//! Контракт — `POST /order/pay` и `GET /order/{id}/payment-channel` модуля
//! `mediagroup.bxapi`. Провайдер (ЮKassa, СБП, счёт) подключает проект через
//! `PaymentExtension`; ядро хранит платёж и состояние оплаты.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use chrono::Utc;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, types::Json};

use crate::{
    bxapi::{
        BxError, BxResult, auth,
        project::{PaymentProvider, PaymentRef, PaymentStarted, PaymentState},
        success,
    },
    state::AppState,
};

/// Cookie с токенами оформленных заказов: гость иначе не докажет доступ.
pub const CHECKOUT_COOKIE: &str = "CMS_CHECKOUT";
const MAX_TOKENS: usize = 10;
const COOKIE_DAYS: i64 = 30;
const TOKEN_DAYS: i64 = 30;

#[derive(FromRow, Clone)]
struct Order {
    id: i64,
    user_id: Option<i64>,
    status: String,
    account_number: String,
    price: f64,
    canceled: bool,
    paid: bool,
}

#[derive(FromRow, Clone)]
struct Payment {
    id: i64,
    pay_system_id: Option<i64>,
    sum: f64,
    currency: String,
    paid: bool,
    data: Json<Value>,
}

fn not_found() -> BxError {
    BxError::new("order_not_found", "Заказ не найден")
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Токены оформленных заказов из cookie.
pub fn checkout_tokens(jar: &CookieJar) -> Vec<String> {
    jar.get(CHECKOUT_COOKIE)
        .map(|cookie| {
            cookie
                .value()
                .split(',')
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Выдаёт непрозрачный токен доступа к заказу (в БД только sha256).
pub async fn new_checkout_token(state: &AppState, order_id: i64) -> sqlx::Result<String> {
    let token = hex::encode(rand::random::<[u8; 32]>());
    sqlx::query(
        "INSERT INTO checkout_tokens (token_hash, order_id, expires_at)
         VALUES ($1, $2, now() + make_interval(days => $3))",
    )
    .bind(token_hash(&token))
    .bind(order_id)
    .bind(TOKEN_DAYS as i32)
    .execute(&state.db)
    .await?;
    Ok(token)
}

/// Cookie для `order/submit`: новый токен первым, прежние сохраняются без дублей.
pub fn checkout_cookie(state: &AppState, jar: &CookieJar, token: &str) -> Cookie<'static> {
    let mut tokens = vec![token.to_string()];
    for old in checkout_tokens(jar) {
        if !tokens.contains(&old) {
            tokens.push(old);
        }
    }
    tokens.truncate(MAX_TOKENS);
    Cookie::build((CHECKOUT_COOKIE, tokens.join(",")))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.config.cookie_secure)
        .max_age(time::Duration::days(COOKIE_DAYS))
        .build()
}

fn provider(state: &AppState, pay_system_id: Option<i64>) -> Option<&dyn PaymentProvider> {
    let id = pay_system_id?;
    state
        .project
        .payment_extensions
        .iter()
        .find(|extension| extension.pay_system_id == id)
        .map(|extension| extension.provider.as_ref())
}

async fn accessible(state: &AppState, jar: &CookieJar, order_id: i64) -> Result<Order, BxError> {
    let order: Option<Order> = sqlx::query_as(
        "SELECT id, user_id, status, account_number, price::float8 AS price, canceled, paid
         FROM orders WHERE id = $1",
    )
    .bind(order_id)
    .fetch_optional(&state.db)
    .await?;
    let order = order.ok_or_else(not_found)?;

    let hashes: Vec<String> = checkout_tokens(jar)
        .iter()
        .map(|token| token_hash(token))
        .collect();
    if !hashes.is_empty() {
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT order_id FROM checkout_tokens
             WHERE token_hash = ANY($1) AND expires_at > now()",
        )
        .bind(&hashes)
        .fetch_all(&state.db)
        .await?;
        if ids.contains(&order_id) {
            return Ok(order);
        }
    }

    let user = auth::current_user_id(state, jar).await?;
    if user.is_some_and(|id| order.user_id == Some(id)) {
        Ok(order)
    } else {
        Err(not_found())
    }
}

/// Неоплаченный платёж заказа.
async fn unpaid_payment(
    conn: &mut sqlx::PgConnection,
    order_id: i64,
) -> sqlx::Result<Option<Payment>> {
    sqlx::query_as(
        "SELECT id, pay_system_id, sum::float8 AS sum, currency, paid, data
         FROM payments WHERE order_id = $1 AND NOT paid
         ORDER BY id
         LIMIT 1
         FOR UPDATE",
    )
    .bind(order_id)
    .fetch_optional(conn)
    .await
}

async fn last_payment(db: &PgPool, order_id: i64) -> sqlx::Result<Option<Payment>> {
    sqlx::query_as(
        "SELECT id, pay_system_id, sum::float8 AS sum, currency, paid, data
         FROM payments WHERE order_id = $1
         ORDER BY id DESC
         LIMIT 1",
    )
    .bind(order_id)
    .fetch_optional(db)
    .await
}

async fn pay_system(db: &PgPool, id: i64) -> sqlx::Result<(String, String)> {
    Ok(
        sqlx::query_as("SELECT code, api_type FROM pay_systems WHERE id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?
            .unwrap_or_default(),
    )
}

/// Данные для провайдера: в `status` он видит то, что вернул `start`.
async fn reference(state: &AppState, order: &Order, payment: &Payment) -> sqlx::Result<PaymentRef> {
    let (code, api_type) = match payment.pay_system_id {
        Some(id) => pay_system(&state.db, id).await?,
        None => (String::new(), String::new()),
    };
    let data = payment
        .data
        .0
        .get("started")
        .cloned()
        .unwrap_or_else(|| payment.data.0.clone());
    Ok(PaymentRef {
        order_id: order.id,
        account_number: order.account_number.clone(),
        payment_id: payment.id,
        pay_system_id: payment.pay_system_id,
        pay_system_code: code,
        pay_system_type: api_type,
        sum: payment.sum,
        currency: payment.currency.clone(),
        data,
    })
}

/// `POST /order/pay`: запускает платёж у провайдера и запоминает результат.
pub async fn start(state: &AppState, jar: &CookieJar, order_id: i64) -> Result<Value, BxError> {
    let order = accessible(state, jar, order_id).await?;
    if order.canceled {
        return Err(BxError::with_status(
            StatusCode::FORBIDDEN,
            "payment_forbidden",
            "Заказ отменён",
        ));
    }
    if order.paid {
        return Ok(json!({ "success": true, "payment": Value::Null }));
    }

    // Строка платежа блокируется до конца транзакции: параллельный запрос
    // дождётся и увидит уже сохранённый результат, второго вызова провайдера нет.
    let mut tx = state.db.begin().await?;
    let payment = unpaid_payment(&mut tx, order_id)
        .await?
        .ok_or_else(|| BxError::new("payment_not_found", "Платёж заказа не найден"))?;
    if let Some(saved) = payment
        .data
        .0
        .get("started")
        .filter(|value| !value.is_null())
    {
        tx.commit().await?;
        return Ok(json!({ "success": true, "payment": saved.clone() }));
    }
    let provider = provider(state, payment.pay_system_id).ok_or_else(|| {
        BxError::new(
            "payment_provider_missing",
            "Платёжный провайдер не подключён",
        )
    })?;

    let reference = reference(state, &order, &payment).await?;
    let started = provider.start(state, &reference).await.map_err(|error| {
        tracing::error!(
            order_id,
            payment_id = payment.id,
            ?error,
            "провайдер не запустил платёж"
        );
        BxError::new(
            "payment_provider_failed",
            "Платёжный провайдер временно недоступен",
        )
    })?;
    let PaymentStarted {
        url,
        data,
        external_id,
    } = started;
    let mut payment_data = data;
    if let Some(url) = url {
        match payment_data.as_object_mut() {
            Some(object) => {
                object.insert("paymentUrl".to_string(), json!(url));
            }
            None => payment_data = json!({ "paymentUrl": url }),
        }
    }
    sqlx::query(
        "UPDATE payments
         SET data = jsonb_set(data, '{started}', $2, true),
             external_id = COALESCE(NULLIF($3, ''), external_id)
         WHERE id = $1",
    )
    .bind(payment.id)
    .bind(Json(payment_data.clone()))
    .bind(external_id.unwrap_or_default())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(json!({ "success": true, "payment": payment_data }))
}

/// `GET /order/{id}/payment-channel`: состояние оплаты для опроса.
pub async fn channel(state: &AppState, jar: &CookieJar, order_id: i64) -> Result<Value, BxError> {
    let order = accessible(state, jar, order_id).await?;
    let payment = last_payment(&state.db, order_id).await?;
    let mut paid = order.paid || payment.as_ref().is_some_and(|p| p.paid);
    let mut notice = Value::Null;
    let mut payment_type = String::new();
    let mut sum = order.price;
    let mut payment_id = None;

    if let Some(payment) = &payment {
        payment_id = Some(payment.id);
        sum = payment.sum;
        let reference = reference(state, &order, payment).await?;
        payment_type = reference.pay_system_type.clone();
        if !paid && let Some(provider) = provider(state, payment.pay_system_id) {
            match provider.status(state, &reference).await {
                Ok(status) => {
                    notice = status.notice.unwrap_or(Value::Null);
                    if status.paid {
                        paid = mark_paid(state, &order, payment).await?;
                    }
                }
                // Сбой провайдера не должен ломать опрос: отдаём последнее
                // известное состояние, следующий опрос попробует снова.
                Err(error) => {
                    tracing::warn!(
                        order_id,
                        payment_id = payment.id,
                        ?error,
                        "провайдер не отдал статус"
                    );
                }
            }
        }
    }
    if paid && notice.is_null() {
        notice = paid_notice();
    }

    Ok(json!({
        "pull": null,
        "moduleId": "cms",
        "command": "paid",
        "state": {
            "orderId": order.id,
            "paymentId": payment_id,
            "paid": paid,
            "sum": sum,
            "paymentType": payment_type,
            "deliveryType": null,
            "notice": notice,
        },
    }))
}

/// Уведомление об успешной оплате — фронт останавливает опрос и показывает модалку
/// только при `paid && notice`.
fn paid_notice() -> Value {
    json!({
        "title": "Оплата получена",
        "text": "Заказ оплачен, мы начали его собирать.",
        "link": { "text": "Мои заказы", "url": "/profile/" },
        "plainText": "Оплата получена. Заказ оплачен, мы начали его собирать.",
        "html": "<p>Оплата получена. Заказ оплачен, мы начали его собирать.</p>",
    })
}

/// Оплата зафиксирована: оплата, пересчёт заказа и резервов, история статусов.
/// Возвращает, оплачен ли заказ целиком.
async fn mark_paid(state: &AppState, order: &Order, payment: &Payment) -> Result<bool, BxError> {
    let mut tx = state.db.begin().await?;
    // Порядок блокировок как в `set_paid_in`: заказ раньше оплаты.
    sqlx::query("SELECT id FROM orders WHERE id = $1 FOR UPDATE")
        .bind(order.id)
        .fetch_optional(&mut *tx)
        .await?;
    let marked =
        sqlx::query("UPDATE payments SET paid = TRUE, paid_at = now() WHERE id = $1 AND NOT paid")
            .bind(payment.id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
    let paid = crate::sale::repo::sync_paid_in(&mut tx, order.id)
        .await?
        .unwrap_or(order.paid);
    if marked && paid {
        sqlx::query(
            "INSERT INTO order_status_history (order_id, status, user_id, comment)
             VALUES ($1, $2, NULL, $3)",
        )
        .bind(order.id)
        .bind(&order.status)
        .bind(format!(
            "Оплата получена {}",
            Utc::now().format("%d.%m.%Y %H:%M")
        ))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(paid)
}

/// `POST /payment/webhook/{code}`: уведомление провайдера.
///
/// Тело — только чтобы понять, о каком платеже речь; оплату подтверждает
/// отдельный запрос к провайдеру, сумма сверяется.
pub async fn webhook_handler(
    State(state): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> BxResult {
    let payload = serde_json::from_slice(&body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).to_string()));
    Ok(success(webhook(&state, &code, &headers, &payload).await?))
}

async fn webhook(
    state: &AppState,
    code: &str,
    headers: &HeaderMap,
    payload: &Value,
) -> Result<Value, BxError> {
    let pay_system_id: Option<i64> =
        sqlx::query_scalar("SELECT id FROM pay_systems WHERE code = $1 AND active")
            .bind(code)
            .fetch_optional(&state.db)
            .await?;
    let Some(pay_system_id) = pay_system_id else {
        return Err(BxError::with_status(
            StatusCode::NOT_FOUND,
            "payment_webhook_unknown",
            "Неизвестный платёжный провайдер",
        ));
    };
    let Some(provider) = provider(state, Some(pay_system_id)) else {
        return Err(BxError::with_status(
            StatusCode::NOT_FOUND,
            "payment_webhook_unknown",
            "Неизвестный платёжный провайдер",
        ));
    };
    let hook = provider
        .webhook(state, payload, headers)
        .await
        .map_err(|error| {
            tracing::warn!(code, ?error, "уведомление провайдера отклонено");
            BxError::with_status(
                StatusCode::FORBIDDEN,
                "payment_webhook_rejected",
                "Уведомление отклонено",
            )
        })?;

    let payment: Option<Payment> = sqlx::query_as(
        "SELECT id, pay_system_id, sum::float8 AS sum, currency, paid, data
         FROM payments WHERE id = $1",
    )
    .bind(hook.payment_id)
    .fetch_optional(&state.db)
    .await?;
    // Неизвестный платёж и повтор уведомления — успех: провайдеру нечего чинить.
    let Some(payment) = payment.filter(|payment| !payment.paid) else {
        return Ok(json!({ "success": true }));
    };
    let order = order_by_id(&state.db, payment_order_id(&state.db, payment.id).await?)
        .await?
        .ok_or_else(not_found)?;
    if order.canceled {
        return Ok(json!({ "success": true }));
    }
    let reference = reference(state, &order, &payment).await?;
    let status = provider.status(state, &reference).await.map_err(|error| {
        tracing::error!(
            payment_id = payment.id,
            ?error,
            "провайдер не подтвердил статус"
        );
        BxError::new(
            "payment_provider_failed",
            "Платёжный провайдер временно недоступен",
        )
    })?;
    apply_status(state, &order, &payment, status).await?;
    Ok(json!({ "success": true }))
}

async fn payment_order_id(db: &PgPool, payment_id: i64) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT order_id FROM payments WHERE id = $1")
        .bind(payment_id)
        .fetch_one(db)
        .await
}

async fn order_by_id(db: &PgPool, order_id: i64) -> sqlx::Result<Option<Order>> {
    sqlx::query_as(
        "SELECT id, user_id, status, account_number, price::float8 AS price, canceled, paid
         FROM orders WHERE id = $1",
    )
    .bind(order_id)
    .fetch_optional(db)
    .await
}

/// Фиксирует оплату, только если провайдер подтвердил её и сумма совпала.
async fn apply_status(
    state: &AppState,
    order: &Order,
    payment: &Payment,
    status: PaymentState,
) -> Result<bool, BxError> {
    if !status.paid {
        return Ok(false);
    }
    if let Some(amount) = status.amount
        && (amount - payment.sum).abs() > 0.01
    {
        tracing::warn!(
            payment_id = payment.id,
            amount,
            sum = payment.sum,
            "провайдер подтвердил другую сумму"
        );
        return Ok(false);
    }
    mark_paid(state, order, payment).await
}

/// Строка сверки: неоплаченный платёж с полями заказа.
#[derive(FromRow)]
struct ReconcileRow {
    payment_id: i64,
    pay_system_id: Option<i64>,
    sum: f64,
    currency: String,
    data: Json<Value>,
    order_id: i64,
    order_status: String,
    account_number: String,
    order_price: f64,
    canceled: bool,
    order_paid: bool,
}

/// Фоновая сверка: подтверждает «забытые» оплаты у провайдера.
pub async fn reconcile(state: &AppState) -> Result<usize, BxError> {
    let rows: Vec<ReconcileRow> = sqlx::query_as(
        "SELECT p.id AS payment_id, p.pay_system_id, p.sum::float8 AS sum, p.currency, p.data,
                o.id AS order_id, o.status AS order_status, o.account_number,
                o.price::float8 AS order_price, o.canceled, o.paid AS order_paid
         FROM payments p
         JOIN orders o ON o.id = p.order_id
         WHERE NOT p.paid AND NOT o.canceled AND o.created_at > now() - interval '30 days'
         ORDER BY p.id
         LIMIT 50",
    )
    .fetch_all(&state.db)
    .await?;

    let mut confirmed = 0;
    for row in rows {
        let Some(provider) = provider(state, row.pay_system_id) else {
            continue;
        };
        let order = Order {
            id: row.order_id,
            user_id: None,
            status: row.order_status,
            account_number: row.account_number,
            price: row.order_price,
            canceled: row.canceled,
            paid: row.order_paid,
        };
        let payment = Payment {
            id: row.payment_id,
            pay_system_id: row.pay_system_id,
            sum: row.sum,
            currency: row.currency,
            paid: false,
            data: row.data,
        };
        let reference = reference(state, &order, &payment).await?;
        match provider.status(state, &reference).await {
            Ok(status) => {
                if apply_status(state, &order, &payment, status).await? {
                    confirmed += 1;
                }
            }
            Err(error) => {
                tracing::warn!(
                    payment_id = payment.id,
                    ?error,
                    "сверка: провайдер не ответил"
                );
            }
        }
    }
    Ok(confirmed)
}

/// Раз в 10 минут сверяет неоплаченные платежи.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(600));
        loop {
            timer.tick().await;
            if let Err(error) = reconcile(&state).await {
                tracing::warn!(?error, "фоновая сверка платежей не удалась");
            }
        }
    });
}
