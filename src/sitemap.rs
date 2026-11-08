//! Sitemap-эндпоинты: индекс типов и записи по типу в контракте Битрикса.
//!
//! Фронт собирает `/sitemap.xml` и `/sitemap-{type}.xml` из этих двух ответов,
//! поэтому формат (`status/data/errors`, `lastmod` в RFC3339) повторяет живой
//! Битрикс. См. `docs/superpowers/specs/2026-11-09-sitemap-api-design.md`.

use axum::{
    extract::{Path, State},
    response::Response,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::json;
use sqlx::PgPool;

use crate::{
    error::{AppError, AppResult},
    state::AppState,
};

/// Типы sitemap в том порядке, в котором их ждёт фронт.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Products,
    Catalog,
    News,
    Schemes,
    Contacts,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Products,
        Kind::Catalog,
        Kind::News,
        Kind::Schemes,
        Kind::Contacts,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Products => "products",
            Kind::Catalog => "catalog",
            Kind::News => "news",
            Kind::Schemes => "schemes",
            Kind::Contacts => "contacts",
        }
    }

    pub fn parse(raw: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.name() == raw)
    }

    /// Путь страницы записи на витрине. `catalog` пути не строит: его адреса
    /// собирает фронт по slug семейства.
    pub fn path(self, id: i64, code: &str) -> Option<String> {
        match self {
            Kind::Products if code.is_empty() => Some(format!("/product/{id}/")),
            Kind::Products => Some(format!("/product/{}/", code.to_lowercase())),
            Kind::News => Some(format!("/news/{code}/")),
            Kind::Schemes => Some(format!("/schemes/{code}/")),
            Kind::Contacts => Some(format!("/contacts/{id}/")),
            Kind::Catalog => None,
        }
    }

    /// Коллекция-источник. `catalog` пока пуст: адреса его страниц строит
    /// `slugifyMarkFamily` проекта, в CMS такой логики нет (отдельная задача).
    pub fn collection(self) -> Option<&'static str> {
        match self {
            Kind::Products => Some("catalog"),
            Kind::Catalog => None,
            Kind::News => Some("info"),
            Kind::Schemes => Some("schemes"),
            Kind::Contacts => Some("contacts"),
        }
    }
}

pub async fn index(State(state): State<AppState>) -> AppResult<Response> {
    let mut items = Vec::with_capacity(Kind::ALL.len());
    for kind in Kind::ALL {
        let (count, lastmod) = match kind.collection() {
            Some(collection) => count(&state.db, collection).await?,
            None => (0, None),
        };
        items.push(json!({
            "type": kind.name(),
            "count": count,
            "lastmod": lastmod.map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, false)),
        }));
    }
    Ok(crate::bxapi::success(json!({ "items": items })))
}

/// Сколько записей типа видит витрина и когда последняя менялась.
///
/// Правила видимости те же, что у публичного API: коллекция открыта в API,
/// запись активна и её дата публикации наступила. Разделы не фильтруются —
/// иначе счётчик разойдётся с Битриксом (4330 против 4319 у товаров).
async fn count(db: &PgPool, collection: &str) -> AppResult<(i64, Option<DateTime<Utc>>)> {
    let row = sqlx::query_as(
        "SELECT count(*), max(i.updated_at)
         FROM collection_items i
         JOIN collections c ON c.id = i.collection_id
         WHERE c.code = $1
           AND c.api_enabled
           AND i.active
           AND (i.published_at IS NULL OR i.published_at <= now())",
    )
    .bind(collection)
    .fetch_one(db)
    .await?;
    Ok(row)
}

pub async fn kind(State(state): State<AppState>, Path(raw): Path<String>) -> AppResult<Response> {
    let kind = Kind::parse(&raw).ok_or(AppError::NotFound)?;
    let mut items = Vec::new();
    if let Some(collection) = kind.collection() {
        let rows: Vec<(i64, String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT i.id, i.code, i.updated_at
             FROM collection_items i
             JOIN collections c ON c.id = i.collection_id
             WHERE c.code = $1
               AND c.api_enabled
               AND i.active
               AND (i.published_at IS NULL OR i.published_at <= now())
             ORDER BY i.id",
        )
        .bind(collection)
        .fetch_all(&state.db)
        .await?;
        for (id, code, updated_at) in rows {
            if let Some(path) = kind.path(id, &code) {
                items.push(json!({
                    "path": path,
                    "lastmod": updated_at.to_rfc3339_opts(SecondsFormat::Secs, false),
                }));
            }
        }
    }
    Ok(crate::bxapi::success(
        json!({ "type": kind.name(), "items": items }),
    ))
}
