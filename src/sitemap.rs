//! Sitemap-эндпоинты: индекс типов и записи по типу в контракте Битрикса.
//!
//! Фронт собирает `/sitemap.xml` и `/sitemap-{type}.xml` из этих двух ответов,
//! поэтому формат (`status/data/errors`, `lastmod` в RFC3339) повторяет живой
//! Битрикс. См. `docs/superpowers/specs/2026-11-09-sitemap-api-design.md`.

use axum::{extract::State, response::Response};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::json;
use sqlx::PgPool;

use crate::{error::AppResult, state::AppState};

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
