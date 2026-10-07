//! Раздача `/upload/*`: файлы из `UPLOAD_DIR`, уменьшенные копии по
//! `/upload/resize_cache/<ширина>/<путь>` (создаются при первом запросе) и ленивая
//! докачка отсутствующих файлов с `UPLOAD_ORIGIN_URL` (старый сайт) — после
//! переноса из Битрикса не нужно выкачивать весь `upload` заранее.

use std::{
    collections::HashMap,
    path::{Path as FsPath, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};

use crate::{files, state::AppState};

/// Не спрашивать origin повторно о файле, которого там нет.
const MISS_TTL: Duration = Duration::from_secs(600);
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_FETCH_BYTES: usize = 20 * 1024 * 1024;
const MAX_WIDTH: u32 = 4000;

pub fn router() -> Router<AppState> {
    Router::new().route("/{*path}", get(serve))
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not found").into_response()
}

/// Относительный путь без `..`, пустых сегментов и обратных слешей.
fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && path
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
}

fn content_type(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "pdf" => "application/pdf",
        "txt" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn send_file(file: &FsPath, ext: &str) -> Response {
    match tokio::fs::read(file).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, content_type(ext)),
                (header::CACHE_CONTROL, "public, max-age=86400"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => not_found(),
    }
}

async fn serve(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    if !safe_path(&path) {
        return not_found();
    }
    let Some(ext) = files::allowed_extension(&path) else {
        return not_found();
    };
    if let Some(rest) = path.strip_prefix("resize_cache/") {
        let Some((width, original)) = rest.split_once('/') else {
            return not_found();
        };
        return match resized(&state, width, original, &ext).await {
            Some(file) => send_file(&file, &ext).await,
            None => not_found(),
        };
    }
    match ensure_original(&state, &path).await {
        Some(file) => send_file(&file, &ext).await,
        None => not_found(),
    }
}

/// Уменьшенная копия: ширина из белого списка проекта; меньше исходника не растягиваем.
async fn resized(state: &AppState, width: &str, original: &str, ext: &str) -> Option<PathBuf> {
    let width: u32 = width.parse().ok().filter(|w| *w > 0 && *w <= MAX_WIDTH)?;
    let allowed = &state.project.image_widths;
    if !allowed.is_empty() && !allowed.contains(&width) {
        return None;
    }
    let target = state
        .config
        .upload_dir
        .join("resize_cache")
        .join(width.to_string())
        .join(original);
    if target.is_file() {
        return Some(target);
    }
    let source = ensure_original(state, original).await?;
    // Форматы без кодировщика (avif) отдаём как есть
    if !matches!(ext, "jpg" | "jpeg" | "png" | "gif" | "webp") {
        return Some(source);
    }
    let out = target.clone();
    let done = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let img = image::open(&source)?;
        let img = if img.width() > width {
            img.resize(width, u32::MAX, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };
        std::fs::create_dir_all(out.parent().expect("путь с каталогом"))?;
        // Через временный файл: параллельный запрос не увидит недописанный
        let tmp = out.with_extension(format!("tmp{}", rand::random::<u32>()));
        img.save_with_format(&tmp, image::ImageFormat::from_path(&out)?)?;
        std::fs::rename(&tmp, &out)?;
        Ok(())
    })
    .await;
    match done {
        Ok(Ok(())) => Some(target),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, original, "не удалось сделать ресайз");
            None
        }
        Err(_) => None,
    }
}

fn misses() -> &'static Mutex<HashMap<String, Instant>> {
    static MISSES: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    MISSES.get_or_init(Default::default)
}

/// Локальный файл; если его нет — докачка с origin (если он задан).
async fn ensure_original(state: &AppState, path: &str) -> Option<PathBuf> {
    let local = state.config.upload_dir.join(path);
    if local.is_file() {
        return Some(local);
    }
    let origin = state.config.upload_origin_url.as_deref()?;
    {
        let mut misses = misses().lock().expect("mutex");
        misses.retain(|_, at| at.elapsed() < MISS_TTL);
        if misses.contains_key(path) {
            return None;
        }
    }
    let url = format!("{}/upload/{path}", origin.trim_end_matches('/'));
    match fetch(&url).await {
        Ok(Some(bytes)) => {
            let write = async {
                tokio::fs::create_dir_all(local.parent().expect("путь с каталогом")).await?;
                let tmp = local.with_extension(format!("tmp{}", rand::random::<u32>()));
                tokio::fs::write(&tmp, &bytes).await?;
                tokio::fs::rename(&tmp, &local).await
            };
            match write.await {
                Ok(()) => Some(local),
                Err(e) => {
                    tracing::warn!(error = %e, path, "не удалось сохранить файл с origin");
                    None
                }
            }
        }
        Ok(None) => {
            misses()
                .lock()
                .expect("mutex")
                .insert(path.to_string(), Instant::now());
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, url, "origin недоступен");
            None
        }
    }
}

async fn fetch(url: &str) -> anyhow::Result<Option<Vec<u8>>> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .expect("HTTP-клиент")
    });
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    if resp
        .content_length()
        .is_some_and(|l| l as usize > MAX_FETCH_BYTES)
    {
        return Ok(None);
    }
    let bytes = resp.bytes().await?;
    Ok((bytes.len() <= MAX_FETCH_BYTES).then(|| bytes.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert!(safe_path("iblock/97a/x.jpg"));
        assert!(!safe_path("../etc/passwd"));
        assert!(!safe_path("iblock//x.jpg"));
        assert!(!safe_path("a\\b.jpg"));
        assert!(!safe_path(""));
    }
}
