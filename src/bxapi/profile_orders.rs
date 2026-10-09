//! Заказы личного кабинета (`/profile/orders`, `OrderHistoryReader` модуля bxapi).

use std::collections::{HashMap, HashSet};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
};
use axum_extra::extract::cookie::CookieJar;
use chrono::{DateTime, FixedOffset, Local, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::FromRow;

use super::{BxError, BxResult, auth::require_user, cart::product_views, profile::camel, success};
use crate::{
    bxapi::project::{CartConfig, ProfileConfig},
    cart::ProductView,
    files::{self, FileRecord},
    sale::{
        self, Delivery, OrderProperty, PaySystem,
        history::{self, UserOrder},
        repo::file_ids,
    },
    state::AppState,
};

/// Ключ свойства заказа: код в camelCase (`SDEK_TRACKING_URL` → `sdekTrackingUrl`),
/// без кода — `prop<ID>`.
pub(super) fn camel_key(code: &str, id: i64) -> String {
    let key = camel(code);
    if key.is_empty() {
        format!("prop{id}")
    } else {
        key
    }
}

/// «1 товар», «4 товара», «11 товаров».
pub(super) fn items_count_label(n: i64) -> String {
    let n100 = n.abs() % 100;
    let n10 = n.abs() % 10;
    let word = if (11..=14).contains(&n100) {
        "товаров"
    } else if n10 == 1 {
        "товар"
    } else if (2..=4).contains(&n10) {
        "товара"
    } else {
        "товаров"
    };
    format!("{n} {word}")
}

/// Число товаров: `positions` — позиций, иначе округлённая сумма количеств.
pub(super) fn items_count(quantities: &[f64], positions: bool) -> i64 {
    if positions {
        quantities.len() as i64
    } else {
        quantities.iter().sum::<f64>().round() as i64
    }
}

/// Дата по формату PHP `date()`: токены `d m Y H i s`, остальное — как есть.
pub(super) fn php_date(dt: &DateTime<FixedOffset>, fmt: &str) -> String {
    let mut out = String::new();
    for c in fmt.chars() {
        let part = match c {
            'd' => "%d",
            'm' => "%m",
            'Y' => "%Y",
            'H' => "%H",
            'i' => "%M",
            's' => "%S",
            _ => {
                out.push(c);
                continue;
            }
        };
        out.push_str(&dt.format(part).to_string());
    }
    out
}

fn file_json(f: &FileRecord) -> Value {
    json!({
        "ID": f.id,
        "SRC": f.url(),
        "ORIGINAL_NAME": f.original_name,
        "FILE_NAME": f.path.rsplit('/').next().unwrap_or(&f.path),
        "CONTENT_TYPE": f.content_type,
        "FILE_SIZE": f.size,
    })
}

/// Значение файлового свойства как у Битрикса: объект файла (одиночное, пусто — `""`)
/// или массив объектов (множественное). Пропавшие файлы пропускаются.
pub(super) fn file_value(raw: &str, multiple: bool, files: &HashMap<i64, FileRecord>) -> Value {
    let mut found = file_ids(raw)
        .into_iter()
        .filter_map(|id| files.get(&id))
        .map(file_json);
    if multiple {
        Value::Array(found.collect())
    } else {
        found.next().unwrap_or_else(|| Value::from(""))
    }
}

/// Лимит максимум заказов на странице.
const MAX_LIMIT: i64 = 200;

/// `limit` и `offset` запроса: некорректный лимит — по умолчанию, больше предела — предел.
pub(super) fn page(limit: Option<&str>, offset: Option<&str>, default: i64) -> (i64, i64) {
    let limit = match limit.and_then(|l| l.trim().parse::<i64>().ok()) {
        Some(l) if l > MAX_LIMIT => MAX_LIMIT,
        Some(l) if l >= 1 => l,
        _ => default,
    };
    let offset = offset
        .and_then(|o| o.trim().parse::<i64>().ok())
        .filter(|o| *o >= 0)
        .unwrap_or(0);
    (limit, offset)
}

/// Склад для отгрузок и позиций заказа.
#[derive(Debug, Clone, FromRow)]
pub(super) struct ProfileStore {
    pub id: i64,
    pub name: String,
    pub address: String,
    pub phone: String,
    pub image: Option<String>,
    pub extra: sqlx::types::Json<Map<String, Value>>,
}

/// Справочники для сборки страницы заказов — загружаются один раз на страницу.
pub(super) struct OrderRefs {
    pub deliveries: HashMap<i64, Delivery>,
    pub pay_systems: HashMap<i64, PaySystem>,
    /// Все свойства заказа (по сортировке).
    pub properties: Vec<OrderProperty>,
    pub stores: HashMap<i64, ProfileStore>,
    /// id товара → URL и картинка (правила корзины).
    pub products: HashMap<i64, ProductView>,
    pub files: HashMap<i64, FileRecord>,
}

fn iso(dt: &DateTime<Utc>) -> String {
    dt.with_timezone(&Local)
        .to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// UF-значение склада строкой, как отдаёт Битрикс.
fn uf_value(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => Value::from(s.clone()),
        Some(Value::Null) | None => Value::Null,
        Some(other) => Value::from(other.to_string()),
    }
}

fn shipment_store(s: &ProfileStore, cart: &CartConfig) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), json!(s.id));
    obj.insert("name".into(), json!(s.name));
    obj.insert("address".into(), json!(s.address));
    obj.insert("phone".into(), json!(s.phone));
    obj.insert("image".into(), json!(s.image));
    for (uf, key) in &cart.store_user_fields {
        obj.insert(key.to_string(), uf_value(s.extra.get(*uf)));
    }
    Value::Object(obj)
}

fn property_value(p: Option<&OrderProperty>, raw: &str, files: &HashMap<i64, FileRecord>) -> Value {
    match p {
        Some(p) if p.kind == "file" => file_value(raw, p.multiple, files),
        _ => Value::from(raw),
    }
}

/// Заказ в формате `/profile/orders` (§7.3 контракта).
pub(super) fn order_json(
    o: &UserOrder,
    r: &OrderRefs,
    cfg: &ProfileConfig,
    cart: &CartConfig,
) -> Value {
    let quantities: Vec<f64> = o.items.iter().map(|i| i.quantity).collect();
    let count = items_count(&quantities, cart.items_count_positions);
    let created = o.created_at.with_timezone(&Local).fixed_offset();

    let shipments: Vec<Value> = o
        .shipments
        .iter()
        .map(|s| {
            let service = s.delivery_id.and_then(|id| r.deliveries.get(&id));
            let name = if s.delivery_name.is_empty() {
                service.map(|d| d.name.clone()).unwrap_or_default()
            } else {
                s.delivery_name.clone()
            };
            let mut obj = json!({
                "id": s.id,
                "deliveryId": s.delivery_id.unwrap_or(0),
                "deliveryCode": service.map(|d| d.code.as_str()).unwrap_or(""),
                "deliveryName": name,
                "price": s.price,
                "currency": o.currency,
                "allowDelivery": s.allow_delivery,
                "deducted": o.stock_deducted,
                "trackingNumber": s.tracking_number,
            });
            let stores: Vec<Value> = service
                .map(|d| d.store_ids.as_slice())
                .unwrap_or_default()
                .iter()
                .filter_map(|id| r.stores.get(id))
                .map(|st| shipment_store(st, cart))
                .collect();
            if !stores.is_empty() {
                obj["stores"] = Value::Array(stores);
            }
            obj
        })
        .collect();

    let payments: Vec<Value> = o
        .payments
        .iter()
        .map(|p| {
            let service = p.pay_system_id.and_then(|id| r.pay_systems.get(&id));
            let name = if p.name.is_empty() {
                service.map(|s| s.name.clone()).unwrap_or_default()
            } else {
                p.name.clone()
            };
            json!({
                "id": p.id,
                "paySystemId": p.pay_system_id.unwrap_or(0),
                "paySystemCode": service.map(|s| s.code.as_str()).unwrap_or(""),
                "paySystemName": name,
                "sum": p.sum,
                "currency": p.currency,
                "paid": p.paid,
                "datePaid": p.paid_at.as_ref().map(iso),
            })
        })
        .collect();

    let items: Vec<Value> = o
        .items
        .iter()
        .map(|i| {
            let product = r.products.get(&i.product_id);
            let store = i.store_id.and_then(|id| r.stores.get(&id));
            let mut obj = json!({
                "id": i.id,
                "productId": i.product_id,
                "name": i.name,
                "quantity": i.quantity,
                "price": i.price,
                "basePrice": i.price,
                "currency": o.currency,
                "weight": 0,
                "detailPageUrl": product.map(|p| p.slug.as_str()).unwrap_or(""),
                "properties": i.store_id.map_or(json!([]), |id| {
                    json!([{"code": "STORE_ID", "name": "Склад", "value": id.to_string()}])
                }),
            });
            if let Some(st) = store {
                obj["store"] = json!({
                    "id": st.id,
                    "name": st.name,
                    "xmlId": match st.extra.get("xml_id") {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Null) | None => String::new(),
                        Some(v) => v.to_string(),
                    },
                    "address": st.address,
                });
            }
            if let Some(image) = product.map(|p| &p.image).filter(|v| !v.is_null()) {
                obj["image"] = image.clone();
            }
            obj
        })
        .collect();

    let mut properties = Map::new();
    let values: HashMap<i64, &(i64, String, String, String)> =
        o.properties.iter().map(|v| (v.0, v)).collect();
    for p in r
        .properties
        .iter()
        .filter(|p| p.person_type_id == o.person_type_id && p.active)
    {
        let (name, raw) = values
            .get(&p.id)
            .map_or((p.name.as_str(), ""), |v| (v.2.as_str(), v.3.as_str()));
        let key = camel_key(&p.code, p.id);
        properties.insert(
            key.clone(),
            json!({"id": p.id, "code": key, "name": name, "value": property_value(Some(p), raw, &r.files)}),
        );
    }
    let listed: HashSet<i64> = r
        .properties
        .iter()
        .filter(|p| p.person_type_id == o.person_type_id && p.active)
        .map(|p| p.id)
        .collect();
    for (pid, code, name, raw) in o.properties.iter().filter(|v| !listed.contains(&v.0)) {
        let p = r.properties.iter().find(|p| p.id == *pid);
        let key = camel_key(code, *pid);
        properties.insert(
            key.clone(),
            json!({"id": pid, "code": key, "name": name, "value": property_value(p, raw, &r.files)}),
        );
    }

    json!({
        "id": o.id,
        "accountNumber": o.account_number,
        "status": {"id": o.status, "name": o.status_name},
        "createdAt": iso(&o.created_at),
        "createdAtLabel": php_date(&created, cfg.order_created_at_format),
        "totalPrice": o.price,
        "currency": o.currency,
        "canceled": o.canceled,
        "paid": o.paid,
        "itemsCount": count,
        "itemsCountLabel": items_count_label(count),
        "personTypeId": o.person_type_id,
        "comment": o.user_comment,
        "shipments": shipments,
        "payments": payments,
        "items": items,
        "properties": properties,
    })
}

/// Справочники для страницы заказов: доставки, платёжки, свойства, склады, товары, файлы.
async fn load_refs(state: &AppState, orders: &[UserOrder]) -> Result<OrderRefs, BxError> {
    let settings = sale::load_settings(&state.db).await?;
    let stores: Vec<ProfileStore> = sqlx::query_as(
        "SELECT s.id, s.name, s.address, s.phone, '/upload/' || f.path AS image, s.extra
         FROM catalog_stores s LEFT JOIN files f ON f.id = s.image_id",
    )
    .fetch_all(&state.db)
    .await?;
    let product_ids: Vec<i64> = orders
        .iter()
        .flat_map(|o| o.items.iter().map(|i| i.product_id))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let products = if product_ids.is_empty() {
        HashMap::new()
    } else {
        product_views(state, &product_ids).await?
    };
    let file_props: HashSet<i64> = settings
        .properties
        .iter()
        .filter(|p| p.kind == "file")
        .map(|p| p.id)
        .collect();
    let file_ids: Vec<i64> = orders
        .iter()
        .flat_map(|o| o.properties.iter())
        .filter(|v| file_props.contains(&v.0))
        .flat_map(|v| file_ids(&v.3))
        .collect();
    let files = files::get_many(&state.db, &file_ids)
        .await?
        .into_iter()
        .map(|f| (f.id, f))
        .collect();
    Ok(OrderRefs {
        deliveries: settings.deliveries.into_iter().map(|d| (d.id, d)).collect(),
        pay_systems: settings
            .pay_systems
            .into_iter()
            .map(|p| (p.id, p))
            .collect(),
        properties: settings.properties,
        stores: stores.into_iter().map(|s| (s.id, s)).collect(),
        products,
        files,
    })
}

#[derive(Deserialize)]
pub struct OrdersQuery {
    limit: Option<String>,
    offset: Option<String>,
}

/// `GET /profile/orders`
pub async fn orders(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(q): Query<OrdersQuery>,
) -> BxResult {
    let user_id = require_user(&state, &jar).await?;
    let cfg = &state.project.profile;
    let (limit, offset) = page(q.limit.as_deref(), q.offset.as_deref(), cfg.orders_limit);
    let orders = history::list(&state.db, user_id, limit, offset).await?;
    let refs = load_refs(&state, &orders).await?;
    let list: Vec<Value> = orders
        .iter()
        .map(|o| order_json(o, &refs, cfg, &state.project.cart))
        .collect();
    Ok(success(json!({ "orders": list })))
}

/// `GET /profile/orders/{id}`
pub async fn order(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(id): Path<String>,
) -> BxResult {
    let user_id = require_user(&state, &jar).await?;
    let id: i64 = id
        .trim()
        .parse()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| BxError::bad_request("order_id_invalid", "Некорректный ID заказа"))?;
    let Some(order) = history::detail(&state.db, id, user_id).await? else {
        return Err(BxError::with_status(
            StatusCode::NOT_FOUND,
            "order_not_found",
            "Заказ не найден",
        ));
    };
    let refs = load_refs(&state, std::slice::from_ref(&order)).await?;
    let cfg = &state.project.profile;
    Ok(success(
        json!({ "order": order_json(&order, &refs, cfg, &state.project.cart) }),
    ))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use crate::sale::{
        history::{UserPayment, UserShipment},
        repo::OrderItem,
    };
    use serde_json::json;

    use super::*;

    fn file(id: i64, path: &str) -> FileRecord {
        FileRecord {
            id,
            path: path.into(),
            original_name: "Накладная.pdf".into(),
            content_type: "application/pdf".into(),
            size: 1024,
            width: None,
            height: None,
            created_at: DateTime::<Utc>::default(),
        }
    }

    fn files() -> HashMap<i64, FileRecord> {
        HashMap::from([(5, file(5, "sale/ab/x.pdf"))])
    }

    fn file_json() -> serde_json::Value {
        json!({
            "ID": 5,
            "SRC": "/upload/sale/ab/x.pdf",
            "ORIGINAL_NAME": "Накладная.pdf",
            "FILE_NAME": "x.pdf",
            "CONTENT_TYPE": "application/pdf",
            "FILE_SIZE": 1024,
        })
    }

    #[test]
    fn camel_keys() {
        assert_eq!(camel_key("SDEK_TRACKING_URL", 1), "sdekTrackingUrl");
        assert_eq!(camel_key("Store_Keeper", 1), "storeKeeper");
        assert_eq!(camel_key("phone", 1), "phone");
        assert_eq!(camel_key("WAYBILL", 1), "waybill");
        assert_eq!(camel_key("", 23), "prop23");
    }

    #[test]
    fn count_labels() {
        for (n, s) in [
            (1, "1 товар"),
            (4, "4 товара"),
            (11, "11 товаров"),
            (21, "21 товар"),
            (0, "0 товаров"),
            (112, "112 товаров"),
        ] {
            assert_eq!(items_count_label(n), s);
        }
        assert_eq!(items_count(&[2.0, 1.5], false), 4);
        assert_eq!(items_count(&[2.0, 1.5], true), 2);
    }

    #[test]
    fn date_format() {
        let dt = DateTime::parse_from_rfc3339("2026-05-01T10:00:00+03:00").unwrap();
        assert_eq!(php_date(&dt, "d.m.Y H:i:s"), "01.05.2026 10:00:00");
        assert_eq!(php_date(&dt, "Y-m-d"), "2026-05-01");
    }

    #[test]
    fn file_value_single_and_multiple() {
        assert_eq!(file_value("5", false, &files()), file_json());
        assert_eq!(file_value("", false, &files()), json!(""));
        assert_eq!(file_value("5", true, &files()), json!([file_json()]));
        assert_eq!(file_value("", true, &files()), json!([]));
    }

    #[test]
    fn file_value_skips_missing_and_garbage() {
        assert_eq!(file_value("abc,,7,5", false, &files()), file_json());
        assert_eq!(file_value("abc,,7,5", true, &files()), json!([file_json()]));
        assert_eq!(file_value("7", false, &files()), json!(""));
    }

    #[test]
    fn page_parsing() {
        assert_eq!(page(None, None, 50), (50, 0));
        assert_eq!(page(Some("20"), Some("40"), 50), (20, 40));
        assert_eq!(page(Some("0"), Some("-5"), 50), (50, 0));
        assert_eq!(page(Some("1000"), Some("x"), 50), (200, 0));
        assert_eq!(page(Some("abc"), None, 50), (50, 0));
    }

    fn refs() -> OrderRefs {
        let delivery = Delivery {
            id: 3,
            code: "pickup".into(),
            name: "Самовывоз".into(),
            description: String::new(),
            active: true,
            public: true,
            sort: 100,
            price: 0.0,
            currency: "RUB".into(),
            store_ids: vec![3],
        };
        let pay = PaySystem {
            id: 1,
            code: "cash".into(),
            name: "Наличные".into(),
            description: String::new(),
            active: true,
            sort: 100,
            handler: String::new(),
            api_type: "cash".into(),
            group_ids: Vec::new(),
        };
        let prop = |id: i64, code: &str, kind: &str, multiple: bool| OrderProperty {
            id,
            person_type_id: 1,
            group_id: None,
            code: code.into(),
            name: code.into(),
            kind: kind.into(),
            required: false,
            util: false,
            is_email: false,
            is_phone: false,
            is_payer: false,
            is_profile_name: false,
            is_location: false,
            is_address: false,
            is_zip: false,
            default_value: String::new(),
            description: String::new(),
            sort: id as i32,
            active: true,
            multiple,
        };
        OrderRefs {
            deliveries: HashMap::from([(3, delivery)]),
            pay_systems: HashMap::from([(1, pay)]),
            properties: vec![
                prop(1, "FIO", "text", false),
                prop(23, "WAYBILL", "file", true),
                prop(32, "INVOICE", "file", false),
                prop(40, "OTHER", "text", false),
            ],
            stores: HashMap::from([(
                3,
                ProfileStore {
                    id: 3,
                    name: "Краснодар".into(),
                    address: "ул. Складская, 1".into(),
                    phone: "+7 861".into(),
                    image: Some("/upload/st.jpg".into()),
                    extra: sqlx::types::Json(
                        json!({"uf_city_id": 77, "xml_id": "krd"})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                },
            )]),
            products: HashMap::from([(
                33,
                ProductView {
                    name: "Товар".into(),
                    slug: "/catalog/tovar/".into(),
                    article: json!(null),
                    image: json!(null),
                    properties: Default::default(),
                },
            )]),
            files: files(),
        }
    }

    fn order() -> UserOrder {
        UserOrder {
            id: 12,
            account_number: "12".into(),
            status: "N".into(),
            status_name: "Принят".into(),
            created_at: DateTime::parse_from_rfc3339("2026-05-01T07:00:00Z")
                .unwrap()
                .to_utc(),
            price: 200.0,
            currency: "RUB".into(),
            canceled: false,
            paid: false,
            stock_deducted: true,
            person_type_id: 1,
            user_comment: "позвонить".into(),
            shipments: vec![UserShipment {
                order_id: 12,
                id: 7,
                delivery_id: Some(3),
                delivery_name: "Самовывоз".into(),
                price: 0.0,
                tracking_number: "TRK".into(),
                allow_delivery: false,
            }],
            payments: vec![UserPayment {
                order_id: 12,
                id: 9,
                pay_system_id: Some(1),
                name: "Наличные".into(),
                sum: 200.0,
                currency: "RUB".into(),
                paid: false,
                paid_at: None,
            }],
            items: vec![OrderItem {
                id: 100,
                product_id: 33,
                store_id: Some(3),
                quantity: 2.0,
                price: 100.0,
                name: "Товар".into(),
                custom_price: false,
            }],
            properties: vec![
                (1, "FIO".into(), "ФИО".into(), "Иван".into()),
                (23, "WAYBILL".into(), "Накладная".into(), "5".into()),
                (99, "OLD".into(), "Старое".into(), "x".into()),
            ],
        }
    }

    #[test]
    fn order_json_contract_shape() {
        let cart = CartConfig {
            store_user_fields: vec![("uf_city_id", "cityId")],
            ..Default::default()
        };
        let v = order_json(&order(), &refs(), &ProfileConfig::default(), &cart);
        assert_eq!(v["id"], 12);
        assert_eq!(v["accountNumber"], "12");
        assert_eq!(v["status"], json!({"id": "N", "name": "Принят"}));
        assert_eq!(v["totalPrice"], 200.0);
        assert_eq!(v["itemsCount"], 2);
        assert_eq!(v["itemsCountLabel"], "2 товара");
        assert_eq!(v["comment"], "позвонить");
        assert_eq!(v["personTypeId"], 1);
        let sh = &v["shipments"][0];
        assert_eq!(sh["deliveryCode"], "pickup");
        assert_eq!(sh["trackingNumber"], "TRK");
        assert_eq!(sh["deducted"], true);
        assert_eq!(sh["allowDelivery"], false);
        assert_eq!(
            sh["stores"][0],
            json!({"id": 3, "name": "Краснодар", "address": "ул. Складская, 1", "phone": "+7 861",
                   "image": "/upload/st.jpg", "cityId": "77"})
        );
        let pay = &v["payments"][0];
        assert_eq!(pay["paySystemCode"], "cash");
        assert_eq!(pay["datePaid"], json!(null));
        let item = &v["items"][0];
        assert_eq!(item["productId"], 33);
        assert_eq!(item["basePrice"], 100.0);
        assert_eq!(item["weight"], 0);
        assert_eq!(item["detailPageUrl"], "/catalog/tovar/");
        assert_eq!(
            item["properties"],
            json!([{"code": "STORE_ID", "name": "Склад", "value": "3"}])
        );
        assert_eq!(
            item["store"],
            json!({"id": 3, "name": "Краснодар", "xmlId": "krd", "address": "ул. Складская, 1"})
        );
        assert!(item.get("image").is_none());
        let props = &v["properties"];
        assert_eq!(
            props["fio"],
            json!({"id": 1, "code": "fio", "name": "ФИО", "value": "Иван"})
        );
        assert_eq!(props["waybill"]["value"][0]["SRC"], "/upload/sale/ab/x.pdf");
        assert_eq!(props["waybill"]["code"], "waybill");
        assert_eq!(props["invoice"]["value"], "");
        assert_eq!(props["other"]["value"], "");
        assert_eq!(props["old"]["value"], "x");
    }
}
