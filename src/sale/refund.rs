//! Возвраты по заказу: полные и частичные, с журналом и аудитом.

use axum::{
    Extension, Form,
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;

use crate::{
    access::Access,
    error::{AppError, AppResult},
    state::AppState,
};

#[derive(Deserialize)]
pub struct RefundForm {
    #[serde(default)]
    amount: String,
    #[serde(default)]
    reason: String,
}

/// `POST /admin/shop/orders/{id}/refund`.
pub async fn refund(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<RefundForm>,
) -> AppResult<Response> {
    user.require(crate::access::ORDERS_MANAGE)?;
    let amount: f64 = form
        .amount
        .trim()
        .replace(',', ".")
        .parse()
        .ok()
        .filter(|amount: &f64| *amount > 0.0 && amount.is_finite())
        .ok_or_else(|| AppError::BadRequest("Сумма возврата должна быть больше нуля".into()))?;
    let amount = (amount * 100.0).round() / 100.0;
    let reason = form.reason.trim().to_string();

    let mut tx = state.db.begin().await?;
    let row: Option<(bool, f64)> =
        sqlx::query_as("SELECT paid, refunded_amount::float8 FROM orders WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((paid, refunded)) = row else {
        return Err(AppError::NotFound);
    };
    let paid_sum: f64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(sum), 0)::float8 FROM payments WHERE order_id = $1 AND paid",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if !paid || paid_sum <= 0.0 {
        return Err(AppError::BadRequest(
            "Возврат возможен только по оплаченному заказу".into(),
        ));
    }
    let left = (paid_sum - refunded).max(0.0);
    if amount > left + 0.005 {
        return Err(AppError::BadRequest(format!(
            "Больше остатка к возврату ({left:.2})"
        )));
    }
    sqlx::query(
        "INSERT INTO order_refunds (order_id, user_id, user_login, amount, reason)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(user.user.id)
    .bind(&user.user.login)
    .bind(amount)
    .bind(&reason)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE orders SET refunded_amount = refunded_amount + $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(amount)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    crate::audit::record(
        &state.db,
        user.user.id,
        &user.user.login,
        "refund",
        "order",
        id,
        format!("возврат {amount:.2}: {reason}"),
    )
    .await;
    Ok(Redirect::to(&format!("/admin/shop/orders/{id}")).into_response())
}
