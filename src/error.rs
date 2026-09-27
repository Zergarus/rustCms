use axum::{
    Json,
    http::StatusCode,
    response::Html,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    BadRequest(String),
    /// Недостаточно прав (используется в админке, поэтому отдаёт HTML).
    #[error("forbidden")]
    Forbidden,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Template(#[from] minijinja::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type AppResult<T> = Result<T, AppError>;

const FORBIDDEN_PAGE: &str = r#"<!doctype html>
<html lang="ru"><head><meta charset="utf-8"><title>Доступ запрещён — CMS</title>
<link rel="stylesheet" href="/admin/static/admin.css"></head>
<body><main class="login-page"><div class="card login-card">
<h1>Доступ запрещён</h1>
<p class="muted">У вас нет прав на этот раздел. Обратитесь к администратору.</p>
<a class="btn" href="/admin">На рабочий стол</a>
</div></main></body></html>"#;

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if let AppError::Forbidden = self {
            return (StatusCode::FORBIDDEN, Html(FORBIDDEN_PAGE)).into_response();
        }
        let (status, code, message) = match &self {
            AppError::NotFound => (StatusCode::NOT_FOUND, "not_found", self.to_string()),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "bad_request", msg.clone()),
            _ => {
                tracing::error!(error = ?self, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "internal server error".to_string(),
                )
            }
        };
        (status, Json(json!({ "error": code, "message": message }))).into_response()
    }
}

/// Нарушение UNIQUE-ограничения (например, занятый символьный код).
pub fn is_unique_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(e) if e.is_unique_violation())
}
