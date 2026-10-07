//! Форма оформления заказа (`GET /order/form`) и итоги — чистые функции над справочниками.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use super::{Delivery, OrderProperty, PaySystem, PersonType, SaleSettings};
use crate::{
    bxapi::project::OrderConfig,
    cart::{StoreInfo, snapshot::store_json},
};

/// Данные вошедшего покупателя для значений по умолчанию.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub email: String,
    pub phone: String,
    pub full_name: String,
}

/// Активные типы плательщика (белый список — по id или коду).
pub fn person_types<'a>(s: &'a SaleSettings, cfg: &OrderConfig) -> Vec<&'a PersonType> {
    s.person_types
        .iter()
        .filter(|p| p.active)
        .filter(|p| {
            cfg.person_type_whitelist.is_empty()
                || cfg
                    .person_type_whitelist
                    .iter()
                    .any(|w| *w == p.code || *w == p.id.to_string())
        })
        .collect()
}

/// Тип из запроса (id или код), иначе первый.
pub fn select_person_type(list: &[&PersonType], requested: Option<&str>) -> Option<i64> {
    let requested = requested.map(str::trim).filter(|r| !r.is_empty());
    requested
        .and_then(|r| {
            list.iter()
                .find(|p| p.id.to_string() == r || (!p.code.is_empty() && p.code == r))
        })
        .or(list.first())
        .map(|p| p.id)
}

/// Свойства типа плательщика, которые видит покупатель.
pub fn visible_properties<'a>(
    s: &'a SaleSettings,
    person_type: i64,
    cfg: &OrderConfig,
) -> Vec<&'a OrderProperty> {
    s.properties
        .iter()
        .filter(|p| p.person_type_id == person_type && p.active && !p.util)
        .filter(|p| !cfg.property_blacklist.contains(&p.code.as_str()))
        .collect()
}

fn field_json(s: &SaleSettings, p: &OrderProperty, profile: Option<&Profile>) -> Value {
    let kind = match p.kind.as_str() {
        "text" if p.is_phone => "phone",
        "text" if p.is_email => "email",
        k => k,
    };
    let from_profile = profile.and_then(|pr| {
        let v = if p.is_email {
            &pr.email
        } else if p.is_phone {
            &pr.phone
        } else if p.is_payer || p.is_profile_name {
            &pr.full_name
        } else {
            return None;
        };
        Some(v.clone()).filter(|v| !v.is_empty())
    });
    let default = from_profile
        .or_else(|| Some(p.default_value.clone()).filter(|v| !v.is_empty()))
        .map_or(Value::Null, Value::from);
    let mut field = json!({
        "id": p.id,
        "code": p.code,
        "name": p.name,
        "type": kind,
        "required": p.required,
        "default": default,
        "description": p.description,
        "multiline": p.kind == "textarea",
        "hints": {
            "isEmail": p.is_email,
            "isPhone": p.is_phone,
            "isProfileName": p.is_profile_name,
            "isPayer": p.is_payer,
        },
    });
    if p.kind == "select" {
        let options: Vec<Value> = s
            .variants
            .get(&p.id)
            .map(|vs| {
                vs.iter()
                    .map(|v| json!({"value": v.value, "name": v.name}))
                    .collect()
            })
            .unwrap_or_default();
        field["options"] = Value::Array(options);
    }
    field
}

/// Блоки формы: свойства по группам, порядок — `block_order`, затем сортировка групп.
pub fn blocks(
    s: &SaleSettings,
    person_type: i64,
    cfg: &OrderConfig,
    profile: Option<&Profile>,
) -> Vec<Value> {
    let props = visible_properties(s, person_type, cfg);
    let known: HashSet<i64> = s
        .groups
        .iter()
        .filter(|g| g.person_type_id == person_type)
        .map(|g| g.id)
        .collect();
    let mut out: Vec<(String, Value)> = Vec::new();
    for g in s.groups.iter().filter(|g| g.person_type_id == person_type) {
        let fields: Vec<Value> = props
            .iter()
            .filter(|p| p.group_id == Some(g.id))
            .map(|p| field_json(s, p, profile))
            .collect();
        if !fields.is_empty() {
            out.push((
                g.block_code.clone(),
                json!({"code": g.block_code, "name": g.name, "fields": fields}),
            ));
        }
    }
    // Свойства без группы (или с группой другого типа)
    let orphans: Vec<Value> = props
        .iter()
        .filter(|p| !p.group_id.is_some_and(|g| known.contains(&g)))
        .map(|p| field_json(s, p, profile))
        .collect();
    if !orphans.is_empty() {
        out.push((
            "group_0".into(),
            json!({"code": "group_0", "name": "Прочее", "fields": orphans}),
        ));
    }
    out.sort_by_key(|(code, _)| {
        cfg.block_order
            .iter()
            .position(|c| c == code)
            .unwrap_or(usize::MAX)
    });
    out.into_iter().map(|(_, v)| v).collect()
}

fn delivery_visible(d: &Delivery, cfg: &OrderConfig, allowed: Option<&HashSet<i64>>) -> bool {
    d.active
        && d.public
        && (cfg.delivery_whitelist.is_empty() || cfg.delivery_whitelist.contains(&d.id))
        && allowed.is_none_or(|a| a.contains(&d.id))
}

/// Доставка, доступная покупателю; `allowed` — результат фильтра проекта.
pub fn find_delivery<'a>(
    s: &'a SaleSettings,
    cfg: &OrderConfig,
    allowed: Option<&HashSet<i64>>,
    id: i64,
) -> Option<&'a Delivery> {
    s.deliveries
        .iter()
        .find(|d| d.id == id && delivery_visible(d, cfg, allowed))
}

/// Склады самовывоза службы: только активные, с фото.
pub fn delivery_stores(d: &Delivery, stores: &[StoreInfo]) -> Vec<Value> {
    d.store_ids
        .iter()
        .filter_map(|id| stores.iter().find(|st| st.id == *id && st.active))
        .map(|st| {
            let mut obj = store_json(st);
            obj.insert(
                "image".into(),
                st.image.clone().map_or(Value::Null, Value::from),
            );
            Value::Object(obj)
        })
        .collect()
}

/// Доставки формы; у служб с активными складами — `stores`.
pub fn deliveries(
    s: &SaleSettings,
    cfg: &OrderConfig,
    allowed: Option<&HashSet<i64>>,
    stores: &[StoreInfo],
) -> Vec<Value> {
    s.deliveries
        .iter()
        .filter(|d| delivery_visible(d, cfg, allowed))
        .map(|d| {
            let mut obj = Map::new();
            obj.insert("id".into(), json!(d.id));
            obj.insert("code".into(), json!(d.code));
            obj.insert("name".into(), json!(d.name));
            obj.insert("price".into(), json!(d.price));
            obj.insert("currency".into(), json!(d.currency));
            obj.insert("description".into(), json!(d.description));
            let pickup = delivery_stores(d, stores);
            if !pickup.is_empty() {
                obj.insert("stores".into(), Value::Array(pickup));
            }
            Value::Object(obj)
        })
        .collect()
}

/// Тип платёжки для API: переопределение проекта, иначе из настроек.
pub fn payment_type(p: &PaySystem, cfg: &OrderConfig) -> String {
    cfg.payment_types
        .iter()
        .find(|(id, _)| *id == p.id)
        .map_or_else(|| p.api_type.clone(), |(_, t)| t.to_string())
}

/// Платёжка, видимая в форме (активна, в белом списке).
pub fn find_payment<'a>(s: &'a SaleSettings, cfg: &OrderConfig, id: i64) -> Option<&'a PaySystem> {
    s.pay_systems.iter().find(|p| {
        p.id == id
            && p.active
            && (cfg.payment_whitelist.is_empty() || cfg.payment_whitelist.contains(&p.id))
    })
}

pub fn payments(s: &SaleSettings, cfg: &OrderConfig) -> Vec<Value> {
    s.pay_systems
        .iter()
        .filter(|p| find_payment(s, cfg, p.id).is_some())
        .map(|p| {
            json!({
                "id": p.id,
                "code": p.code,
                "name": p.name,
                "description": p.description,
                "type": payment_type(p, cfg),
            })
        })
        .collect()
}

/// Строки итогов: товары, доставка, итого (с названием платёжки).
pub fn summary(goods: f64, delivery: f64, payment_name: Option<&str>) -> Vec<Value> {
    let total = ((goods + delivery) * 100.0).round() / 100.0;
    let mut total_line =
        json!({"label": "Итого", "value": total, "type": "total", "format": "price"});
    if let Some(name) = payment_name {
        total_line["hint"] = json!(name);
    }
    vec![
        json!({"label": "Стоимость товаров", "value": goods, "format": "price"}),
        json!({"label": "Доставка", "value": delivery, "format": "price"}),
        total_line,
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::sale::{PropertyGroup, PropertyVariant};

    fn group(id: i64, person_type_id: i64, name: &str, block_code: &str) -> PropertyGroup {
        PropertyGroup {
            id,
            person_type_id,
            name: name.into(),
            sort: id as i32,
            block_code: block_code.into(),
        }
    }

    fn prop(id: i64, code: &str, kind: &str) -> OrderProperty {
        OrderProperty {
            id,
            person_type_id: 1,
            group_id: Some(10),
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
        }
    }

    fn delivery(
        id: i64,
        name: &str,
        active: bool,
        public: bool,
        price: f64,
        stores: Vec<i64>,
    ) -> Delivery {
        Delivery {
            id,
            code: String::new(),
            name: name.into(),
            description: String::new(),
            active,
            public,
            sort: id as i32,
            price,
            currency: "RUB".into(),
            store_ids: stores,
        }
    }

    fn pay(id: i64, name: &str, api_type: &str, groups: Vec<i64>) -> PaySystem {
        PaySystem {
            id,
            code: String::new(),
            name: name.into(),
            description: String::new(),
            active: true,
            sort: id as i32,
            handler: String::new(),
            api_type: api_type.into(),
            group_ids: groups,
        }
    }

    fn store(id: i64, active: bool, image: Option<&str>) -> StoreInfo {
        let mut fields = Map::new();
        fields.insert("address".into(), json!(format!("Адрес {id}")));
        StoreInfo {
            id,
            name: format!("Склад {id}"),
            active,
            fields,
            image: image.map(String::from),
        }
    }

    fn settings() -> SaleSettings {
        let mut fio = prop(1, "fio", "text");
        fio.is_payer = true;
        fio.required = true;
        let mut phone = prop(2, "phone", "text");
        phone.is_phone = true;
        let mut email = prop(3, "EMAIL", "text");
        email.is_email = true;
        let mut waybill = prop(5, "WAYBILL", "file");
        waybill.util = true;
        let variant = |id: i64, value: &str, name: &str| PropertyVariant {
            id,
            property_id: 4,
            value: value.into(),
            name: name.into(),
            sort: id as i32,
        };
        SaleSettings {
            person_types: vec![PersonType {
                id: 1,
                code: String::new(),
                name: "Клиент".into(),
                active: true,
                sort: 100,
            }],
            groups: vec![group(10, 1, "Свойства заказа", "group_10")],
            properties: vec![
                fio,
                phone,
                email,
                prop(4, "UR_LICO", "select"),
                waybill,
                prop(6, "COMMENT", "textarea"),
            ],
            variants: HashMap::from([(4, vec![variant(1, "OOO", "ООО"), variant(2, "IP", "ИП")])]),
            deliveries: vec![
                delivery(1, "Без доставки", true, false, 0.0, vec![]),
                delivery(2, "Самовывоз из Новосибирска", true, true, 0.0, vec![1, 3]),
                delivery(8, "До ТК", true, true, 300.0, vec![]),
                delivery(9, "Курьер", false, true, 0.0, vec![]),
            ],
            pay_systems: vec![
                pay(5, "Счёт", "document", vec![21]),
                pay(7, "QR", "other", vec![]),
            ],
            statuses: Vec::new(),
        }
    }

    #[test]
    fn blocks_types_hints_and_profile_defaults() {
        let s = settings();
        let profile = Profile {
            email: "a@b.ru".into(),
            phone: "+7999".into(),
            full_name: "Иванов Иван".into(),
        };
        let b = blocks(&s, 1, &OrderConfig::default(), Some(&profile));
        assert_eq!(b.len(), 1);
        assert_eq!(b[0]["code"], "group_10");
        assert_eq!(b[0]["name"], "Свойства заказа");
        let fields = b[0]["fields"].as_array().unwrap();
        let codes: Vec<&str> = fields.iter().map(|f| f["code"].as_str().unwrap()).collect();
        assert_eq!(codes, ["fio", "phone", "EMAIL", "UR_LICO", "COMMENT"]);
        assert_eq!(fields[1]["type"], "phone");
        assert_eq!(fields[1]["default"], "+7999");
        assert_eq!(fields[2]["type"], "email");
        assert_eq!(fields[2]["default"], "a@b.ru");
        assert_eq!(fields[0]["default"], "Иванов Иван");
        assert_eq!(fields[0]["required"], true);
        assert_eq!(fields[0]["hints"]["isPayer"], true);
        assert_eq!(fields[0]["id"], 1);
        assert_eq!(
            fields[3]["options"],
            json!([{"value": "OOO", "name": "ООО"}, {"value": "IP", "name": "ИП"}])
        );
        assert_eq!(fields[4]["multiline"], true);
        assert_eq!(fields[0]["multiline"], false);
    }

    #[test]
    fn blocks_respect_blacklist_and_order() {
        let mut s = settings();
        s.groups.push(group(11, 1, "Комментарий", "comment"));
        s.properties
            .iter_mut()
            .find(|p| p.code == "COMMENT")
            .unwrap()
            .group_id = Some(11);
        let cfg = OrderConfig {
            property_blacklist: vec!["UR_LICO"],
            block_order: vec!["comment"],
            ..Default::default()
        };
        let b = blocks(&s, 1, &cfg, None);
        assert_eq!(b[0]["code"], "comment");
        assert!(
            b[1]["fields"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["code"] != "UR_LICO")
        );
        assert_eq!(b[1]["fields"][0]["default"], Value::Null);
    }

    #[test]
    fn person_types_whitelist_and_selection() {
        let mut s = settings();
        s.person_types.push(PersonType {
            id: 2,
            code: "ur".into(),
            name: "Юр. лицо".into(),
            active: true,
            sort: 200,
        });
        let all = person_types(&s, &OrderConfig::default());
        assert_eq!(all.len(), 2);
        assert_eq!(select_person_type(&all, None), Some(1));
        assert_eq!(select_person_type(&all, Some("ur")), Some(2));
        assert_eq!(select_person_type(&all, Some("2")), Some(2));
        assert_eq!(select_person_type(&all, Some("99")), Some(1));
        let only = person_types(
            &s,
            &OrderConfig {
                person_type_whitelist: vec!["ur"],
                ..Default::default()
            },
        );
        assert_eq!(only.iter().map(|p| p.id).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn find_delivery_skips_hidden() {
        let s = settings();
        let cfg = OrderConfig::default();
        assert!(find_delivery(&s, &cfg, None, 1).is_none());
        assert!(find_delivery(&s, &cfg, None, 9).is_none());
        assert_eq!(find_delivery(&s, &cfg, None, 8).map(|d| d.id), Some(8));
        assert!(find_delivery(&s, &cfg, Some(&HashSet::from([2])), 8).is_none());
        let only = OrderConfig {
            delivery_whitelist: vec![2],
            ..Default::default()
        };
        assert!(find_delivery(&s, &only, None, 8).is_none());
    }

    #[test]
    fn deliveries_with_pickup_stores() {
        let s = settings();
        let stores = vec![store(1, true, Some("/upload/a.jpg")), store(3, false, None)];
        let d = deliveries(&s, &OrderConfig::default(), None, &stores);
        let ids: Vec<i64> = d.iter().map(|x| x["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, [2, 8]);
        assert_eq!(d[0]["stores"].as_array().unwrap().len(), 1);
        assert_eq!(d[0]["stores"][0]["id"], 1);
        assert_eq!(d[0]["stores"][0]["address"], "Адрес 1");
        assert_eq!(d[0]["stores"][0]["image"], "/upload/a.jpg");
        assert!(d[1].get("stores").is_none());
        assert_eq!(d[1]["price"], 300.0);
        assert_eq!(d[1]["currency"], "RUB");
    }

    #[test]
    fn payments_types_and_override() {
        let s = settings();
        let p = payments(
            &s,
            &OrderConfig {
                payment_types: vec![(7, "qr")],
                ..Default::default()
            },
        );
        assert_eq!(p[0]["type"], "document");
        assert_eq!(p[1]["type"], "qr");
        assert_eq!(p[1]["name"], "QR");
    }

    #[test]
    fn summary_lines() {
        let lines = summary(90500.0, 300.0, Some("Счёт"));
        assert_eq!(
            lines,
            vec![
                json!({"label": "Стоимость товаров", "value": 90500.0, "format": "price"}),
                json!({"label": "Доставка", "value": 300.0, "format": "price"}),
                json!({"label": "Итого", "value": 90800.0, "hint": "Счёт", "type": "total", "format": "price"}),
            ]
        );
        assert_eq!(summary(10.0, 0.0, None)[2].get("hint"), None);
    }
}
