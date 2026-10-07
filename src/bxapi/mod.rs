//! API, совместимое с модулем `mediagroup.bxapi` 1С-Битрикса: фронт, написанный под
//! Битрикс, работает с CMS без правок. Контракт — `FRONTEND_API.md` модуля; где
//! документация и живой Битрикс расходятся, повторяем Битрикс.
//!
//! Все ответы в обёртке `{status, data, errors}`. Коды инфоблоков и свойств в API —
//! camelCase (`homeTabs`, `relatedBrands`), в CMS — snake_case.

mod auth;
mod cart;
mod elements;
mod filter;
mod forms;
mod images;
mod location;
mod nav;
pub mod project;
mod query;
pub mod registry;
mod search;
mod sections;
mod serialize;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Map, Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/base/csrf/token", get(csrf_token))
        .route("/cart", get(cart::get_cart))
        .route("/cart/items", post(cart::add_item))
        .route("/cart/items/{id}/quantity", post(cart::set_quantity))
        .route("/cart/items/{id}/store", post(cart::set_store))
        .route("/cart/items/{id}/remove", post(cart::remove_item))
        .route("/cart/clear", post(cart::clear))
        .route("/auth/login", post(auth::login))
        .route("/auth/session", get(auth::session))
        .route("/auth/logout", post(auth::logout))
        .route("/location/search", get(location::search))
        .route("/location/set", post(location::set))
        .route("/location/current", get(location::current))
        .route("/iblock/list", get(elements::iblock_list))
        .route("/form/{code}", post(forms::submit))
        .route("/nav/breadcrumbs", post(nav::breadcrumbs))
        .route("/nav/legacy-redirect", post(nav::legacy_redirect))
        .route("/iblock/{*rest}", post(dispatch_post).get(dispatch_get))
}

// ---------------------------------------------------------------------------
// Обёртка ответа и ошибки
// ---------------------------------------------------------------------------

pub fn success(data: Value) -> Response {
    Json(json!({ "status": "success", "data": data, "errors": [] })).into_response()
}

/// Ошибка в формате Битрикса. Как и Битрикс, прикладные ошибки отдаём с HTTP 200,
/// а невалидный запрос и сбои — с настоящим кодом.
#[derive(Debug)]
pub struct BxError {
    status: StatusCode,
    code: Value,
    message: String,
}

impl BxError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            code: Value::from(code),
            message: message.into(),
        }
    }

    pub fn with_status(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            ..Self::new(code, message)
        }
    }

    /// Ошибка с числовым `code`, равным HTTP-статусу.
    pub fn numeric(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code: Value::from(status.as_u16()),
            message: message.into(),
        }
    }

    pub fn bad_request(code: &str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            ..Self::new(code, message)
        }
    }

    fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            ..Self::new("not_found", "Route not found")
        }
    }
}

impl IntoResponse for BxError {
    fn into_response(self) -> Response {
        let body = json!({
            "status": "error",
            "data": null,
            "errors": [{ "message": self.message, "code": self.code, "customData": null }],
        });
        (self.status, Json(body)).into_response()
    }
}

impl From<sqlx::Error> for BxError {
    fn from(err: sqlx::Error) -> Self {
        tracing::error!(error = ?err, "bxapi: ошибка БД");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            ..Self::new("internal", "Internal server error")
        }
    }
}

impl From<anyhow::Error> for BxError {
    fn from(err: anyhow::Error) -> Self {
        tracing::error!(error = ?err, "bxapi: внутренняя ошибка");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            ..Self::new("internal", "Internal server error")
        }
    }
}

pub type BxResult = Result<Response, BxError>;

/// Тело POST-запроса: пустое — пустой объект, невалидный JSON — 400 `invalid_json`.
fn parse_body(body: &Bytes) -> Result<Map<String, Value>, BxError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Ok(Map::new()),
        Err(e) => Err(BxError::bad_request(
            "invalid_json",
            format!("Invalid JSON in request body: {e}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Маршруты
// ---------------------------------------------------------------------------

/// CSRF-токен. Проверка токенов включится вместе с сессиями посетителей;
/// пока, как `dev`-окружение bxapi, токен выдаём, но не проверяем.
async fn csrf_token() -> Response {
    success(json!({ "token": hex::encode(rand::random::<[u8; 16]>()) }))
}

/// Разбор `/iblock/...` вручную: `info/{apiCode}` и `{apiCode}/element/list`
/// конфликтуют в роутере (apiCode может быть и `info`).
async fn dispatch_post(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    body: Bytes,
) -> BxResult {
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let body = parse_body(&body)?;
    match segments.as_slice() {
        [api_code, "element", "list"] => elements::list(&state, api_code, body).await,
        [api_code, "element", "slug", slug] => {
            elements::detail(&state, api_code, elements::Key::Slug(slug), body).await
        }
        [api_code, "element", id] if id.bytes().all(|b| b.is_ascii_digit()) => {
            let id = id.parse().map_err(|_| BxError::not_found())?;
            elements::detail(&state, api_code, elements::Key::Id(id), body).await
        }
        [api_code, "section", "list"] => sections::list(&state, api_code, body).await,
        [api_code, "search"] => search::search(&state, api_code, body).await,
        _ => Err(BxError::not_found()),
    }
}

async fn dispatch_get(State(state): State<AppState>, Path(rest): Path<String>) -> BxResult {
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["info", api_code] => elements::iblock_info(&state, api_code).await,
        _ => Err(BxError::not_found()),
    }
}

// ---------------------------------------------------------------------------
// Имена: camelCase в API ↔ snake_case в CMS
// ---------------------------------------------------------------------------

/// `homeTabs` → `home_tabs`, `cml2Manufacturer` → `cml2_manufacturer`,
/// `CML2_MANUFACTURER` → `cml2_manufacturer`.
pub fn to_snake(code: &str) -> String {
    let mut out = String::with_capacity(code.len() + 4);
    let mut prev_lower = false;
    for ch in code.chars() {
        if ch.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            prev_lower = false;
        } else {
            out.push(ch);
            prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        }
    }
    out
}

/// Ключ сравнения строк как у MySQL (utf8_general_ci): без учёта регистра,
/// знаки препинания и пробелы раньше букв и цифр.
pub fn collate_key(s: &str) -> Vec<(u8, char)> {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| (u8::from(c.is_alphanumeric()), c))
        .collect()
}

/// `related_brands` → `relatedBrands`.
pub fn to_camel(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut upper = false;
    for ch in code.chars() {
        if ch == '_' {
            upper = !out.is_empty();
        } else if upper {
            out.push(ch.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_conversion() {
        for (camel, snake) in [
            ("homeTabs", "home_tabs"),
            ("cml2Manufacturer", "cml2_manufacturer"),
            ("relatedBrands", "related_brands"),
            ("ufXmlId", "uf_xml_id"),
            ("catalog", "catalog"),
        ] {
            assert_eq!(to_snake(camel), snake);
            assert_eq!(to_camel(snake), camel);
        }
        assert_eq!(to_snake("RELATED_BRANDS"), "related_brands");
    }

    #[test]
    fn collation() {
        assert!(collate_key("CHANG’AN") < collate_key("CHANGHE"));
        assert!(collate_key("WILL Vi") < collate_key("WILL VS"));
    }

    #[test]
    fn body_parsing() {
        assert!(parse_body(&Bytes::from_static(b"")).unwrap().is_empty());
        assert!(parse_body(&Bytes::from_static(b"{\"a\":1}")).is_ok());
        let err = parse_body(&Bytes::from_static(b"{'a':1}")).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.code, "invalid_json");
    }
}
