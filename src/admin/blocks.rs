//! Загрузка картинок для поля «Блоки».

use axum::{
    Json,
    extract::{Multipart, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    access::{Access, Level},
    error::{AppError, AppResult},
    files,
    state::AppState,
};

/// Лимит файла блока.
const MAX_BYTES: usize = 10 * 1024 * 1024;

#[derive(Deserialize)]
pub struct UploadQuery {
    collection: i64,
}

/// `POST /admin/blocks/upload?collection=ID`: сохраняет картинку блока и
/// возвращает её путь для JSON поля.
pub async fn upload(
    State(state): State<AppState>,
    axum::Extension(user): axum::Extension<Access>,
    Query(query): Query<UploadQuery>,
    mut multipart: Multipart,
) -> AppResult<Response> {
    user.require_collection(query.collection, Level::Write)?;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::BadRequest("ожидается multipart с файлом".into()))?
    {
        if field.name() != Some("file") {
            continue;
        }
        let name = field.file_name().unwrap_or("file").to_string();
        let content_type = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_string();
        let data = field
            .bytes()
            .await
            .map_err(|_| AppError::BadRequest("не удалось прочитать файл".into()))?;
        if data.len() > MAX_BYTES {
            return Err(AppError::PayloadTooLarge);
        }
        if files::image_size(&data).is_none() {
            return Err(AppError::Unprocessable("файл не является изображением".into()));
        }
        let record = files::save(
            &state.db,
            &state.config.upload_dir,
            "blocks",
            &name,
            &content_type,
            &data,
        )
        .await
        .map_err(|error| AppError::BadRequest(error.to_string()))?;
        return Ok(Json(json!({
            "status": "success",
            "data": { "path": record.url(), "id": record.id },
        }))
        .into_response());
    }
    Err(AppError::BadRequest("файл не передан".into()))
}

#[allow(dead_code)]
const _: StatusCode = StatusCode::OK;
