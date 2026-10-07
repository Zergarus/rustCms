//! Проверки `POST /order/submit`: свойства заказа и выбор позиций корзины.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::{OrderProperty, PropertyVariant};

/// Ошибка оформления: код из контракта, текст, поле для `invalid_property`.
#[derive(Debug, PartialEq)]
pub struct SubmitError {
    pub code: &'static str,
    pub message: String,
    pub field: Option<String>,
}

impl SubmitError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        SubmitError {
            code,
            message: message.into(),
            field: None,
        }
    }

    pub fn basket_changed() -> Self {
        SubmitError::new(
            "basket_changed",
            "Состав или стоимость корзины изменились. Обновите корзину перед оформлением.",
        )
    }

    fn property(p: &OrderProperty, message: String) -> Self {
        SubmitError {
            code: "invalid_property",
            message,
            field: Some(p.code.clone()),
        }
    }
}

/// Значение поля из JSON: строка (без пробелов по краям), число — строкой, массив — JSON.
fn raw_value(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.trim().to_string(),
        Value::Bool(b) => if *b { "Y" } else { "N" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => {
            let list: Vec<String> = items
                .iter()
                .map(raw_value)
                .filter(|s| !s.is_empty())
                .collect();
            if list.is_empty() {
                String::new()
            } else {
                Value::from(list).to_string()
            }
        }
        Value::Object(_) => v.to_string(),
    }
}

/// Проверяет свойства заказа; возвращает (id свойства, значение) заполненных свойств.
/// Свойства не из `props` (служебные, неизвестные) игнорируются.
pub fn validate_properties(
    props: &[&OrderProperty],
    variants: &HashMap<i64, Vec<PropertyVariant>>,
    input: &Map<String, Value>,
    known_locations: &HashSet<String>,
) -> Result<Vec<(i64, String)>, SubmitError> {
    let mut out = Vec::new();
    for p in props {
        let mut value = input.get(&p.code).map(raw_value).unwrap_or_default();
        if p.kind == "checkbox" {
            value = match value.to_lowercase().as_str() {
                "y" | "on" | "1" | "true" => "Y".into(),
                "" => String::new(),
                _ => "N".into(),
            };
        }
        let filled = !value.is_empty() && !(p.kind == "checkbox" && value == "N");
        if !filled {
            if p.required {
                return Err(SubmitError::property(
                    p,
                    format!("Заполните поле «{}»", p.name),
                ));
            }
            if value.is_empty() {
                continue;
            }
        }
        let invalid = || SubmitError::property(p, format!("Неверное значение поля «{}»", p.name));
        if p.is_email && !crate::users::is_valid_email(&value) {
            return Err(invalid());
        }
        match p.kind.as_str() {
            "select" => {
                let options = variants.get(&p.id).map(Vec::as_slice).unwrap_or_default();
                if !options.iter().any(|v| v.value == value) {
                    return Err(invalid());
                }
            }
            "location" if !known_locations.contains(&value) => return Err(invalid()),
            "number" if value.replace(',', ".").parse::<f64>().is_err() => return Err(invalid()),
            _ => {}
        }
        out.push((p.id, value));
    }
    Ok(out)
}

/// Что оформляется: вся корзина (со сверкой снимка) или выбранные позиции.
#[derive(Debug, PartialEq)]
pub enum Selection {
    All {
        snapshot: String,
    },
    /// (id позиции, её снимок — если прислан)
    Items(Vec<(i64, Option<String>)>),
}

pub fn parse_selection(body: &Value) -> Result<Selection, SubmitError> {
    if let Some(items) = body.get("basketItems").and_then(Value::as_array) {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let (id, snapshot) = match item {
                Value::Object(o) => (
                    o.get("id"),
                    o.get("snapshot").and_then(Value::as_str).map(String::from),
                ),
                other => (Some(other), None),
            };
            let id = id
                .and_then(|v| v.as_i64().or_else(|| v.as_str()?.trim().parse().ok()))
                .ok_or_else(SubmitError::basket_changed)?;
            out.push((id, snapshot));
        }
        return Ok(Selection::Items(out));
    }
    match body
        .get("basketSnapshot")
        .and_then(Value::as_str)
        .map(str::trim)
    {
        Some(s) if !s.is_empty() => Ok(Selection::All {
            snapshot: s.to_string(),
        }),
        _ => Err(SubmitError::new(
            "missing_snapshot",
            "Не передан снимок корзины (basketSnapshot)",
        )),
    }
}

/// Позиции заказа по снимку корзины (`GET /cart`).
pub fn pick_items(selection: &Selection, basket: &Value) -> Result<Vec<i64>, SubmitError> {
    let items = basket
        .get("items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let empty = || SubmitError::new("empty_basket", "Корзина пуста");
    let picked: Vec<&Value> = match selection {
        Selection::All { snapshot } => {
            if items.is_empty() {
                return Err(empty());
            }
            if basket.get("snapshot").and_then(Value::as_str) != Some(snapshot.as_str()) {
                return Err(SubmitError::basket_changed());
            }
            items.iter().collect()
        }
        Selection::Items(wanted) => {
            if wanted.is_empty() {
                return Err(empty());
            }
            let mut out = Vec::with_capacity(wanted.len());
            for (id, snapshot) in wanted {
                let item = items
                    .iter()
                    .find(|i| i.get("id").and_then(Value::as_i64) == Some(*id))
                    .ok_or_else(SubmitError::basket_changed)?;
                if let Some(s) = snapshot
                    && item.get("snapshot").and_then(Value::as_str) != Some(s.as_str())
                {
                    return Err(SubmitError::basket_changed());
                }
                out.push(item);
            }
            out
        }
    };
    if picked
        .iter()
        .any(|i| i.get("isAvailable").and_then(Value::as_bool) != Some(true))
    {
        return Err(SubmitError::basket_changed());
    }
    let mut ids: Vec<i64> = picked
        .iter()
        .filter_map(|i| i.get("id").and_then(Value::as_i64))
        .collect();
    let mut seen = HashSet::new();
    ids.retain(|id| seen.insert(*id));
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const FIO_ID: i64 = 1;
    const CONFIRM_ID: i64 = 2;

    fn prop(id: i64, code: &str, kind: &str, required: bool) -> OrderProperty {
        OrderProperty {
            id,
            person_type_id: 1,
            group_id: None,
            code: code.into(),
            name: format!("Поле {code}"),
            kind: kind.into(),
            required,
            util: false,
            is_email: false,
            is_phone: false,
            is_payer: false,
            is_profile_name: false,
            is_location: kind == "location",
            is_address: false,
            is_zip: false,
            default_value: String::new(),
            description: String::new(),
            sort: id as i32,
            active: true,
        }
    }

    fn fixture() -> (Vec<OrderProperty>, HashMap<i64, Vec<PropertyVariant>>) {
        let mut email = prop(3, "EMAIL", "text", true);
        email.is_email = true;
        let props = vec![
            prop(FIO_ID, "fio", "text", true),
            prop(CONFIRM_ID, "CONFIRM", "checkbox", true),
            email,
            prop(4, "UR_LICO", "select", true),
            prop(5, "city", "location", true),
            prop(6, "KPP", "text", false),
        ];
        let variant = |id: i64, value: &str| PropertyVariant {
            id,
            property_id: 4,
            value: value.into(),
            name: value.into(),
            sort: id as i32,
        };
        (
            props,
            HashMap::from([(4, vec![variant(1, "OOO"), variant(2, "IP")])]),
        )
    }

    #[test]
    fn required_blank_and_unchecked() {
        let (props, variants) = fixture();
        let ok = json!({"fio": "Иван", "CONFIRM": "Y", "EMAIL": "a@b.ru", "UR_LICO": "IP", "city": "0000073738", "KPP": "", "unknown": "x"});
        let known = HashSet::from(["0000073738".to_string()]);
        let refs: Vec<&OrderProperty> = props.iter().collect();
        let values =
            validate_properties(&refs, &variants, ok.as_object().unwrap(), &known).unwrap();
        assert_eq!(values.len(), 5);
        assert!(values.contains(&(FIO_ID, "Иван".to_string())));
        for (patch, field) in [
            (json!({"fio": "   "}), "fio"),
            (json!({"CONFIRM": "N"}), "CONFIRM"),
            (json!({"EMAIL": "не-почта"}), "EMAIL"),
            (json!({"UR_LICO": "ZAO"}), "UR_LICO"),
            (json!({"city": "999"}), "city"),
        ] {
            let mut bad = ok.clone();
            bad.as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            let err = validate_properties(&refs, &variants, bad.as_object().unwrap(), &known)
                .unwrap_err();
            assert_eq!(
                (err.code, err.field.as_deref()),
                ("invalid_property", Some(field))
            );
        }
        let mut missing = ok.clone();
        missing.as_object_mut().unwrap().remove("fio");
        let err = validate_properties(&refs, &variants, missing.as_object().unwrap(), &known)
            .unwrap_err();
        assert_eq!(err.message, "Заполните поле «Поле fio»");
    }

    #[test]
    fn checkbox_and_numbers_normalized() {
        let (props, variants) = fixture();
        let refs: Vec<&OrderProperty> = props.iter().collect();
        let known = HashSet::from(["0000073738".to_string()]);
        for confirm in [json!(true), json!("Y"), json!("on")] {
            let input = json!({"fio": 7701234567u64, "CONFIRM": confirm, "EMAIL": "a@b.ru", "UR_LICO": "IP", "city": "0000073738"});
            let values: HashMap<i64, String> =
                validate_properties(&refs, &variants, input.as_object().unwrap(), &known)
                    .unwrap()
                    .into_iter()
                    .collect();
            assert_eq!(values[&CONFIRM_ID], "Y");
            assert_eq!(values[&FIO_ID], "7701234567");
        }
    }

    #[test]
    fn parse_selection_forms() {
        assert_eq!(
            parse_selection(&json!({"basketSnapshot": "abc"})).unwrap(),
            Selection::All {
                snapshot: "abc".into()
            }
        );
        assert_eq!(
            parse_selection(&json!({"basketItems": [7, 8]})).unwrap(),
            Selection::Items(vec![(7, None), (8, None)])
        );
        assert_eq!(
            parse_selection(
                &json!({"basketItems": [{"id": 7, "snapshot": "x"}], "basketSnapshot": "abc"})
            )
            .unwrap(),
            Selection::Items(vec![(7, Some("x".into()))])
        );
        assert_eq!(
            parse_selection(&json!({})).unwrap_err().code,
            "missing_snapshot"
        );
        assert_eq!(
            parse_selection(&json!({"basketSnapshot": ""}))
                .unwrap_err()
                .code,
            "missing_snapshot"
        );
    }

    #[test]
    fn pick_items_whole_basket() {
        let basket = json!({"snapshot": "S", "items": [
            {"id": 7, "snapshot": "a", "isAvailable": true},
            {"id": 8, "snapshot": "b", "isAvailable": true}
        ]});
        let all = |s: &str| Selection::All { snapshot: s.into() };
        assert_eq!(pick_items(&all("S"), &basket).unwrap(), vec![7, 8]);
        assert_eq!(
            pick_items(&all("old"), &basket).unwrap_err().code,
            "basket_changed"
        );
        assert_eq!(
            pick_items(&all("S"), &json!({"snapshot": "S", "items": []}))
                .unwrap_err()
                .code,
            "empty_basket"
        );
        let unavailable =
            json!({"snapshot": "S", "items": [{"id": 7, "snapshot": "a", "isAvailable": false}]});
        assert_eq!(
            pick_items(&all("S"), &unavailable).unwrap_err().code,
            "basket_changed"
        );
    }

    #[test]
    fn pick_items_rejects_missing_item() {
        let basket = json!({"snapshot": "S", "items": [
            {"id": 7, "snapshot": "a", "isAvailable": true},
            {"id": 9, "snapshot": "c", "isAvailable": false}
        ]});
        let items = |v: Vec<(i64, Option<&str>)>| {
            Selection::Items(
                v.into_iter()
                    .map(|(i, s)| (i, s.map(String::from)))
                    .collect(),
            )
        };
        assert_eq!(
            pick_items(&items(vec![(7, Some("a"))]), &basket).unwrap(),
            vec![7]
        );
        assert_eq!(
            pick_items(&items(vec![(7, Some("zz"))]), &basket)
                .unwrap_err()
                .code,
            "basket_changed"
        );
        assert_eq!(
            pick_items(&items(vec![(8, None)]), &basket)
                .unwrap_err()
                .code,
            "basket_changed"
        );
        assert_eq!(
            pick_items(&items(vec![(9, None)]), &basket)
                .unwrap_err()
                .code,
            "basket_changed"
        );
        assert_eq!(
            pick_items(&items(vec![]), &basket).unwrap_err().code,
            "empty_basket"
        );
    }
}
