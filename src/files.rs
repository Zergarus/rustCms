//! Загруженные файлы: лежат в `UPLOAD_DIR`, метаданные — в таблице `files`.
//! Наружу отдаются как `/upload/<path>`.

use std::{io::Cursor, path::Path};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgPool};

/// Расширения, которые можно загрузить. HTML/SVG и прочее, что браузер
/// исполнит на нашем origin, сюда не входит.
const ALLOWED_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "avif", "pdf", "doc", "docx", "xls", "xlsx", "zip", "txt",
    "csv",
];

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub original_name: String,
    pub content_type: String,
    pub size: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub created_at: DateTime<Utc>,
}

impl FileRecord {
    pub fn url(&self) -> String {
        format!("/upload/{}", self.path)
    }

    pub fn is_image(&self) -> bool {
        self.width.is_some()
    }
}

const FILE_COLS: &str = "id, path, original_name, content_type, size, width, height, created_at";

/// Расширение из имени файла в нижнем регистре, если оно разрешено.
pub fn allowed_extension(original_name: &str) -> Option<String> {
    let ext = Path::new(original_name)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    ALLOWED_EXTENSIONS.contains(&ext.as_str()).then_some(ext)
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    /// Файл отклонён — сообщение для пользователя.
    #[error("{0}")]
    Rejected(String),
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// Сохраняет файл на диск и в БД. `subdir` — раздел хранилища (например, `iblock`).
pub async fn save(
    db: &PgPool,
    upload_dir: &Path,
    subdir: &str,
    original_name: &str,
    content_type: &str,
    data: &[u8],
) -> Result<FileRecord, SaveError> {
    let ext = allowed_extension(original_name)
        .ok_or_else(|| SaveError::Rejected(format!("«{original_name}»: недопустимый тип файла")))?;
    let name = hex::encode(rand::random::<[u8; 12]>());
    let rel = format!("{subdir}/{}/{name}.{ext}", &name[..2]);
    let full = upload_dir.join(&rel);

    let write = async {
        tokio::fs::create_dir_all(full.parent().expect("путь с каталогом")).await?;
        tokio::fs::write(&full, data).await
    };
    write
        .await
        .with_context(|| format!("не удалось записать {}", full.display()))?;

    let (width, height) = match image_size(data) {
        Some((w, h)) => (Some(w as i32), Some(h as i32)),
        None => (None, None),
    };
    let record = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO files (path, original_name, content_type, size, width, height)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING {FILE_COLS}"
    )))
    .bind(&rel)
    .bind(original_name)
    .bind(content_type)
    .bind(data.len() as i64)
    .bind(width)
    .bind(height)
    .fetch_one(db)
    .await
    .context("не удалось сохранить файл в БД")?;
    Ok(record)
}

/// Размеры картинки без полного декодирования; `None` — не картинка.
fn image_size(data: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

pub async fn get_many(db: &PgPool, ids: &[i64]) -> sqlx::Result<Vec<FileRecord>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FILE_COLS} FROM files WHERE id = ANY($1)"
    )))
    .bind(ids)
    .fetch_all(db)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert_eq!(allowed_extension("Фото.JPG").as_deref(), Some("jpg"));
        assert_eq!(allowed_extension("doc.pdf").as_deref(), Some("pdf"));
        assert_eq!(allowed_extension("x.svg"), None);
        assert_eq!(allowed_extension("x.html"), None);
        assert_eq!(allowed_extension("noext"), None);
    }
}
