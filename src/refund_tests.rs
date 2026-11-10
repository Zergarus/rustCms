//! Возвраты по заказу.

use axum::http::StatusCode;
use sqlx::PgPool;

async fn paid_order(db: &PgPool) -> i64 {
    let fixture = crate::test_support::order_fixture(db).await;
    let order_id = crate::test_support::place_order(db, &fixture, None).await;
    sqlx::query("UPDATE orders SET paid = TRUE WHERE id = $1")
        .bind(order_id)
        .execute(db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO payments (order_id, pay_system_id, sum, currency, paid)
         VALUES ($1, 1, 100, 'RUB', TRUE)",
    )
    .bind(order_id)
    .execute(db)
    .await
    .ok();
    order_id
}

async fn refund(
    app: &axum::Router,
    cookie: &str,
    order_id: i64,
    amount: &str,
) -> (StatusCode, String) {
    let body = format!("amount={amount}&reason=тест");
    let (status, _, text) = crate::test_support::post_form_raw(
        app,
        &format!("/admin/shop/orders/{order_id}/refund"),
        cookie,
        &body,
    )
    .await;
    (status, text)
}

#[sqlx::test]
async fn refund_reduces_remaining(db: PgPool) {
    let order_id = paid_order(&db).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;

    let (status, text) = refund(&app, &cookie, order_id, "30").await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{text}");
    let (amount, refunded): (f64, f64) = sqlx::query_as(
        "SELECT (SELECT sum(amount)::float8 FROM order_refunds WHERE order_id = $1),
                refunded_amount::float8 FROM orders WHERE id = $1",
    )
    .bind(order_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(amount, 30.0);
    assert_eq!(refunded, 30.0);

    // Частичный возврат повторяется до остатка.
    let (status, _) = refund(&app, &cookie, order_id, "70").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    // Больше остатка — отказ.
    let (status, text) = refund(&app, &cookie, order_id, "1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM order_refunds WHERE order_id = $1")
        .bind(order_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[sqlx::test]
async fn unpaid_order_cannot_be_refunded(db: PgPool) {
    let fixture = crate::test_support::order_fixture(db).await;
    let order_id = crate::test_support::place_order(db, &fixture, None).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;
    let (status, text) = refund(&app, &cookie, order_id, "10").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
}

#[sqlx::test]
async fn refund_requires_orders_permission(db: PgPool) {
    let order_id = paid_order(&db).await;
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::permitted_cookie(&db, "manager", &[]).await;
    let (status, _) = refund(&app, &cookie, order_id, "10").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
