//! Оформление и заказы (аналог модуля `sale`): справочники оформления, форма,
//! проверки, остатки, заказ, письма.

pub mod form;
pub mod guest;
pub mod mail;
pub mod repo;
pub mod stock;
pub mod validate;

use std::collections::HashMap;

use serde::Serialize;
use sqlx::{FromRow, PgPool};

/// Тип плательщика (`b_sale_person_type`).
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct PersonType {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub active: bool,
    pub sort: i32,
}

/// Группа свойств заказа; `block_code` — блок формы для фронта.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct PropertyGroup {
    pub id: i64,
    pub person_type_id: i64,
    pub name: String,
    pub sort: i32,
    pub block_code: String,
}

/// Свойство заказа (`b_sale_order_props`).
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct OrderProperty {
    pub id: i64,
    pub person_type_id: i64,
    pub group_id: Option<i64>,
    pub code: String,
    pub name: String,
    /// `text`, `textarea`, `number`, `select`, `checkbox`, `date`, `file`, `location`, `address`
    pub kind: String,
    pub required: bool,
    /// Служебное — покупателю не показывается.
    pub util: bool,
    pub is_email: bool,
    pub is_phone: bool,
    pub is_payer: bool,
    pub is_profile_name: bool,
    pub is_location: bool,
    pub is_address: bool,
    pub is_zip: bool,
    pub default_value: String,
    pub description: String,
    pub sort: i32,
    pub active: bool,
    /// Несколько значений (файлы).
    pub multiple: bool,
}

/// Вариант свойства-списка.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct PropertyVariant {
    pub id: i64,
    pub property_id: i64,
    pub value: String,
    pub name: String,
    pub sort: i32,
}

/// Служба доставки; `store_ids` — склады самовывоза.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Delivery {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub description: String,
    pub active: bool,
    /// `false` — покупателю не показывается (`ByPublicMode`).
    pub public: bool,
    pub sort: i32,
    pub price: f64,
    pub currency: String,
    pub store_ids: Vec<i64>,
}

/// Платёжная система.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct PaySystem {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub description: String,
    pub active: bool,
    pub sort: i32,
    /// Обработчик онлайн-оплаты: пусто — нет.
    pub handler: String,
    /// Тип для API: `cashless`, `cash`, `document`, `redirect`, `qr`, `other`.
    pub api_type: String,
    /// Кому доступна; пусто — всем.
    pub group_ids: Vec<i64>,
}

/// Статус заказа.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct OrderStatus {
    pub code: String,
    pub name: String,
    pub sort: i32,
    pub description: String,
    /// Письмо `SALE_STATUS_CHANGED_<код>` при смене статуса.
    pub notify: bool,
}

/// Привязки свойства (`b_sale_order_props_relation`); пустой список — без ограничения.
#[derive(Debug, Clone, Default)]
pub struct PropertyRelations {
    pub payment_ids: Vec<i64>,
    pub delivery_ids: Vec<i64>,
}

/// Все справочники оформления; списки — по сортировке и id.
#[derive(Debug, Clone, Default)]
pub struct SaleSettings {
    pub person_types: Vec<PersonType>,
    pub groups: Vec<PropertyGroup>,
    pub properties: Vec<OrderProperty>,
    /// id свойства → варианты.
    pub variants: HashMap<i64, Vec<PropertyVariant>>,
    pub deliveries: Vec<Delivery>,
    pub pay_systems: Vec<PaySystem>,
    pub statuses: Vec<OrderStatus>,
    /// id свойства → привязки к платёжкам и доставкам.
    pub relations: HashMap<i64, PropertyRelations>,
}

/// Начальный статус заказа в Битриксе (`OrderStatus::getInitialStatus`).
const INITIAL_STATUS: &str = "N";

impl SaleSettings {
    /// Участвует ли свойство в заказе с этими платёжкой и доставкой.
    pub fn property_applies(&self, property_id: i64, payment_id: i64, delivery_id: i64) -> bool {
        self.relations.get(&property_id).is_none_or(|r| {
            (r.payment_ids.is_empty() || r.payment_ids.contains(&payment_id))
                && (r.delivery_ids.is_empty() || r.delivery_ids.contains(&delivery_id))
        })
    }

    /// Статус нового заказа: `N`, если он есть, иначе первый по сортировке.
    pub fn default_status(&self) -> Option<&OrderStatus> {
        self.statuses
            .iter()
            .find(|s| s.code == INITIAL_STATUS)
            .or_else(|| self.statuses.iter().min_by_key(|s| s.sort))
    }
}

pub async fn load_settings(db: &PgPool) -> sqlx::Result<SaleSettings> {
    let mut variants: HashMap<i64, Vec<PropertyVariant>> = HashMap::new();
    for v in sqlx::query_as::<_, PropertyVariant>(
        "SELECT id, property_id, value, name, sort FROM order_property_variants ORDER BY sort, id",
    )
    .fetch_all(db)
    .await?
    {
        variants.entry(v.property_id).or_default().push(v);
    }
    let mut relations: HashMap<i64, PropertyRelations> = HashMap::new();
    for (property, kind, entity) in sqlx::query_as::<_, (i64, String, i64)>(
        "SELECT property_id, entity_type, entity_id FROM order_property_relations ORDER BY entity_id",
    )
    .fetch_all(db)
    .await?
    {
        let r = relations.entry(property).or_default();
        if kind == "P" {
            r.payment_ids.push(entity);
        } else {
            r.delivery_ids.push(entity);
        }
    }
    Ok(SaleSettings {
        relations,
        person_types: sqlx::query_as(
            "SELECT id, code, name, active, sort FROM person_types ORDER BY sort, id",
        )
        .fetch_all(db)
        .await?,
        groups: sqlx::query_as(
            "SELECT id, person_type_id, name, sort, block_code FROM order_property_groups ORDER BY sort, id",
        )
        .fetch_all(db)
        .await?,
        properties: sqlx::query_as(
            "SELECT id, person_type_id, group_id, code, name, kind, required, util, is_email, is_phone,
                    is_payer, is_profile_name, is_location, is_address, is_zip, default_value,
                    description, sort, active, multiple
             FROM order_properties ORDER BY sort, id",
        )
        .fetch_all(db)
        .await?,
        variants,
        deliveries: sqlx::query_as(
            "SELECT d.id, d.code, d.name, d.description, d.active, d.public, d.sort,
                    d.price::float8 AS price, d.currency,
                    COALESCE(array_agg(s.store_id ORDER BY s.store_id)
                             FILTER (WHERE s.store_id IS NOT NULL), '{}') AS store_ids
             FROM deliveries d LEFT JOIN delivery_stores s ON s.delivery_id = d.id
             GROUP BY d.id ORDER BY d.sort, d.id",
        )
        .fetch_all(db)
        .await?,
        pay_systems: sqlx::query_as(
            "SELECT id, code, name, description, active, sort, handler, api_type, group_ids
             FROM pay_systems ORDER BY sort, id",
        )
        .fetch_all(db)
        .await?,
        statuses: sqlx::query_as(
            "SELECT code, name, sort, description, notify FROM order_statuses ORDER BY sort, code",
        )
        .fetch_all(db)
        .await?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: &str, sort: i32) -> OrderStatus {
        OrderStatus {
            code: code.into(),
            name: code.into(),
            sort,
            description: String::new(),
            notify: false,
        }
    }

    #[test]
    fn property_applicable_by_relations() {
        let s = SaleSettings {
            relations: HashMap::from([
                (
                    7,
                    PropertyRelations {
                        payment_ids: vec![5],
                        delivery_ids: vec![],
                    },
                ),
                (
                    19,
                    PropertyRelations {
                        payment_ids: vec![],
                        delivery_ids: vec![20],
                    },
                ),
                (
                    2,
                    PropertyRelations {
                        payment_ids: vec![5],
                        delivery_ids: vec![8, 19],
                    },
                ),
            ]),
            ..Default::default()
        };
        assert!(s.property_applies(1, 7, 8)); // без привязок — всегда
        assert!(s.property_applies(7, 5, 8));
        assert!(!s.property_applies(7, 7, 8));
        assert!(s.property_applies(19, 7, 20));
        assert!(!s.property_applies(19, 7, 2));
        assert!(s.property_applies(2, 5, 19));
        assert!(!s.property_applies(2, 5, 2));
    }

    #[test]
    fn default_status_is_first_by_sort() {
        let s = SaleSettings {
            statuses: vec![status("C", 100), status("N", 100), status("F", 200)],
            ..Default::default()
        };
        assert_eq!(s.default_status().map(|st| st.code.as_str()), Some("N"));
        let s = SaleSettings {
            statuses: vec![status("A", 100), status("B", 50)],
            ..Default::default()
        };
        assert_eq!(s.default_status().map(|st| st.code.as_str()), Some("B"));
    }
}
