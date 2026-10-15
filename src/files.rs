//! Загруженные файлы: лежат в `UPLOAD_DIR`, метаданные — в таблице `files`.
//! Наружу отдаются как `/upload/<path>`.

use std::{io::Cursor, path::Path};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgConnection, PgPool};

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
    /// Хеш содержимого (hex sha256); у файлов, сохранённых до появления поля, пусто.
    pub sha256: String,
}

impl FileRecord {
    pub fn url(&self) -> String {
        format!("/upload/{}", self.path)
    }

    pub fn is_image(&self) -> bool {
        self.width.is_some()
    }
}

const FILE_COLS: &str =
    "id, path, original_name, content_type, size, width, height, created_at, sha256";

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
    let mut conn = db
        .acquire()
        .await
        .context("не удалось получить соединение с БД")?;
    save_in(
        &mut conn,
        upload_dir,
        subdir,
        original_name,
        content_type,
        data,
    )
    .await
}

/// Как [`save`], но запись в БД идёт в переданном соединении (например, внутри транзакции).
pub async fn save_in(
    conn: &mut PgConnection,
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
        "INSERT INTO files (path, original_name, content_type, size, width, height, sha256)
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING {FILE_COLS}"
    )))
    .bind(&rel)
    .bind(original_name)
    .bind(content_type)
    .bind(data.len() as i64)
    .bind(width)
    .bind(height)
    .bind(hex::encode(Sha256::digest(data)))
    .fetch_one(conn)
    .await
    .context("не удалось сохранить файл в БД")?;
    Ok(record)
}

/// Размеры картинки без полного декодирования; `None` — не картинка.
pub fn image_size(data: &[u8]) -> Option<(u32, u32)> {
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

    #[sqlx::test]
    async fn saved_file_has_sha256(db: PgPool) {
        let dir = std::env::temp_dir().join(format!("cms-files-{}", rand::random::<u64>()));
        let data = b"hello 1c";
        let expected = hex::encode(sha2::Sha256::digest(data));

        let rec = save(&db, &dir, "t", "a.txt", "text/plain", data)
            .await
            .unwrap();
        assert_eq!(rec.sha256, expected);
        let stored: String = sqlx::query_scalar("SELECT sha256 FROM files WHERE id = $1")
            .bind(rec.id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(stored, expected);

        // контрольное значение: sha256("abc") — известный дайджест, а не пересчёт тем же кодом
        let abc = save(&db, &dir, "t", "abc.txt", "text/plain", b"abc")
            .await
            .unwrap();
        assert_eq!(
            abc.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let mut conn = db.acquire().await.unwrap();
        let rec2 = save_in(&mut conn, &dir, "t", "b.txt", "text/plain", b"other")
            .await
            .unwrap();
        assert_eq!(rec2.sha256, hex::encode(sha2::Sha256::digest(b"other")));
        assert_ne!(rec2.sha256, rec.sha256);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
