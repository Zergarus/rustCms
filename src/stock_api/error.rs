//! Ошибки API склада: JSON-ответы по таблице спеки.

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::stock::StockError;

pub const UNAUTHORIZED_MESSAGE: &str = "Неверный или отозванный API-ключ";

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    Forbidden(String),
    NotFound,
    BadRequest(String),
    Conflict { message: String, id: Option<i64> },
    Rejected(Vec<String>),
    Internal(anyhow::Error),
}

pub type ApiResult<T> = Result<T, ApiError>;

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.into())
    }
}

impl From<StockError> for ApiError {
    fn from(e: StockError) -> Self {
        match e {
            StockError::Rejected(messages) => ApiError::Rejected(messages),
            StockError::Db(e) => ApiError::Internal(e.into()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Unauthorized => {
                let mut res = (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "unauthorized", "message": UNAUTHORIZED_MESSAGE})),
                )
                    .into_response();
                res.headers_mut()
                    .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                res
            }
            ApiError::Forbidden(message) => (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "forbidden", "message": message})),
            )
                .into_response(),
            ApiError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "not_found", "message": "Не найдено"})),
            )
                .into_response(),
            ApiError::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "bad_request", "message": message})),
            )
                .into_response(),
            ApiError::Conflict { message, id } => {
                let mut body = json!({"error": "conflict", "message": message});
                if let Some(id) = id {
                    body["id"] = json!(id);
                }
                (StatusCode::CONFLICT, Json(body)).into_response()
            }
            ApiError::Rejected(messages) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": "rejected", "messages": messages})),
            )
                .into_response(),
            ApiError::Internal(e) => {
                tracing::error!(error = ?e, "stock api internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal", "message": "Внутренняя ошибка"})),
                )
                    .into_response()
            }
        }
    }
}
