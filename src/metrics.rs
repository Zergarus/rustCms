//! Метрики приложения в формате Prometheus: счётчики запросов и живость БД.

use std::{collections::HashMap, sync::Mutex};

use axum::{
    extract::{Request, State},
    http::header,
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::state::AppState;

static COUNTERS: Mutex<Option<HashMap<(String, u16), u64>>> = Mutex::new(None);

fn increment(method: &str, status: u16) {
    let mut guard = COUNTERS.lock().unwrap_or_else(|error| error.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    *map.entry((method.to_string(), status)).or_insert(0) += 1;
}

/// Считает каждый запрос: метод и http-статус.
pub async fn track(req: Request, next: Next) -> Response {
    let method = req.method().to_string();
    let response = next.run(req).await;
    increment(&method, response.status().as_u16());
    response
}

/// `GET /metrics`: текст Prometheus.
pub async fn render(State(state): State<AppState>) -> Response {
    let snapshot: Vec<((String, u16), u64)> = {
        let guard = COUNTERS.lock().unwrap_or_else(|error| error.into_inner());
        guard
            .as_ref()
            .map(|map| {
                map.iter()
                    .map(|(key, value)| (key.clone(), *value))
                    .collect()
            })
            .unwrap_or_default()
    };
    let db_up = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.db)
        .await
        .is_ok();
    let mut body = String::from("# TYPE cms_requests_total counter\n");
    let mut rows = snapshot;
    rows.sort();
    for ((method, status), count) in rows {
        body.push_str(&format!(
            "cms_requests_total{{method=\"{method}\",status=\"{status}\"}} {count}\n"
        ));
    }
    body.push_str("# TYPE cms_db_up gauge\n");
    body.push_str(&format!("cms_db_up {}\n", if db_up { 1 } else { 0 }));
    body.push_str("# TYPE cms_build_info gauge\n");
    body.push_str(&format!(
        "cms_build_info{{version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION")
    ));
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}
