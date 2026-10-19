//! Скидки и наценки: правила работы с корзиной и купоны (как модуль `sale` Битрикса).

pub mod catalog;
pub mod cond;
pub mod context;
pub mod engine;
pub mod model;
pub mod repo;

/// Часовой пояс скидок — Москва (UTC+3), как время сервера Битрикса: дни недели условий,
/// даты активности в админке и при импорте.
pub fn msk() -> chrono::FixedOffset {
    chrono::FixedOffset::east_opt(3 * 3600).expect("valid offset")
}
