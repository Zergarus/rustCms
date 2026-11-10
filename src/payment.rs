//! Платежи заказа: доступ, провайдер проекта, отметка оплаты.
//!
//! Контракт — `POST /order/pay` и `GET /order/{id}/payment-channel` модуля
//! `mediagroup.bxapi`. Провайдер (ЮKassa, СБП, счёт) подключает проект через
//! `PaymentExtension`; ядро хранит платёж и состояние оплаты.

use axum::http::StatusCode;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool, types::Json};

use crate::{
    bxapi::{
        BxError, auth,
        project::{PaymentProvider, PaymentRef, PaymentStarted},
    },
    state::AppState,
};

/// Cookie с заказами, оформленными в этой сессии: гость иначе не докажет доступ.
pub const CHECKOUT_COOKIE: &str = "CMS_CHECKOUT";
const MAX_ORDERS: usize = 10;
const COOKIE_DAYS: i64 = 30;

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

/// Заказы, оформленные в этой сессии.
pub fn checkout_ids(jar: &CookieJar) -> Vec<i64> {
    jar.get(CHECKOUT_COOKIE)
        .map(|cookie| {
            cookie
                .value()
                .split(',')
                .filter_map(|id| id.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Cookie для `order/submit`: последние оформленные заказы этой сессии.
pub fn checkout_cookie(state: &AppState, jar: &CookieJar, order_id: i64) -> Cookie<'static> {
    let mut ids = vec![order_id];
    for id in checkout_ids(jar)
        .into_iter()
        .filter(|id| *id != order_id)
        .take(MAX_ORDERS - 1)
    {
        ids.push(id);
    }
    Cookie::build((
        CHECKOUT_COOKIE,
        ids.iter().map(i64::to_string).collect::<Vec<_>>().join(","),
    ))
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
    if checkout_ids(jar).contains(&order_id) {
        return Ok(order);
    }
    let user = auth::current_user_id(state, jar).await?;
    if user.is_some_and(|id| order.user_id == Some(id)) {
        Ok(order)
    } else {
        Err(not_found())
    }
}

/// Неоплаченный платёж заказа (или последний, если все оплачены).
async fn payment_of(db: &PgPool, order_id: i64) -> sqlx::Result<Option<Payment>> {
    sqlx::query_as(
        "SELECT id, pay_system_id, sum::float8 AS sum, currency, paid, data
         FROM payments WHERE order_id = $1
         ORDER BY paid, id
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

async fn reference(state: &AppState, order: &Order, payment: &Payment) -> sqlx::Result<PaymentRef> {
    let (code, api_type) = match payment.pay_system_id {
        Some(id) => pay_system(&state.db, id).await?,
        None => (String::new(), String::new()),
    };
    Ok(PaymentRef {
        order_id: order.id,
        account_number: order.account_number.clone(),
        payment_id: payment.id,
        pay_system_id: payment.pay_system_id,
        pay_system_code: code,
        pay_system_type: api_type,
        sum: payment.sum,
        currency: payment.currency.clone(),
        data: payment.data.0.clone(),
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
    let payment = payment_of(&state.db, order_id)
        .await?
        .ok_or_else(|| BxError::new("payment_not_found", "Платёж заказа не найден"))?;
    let provider = provider(state, payment.pay_system_id).ok_or_else(|| {
        BxError::new(
            "payment_provider_missing",
            "Платёжный провайдер не подключён",
        )
    })?;

    // Идемпотентность: повторный запрос отдаёт уже полученные данные.
    if let Some(saved) = payment
        .data
        .0
        .get("started")
        .filter(|value| !value.is_null())
    {
        return Ok(json!({ "success": true, "payment": saved.clone() }));
    }

    let reference = reference(state, &order, &payment).await?;
    let PaymentStarted { url, data } = provider.start(state, &reference).await?;
    let mut payment_data = data;
    if let Some(url) = url {
        match payment_data.as_object_mut() {
            Some(object) => {
                object.insert("paymentUrl".to_string(), json!(url));
            }
            None => payment_data = json!({ "paymentUrl": url }),
        }
    }
    sqlx::query("UPDATE payments SET data = jsonb_set(data, '{started}', $2, true) WHERE id = $1")
        .bind(payment.id)
        .bind(Json(payment_data.clone()))
        .execute(&state.db)
        .await?;

    Ok(json!({ "success": true, "payment": payment_data }))
}

/// `GET /order/{id}/payment-channel`: состояние оплаты для опроса.
pub async fn channel(state: &AppState, jar: &CookieJar, order_id: i64) -> Result<Value, BxError> {
    let order = accessible(state, jar, order_id).await?;
    let payment = payment_of(&state.db, order_id).await?;
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
            let status = provider.status(state, &reference).await?;
            notice = status.notice.unwrap_or(Value::Null);
            if status.paid {
                mark_paid(state, &order, payment).await?;
                paid = true;
            }
        }
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

/// Оплата зафиксирована: платёж, заказ и запись в истории статусов.
async fn mark_paid(state: &AppState, order: &Order, payment: &Payment) -> Result<(), BxError> {
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE payments SET paid = TRUE, paid_at = now() WHERE id = $1")
        .bind(payment.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE orders SET paid = TRUE, paid_at = now(), updated_at = now() WHERE id = $1")
        .bind(order.id)
        .execute(&mut *tx)
        .await?;
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
    tx.commit().await?;
    Ok(())
}
