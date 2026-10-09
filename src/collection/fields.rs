//! Типы полей коллекции и преобразование значений из формы в JSON и обратно.
//!
//! Одиночное значение хранится скаляром или `null`, множественное — массивом.
//! Список хранит id варианта, привязка к записи — id записи, файл — id файла.

use chrono::NaiveDate;
use serde::Serialize;
use serde_json::Value;

use super::{Field, FieldOption};

#[derive(Debug, Clone, Copy, Serialize)]
pub struct FieldKind {
    pub code: &'static str,
    pub name: &'static str,
    /// Можно ли сделать поле множественным.
    pub multiple: bool,
}

pub const KINDS: &[FieldKind] = &[
    FieldKind {
        code: "string",
        name: "Строка",
        multiple: true,
    },
    FieldKind {
        code: "text",
        name: "Текст",
        multiple: false,
    },
    FieldKind {
        code: "number",
        name: "Число",
        multiple: true,
    },
    FieldKind {
        code: "boolean",
        name: "Да/Нет",
        multiple: false,
    },
    FieldKind {
        code: "date",
        name: "Дата",
        multiple: true,
    },
    FieldKind {
        code: "list",
        name: "Список",
        multiple: true,
    },
    FieldKind {
        code: "element",
        name: "Привязка к записи",
        multiple: true,
    },
    FieldKind {
        code: "file",
        name: "Файл",
        multiple: true,
    },
];

pub fn kind(code: &str) -> Option<&'static FieldKind> {
    KINDS.iter().find(|k| k.code == code)
}

/// Разбирает значение поля из полей формы. `raws` — все значения,
/// пришедшие под ключом поля (пусто — поле не пришло, так браузер передаёт
/// снятый чекбокс). Множественные строки, числа и даты вводятся по одному на строку,
/// id привязок — через запятую или пробел. `enums` — варианты поля-списка.
pub fn parse_value(prop: &Field, raws: &[&str], enums: &[FieldOption]) -> Result<Value, String> {
    if prop.kind == "boolean" {
        return Ok(Value::Bool(raws.iter().any(|s| !s.trim().is_empty())));
    }

    let items: Vec<&str> = raws
        .iter()
        .flat_map(|raw| -> Box<dyn Iterator<Item = &str>> {
            match prop.kind.as_str() {
                "text" => Box::new(std::iter::once(*raw)),
                "element" | "file" | "list" => {
                    Box::new(raw.split(|c: char| c == ',' || c.is_whitespace()))
                }
                _ => Box::new(raw.lines()),
            }
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    let mut values = Vec::with_capacity(items.len());
    for item in items {
        let value = parse_item(prop, item, enums)?;
        if !values.contains(&value) {
            values.push(value);
        }
    }

    let value = if prop.multiple {
        Value::Array(values)
    } else if values.len() > 1 {
        return Err(format!("«{}»: допускается одно значение", prop.name));
    } else {
        values.pop().unwrap_or(Value::Null)
    };
    if prop.is_required && is_empty(&value) {
        return Err(format!("«{}»: обязательное поле", prop.name));
    }
    Ok(value)
}

fn parse_item(prop: &Field, s: &str, enums: &[FieldOption]) -> Result<Value, String> {
    Ok(match prop.kind.as_str() {
        "string" | "text" => Value::String(s.to_string()),
        "number" => {
            let s = s.replace(',', ".");
            if let Ok(i) = s.parse::<i64>() {
                Value::from(i)
            } else {
                let f: f64 = s
                    .parse()
                    .ok()
                    .filter(|f: &f64| f.is_finite())
                    .ok_or_else(|| format!("«{}»: ожидается число", prop.name))?;
                Value::from(f)
            }
        }
        "date" => {
            let d = NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|_| format!("«{}»: ожидается дата ГГГГ-ММ-ДД", prop.name))?;
            Value::String(d.format("%Y-%m-%d").to_string())
        }
        "list" => {
            let id = parse_id(prop, s)?;
            if !enums.iter().any(|e| e.id == id) {
                return Err(format!("«{}»: неизвестный вариант", prop.name));
            }
            Value::from(id)
        }
        "element" | "file" => Value::from(parse_id(prop, s)?),
        kind => return Err(format!("неизвестный тип поля {kind}")),
    })
}

fn parse_id(prop: &Field, s: &str) -> Result<i64, String> {
    s.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| format!("«{}»: ожидается id, получено «{s}»", prop.name))
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// Id из значения поля-привязки или файла (скаляр или массив).
pub fn ids(value: &Value) -> Vec<i64> {
    match value {
        Value::Array(items) => items.iter().filter_map(Value::as_i64).collect(),
        other => other.as_i64().into_iter().collect(),
    }
}

/// Значения поля для подстановки обратно в поля формы: по одному на
/// каждый выбранный вариант / файл, либо одна строка для текстовых полей.
pub fn to_form_values(prop: &Field, value: &Value) -> Vec<String> {
    let scalar = |v: &Value| match v {
        Value::Null | Value::Bool(false) => None,
        Value::Bool(true) => Some("on".to_string()),
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    };
    let items: Vec<String> = match value {
        Value::Array(items) => items.iter().filter_map(scalar).collect(),
        other => scalar(other).into_iter().collect(),
    };
    match prop.kind.as_str() {
        "list" | "file" => items,
        "element" => vec![items.join(", ")],
        _ if prop.multiple => vec![items.join("\n")],
        _ => items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prop(kind: &str, required: bool) -> Field {
        Field {
            id: 1,
            collection_id: 1,
            code: "p".into(),
            name: "P".into(),
            kind: kind.into(),
            is_required: required,
            sort: 500,
            multiple: false,
            link_collection_id: None,
            user_type: String::new(),
            in_basket: false,
            offer_tree: false,
        }
    }

    fn multi(kind: &str) -> Field {
        Field {
            multiple: true,
            ..prop(kind, false)
        }
    }

    fn enum_value(id: i64) -> FieldOption {
        FieldOption {
            id,
            field_id: 1,
            value: format!("v{id}"),
            xml_id: format!("x{id}"),
            sort: 500,
            is_default: false,
        }
    }

    #[test]
    fn numbers() {
        let p = prop("number", false);
        assert_eq!(parse_value(&p, &["42"], &[]), Ok(Value::from(42)));
        assert_eq!(parse_value(&p, &["1,5"], &[]), Ok(Value::from(1.5)));
        assert!(parse_value(&p, &["abc"], &[]).is_err());
        assert_eq!(parse_value(&p, &[" "], &[]), Ok(Value::Null));
    }

    #[test]
    fn required_and_bool() {
        assert!(parse_value(&prop("string", true), &[], &[]).is_err());
        assert!(parse_value(&multi("string"), &[""], &[]).is_ok());
        assert_eq!(
            parse_value(&prop("boolean", false), &[], &[]),
            Ok(Value::Bool(false))
        );
        assert_eq!(
            parse_value(&prop("boolean", false), &["on"], &[]),
            Ok(Value::Bool(true))
        );
    }

    #[test]
    fn dates() {
        let p = prop("date", false);
        assert_eq!(
            parse_value(&p, &["2026-09-27"], &[]),
            Ok(Value::String("2026-09-27".into()))
        );
        assert!(parse_value(&p, &["27.09.2026"], &[]).is_err());
    }

    #[test]
    fn multiple_lines_and_ids() {
        assert_eq!(
            parse_value(&multi("string"), &["+7 900\n\n+7 901 "], &[]),
            Ok(serde_json::json!(["+7 900", "+7 901"]))
        );
        assert_eq!(
            parse_value(&multi("element"), &["3, 5 7"], &[]),
            Ok(serde_json::json!([3, 5, 7]))
        );
        assert!(parse_value(&prop("element", false), &["3, 5"], &[]).is_err());
        assert!(parse_value(&prop("element", false), &["x"], &[]).is_err());
    }

    #[test]
    fn lists() {
        let enums = [enum_value(10), enum_value(11)];
        assert_eq!(
            parse_value(&prop("list", false), &["11"], &enums),
            Ok(Value::from(11))
        );
        assert_eq!(
            parse_value(&multi("list"), &["10", "11"], &enums),
            Ok(serde_json::json!([10, 11]))
        );
        assert!(parse_value(&prop("list", false), &["12"], &enums).is_err());
    }

    #[test]
    fn form_roundtrip() {
        let p = multi("string");
        assert_eq!(
            to_form_values(&p, &serde_json::json!(["a", "b"])),
            vec!["a\nb".to_string()]
        );
        assert_eq!(
            to_form_values(&multi("list"), &serde_json::json!([1, 2])),
            vec!["1".to_string(), "2".to_string()]
        );
        assert_eq!(
            to_form_values(&multi("element"), &serde_json::json!([1, 2])),
            vec!["1, 2".to_string()]
        );
        assert!(to_form_values(&prop("file", false), &Value::Null).is_empty());
    }
}
