//! Типы свойств инфоблока и преобразование значений из формы в JSON и обратно.

use chrono::NaiveDate;
use serde::Serialize;
use serde_json::Value;

use super::Property;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct PropertyKind {
    pub code: &'static str,
    pub name: &'static str,
}

pub const KINDS: &[PropertyKind] = &[
    PropertyKind {
        code: "string",
        name: "Строка",
    },
    PropertyKind {
        code: "text",
        name: "Текст",
    },
    PropertyKind {
        code: "number",
        name: "Число",
    },
    PropertyKind {
        code: "boolean",
        name: "Да/Нет",
    },
    PropertyKind {
        code: "date",
        name: "Дата",
    },
];

pub fn is_valid_kind(kind: &str) -> bool {
    KINDS.iter().any(|k| k.code == kind)
}

/// Разбирает значение свойства из поля формы. `raw` = None — поле не пришло
/// (так браузер передаёт снятый чекбокс).
pub fn parse_value(prop: &Property, raw: Option<&str>) -> Result<Value, String> {
    let raw = raw.map(str::trim).filter(|s| !s.is_empty());
    let value = match (prop.kind.as_str(), raw) {
        ("boolean", raw) => Value::Bool(raw.is_some()),
        (_, None) => Value::Null,
        ("string" | "text", Some(s)) => Value::String(s.to_string()),
        ("number", Some(s)) => {
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
        ("date", Some(s)) => {
            let d = NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|_| format!("«{}»: ожидается дата ГГГГ-ММ-ДД", prop.name))?;
            Value::String(d.format("%Y-%m-%d").to_string())
        }
        (kind, _) => return Err(format!("неизвестный тип свойства {kind}")),
    };
    if prop.is_required && value.is_null() {
        return Err(format!("«{}»: обязательное поле", prop.name));
    }
    Ok(value)
}

/// Значение свойства для подстановки обратно в поле формы.
pub fn to_form_value(value: &Value) -> Option<String> {
    match value {
        Value::Null | Value::Bool(false) => None,
        Value::Bool(true) => Some("on".into()),
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prop(kind: &str, required: bool) -> Property {
        Property {
            id: 1,
            iblock_id: 1,
            code: "p".into(),
            name: "P".into(),
            kind: kind.into(),
            is_required: required,
            sort: 500,
        }
    }

    #[test]
    fn numbers() {
        assert_eq!(
            parse_value(&prop("number", false), Some("42")),
            Ok(Value::from(42))
        );
        assert_eq!(
            parse_value(&prop("number", false), Some("1,5")),
            Ok(Value::from(1.5))
        );
        assert!(parse_value(&prop("number", false), Some("abc")).is_err());
        assert_eq!(
            parse_value(&prop("number", false), Some(" ")),
            Ok(Value::Null)
        );
    }

    #[test]
    fn required_and_bool() {
        assert!(parse_value(&prop("string", true), None).is_err());
        assert_eq!(
            parse_value(&prop("boolean", false), None),
            Ok(Value::Bool(false))
        );
        assert_eq!(
            parse_value(&prop("boolean", false), Some("on")),
            Ok(Value::Bool(true))
        );
    }

    #[test]
    fn dates() {
        assert_eq!(
            parse_value(&prop("date", false), Some("2026-09-27")),
            Ok(Value::String("2026-09-27".into()))
        );
        assert!(parse_value(&prop("date", false), Some("27.09.2026")).is_err());
    }
}
