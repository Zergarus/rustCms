//! Методы чтения API склада (`stock.read`).

use axum::{Json, extract::State};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::FromRow;

use super::{ApiAccess, Page, error::ApiResult};
use crate::{access::SHOP_MANAGE, state::AppState};

#[derive(Serialize, FromRow)]
struct StoreRow {
    id: i64,
    xml_id: String,
    name: String,
    address: String,
    active: bool,
    is_default: bool,
}

/// `GET /stores`
pub async fn stores(
    State(state): State<AppState>,
    api: ApiAccess,
    page: Page,
) -> ApiResult<Json<Value>> {
    api.require("stock.read", SHOP_MANAGE)?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_stores")
        .fetch_one(&state.db)
        .await?;
    let items: Vec<StoreRow> = sqlx::query_as(
        "SELECT id, xml_id, name, address, active, is_default FROM catalog_stores
         ORDER BY id LIMIT $1 OFFSET $2",
    )
    .bind(page.limit)
    .bind(page.offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({"items": items, "total": total})))
}
