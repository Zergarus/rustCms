//! Полный список штрихкодов товара: общие, единицы на складах и в заказах.

use axum::{
    Extension,
    extract::{Query, State},
    response::Html,
};
use minijinja::context;
use serde::{Deserialize, Serialize};

use super::require_shop;
use crate::{
    access::Access,
    admin::render,
    error::{AppError, AppResult},
    state::AppState,
    stock::barcodes,
};

#[derive(Deserialize)]
pub struct ListQuery {
    item: i64,
}

#[derive(Serialize)]
struct Row {
    barcode: String,
    store: String,
    order_id: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<ListQuery>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let name: String = sqlx::query_scalar("SELECT name FROM collection_items WHERE id = $1")
        .bind(q.item)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let stores: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM catalog_stores")
        .fetch_all(&state.db)
        .await?;
    let rows: Vec<Row> = barcodes::list(&mut *state.db.acquire().await?, q.item)
        .await?
        .into_iter()
        .map(|b| Row {
            store: match (b.store_id, b.order_id) {
                (Some(s), _) => stores
                    .iter()
                    .find(|(id, _)| *id == s)
                    .map_or_else(|| format!("#{s}"), |(_, n)| n.clone()),
                (None, Some(_)) => "В заказе".into(),
                (None, None) => "Общий".into(),
            },
            barcode: b.barcode,
            order_id: b.order_id,
        })
        .collect();
    let item_id = q.item;
    render(
        &state,
        "shop/barcodes.html",
        context! { user, item_id, name, rows },
    )
}
