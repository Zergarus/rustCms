//! Публичный JSON API для фронтенда (Vue и т.п.). Только чтение, без авторизации.
//! Отдаются только инфоблоки с `api_enabled` и только активные, опубликованные элементы.

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::{FromRow, Postgres, QueryBuilder, types::Json as SqlJson};

use crate::{
    error::{AppError, AppResult},
    iblock::{Iblock, is_valid_code, repo},
    state::AppState,
};

const DEFAULT_PER_PAGE: i64 = 20;
const MAX_PER_PAGE: i64 = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/v1/iblocks", get(list_iblocks))
        .route("/v1/iblocks/{iblock}", get(get_iblock))
        .route("/v1/iblocks/{iblock}/elements", get(list_elements))
        .route("/v1/iblocks/{iblock}/elements/{element}", get(get_element))
}

async fn health(State(state): State<AppState>) -> AppResult<Json<Value>> {
    sqlx::query("SELECT 1").execute(&state.db).await?;
    Ok(Json(json!({ "status": "ok" })))
}

#[derive(Serialize)]
struct ApiIblock {
    code: String,
    name: String,
    description: String,
}

#[derive(Serialize)]
struct ApiProperty {
    code: String,
    name: String,
    kind: String,
    required: bool,
}

#[derive(Serialize, FromRow)]
struct ApiElement {
    id: i64,
    code: String,
    name: String,
    sort: i32,
    preview_text: String,
    detail_text: String,
    published_at: Option<DateTime<Utc>>,
    properties: SqlJson<Map<String, Value>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct Pagination {
    page: i64,
    per_page: i64,
    total: i64,
    pages: i64,
}

async fn list_iblocks(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let items: Vec<ApiIblock> = repo::list_iblocks(&state.db)
        .await?
        .into_iter()
        .filter(|s| s.iblock.api_enabled)
        .map(|s| ApiIblock {
            code: s.iblock.code,
            name: s.iblock.name,
            description: s.iblock.description,
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

async fn public_iblock(state: &AppState, code: &str) -> AppResult<Iblock> {
    repo::get_iblock_by_code(&state.db, code)
        .await?
        .filter(|i| i.api_enabled)
        .ok_or(AppError::NotFound)
}

async fn get_iblock(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> AppResult<Json<Value>> {
    let iblock = public_iblock(&state, &code).await?;
    let properties: Vec<ApiProperty> = repo::list_properties(&state.db, iblock.id)
        .await?
        .into_iter()
        .map(|p| ApiProperty {
            code: p.code,
            name: p.name,
            kind: p.kind,
            required: p.is_required,
        })
        .collect();
    Ok(Json(json!({
        "code": iblock.code,
        "name": iblock.name,
        "description": iblock.description,
        "properties": properties,
    })))
}

/// Параметры списка:
/// - `page`, `per_page` (≤ 100)
/// - `sort`: `sort` | `name` | `published_at` | `created_at` | `id`, префикс `-` — по убыванию
/// - `prop.<код>=<значение>` — фильтр по значению свойства (сравнение как строк)
async fn list_elements(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Json<Value>> {
    let iblock = public_iblock(&state, &code).await?;

    let page = parse_positive(params.get("page"), 1).min(1_000_000);
    let per_page = parse_positive(params.get("per_page"), DEFAULT_PER_PAGE).min(MAX_PER_PAGE);
    let order_by = match params.get("sort").map(String::as_str).unwrap_or("sort") {
        "sort" => "sort ASC, id DESC",
        "-sort" => "sort DESC, id DESC",
        "name" => "name ASC, id ASC",
        "-name" => "name DESC, id DESC",
        "published_at" => "published_at ASC NULLS FIRST, id ASC",
        "-published_at" => "published_at DESC NULLS LAST, id DESC",
        "created_at" => "created_at ASC, id ASC",
        "-created_at" => "created_at DESC, id DESC",
        "id" => "id ASC",
        "-id" => "id DESC",
        other => {
            return Err(AppError::BadRequest(format!(
                "недопустимая сортировка: {other}"
            )));
        }
    };

    let mut prop_filters = Vec::new();
    for (key, value) in &params {
        if let Some(prop) = key.strip_prefix("prop.") {
            if !is_valid_code(prop) {
                return Err(AppError::BadRequest(format!(
                    "недопустимый код свойства: {prop}"
                )));
            }
            prop_filters.push((prop.to_string(), value.clone()));
        }
    }

    let push_where = |qb: &mut QueryBuilder<Postgres>| {
        qb.push(" FROM collection_items WHERE collection_id = ")
            .push_bind(iblock.id)
            .push(" AND active AND (published_at IS NULL OR published_at <= now())");
        for (prop, value) in &prop_filters {
            qb.push(" AND field_values ->> ")
                .push_bind(prop.clone())
                .push(" = ")
                .push_bind(value.clone());
        }
    };

    let mut count_qb = QueryBuilder::new("SELECT count(*)");
    push_where(&mut count_qb);
    let (total,): (i64,) = count_qb.build_query_as().fetch_one(&state.db).await?;

    let mut qb = QueryBuilder::new(
        "SELECT id, code, name, sort, preview_text, detail_text, published_at, field_values AS properties, \
         created_at, updated_at",
    );
    push_where(&mut qb);
    qb.push(" ORDER BY ")
        .push(order_by)
        .push(" LIMIT ")
        .push_bind(per_page)
        .push(" OFFSET ")
        .push_bind((page - 1) * per_page);
    let items: Vec<ApiElement> = qb.build_query_as().fetch_all(&state.db).await?;

    let pagination = Pagination {
        page,
        per_page,
        total,
        pages: (total + per_page - 1) / per_page,
    };
    Ok(Json(json!({ "items": items, "pagination": pagination })))
}

async fn get_element(
    State(state): State<AppState>,
    Path((iblock_code, element_code)): Path<(String, String)>,
) -> AppResult<Json<ApiElement>> {
    let iblock = public_iblock(&state, &iblock_code).await?;
    let element: ApiElement = sqlx::query_as(
        "SELECT id, code, name, sort, preview_text, detail_text, published_at, field_values AS properties,
                created_at, updated_at
         FROM collection_items
         WHERE collection_id = $1 AND code = $2 AND active
           AND (published_at IS NULL OR published_at <= now())",
    )
    .bind(iblock.id)
    .bind(&element_code)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(element))
}

fn parse_positive(raw: Option<&String>, default: i64) -> i64 {
    raw.and_then(|s| s.parse::<i64>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(default)
}
