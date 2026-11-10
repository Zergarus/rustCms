//! Ограничитель попыток: счётчики в памяти процесса.
//!
//! Общий для авторизации и публичных форм. Ключ — произвольная строка (эндпоинт,
//! IP, логин); карта ограничена по размеру, просроченные записи вычищаются.
//! При нескольких инстансах бюджеты независимы — общий счётчик в техдолге.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

const WINDOW_SECS: u64 = 15 * 60;
const CAP: usize = 1024;

static ATTEMPTS: Mutex<Option<HashMap<String, (u32, Instant)>>> = Mutex::new(None);

/// Считает попытку. `limit` — сколько попыток разрешено за окно; `false` — лимит исчерпан.
pub fn check(key: &str, limit: u32) -> bool {
    let mut guard = ATTEMPTS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    if map.len() >= CAP {
        map.retain(|_, (_, at)| now.duration_since(*at) <= Duration::from_secs(WINDOW_SECS));
        if map.len() >= CAP {
            map.clear();
        }
    }
    let entry = map.entry(key.to_string()).or_insert((0, now));
    if now.duration_since(entry.1) > Duration::from_secs(WINDOW_SECS) {
        *entry = (0, now);
    }
    entry.0 += 1;
    entry.0 <= limit
}

/// Успешная операция не расходует бюджет.
pub fn forgive(key: &str) {
    let mut guard = ATTEMPTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = guard.as_mut() {
        map.remove(key);
    }
}
