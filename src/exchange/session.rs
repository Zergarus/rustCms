//! Сессии обмена с 1С: токен, каталог файлов, распаковка zip.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use sqlx::PgPool;

pub const COOKIE: &str = "CMS_1C_SID";
/// Срок жизни сессии с последней активности, часы.
pub const TTL_HOURS: i64 = 24;
pub const MAX_UNPACKED_BYTES: u64 = 2_147_483_648;
pub const MAX_ENTRIES: usize = 100_000;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Недопустимое имя файла «{0}»")]
    BadPath(String),
    #[error("Ошибка работы с файлом: {0}")]
    Io(String),
    /// Недогруженный (загрузка ещё идёт) или повреждённый архив; детали - в журнале сервера.
    #[error("Архив не дочитан или повреждён")]
    Zip(String),
    #[error("Размер части файла превышает допустимый ({0} байт)")]
    TooLarge(u64),
    #[error("Архив превышает допустимый размер или число записей")]
    Limit,
    #[error("Ошибка базы данных: {0}")]
    Db(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_bytes: u64,
    pub max_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_bytes: MAX_UNPACKED_BYTES,
            max_entries: MAX_ENTRIES,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: i64,
    pub user_id: Option<i64>,
    pub state: serde_json::Value,
    pub expired: bool,
}

impl Session {
    /// Тип обмена сессии (`state.type`): `catalog` / `sale`; без типа (сессии до обмена
    /// заказами) — `catalog`.
    pub fn kind(&self) -> &str {
        self.state["type"].as_str().unwrap_or(KIND_CATALOG)
    }

    pub fn is_imported(&self, filename: &str) -> bool {
        self.state["imported"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(filename)))
    }
}

pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub const KIND_CATALOG: &str = "catalog";
pub const KIND_SALE: &str = "sale";

/// Новая сессия обмена типа `kind` (`state.type`): токен возвращается один раз, в БД лежит
/// его sha256.
pub async fn start(
    db: &PgPool,
    user_id: i64,
    kind: &str,
) -> Result<(Session, String), SessionError> {
    let token = hex::encode(rand::random::<[u8; 32]>());
    let state = serde_json::json!({ "type": kind });
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO exchange_sessions (token_hash, user_id, state) VALUES ($1, $2, $3)
         RETURNING id",
    )
    .bind(token_hash(&token))
    .bind(user_id)
    .bind(&state)
    .fetch_one(db)
    .await?;
    let session = Session {
        id,
        user_id: Some(user_id),
        state,
        expired: false,
    };
    Ok((session, token))
}

/// Сессия по токену. Живая сессия получает новую отметку активности; просроченная
/// возвращается с `expired = true` и не продлевается.
pub async fn find(db: &PgPool, token: &str) -> Result<Option<Session>, SessionError> {
    let row: Option<(i64, Option<i64>, serde_json::Value, bool)> = sqlx::query_as(
        "SELECT id, user_id, state,
                touched_at < now() - make_interval(hours => $2)
         FROM exchange_sessions WHERE token_hash = $1",
    )
    .bind(token_hash(token))
    .bind(TTL_HOURS as i32)
    .fetch_optional(db)
    .await?;
    let Some((id, user_id, state, expired)) = row else {
        return Ok(None);
    };
    if !expired {
        sqlx::query("UPDATE exchange_sessions SET touched_at = now() WHERE id = $1")
            .bind(id)
            .execute(db)
            .await?;
    }
    Ok(Some(Session {
        id,
        user_id,
        state,
        expired,
    }))
}

pub fn dir(upload_dir: &Path, session: &Session) -> PathBuf {
    upload_dir.join("1c_exchange").join(session.id.to_string())
}

/// Относительный путь внутри каталога сессии; `None` — путь недопустим
/// (пустой, абсолютный, с `..`, `.`, пустыми частями, обратным слэшем, NUL или диском).
pub fn safe_rel_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains(['\0', '\\']) || name.starts_with('/') {
        return None;
    }
    let mut out = PathBuf::new();
    for (i, part) in name.split('/').enumerate() {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        if i == 0 && part.chars().nth(1) == Some(':') {
            return None;
        }
        out.push(part);
    }
    Some(out)
}

/// Дописывает `bytes` к файлу `filename` (для тестов; протокол пишет потоком, см. `append_body`).
#[cfg(test)]
pub async fn append(dir: &Path, filename: &str, bytes: &[u8]) -> Result<(), SessionError> {
    append_body(
        dir,
        filename,
        u64::MAX,
        axum::body::Body::from(bytes.to_vec()),
    )
    .await
}

/// Дописывает тело запроса потоком; сверх `limit` байт - отказ, уже записанная часть
/// этого запроса убирается (файл возвращается к прежнему размеру).
pub async fn append_body(
    dir: &Path,
    filename: &str,
    limit: u64,
    body: axum::body::Body,
) -> Result<(), SessionError> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let rel = safe_rel_path(filename).ok_or_else(|| SessionError::BadPath(filename.into()))?;
    let path = dir.join(rel);
    let io = |e: std::io::Error| SessionError::Io(e.to_string());
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(io)?;
    }
    let before = tokio::fs::metadata(&path).await.map(|m| m.len()).ok();
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .map_err(io)?;
    let mut total: u64 = 0;
    let mut stream = body.into_data_stream();
    let result: Result<(), SessionError> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| SessionError::Io(e.to_string()))?;
            total += chunk.len() as u64;
            if total > limit {
                return Err(SessionError::TooLarge(limit));
            }
            file.write_all(&chunk).await.map_err(io)?;
        }
        file.flush().await.map_err(io)
    }
    .await;
    if result.is_err() {
        drop(file);
        match before {
            Some(len) => {
                if let Ok(f) = tokio::fs::OpenOptions::new().write(true).open(&path).await {
                    let _ = f.set_len(len).await;
                }
            }
            None => {
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
    }
    result
}

/// Распаковывает все `*.zip` из корня каталога сессии в него же и удаляет архивы.
/// Лимиты действуют на все архивы вызова вместе. При ошибке записанные файлы убираются,
/// архивы остаются. Блокирующая: вызывать через `spawn_blocking`.
pub fn unpack_zips(dir: &Path, limits: Limits) -> Result<(), SessionError> {
    let io = |e: std::io::Error| SessionError::Io(e.to_string());
    let mut zips: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(e)),
    };
    zips.sort();
    let mut written: Vec<PathBuf> = Vec::new();
    let mut budget = Budget {
        bytes: limits.max_bytes,
        entries: limits.max_entries,
    };
    for zip in &zips {
        if let Err(e) = unpack_one(zip, dir, &mut budget, &mut written) {
            for path in written {
                let _ = std::fs::remove_file(path);
            }
            // Недогруженный архив не удаляем: следующая часть дописывается к нему
            if let SessionError::Zip(detail) = &e {
                tracing::warn!(zip = %zip.display(), "обмен с 1С: архив не открыт: {detail}");
            }
            return Err(e);
        }
    }
    for zip in &zips {
        std::fs::remove_file(zip).map_err(io)?;
    }
    Ok(())
}

struct Budget {
    bytes: u64,
    entries: usize,
}

fn unpack_one(
    zip_path: &Path,
    dir: &Path,
    budget: &mut Budget,
    written: &mut Vec<PathBuf>,
) -> Result<(), SessionError> {
    use std::io::Read;
    let io = |e: std::io::Error| SessionError::Io(e.to_string());
    let zip_err = |e: zip::result::ZipError| SessionError::Zip(e.to_string());
    let file = std::fs::File::open(zip_path).map_err(io)?;
    let mut archive = zip::ZipArchive::new(file).map_err(zip_err)?;
    budget.entries = budget
        .entries
        .checked_sub(archive.len())
        .ok_or(SessionError::Limit)?;
    // Заявленные размеры проверяем заранее, фактические — при распаковке
    let mut declared: u64 = 0;
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i).map_err(zip_err)?;
        declared = declared.saturating_add(entry.size());
    }
    if declared > budget.bytes {
        return Err(SessionError::Limit);
    }
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(zip_err)?;
        let name = entry.name().to_string();
        let rel = safe_rel_path(name.strip_suffix('/').unwrap_or(&name))
            .ok_or_else(|| SessionError::BadPath(name.clone()))?;
        if entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            return Err(SessionError::BadPath(name));
        }
        let target = dir.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(io)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let mut out = std::fs::File::create(&target).map_err(io)?;
        written.push(target);
        let copied =
            std::io::copy(&mut (&mut entry).take(budget.bytes + 1), &mut out).map_err(io)?;
        if copied > budget.bytes {
            return Err(SessionError::Limit);
        }
        budget.bytes -= copied;
    }
    Ok(())
}

/// Удаляет просроченные сессии и их каталоги.
pub async fn cleanup_expired(db: &PgPool, upload_dir: &Path) -> Result<(), SessionError> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "DELETE FROM exchange_sessions WHERE touched_at < now() - make_interval(hours => $1)
         RETURNING id",
    )
    .bind(TTL_HOURS as i32)
    .fetch_all(db)
    .await?;
    for id in ids {
        let path = upload_dir.join("1c_exchange").join(id.to_string());
        if let Err(e) = tokio::fs::remove_dir_all(&path).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %path.display(), "обмен с 1С: не удалён каталог сессии: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("cms-sess-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
        buf.into_inner()
    }

    #[test]
    fn rel_path_rules() {
        assert!(safe_rel_path("a.xml").is_some());
        assert!(safe_rel_path("import_files/0b/x.jpeg").is_some());
        assert!(safe_rel_path("goods/goods_1_1.xml").is_some());
        for bad in [
            "",
            "../x",
            "a/../../x",
            "/etc/passwd",
            "a\\b",
            "a\0b",
            "a/./b",
            "a//b",
            "C:/x",
        ] {
            assert!(safe_rel_path(bad).is_none(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn append_writes_in_subdirs_and_appends() {
        let d = tmp();
        append(&d, "goods/g.xml", b"ab").await.unwrap();
        append(&d, "goods/g.xml", b"cd").await.unwrap();
        assert_eq!(std::fs::read(d.join("goods/g.xml")).unwrap(), b"abcd");
        assert!(append(&d, "../x", b"1").await.is_err());
        assert!(!d.parent().unwrap().join("x").exists());
    }

    #[test]
    fn unpack_extracts_and_removes_zip() {
        let d = tmp();
        std::fs::write(
            d.join("a.zip"),
            make_zip(&[("x.xml", b"<a/>"), ("import_files/0b/p.jpeg", b"JPG")]),
        )
        .unwrap();
        unpack_zips(&d, Limits::default()).unwrap();
        assert_eq!(std::fs::read(d.join("x.xml")).unwrap(), b"<a/>");
        assert_eq!(
            std::fs::read(d.join("import_files/0b/p.jpeg")).unwrap(),
            b"JPG"
        );
        assert!(!d.join("a.zip").exists());
        // повторный вызов ничего не делает
        unpack_zips(&d, Limits::default()).unwrap();
    }

    #[test]
    fn unpack_rejects_bad_names_and_cleans_up() {
        let d = tmp();
        let inner = d.join("s");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(
            inner.join("a.zip"),
            make_zip(&[("ok.xml", b"1"), ("../evil", b"2")]),
        )
        .unwrap();
        assert!(unpack_zips(&inner, Limits::default()).is_err());
        assert!(!d.join("evil").exists());
        assert!(
            !inner.join("ok.xml").exists(),
            "частично распакованное убрано"
        );
    }

    #[test]
    fn unpack_enforces_limits() {
        let d = tmp();
        let zipped = make_zip(&[("a", &[0u8; 100]), ("b", &[0u8; 100])]);
        std::fs::write(d.join("a.zip"), &zipped).unwrap();
        let small = Limits {
            max_bytes: 150,
            max_entries: 10,
        };
        assert!(matches!(unpack_zips(&d, small), Err(SessionError::Limit)));
        assert!(!d.join("a").exists());
        let few = Limits {
            max_bytes: 1000,
            max_entries: 1,
        };
        assert!(matches!(unpack_zips(&d, few), Err(SessionError::Limit)));
        unpack_zips(
            &d,
            Limits {
                max_bytes: 1000,
                max_entries: 2,
            },
        )
        .unwrap();
        assert!(d.join("b").exists());
    }

    #[test]
    fn unpack_broken_zip_errors() {
        let d = tmp();
        std::fs::write(d.join("a.zip"), b"not a zip at all").unwrap();
        assert!(matches!(
            unpack_zips(&d, Limits::default()),
            Err(SessionError::Zip(_))
        ));
        assert!(d.join("a.zip").exists(), "недогруженный архив остаётся");
    }

    #[sqlx::test]
    async fn start_find_and_cleanup(db: PgPool) {
        let (user_id,): (i64,) = sqlx::query_as(
            "INSERT INTO users (login, password_hash) VALUES ('u', 'x') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let (s, token) = start(&db, user_id, KIND_CATALOG).await.unwrap();
        assert_eq!(token.len(), 64);
        let hash: String =
            sqlx::query_scalar("SELECT token_hash FROM exchange_sessions WHERE id = $1")
                .bind(s.id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_ne!(hash, token);
        let found = find(&db, &token).await.unwrap().unwrap();
        assert_eq!(found.id, s.id);
        assert!(!found.expired);
        assert!(find(&db, "nope").await.unwrap().is_none());

        let up = tmp();
        let sdir = dir(&up, &s);
        std::fs::create_dir_all(&sdir).unwrap();
        sqlx::query("UPDATE exchange_sessions SET touched_at = now() - interval '25 hours'")
            .execute(&db)
            .await
            .unwrap();
        assert!(find(&db, &token).await.unwrap().unwrap().expired);
        cleanup_expired(&db, &up).await.unwrap();
        assert!(find(&db, &token).await.unwrap().is_none());
        assert!(!sdir.exists());
    }
}
