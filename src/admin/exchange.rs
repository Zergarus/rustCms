//! Обмен с 1С: настройки и журнал последних обменов.

use axum::{
    Extension, Form,
    extract::State,
    response::{Html, IntoResponse, Redirect, Response},
};
use minijinja::context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::render;
use crate::{
    access::{Access, EXCHANGE_1C},
    error::AppResult,
    exchange::settings::{Action, Settings},
    state::AppState,
};

const LOG_ROWS: i64 = 50;
const MB: u64 = 1024 * 1024;

#[derive(Serialize)]
struct LogRow {
    created_at: String,
    mode: String,
    filename: String,
    result: String,
    summary: String,
    error: String,
}

#[derive(Serialize)]
struct CollectionOption {
    id: i64,
    name: String,
}

/// Счётчики из `stats` одной строкой: «создано: items 3, sections 1; обновлено: …».
fn summarize(stats: &Value) -> String {
    const KINDS: [(&str, &str); 5] = [
        ("created", "создано"),
        ("updated", "обновлено"),
        ("deactivated", "деактивировано"),
        ("deleted", "удалено"),
        ("skipped", "пропущено"),
    ];
    let mut parts = Vec::new();
    for (key, label) in KINDS {
        let Some(map) = stats.get(key).and_then(Value::as_object) else {
            continue;
        };
        let items: Vec<String> = map
            .iter()
            .filter(|(_, n)| n.as_u64().unwrap_or(0) > 0)
            .map(|(k, n)| format!("{k} {n}"))
            .collect();
        if !items.is_empty() {
            parts.push(format!("{label}: {}", items.join(", ")));
        }
    }
    parts.join("; ")
}

async fn page(
    state: &AppState,
    user: &Access,
    settings: &Settings,
    file_limit_mb: String,
    interval_secs: String,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let collections: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, name FROM collections
             WHERE is_catalog AND product_collection_id IS NULL ORDER BY name, id",
    )
    .fetch_all(&state.db)
    .await?;
    let collections: Vec<CollectionOption> = collections
        .into_iter()
        .map(|(id, name)| CollectionOption { id, name })
        .collect();
    let rows: Vec<(String, String, String, String, Value, String)> = sqlx::query_as(
        "SELECT to_char(created_at, 'YYYY-MM-DD HH24:MI:SS'), mode, filename, result, stats, error
         FROM exchange_log ORDER BY id DESC LIMIT $1",
    )
    .bind(LOG_ROWS)
    .fetch_all(&state.db)
    .await?;
    let log: Vec<LogRow> = rows
        .into_iter()
        .map(
            |(created_at, mode, filename, result, stats, error)| LogRow {
                created_at,
                mode,
                filename,
                result,
                summary: summarize(&stats),
                error,
            },
        )
        .collect();
    render(
        state,
        "exchange.html",
        context! {
            user,
            error,
            collections,
            log,
            default_collection => settings.default_collection_id,
            section_action => settings.section_action.code(),
            element_action => settings.element_action.code(),
            translit_on_add => settings.translit_on_add,
            translit_on_update => settings.translit_on_update,
            use_offers => settings.use_offers,
            deactivate_without_price => settings.deactivate_without_price,
            use_zip => settings.use_zip,
            interval_secs,
            file_limit_mb,
        },
    )
}

pub async fn form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require(EXCHANGE_1C)?;
    let s = Settings::load(&state.db).await?;
    let mb = (s.file_limit / MB).max(1).to_string();
    page(&state, &user, &s, mb, s.interval_secs.to_string(), None).await
}

#[derive(Deserialize)]
pub struct ExchangeForm {
    #[serde(default)]
    default_collection: String,
    #[serde(default)]
    section_action: String,
    #[serde(default)]
    element_action: String,
    translit_on_add: Option<String>,
    translit_on_update: Option<String>,
    use_offers: Option<String>,
    deactivate_without_price: Option<String>,
    use_zip: Option<String>,
    #[serde(default)]
    interval_secs: String,
    #[serde(default)]
    file_limit_mb: String,
}

pub async fn save(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<ExchangeForm>,
) -> AppResult<Response> {
    user.require(EXCHANGE_1C)?;
    let raw_collection = form.default_collection.trim();
    let collection_parsed = if raw_collection.is_empty() {
        Ok(None)
    } else {
        raw_collection.parse::<i64>().map(Some)
    };
    let default_collection_id = collection_parsed.clone().ok().flatten();
    let mut settings = Settings {
        default_collection_id,
        section_action: Action::from_code(&form.section_action),
        element_action: Action::from_code(&form.element_action),
        translit_on_add: form.translit_on_add.is_some(),
        translit_on_update: form.translit_on_update.is_some(),
        use_offers: form.use_offers.is_some(),
        deactivate_without_price: form.deactivate_without_price.is_some(),
        use_zip: form.use_zip.is_some(),
        ..Settings::default()
    };
    let interval = form
        .interval_secs
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|n| (1..=3600).contains(n));
    let limit_mb = form
        .file_limit_mb
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|n| (1..=u64::MAX / MB).contains(n));
    let collection_ok = match default_collection_id {
        None => true,
        Some(id) => {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM collections
                         WHERE id = $1 AND is_catalog AND product_collection_id IS NULL)",
            )
            .bind(id)
            .fetch_one(&state.db)
            .await?
        }
    };
    let error = if collection_parsed.is_err() {
        Some("Коллекция по умолчанию: некорректный номер")
    } else if interval.is_none() {
        Some("Интервал — целое число секунд от 1 до 3600")
    } else if limit_mb.is_none() {
        Some("Лимит файла — целое число мегабайт, не меньше 1")
    } else if !collection_ok {
        Some("Коллекция по умолчанию должна быть коллекцией каталога (не предложений)")
    } else {
        None
    };
    if let Some(error) = error {
        settings.interval_secs = interval.unwrap_or(30);
        return Ok(page(
            &state,
            &user,
            &settings,
            form.file_limit_mb,
            form.interval_secs,
            Some(error.to_string()),
        )
        .await?
        .into_response());
    }
    settings.interval_secs = interval.unwrap_or(30);
    settings.file_limit = limit_mb.unwrap_or(1) * MB;
    settings.save(&state.db).await?;
    Ok(Redirect::to("/admin/exchange").into_response())
}
