//! Данные для проверки условий и расчёта скидок — без БД (как `orderData` у `Sale\DiscountBase`
//! и данные каталога, которые `Catalog\Discount\DiscountManager::extendOrderData` дописывает
//! в позиции корзины).

use std::collections::HashMap;

use chrono::{DateTime, Datelike, FixedOffset, Utc};
use serde_json::{Map, Value};

use super::model::Coupon;

/// Родительский товар предложения: поля, которые условия товара проверяют и у родителя
/// (`PARENT_*` у Битрикса).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CtxParent {
    pub collection_id: i64,
    pub xml_id: String,
    pub name: String,
    pub code: String,
}

/// Позиция корзины с данными товара.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CtxItem {
    /// id позиции.
    pub key: i64,
    /// Запись коллекции (у предложения — само предложение).
    pub product_id: i64,
    /// Товар предложения.
    pub parent_id: Option<i64>,
    pub collection_id: i64,
    /// Данные родителя; у предложения без загруженного родителя — `None`.
    pub parent: Option<CtxParent>,
    /// Разделы записи и всех их предков; у предложения — и родителя.
    pub section_ids: Vec<i64>,
    pub xml_id: String,
    pub name: String,
    pub code: String,
    /// `field_values` записи; у предложения — объединение с родителем, как Битрикс.
    pub fields: Map<String, Value>,
    /// id поля → код: поля коллекции записи (у предложения — и коллекции родителя).
    pub field_ids: HashMap<i64, String>,
    pub quantity: f64,
    /// Цена единицы до скидок.
    pub base_price: f64,
    pub custom_price: bool,
    /// Вес единицы.
    pub weight: f64,
    /// Остаток на складе.
    pub stock_quantity: f64,
}

/// Данные заказа; в корзине до оформления — пусто.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OrderCtx {
    pub person_type_id: Option<i64>,
    pub pay_system_id: Option<i64>,
    pub delivery_id: Option<i64>,
    /// Цена доставки до скидок.
    pub delivery_price: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Context {
    pub items: Vec<CtxItem>,
    pub user_id: Option<i64>,
    pub group_ids: Vec<i64>,
    pub coupons: Vec<Coupon>,
    pub order: OrderCtx,
    pub now: DateTime<Utc>,
}

impl Context {
    /// День недели по МСК (как время bxapi): 1 — понедельник … 7 — воскресенье (`date('N')`).
    pub fn day_of_week(&self) -> u32 {
        let msk = FixedOffset::east_opt(3 * 3600).expect("valid offset");
        self.now.with_timezone(&msk).weekday().number_from_monday()
    }
}
