//! Корзина покупателя (аналог `b_sale_fuser` + `b_sale_basket`): позиции, расчёт
//! снимка для API и хранилище.

pub mod repo;
pub mod snapshot;

use serde_json::{Map, Value};

/// Позиция корзины.
#[derive(Debug, Clone, PartialEq)]
pub struct CartItem {
    pub id: i64,
    pub product_id: i64,
    pub store_id: Option<i64>,
    pub quantity: f64,
    /// Название на момент добавления (если товар потом удалят из выдачи).
    pub name: String,
}

/// Склад для снимка: поля карточки и UF уже выбраны по настройкам проекта.
#[derive(Debug, Clone)]
pub struct StoreInfo {
    pub id: i64,
    pub name: String,
    pub active: bool,
    pub fields: Map<String, Value>,
    /// Фото склада (`/upload/...`) — в пунктах самовывоза формы заказа, не в корзине.
    pub image: Option<String>,
}

/// Отображение товара в позиции: берётся из сериализации элемента.
#[derive(Debug, Clone, Default)]
pub struct ProductView {
    pub name: String,
    pub slug: String,
    pub article: Value,
    pub image: Value,
    pub properties: Map<String, Value>,
}

/// Настройки расчёта снимка (из `CartConfig` проекта).
#[derive(Debug, Clone)]
pub struct SnapshotConfig {
    /// `itemsCount` — число позиций, а не штук.
    pub items_count_positions: bool,
    /// Оформлять можно только с выбранным складом у каждой позиции.
    pub required_store: bool,
    /// Валюта пустой корзины.
    pub currency: String,
}
