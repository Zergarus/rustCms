//! Раздел админки «Магазин» (модули sale и catalog): склады, корзины, типы цен,
//! валюты, настройки каталога. Право — `shop.manage`.

pub mod stores;

use axum::{
    Extension,
    extract::{Query, State},
    response::Html,
};
use serde::Deserialize;

use crate::{
    access::{Access, SHOP_MANAGE},
    error::AppResult,
    state::AppState,
};

pub(crate) fn require_shop(user: &Access) -> AppResult<()> {
    user.require(SHOP_MANAGE)
}

#[derive(Deserialize)]
pub struct LocationQuery {
    #[serde(default)]
    q: String,
}

/// Подпись местоположения: «Город, предки снизу вверх» (для выбора города склада).
pub(crate) async fn location_labels(
    state: &AppState,
    ids: &[i64],
) -> sqlx::Result<Vec<(i64, String)>> {
    sqlx::query_as(
        "WITH RECURSIVE chain AS (
             SELECT id AS root, id, parent_id, name, 0 AS depth FROM locations WHERE id = ANY($1)
             UNION ALL
             SELECT c.root, l.id, l.parent_id, l.name, c.depth + 1
             FROM locations l JOIN chain c ON l.id = c.parent_id
         )
         SELECT root, string_agg(name, ', ' ORDER BY depth) FROM chain GROUP BY root",
    )
    .bind(ids)
    .fetch_all(&state.db)
    .await
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Варианты городов для `<select>` формы склада (HTMX): до 20 по началу названия.
pub async fn locations(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<LocationQuery>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let q = q.q.trim();
    let mut html = String::from("<option value=\"\">— не задан —</option>");
    if q.chars().count() >= 2 {
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM locations WHERE type_code IN ('CITY', 'VILLAGE') AND name ILIKE $1
             ORDER BY lower(name), id LIMIT 20",
        )
        .bind(format!("{}%", q.replace('%', "")))
        .fetch_all(&state.db)
        .await?;
        let mut labels = location_labels(&state, &ids).await?;
        labels.sort_by_key(|(id, _)| ids.iter().position(|i| i == id));
        for (id, label) in labels {
            html.push_str(&format!(
                "<option value=\"{id}\">{}</option>",
                escape(&label)
            ));
        }
    }
    Ok(Html(html))
}
