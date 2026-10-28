//! Полоса открытых вкладок (заготовка: логика появится в следующей задаче).

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Tab {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub kind: String,
    pub active: bool,
}
