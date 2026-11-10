//! Общий rate-limit публичного API: бюджет запросов на IP в минуту.
//!
//! Счётчики в памяти процесса; окно — минута. При нескольких инстансах
//! бюджеты независимы (как у `throttle`), это записано в техдолг.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{bxapi::BxError, state::AppState};

/// Сколько ключей держим в памяти.
const CAP: usize = 10_000;

static BUCKETS: Mutex<Option<HashMap<String, (u64, u32)>>> = Mutex::new(None);

fn minute() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 60)
        .unwrap_or(0)
}

/// Считает запрос; `false` — бюджет на эту минуту исчерпан.
pub fn allow(key: &str, limit: u32) -> bool {
    let now = minute();
    let mut guard = BUCKETS.lock().unwrap_or_else(|error| error.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() >= CAP {
        map.retain(|_, (minute, _)| *minute == now);
        if map.len() >= CAP {
            map.clear();
        }
    }
    let entry = map.entry(key.to_string()).or_insert((now, 0));
    if entry.0 != now {
        *entry = (now, 0);
    }
    entry.1 += 1;
    entry.1 <= limit
}

/// Адрес клиента: за прокси — первый из `X-Forwarded-For`.
pub fn client_ip(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|ip| !ip.is_empty())
        .unwrap_or("local")
        .to_string()
}

/// Middleware: `RATE_LIMIT_PER_MIN` запросов на IP в минуту, 0 — выключено.
pub async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let limit = state.config.rate_limit_per_min;
    if limit == 0 {
        return next.run(req).await;
    }
    if !allow(&client_ip(req.headers()), limit) {
        return BxError::with_status(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "too_many_requests",
            "Слишком много запросов — попробуйте через минуту",
        )
        .into_response();
    }
    next.run(req).await
}
