//! Корзины покупателей (как «Корзины» модуля sale): только просмотр.

use std::collections::HashMap;

use axum::{
    Extension,
    extract::{Path, Query, State},
    response::Html,
};
use chrono::{DateTime, Utc};
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use super::require_shop;
use crate::{
    access::Access,
    admin::render,
    cart::{CartItem, SnapshotConfig, StoreInfo, repo, snapshot::build_snapshot},
    catalog,
    discount::{context, engine, model::Rule, repo as discount_repo},
    error::{AppError, AppResult},
    state::AppState,
};

const PER_PAGE: i64 = 50;

/// «логин (имя)», «логин» или «Гость».
pub fn buyer_label(login: Option<&str>, name: Option<&str>) -> String {
    match (login, name.map(str::trim).filter(|n| !n.is_empty())) {
        (Some(l), Some(n)) => format!("{l} ({n})"),
        (Some(l), None) => l.to_string(),
        (None, _) => "Гость".into(),
    }
}

#[derive(FromRow)]
struct BuyerRow {
    id: i64,
    user_id: Option<i64>,
    login: Option<String>,
    name: Option<String>,
    updated_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct CartRow {
    id: i64,
    buyer: String,
    items: usize,
    total: serde_json::Value,
    updated_at: String,
}

fn snapshot_config() -> SnapshotConfig {
    SnapshotConfig {
        items_count_positions: false,
        required_store: false,
        currency: "RUB".into(),
    }
}

async fn stores(state: &AppState) -> sqlx::Result<Vec<StoreInfo>> {
    let rows: Vec<(i64, String, bool)> =
        sqlx::query_as("SELECT id, name, active FROM catalog_stores ORDER BY sort, id")
            .fetch_all(&state.db)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, active)| StoreInfo {
            id,
            name,
            active,
            fields: Default::default(),
            image: None,
        })
        .collect())
}

/// Цены единицы позиций со скидками — тем же расчётом, что корзина в API (без данных
/// заказа), но без записи цен.
async fn discounted(
    state: &AppState,
    rules: &[Rule],
    buyer: &BuyerRow,
    items: &[CartItem],
    info: &HashMap<i64, catalog::PurchaseInfo>,
) -> AppResult<HashMap<i64, f64>> {
    if items.is_empty() {
        return Ok(HashMap::new());
    }
    let coupons = discount_repo::buyer_coupons(&state.db, buyer.id).await?;
    let mut conn = state.db.acquire().await?;
    let ctx = context::for_cart(
        &mut conn,
        items,
        info,
        buyer.user_id,
        coupons,
        context::OrderCtx::default(),
    )
    .await?;
    let hook = state.project.discount_condition_hook.as_deref();
    Ok(engine::calculate(rules, &ctx, hook)
        .items
        .into_iter()
        .map(|(key, r)| (key, r.price))
        .collect())
}

#[derive(Deserialize)]
pub struct PageQuery {
    page: Option<i64>,
}

/// Непустые корзины, новые первыми; сумма — по тем же правилам и скидкам, что в API.
pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<PageQuery>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let page = q.page.unwrap_or(1).clamp(1, 100_000);
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM buyers b WHERE EXISTS (SELECT 1 FROM cart_items i WHERE i.buyer_id = b.id AND i.order_id IS NULL)",
    )
    .fetch_one(&state.db)
    .await?;
    let buyers: Vec<BuyerRow> = sqlx::query_as(
        "SELECT b.id, b.user_id, u.login, NULLIF(trim(u.name || ' ' || u.last_name), '') AS name, b.updated_at
         FROM buyers b LEFT JOIN users u ON u.id = b.user_id
         WHERE EXISTS (SELECT 1 FROM cart_items i WHERE i.buyer_id = b.id AND i.order_id IS NULL)
         ORDER BY b.updated_at DESC, b.id DESC LIMIT $1 OFFSET $2",
    )
    .bind(PER_PAGE)
    .bind((page - 1) * PER_PAGE)
    .fetch_all(&state.db)
    .await?;
    let mut items_by_buyer = HashMap::new();
    let mut product_ids = Vec::new();
    for b in &buyers {
        let items = repo::items(&state.db, b.id).await?;
        product_ids.extend(items.iter().map(|i| i.product_id));
        items_by_buyer.insert(b.id, items);
    }
    let info = catalog::load(&state.db, &product_ids).await?;
    let stores = stores(&state).await?;
    let config = snapshot_config();
    let rules = discount_repo::active_rules(&state.db, Utc::now()).await?;
    let mut rows = Vec::with_capacity(buyers.len());
    for b in &buyers {
        let items = &items_by_buyer[&b.id];
        let prices = discounted(&state, &rules, b, items, &info).await?;
        let snap = build_snapshot(items, &info, &HashMap::new(), &stores, &prices, &config);
        rows.push(CartRow {
            id: b.id,
            buyer: buyer_label(b.login.as_deref(), b.name.as_deref()),
            items: items.len(),
            total: snap["totalPrice"].clone(),
            updated_at: b.updated_at.format("%d.%m.%Y %H:%M").to_string(),
        });
    }
    let pages = ((total + PER_PAGE - 1) / PER_PAGE).max(1);
    render(
        &state,
        "shop/carts.html",
        context! { user, rows, total, page, pages },
    )
}

/// Позиции одной корзины.
pub async fn view(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let buyer: BuyerRow = sqlx::query_as(
        "SELECT b.id, b.user_id, u.login, NULLIF(trim(u.name || ' ' || u.last_name), '') AS name, b.updated_at
         FROM buyers b LEFT JOIN users u ON u.id = b.user_id WHERE b.id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let items = repo::items(&state.db, id).await?;
    let product_ids: Vec<i64> = items.iter().map(|i| i.product_id).collect();
    let info = catalog::load(&state.db, &product_ids).await?;
    let stores = stores(&state).await?;
    let rules = discount_repo::active_rules(&state.db, Utc::now()).await?;
    let prices = discounted(&state, &rules, &buyer, &items, &info).await?;
    let snap = build_snapshot(
        &items,
        &info,
        &HashMap::new(),
        &stores,
        &prices,
        &snapshot_config(),
    );
    let label = buyer_label(buyer.login.as_deref(), buyer.name.as_deref());
    render(
        &state,
        "shop/cart.html",
        context! { user, id, label, snap => minijinja::Value::from_serialize(&snap) },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Корзина в админке — с ценами со скидками, как её видит покупатель в API.
    #[sqlx::test]
    async fn cart_shows_discounted_prices(db: sqlx::PgPool) {
        use crate::smoke_tests::get;
        let fx = crate::test_support::cart_fixture(&db).await;
        sqlx::query("INSERT INTO discounts (name, actions) VALUES ('Скидка', $1)")
            .bind(serde_json::json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": [{
                "CLASS_ID": "ActSaleBsktGrp",
                "DATA": {"Type": "Discount", "Value": 10, "Unit": "Perc", "Max": 0, "All": "AND", "True": "True"},
                "CHILDREN": []}]}))
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let cookie = crate::test_support::admin_cookie(&db).await;
        let (status, html) = get(
            &app,
            &format!("/admin/shop/carts/{}", fx.f.buyer_id),
            &cookie,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(html.contains("3 шт. на 2250 RUB"), "{html}");
        assert!(html.contains("<td>900</td>"), "{html}");
        assert!(html.contains("<td>1800</td>"), "{html}");
        let (_, list) = get(&app, "/admin/shop/carts", &cookie).await;
        assert!(list.contains("2250"), "{list}");
        assert!(!list.contains("2500"), "{list}");
    }

    #[test]
    fn buyer_label_values() {
        assert_eq!(
            buyer_label(Some("ivan"), Some("Иван Петров")),
            "ivan (Иван Петров)"
        );
        assert_eq!(buyer_label(Some("ivan"), Some(" ")), "ivan");
        assert_eq!(buyer_label(None, None), "Гость");
    }
}
