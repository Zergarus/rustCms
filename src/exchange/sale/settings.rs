//! Настройки обмена заказами с 1С (`options`, модуль `exchange`, ключи `sale_*`) и
//! соответствие реквизитов (`exchange_order_map`, аналог `b_sale_bizval` с потребителем «1C»).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::exchange::settings::MODULE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaleSettings {
    /// Выгружать только оплаченные (`1C_EXPORT_PAYED_ORDERS`).
    pub export_paid_only: bool,
    /// Выгружать только с разрешённой доставкой (`1C_EXPORT_ALLOW_DELIVERY_ORDERS`).
    pub export_allow_delivery_only: bool,
    /// Выгружать начиная со статуса (`1C_EXPORT_FINAL_ORDERS`); `None` — все.
    pub export_from_status: Option<String>,
    /// Замена валюты в выгрузке (`1C_REPLACE_CURRENCY`).
    pub replace_currency: String,
    /// Префикс номера заказа (`1C_SALE_ACCOUNT_NUMBER_SHOP_PREFIX`).
    pub number_prefix: String,
    /// Менять статус заказа по данным 1С (`1C_CHANGE_STATUS_FROM_1C`).
    pub change_status_from_1c: bool,
    /// Статус при отгрузке (`1C_FINAL_STATUS_ON_DELIVERY`).
    pub final_status_on_delivery: Option<String>,
    /// Обновлять количество в корзине (`1C_IMPORT_UPDATE_BASKET_QUANTITY`).
    pub update_basket_quantity: bool,
    /// Создавать новые заказы (`1C_IMPORT_NEW_ORDERS`).
    pub import_new_orders: bool,
    /// Создавать новые оплаты (`1C_IMPORT_NEW_PAYMENT`).
    pub import_new_payments: bool,
    /// Создавать новые отгрузки (`1C_IMPORT_NEW_SHIPMENT`).
    pub import_new_shipments: bool,
    /// Отгрузка для нового заказа (`1C_IMPORT_NEW_ORDER_NEW_SHIPMENT`).
    pub new_order_new_shipment: bool,
    /// Платёжная система по умолчанию: наличные (`1C_IMPORT_DEFAULT_PS`).
    pub default_ps_cash: Option<i64>,
    /// … безналичные (`1C_IMPORT_DEFAULT_PS_B`).
    pub default_ps_cashless: Option<i64>,
    /// … эквайринг (`1C_IMPORT_DEFAULT_PS_A`).
    pub default_ps_acquiring: Option<i64>,
    /// … для оплаченного заказа (`1C_IMPORT_DEFAULT_PS_ORDER_PAID`).
    pub default_ps_order_paid: Option<i64>,
    /// Служба доставки по умолчанию (`1C_IMPORT_DEFAULT_SHIPMENT_SERVICE`).
    pub default_delivery: Option<i64>,
    /// Время последней подтверждённой выгрузки (`last_export_time_committed_*`).
    pub last_export_committed: Option<DateTime<Utc>>,
}

/// Умолчания — как в коде Битрикса.
impl Default for SaleSettings {
    fn default() -> Self {
        SaleSettings {
            export_paid_only: false,
            export_allow_delivery_only: false,
            export_from_status: None,
            replace_currency: String::new(),
            number_prefix: String::new(),
            change_status_from_1c: false,
            final_status_on_delivery: None,
            update_basket_quantity: true,
            import_new_orders: true,
            import_new_payments: true,
            import_new_shipments: true,
            new_order_new_shipment: true,
            default_ps_cash: None,
            default_ps_cashless: None,
            default_ps_acquiring: None,
            default_ps_order_paid: None,
            default_delivery: None,
            last_export_committed: None,
        }
    }
}

const LAST_EXPORT: &str = "sale_last_export_committed";

fn flag(v: bool) -> String {
    if v { "Y" } else { "N" }.into()
}

async fn put(db: &PgPool, values: &[(&str, String)]) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    for (name, value) in values {
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ($1, $2, $3)
             ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(MODULE)
        .bind(name)
        .bind(value)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

impl SaleSettings {
    pub async fn load(db: &PgPool) -> sqlx::Result<SaleSettings> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, value FROM options WHERE module = $1 AND name LIKE 'sale\\_%'",
        )
        .bind(MODULE)
        .fetch_all(db)
        .await?;
        let map: HashMap<String, String> = rows.into_iter().collect();
        let d = SaleSettings::default();
        let get = |name: &str| map.get(name).map(|v| v.trim()).unwrap_or("");
        let yn = |name: &str, default: bool| match get(name) {
            "Y" => true,
            "N" => false,
            _ => default,
        };
        let text = |name: &str| map.get(name).cloned().unwrap_or_default();
        let code = |name: &str| Some(get(name).to_string()).filter(|v| !v.is_empty());
        let id = |name: &str| get(name).parse::<i64>().ok().filter(|id| *id > 0);
        Ok(SaleSettings {
            export_paid_only: yn("sale_export_paid", d.export_paid_only),
            export_allow_delivery_only: yn(
                "sale_export_allow_delivery",
                d.export_allow_delivery_only,
            ),
            export_from_status: code("sale_export_from_status"),
            replace_currency: text("sale_replace_currency"),
            number_prefix: text("sale_number_prefix"),
            change_status_from_1c: yn("sale_change_status", d.change_status_from_1c),
            final_status_on_delivery: code("sale_final_status_on_delivery"),
            update_basket_quantity: yn("sale_update_basket_quantity", d.update_basket_quantity),
            import_new_orders: yn("sale_import_new_orders", d.import_new_orders),
            import_new_payments: yn("sale_import_new_payments", d.import_new_payments),
            import_new_shipments: yn("sale_import_new_shipments", d.import_new_shipments),
            new_order_new_shipment: yn("sale_new_order_new_shipment", d.new_order_new_shipment),
            default_ps_cash: id("sale_default_ps_cash"),
            default_ps_cashless: id("sale_default_ps_cashless"),
            default_ps_acquiring: id("sale_default_ps_acquiring"),
            default_ps_order_paid: id("sale_default_ps_order_paid"),
            default_delivery: id("sale_default_delivery"),
            last_export_committed: DateTime::parse_from_rfc3339(get(LAST_EXPORT))
                .ok()
                .map(|t| t.with_timezone(&Utc)),
        })
    }

    pub async fn save(&self, db: &PgPool) -> sqlx::Result<()> {
        let id = |v: Option<i64>| v.map(|id| id.to_string()).unwrap_or_default();
        put(
            db,
            &[
                ("sale_export_paid", flag(self.export_paid_only)),
                (
                    "sale_export_allow_delivery",
                    flag(self.export_allow_delivery_only),
                ),
                (
                    "sale_export_from_status",
                    self.export_from_status.clone().unwrap_or_default(),
                ),
                ("sale_replace_currency", self.replace_currency.clone()),
                ("sale_number_prefix", self.number_prefix.clone()),
                ("sale_change_status", flag(self.change_status_from_1c)),
                (
                    "sale_final_status_on_delivery",
                    self.final_status_on_delivery.clone().unwrap_or_default(),
                ),
                (
                    "sale_update_basket_quantity",
                    flag(self.update_basket_quantity),
                ),
                ("sale_import_new_orders", flag(self.import_new_orders)),
                ("sale_import_new_payments", flag(self.import_new_payments)),
                ("sale_import_new_shipments", flag(self.import_new_shipments)),
                (
                    "sale_new_order_new_shipment",
                    flag(self.new_order_new_shipment),
                ),
                ("sale_default_ps_cash", id(self.default_ps_cash)),
                ("sale_default_ps_cashless", id(self.default_ps_cashless)),
                ("sale_default_ps_acquiring", id(self.default_ps_acquiring)),
                ("sale_default_ps_order_paid", id(self.default_ps_order_paid)),
                ("sale_default_delivery", id(self.default_delivery)),
                (
                    LAST_EXPORT,
                    self.last_export_committed
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_default(),
                ),
            ],
        )
        .await
    }
}

/// Подтверждает выгрузку (`success` от 1С): следующая выгрузит заказы, изменённые с `at`.
pub async fn commit_export(db: &PgPool, at: DateTime<Utc>) -> sqlx::Result<()> {
    put(db, &[(LAST_EXPORT, at.to_rfc3339())]).await
}

/// Домен типа плательщика (`person_types.domain`, `b_sale_bizval_persondomain`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    /// Физлицо, `I`.
    Individual,
    /// Юрлицо, `E`.
    Entity,
}

impl Domain {
    pub fn code(self) -> &'static str {
        match self {
            Domain::Individual => "I",
            Domain::Entity => "E",
        }
    }

    /// Неизвестный код — физлицо (умолчание столбца).
    pub fn from_code(code: &str) -> Domain {
        match code.trim() {
            "E" => Domain::Entity,
            _ => Domain::Individual,
        }
    }
}

/// Коды соответствия контрагента: (код Битрикса, ключ поля контрагента, домен) —
/// `CSaleExport::$systemCodes` из `general/export.php`.
pub const CONTRAGENT_CODES: &[(&str, &str, Domain)] = &[
    ("BUYER_PERSON_NAME", "FULL_NAME", Domain::Individual),
    ("BUYER_PERSON_NAME_FIRST", "NAME", Domain::Individual),
    (
        "BUYER_PERSON_NAME_SECOND",
        "SECOND_NAME",
        Domain::Individual,
    ),
    ("BUYER_PERSON_NAME_LAST", "SURNAME", Domain::Individual),
    ("BUYER_PERSON_NAME_AGENT", "AGENT_NAME", Domain::Individual),
    (
        "BUYER_PERSON_NAME_CONTACT",
        "CONTACT_PERSON",
        Domain::Individual,
    ),
    ("BUYER_PERSON_BIRTHDAY", "BIRTHDAY", Domain::Individual),
    ("BUYER_PERSON_GENDER", "MALE", Domain::Individual),
    ("BUYER_PERSON_INN", "INN", Domain::Individual),
    ("BUYER_PERSON_KPP", "KPP", Domain::Individual),
    ("BUYER_PERSON_ADDRESS", "ADDRESS_FULL", Domain::Individual),
    ("BUYER_PERSON_ZIP", "INDEX", Domain::Individual),
    ("BUYER_PERSON_COUNTRY", "COUNTRY", Domain::Individual),
    ("BUYER_PERSON_REGION", "REGION", Domain::Individual),
    ("BUYER_PERSON_STATE", "STATE", Domain::Individual),
    ("BUYER_PERSON_TOWN", "TOWN", Domain::Individual),
    ("BUYER_PERSON_CITY", "CITY", Domain::Individual),
    ("BUYER_PERSON_STREET", "STREET", Domain::Individual),
    ("BUYER_PERSON_HOUSING", "BUILDING", Domain::Individual),
    ("BUYER_PERSON_BUILDING", "HOUSE", Domain::Individual),
    ("BUYER_PERSON_APARTMENT", "FLAT", Domain::Individual),
    ("BUYER_PERSON_PHONE", "PHONE", Domain::Individual),
    ("BUYER_PERSON_EMAIL", "EMAIL", Domain::Individual),
    (
        "BUYER_PERSON_F_ADDRESS_FULL",
        "F_ADDRESS_FULL",
        Domain::Individual,
    ),
    ("BUYER_PERSON_F_INDEX", "F_INDEX", Domain::Individual),
    ("BUYER_PERSON_F_COUNTRY", "F_COUNTRY", Domain::Individual),
    ("BUYER_PERSON_F_REGION", "F_REGION", Domain::Individual),
    ("BUYER_PERSON_F_STATE", "F_STATE", Domain::Individual),
    ("BUYER_PERSON_F_TOWN", "F_TOWN", Domain::Individual),
    ("BUYER_PERSON_F_CITY", "F_CITY", Domain::Individual),
    ("BUYER_PERSON_F_STREET", "F_STREET", Domain::Individual),
    ("BUYER_PERSON_F_BUILDING", "F_BUILDING", Domain::Individual),
    ("BUYER_PERSON_F_HOUSE", "F_HOUSE", Domain::Individual),
    ("BUYER_PERSON_F_FLAT", "F_FLAT", Domain::Individual),
    ("BUYER_COMPANY_NAME", "FULL_NAME", Domain::Entity),
    ("BUYER_COMPANY_NAME_AGENT", "AGENT_NAME", Domain::Entity),
    (
        "BUYER_COMPANY_NAME_CONTACT",
        "CONTACT_PERSON",
        Domain::Entity,
    ),
    ("BUYER_COMPANY_INN", "INN", Domain::Entity),
    ("BUYER_COMPANY_KPP", "KPP", Domain::Entity),
    ("BUYER_COMPANY_ADDRESS", "ADDRESS_FULL", Domain::Entity),
    ("BUYER_COMPANY_ZIP", "INDEX", Domain::Entity),
    ("BUYER_COMPANY_COUNTRY", "COUNTRY", Domain::Entity),
    ("BUYER_COMPANY_REGION", "REGION", Domain::Entity),
    ("BUYER_COMPANY_STATE", "STATE", Domain::Entity),
    ("BUYER_COMPANY_TOWN", "TOWN", Domain::Entity),
    ("BUYER_COMPANY_CITY", "CITY", Domain::Entity),
    ("BUYER_COMPANY_STREET", "STREET", Domain::Entity),
    ("BUYER_COMPANY_HOUSING", "BUILDING", Domain::Entity),
    ("BUYER_COMPANY_BUILDING", "HOUSE", Domain::Entity),
    ("BUYER_COMPANY_APARTMENT", "FLAT", Domain::Entity),
    ("BUYER_COMPANY_PHONE", "PHONE", Domain::Entity),
    ("BUYER_COMPANY_EMAIL", "EMAIL", Domain::Entity),
    ("BUYER_COMPANY_EGRPO", "EGRPO", Domain::Entity),
    ("BUYER_COMPANY_OKVED", "OKVED", Domain::Entity),
    ("BUYER_COMPANY_OKDP", "OKDP", Domain::Entity),
    ("BUYER_COMPANY_OKOPF", "OKOPF", Domain::Entity),
    ("BUYER_COMPANY_OKFC", "OKFC", Domain::Entity),
    ("BUYER_COMPANY_OKPO", "OKPO", Domain::Entity),
    (
        "BUYER_COMPANY_BANK_ACCOUNT",
        "ACCOUNT_NUMBER",
        Domain::Entity,
    ),
    ("BUYER_COMPANY_BANK_NAME", "B_NAME", Domain::Entity),
    ("BUYER_COMPANY_BANK_BIK", "B_BIK", Domain::Entity),
    (
        "BUYER_COMPANY_BANK_ADDRESS_FULL",
        "B_ADDRESS_FULL",
        Domain::Entity,
    ),
    ("BUYER_COMPANY_BANK_INDEX", "B_INDEX", Domain::Entity),
    ("BUYER_COMPANY_BANK_COUNTRY", "B_COUNTRY", Domain::Entity),
    ("BUYER_COMPANY_BANK_REGION", "B_REGION", Domain::Entity),
    ("BUYER_COMPANY_BANK_STATE", "B_STATE", Domain::Entity),
    ("BUYER_COMPANY_BANK_TOWN", "B_TOWN", Domain::Entity),
    ("BUYER_COMPANY_BANK_CITY", "B_CITY", Domain::Entity),
    ("BUYER_COMPANY_BANK_STREET", "B_STREET", Domain::Entity),
    ("BUYER_COMPANY_BANK_BUILDING", "B_BUILDING", Domain::Entity),
    ("BUYER_COMPANY_BANK_HOUSE", "B_HOUSE", Domain::Entity),
    (
        "BUYER_COMPANY_F_ADDRESS_FULL",
        "F_ADDRESS_FULL",
        Domain::Entity,
    ),
    ("BUYER_COMPANY_F_INDEX", "F_INDEX", Domain::Entity),
    ("BUYER_COMPANY_F_COUNTRY", "F_COUNTRY", Domain::Entity),
    ("BUYER_COMPANY_F_REGION", "F_REGION", Domain::Entity),
    ("BUYER_COMPANY_F_STATE", "F_STATE", Domain::Entity),
    ("BUYER_COMPANY_F_TOWN", "F_TOWN", Domain::Entity),
    ("BUYER_COMPANY_F_CITY", "F_CITY", Domain::Entity),
    ("BUYER_COMPANY_F_STREET", "F_STREET", Domain::Entity),
    ("BUYER_COMPANY_F_BUILDING", "F_BUILDING", Domain::Entity),
    ("BUYER_COMPANY_F_HOUSE", "F_HOUSE", Domain::Entity),
    ("BUYER_COMPANY_F_FLAT", "F_FLAT", Domain::Entity),
];

/// Поля пользователя для источника [`MapSource::User`].
pub const USER_FIELDS: &[&str] = &[
    "email",
    "login",
    "name",
    "last_name",
    "second_name",
    "phone",
];

/// Поля заказа для источника [`MapSource::Order`].
pub const ORDER_FIELDS: &[&str] = &["id", "account_number", "user_comment", "price"];

/// Откуда берётся значение реквизита.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapSource {
    /// Свойство заказа, `value` — его id.
    Property,
    /// Поле пользователя из [`USER_FIELDS`].
    User,
    /// Поле заказа из [`ORDER_FIELDS`].
    Order,
    /// Константа.
    Value,
}

impl MapSource {
    pub fn code(self) -> &'static str {
        match self {
            MapSource::Property => "property",
            MapSource::User => "user",
            MapSource::Order => "order",
            MapSource::Value => "value",
        }
    }

    pub fn from_code(code: &str) -> Option<MapSource> {
        match code {
            "property" => Some(MapSource::Property),
            "user" => Some(MapSource::User),
            "order" => Some(MapSource::Order),
            "value" => Some(MapSource::Value),
            _ => None,
        }
    }
}

/// Строка соответствия реквизитов; `person_type_id` 0 — для всех типов плательщика.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapEntry {
    pub person_type_id: i64,
    /// `BUYER_PERSON_*` / `BUYER_COMPANY_*` или `1C_REKV_<n>`.
    pub code: String,
    /// Название произвольного реквизита `1C_REKV_*`.
    pub name: String,
    pub source: MapSource,
    /// [`MapSource::Property`]: id свойства заказа (`"7"`) или часть свойства-местоположения
    /// `"<id>_CITY"` / `"<id>_COUNTRY"` / `"<id>_REGION"` (как `PROVIDER_VALUE` у Битрикса);
    /// [`MapSource::User`] / [`MapSource::Order`]: имя поля из [`USER_FIELDS`] / [`ORDER_FIELDS`];
    /// [`MapSource::Value`]: константа.
    pub value: String,
}

pub async fn load_map(db: &PgPool) -> sqlx::Result<Vec<MapEntry>> {
    let rows: Vec<(i64, String, String, String, String)> = sqlx::query_as(
        "SELECT person_type_id, code, name, source, value FROM exchange_order_map
         ORDER BY person_type_id, code",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(person_type_id, code, name, source, value)| {
            Some(MapEntry {
                person_type_id,
                code,
                name,
                source: MapSource::from_code(&source)?,
                value,
            })
        })
        .collect())
}

/// Заменяет соответствия типа плательщика (`person_type_id` записей не учитывается).
pub async fn save_map(db: &PgPool, person_type_id: i64, entries: &[MapEntry]) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM exchange_order_map WHERE person_type_id = $1")
        .bind(person_type_id)
        .execute(&mut *tx)
        .await?;
    for e in entries {
        sqlx::query(
            "INSERT INTO exchange_order_map (person_type_id, code, name, source, value)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (person_type_id, code) DO UPDATE SET name = EXCLUDED.name,
                 source = EXCLUDED.source, value = EXCLUDED.value",
        )
        .bind(person_type_id)
        .bind(&e.code)
        .bind(&e.name)
        .bind(e.source.code())
        .bind(&e.value)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use sqlx::PgPool;

    use super::*;

    #[sqlx::test]
    async fn sale_settings_defaults_and_roundtrip(db: PgPool) {
        let s = SaleSettings::load(&db).await.unwrap();
        assert_eq!(s, SaleSettings::default());
        assert!(!s.export_paid_only);
        assert!(!s.export_allow_delivery_only);
        assert_eq!(s.export_from_status, None);
        assert_eq!(s.replace_currency, "");
        assert_eq!(s.number_prefix, "");
        assert!(!s.change_status_from_1c);
        assert_eq!(s.final_status_on_delivery, None);
        assert!(s.update_basket_quantity);
        assert!(s.import_new_orders);
        assert!(s.import_new_payments);
        assert!(s.import_new_shipments);
        assert!(s.new_order_new_shipment);
        assert_eq!(
            (
                s.default_ps_cash,
                s.default_ps_cashless,
                s.default_ps_acquiring,
                s.default_ps_order_paid,
                s.default_delivery
            ),
            (None, None, None, None, None)
        );
        assert_eq!(s.last_export_committed, None);

        let at = Utc.with_ymd_and_hms(2026, 5, 13, 12, 59, 22).unwrap();
        let changed = SaleSettings {
            export_paid_only: true,
            export_allow_delivery_only: true,
            export_from_status: Some("P".into()),
            replace_currency: "руб.".into(),
            number_prefix: "TR-".into(),
            change_status_from_1c: true,
            final_status_on_delivery: Some("F".into()),
            update_basket_quantity: false,
            import_new_orders: false,
            import_new_payments: false,
            import_new_shipments: false,
            new_order_new_shipment: false,
            default_ps_cash: Some(1),
            default_ps_cashless: Some(2),
            default_ps_acquiring: Some(3),
            default_ps_order_paid: Some(4),
            default_delivery: Some(5),
            last_export_committed: Some(at),
        };
        changed.save(&db).await.unwrap();
        assert_eq!(SaleSettings::load(&db).await.unwrap(), changed);

        // Сброс необязательных значений очищает их
        SaleSettings::default().save(&db).await.unwrap();
        assert_eq!(
            SaleSettings::load(&db).await.unwrap(),
            SaleSettings::default()
        );

        // Метка выгрузки пишется отдельно и не трогает остальное
        let later = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        commit_export(&db, later).await.unwrap();
        let s = SaleSettings::load(&db).await.unwrap();
        assert_eq!(s.last_export_committed, Some(later));
        assert!(s.import_new_orders);
        let stored: String = sqlx::query_scalar(
            "SELECT value FROM options WHERE module = 'exchange' AND name = 'sale_last_export_committed'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(stored, "2026-06-01T00:00:00+00:00");

        // Мусор в настройках — значения по умолчанию
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES
                ('exchange', 'sale_default_ps_cash', '0'),
                ('exchange', 'sale_import_new_orders', ''),
                ('exchange', 'sale_last_export_committed', 'вчера')
             ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
        )
        .execute(&db)
        .await
        .unwrap();
        let s = SaleSettings::load(&db).await.unwrap();
        assert_eq!(s.default_ps_cash, None);
        assert!(s.import_new_orders);
        assert_eq!(s.last_export_committed, None);
    }

    fn entry(person_type_id: i64, code: &str, source: MapSource, value: &str) -> MapEntry {
        MapEntry {
            person_type_id,
            code: code.into(),
            name: String::new(),
            source,
            value: value.into(),
        }
    }

    #[sqlx::test]
    async fn order_map_roundtrip(db: PgPool) {
        let ids: Vec<i64> = sqlx::query_scalar(
            "INSERT INTO person_types (name) VALUES ('Физлицо'), ('Юрлицо') RETURNING id",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        let (person, company) = (ids[0], ids[1]);
        assert!(load_map(&db).await.unwrap().is_empty());

        let person_rows = vec![
            entry(person, "BUYER_PERSON_EMAIL", MapSource::User, "email"),
            entry(person, "BUYER_PERSON_NAME", MapSource::Property, "7"),
            MapEntry {
                name: "Канал продаж".into(),
                ..entry(person, "1C_REKV_1", MapSource::Value, "Сайт")
            },
        ];
        let company_rows = vec![
            entry(company, "BUYER_COMPANY_INN", MapSource::Property, "9"),
            entry(company, "1C_REKV_1", MapSource::Order, "account_number"),
        ];
        save_map(&db, person, &person_rows).await.unwrap();
        save_map(&db, company, &company_rows).await.unwrap();
        // Общие для всех типов плательщика — тип 0
        save_map(
            &db,
            0,
            &[entry(0, "BUYER_PERSON_PHONE", MapSource::User, "phone")],
        )
        .await
        .unwrap();

        let all = load_map(&db).await.unwrap();
        assert_eq!(all.len(), 6);
        let of = |pt: i64, all: &[MapEntry]| -> Vec<MapEntry> {
            let mut v: Vec<MapEntry> = all
                .iter()
                .filter(|e| e.person_type_id == pt)
                .cloned()
                .collect();
            v.sort_by(|a, b| a.code.cmp(&b.code));
            v
        };
        let mut expected = person_rows.clone();
        expected.sort_by(|a, b| a.code.cmp(&b.code));
        assert_eq!(of(person, &all), expected);
        assert_eq!(of(0, &all).len(), 1);

        // Замена строк одного типа не трогает другой
        let replaced = vec![entry(
            company,
            "BUYER_COMPANY_KPP",
            MapSource::Value,
            "770101001",
        )];
        save_map(&db, company, &replaced).await.unwrap();
        let all = load_map(&db).await.unwrap();
        assert_eq!(of(company, &all), replaced);
        assert_eq!(of(person, &all), expected);

        // Тип плательщика из параметра, а не из записи
        save_map(
            &db,
            person,
            &[entry(company, "BUYER_PERSON_INN", MapSource::Property, "3")],
        )
        .await
        .unwrap();
        let all = load_map(&db).await.unwrap();
        assert_eq!(of(person, &all).len(), 1);
        assert_eq!(of(person, &all)[0].person_type_id, person);

        // Удаление типа плательщика уносит его соответствия
        sqlx::query("DELETE FROM person_types WHERE id = $1")
            .bind(person)
            .execute(&db)
            .await
            .unwrap();
        assert!(of(person, &load_map(&db).await.unwrap()).is_empty());
    }

    #[test]
    fn codes_and_sources() {
        for s in [
            MapSource::Property,
            MapSource::User,
            MapSource::Order,
            MapSource::Value,
        ] {
            assert_eq!(MapSource::from_code(s.code()), Some(s));
        }
        assert_eq!(MapSource::Property.code(), "property");
        assert_eq!(MapSource::from_code("input"), None);
        assert_eq!(Domain::from_code("E"), Domain::Entity);
        assert_eq!(Domain::from_code("I"), Domain::Individual);
        assert_eq!(Domain::Entity.code(), "E");

        let find = |code: &str| CONTRAGENT_CODES.iter().find(|(c, _, _)| *c == code);
        assert_eq!(
            find("BUYER_PERSON_NAME"),
            Some(&("BUYER_PERSON_NAME", "FULL_NAME", Domain::Individual))
        );
        assert_eq!(
            find("BUYER_COMPANY_BANK_ACCOUNT"),
            Some(&(
                "BUYER_COMPANY_BANK_ACCOUNT",
                "ACCOUNT_NUMBER",
                Domain::Entity
            ))
        );
        assert_eq!(
            CONTRAGENT_CODES
                .iter()
                .filter(|c| c.2 == Domain::Individual)
                .count(),
            34
        );
        assert_eq!(
            CONTRAGENT_CODES
                .iter()
                .filter(|c| c.2 == Domain::Entity)
                .count(),
            48
        );
        assert!(USER_FIELDS.contains(&"second_name"));
        assert!(ORDER_FIELDS.contains(&"user_comment"));
    }
}
