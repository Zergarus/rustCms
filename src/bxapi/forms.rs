//! `POST /form/{code}` — формы bxapi: проверка полей, запись в инфоблок, письмо
//! по почтовому событию. Состав форм — в настройках проекта ([`super::project::FormConfig`]).

use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde_json::{Map, Value, json};

use super::{
    BxError, BxResult, parse_body,
    project::{FormConfig, IblockWriter, Rule},
    success, to_snake,
};
use crate::{collection::Field, state::AppState, users::is_valid_email};

/// Строковое значение поля: строки обрезаются, `true`/`false` → `Y`/`N`.
fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.trim().to_string(),
        Value::Bool(true) => "Y".into(),
        Value::Bool(false) => "N".into(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn is_empty(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    }
}

/// Проверка по правилам формы; возвращает прошедшие проверку поля.
fn validate(form: &FormConfig, body: &Map<String, Value>) -> Result<Map<String, Value>, BxError> {
    let mut errors = Vec::new();
    let mut data = Map::new();
    for (field, rules) in &form.rules {
        let value = body.get(*field);
        for rule in rules.iter() {
            match rule {
                Rule::Required if is_empty(value) => errors.push(format!("{field} is required")),
                Rule::Email
                    if !is_empty(value)
                        && !is_valid_email(&text(value.unwrap_or(&Value::Null))) =>
                {
                    errors.push(format!("{field} must be a valid email"))
                }
                _ => {}
            }
        }
        if let Some(v) = value.filter(|v| !v.is_null()) {
            let v = match v {
                Value::String(s) => Value::String(s.trim().to_string()),
                other => other.clone(),
            };
            data.insert(field.to_string(), v);
        }
    }
    if errors.is_empty() {
        Ok(data)
    } else {
        // Как в bxapi: в code — HTTP-код числом
        Err(BxError::numeric(StatusCode::BAD_REQUEST, errors.join(", ")))
    }
}

/// Значение для свойства инфоблока: список — по XML_ID варианта (или id), да/нет — `Y`.
fn property_value(state_enums: &[(i64, String, String)], prop: &Field, value: &Value) -> Value {
    let one = |v: &Value| -> Option<Value> {
        let raw = text(v);
        if raw.is_empty() {
            return None;
        }
        match prop.kind.as_str() {
            "list" => state_enums
                .iter()
                .find(|(_, xml, val)| *xml == raw || *val == raw)
                .map(|(id, ..)| Value::from(*id))
                .or_else(|| {
                    raw.parse::<i64>()
                        .ok()
                        .filter(|id| state_enums.iter().any(|(e, ..)| e == id))
                        .map(Value::from)
                }),
            "number" => raw.replace(',', ".").parse::<f64>().ok().map(Value::from),
            "boolean" => Some(Value::Bool(matches!(raw.as_str(), "Y" | "1" | "true"))),
            "element" | "file" => raw.parse::<i64>().ok().map(Value::from),
            _ => Some(Value::String(raw)),
        }
    };
    let items: Vec<Value> = match value {
        Value::Array(a) => a.iter().filter_map(one).collect(),
        v => one(v).into_iter().collect(),
    };
    if prop.multiple {
        Value::Array(items)
    } else {
        items.into_iter().next().unwrap_or(Value::Null)
    }
}

/// Создаёт элемент инфоблока из полей формы. Цели: `NAME`, `PREVIEW_TEXT`,
/// `DETAIL_TEXT` или код свойства.
async fn write_element(
    state: &AppState,
    writer: &IblockWriter,
    data: &Map<String, Value>,
) -> Result<i64, BxError> {
    let snap = state.registry.snapshot(&state.db).await?;
    let schema = snap.by_code(&to_snake(writer.api_code)).ok_or_else(|| {
        BxError::new(
            "form_writer_failed",
            format!("Инфоблок {} не найден", writer.api_code),
        )
    })?;
    let mut name = String::new();
    let mut preview = String::new();
    let mut detail = String::new();
    let mut props = Map::new();
    for (field, target) in &writer.field_mapping {
        let Some(value) = data.get(*field) else {
            continue;
        };
        match *target {
            "NAME" => name = text(value),
            "PREVIEW_TEXT" => preview = text(value),
            "DETAIL_TEXT" => detail = text(value),
            code => {
                let Some(prop) = schema.prop(&to_snake(code)) else {
                    tracing::warn!(
                        form_field = field,
                        property = code,
                        "свойство для поля формы не найдено"
                    );
                    continue;
                };
                let enums: Vec<(i64, String, String)> = snap
                    .property_enums(prop.id)
                    .into_iter()
                    .map(|e| (e.id, e.xml_id.clone(), e.value.clone()))
                    .collect();
                props.insert(prop.code.clone(), property_value(&enums, prop, value));
            }
        }
    }
    if name.is_empty() {
        name = format!("Заявка от {}", chrono::Utc::now().format("%d.%m.%Y %H:%M"));
    }
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO collection_items (collection_id, code, name, preview_text, detail_text, field_values)
         VALUES ($1, '', $2, $3, $4, $5) RETURNING id",
    )
    .bind(schema.collection.id)
    .bind(name)
    .bind(preview)
    .bind(detail)
    .bind(sqlx::types::Json(props))
    .fetch_one(&state.db)
    .await?;
    Ok(id)
}

pub async fn submit(
    State(state): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> BxResult {
    let body = parse_body(&body)?;
    let form = state.project.form(&code).ok_or_else(|| {
        BxError::with_status(
            StatusCode::NOT_FOUND,
            "form_not_found",
            format!("Форма {code} не найдена"),
        )
    })?;
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string());
    for guard in &state.project.form_guards {
        guard.check(&state, &code, &body, ip.as_deref()).await?;
    }

    let data = validate(form, &body)?;
    let id = match &form.writer {
        Some(writer) => Some(write_element(&state, writer, &data).await?),
        None => None,
    };
    if let Some(notifier) = &form.notifier {
        let mut fields: HashMap<String, String> = HashMap::new();
        for (field, key) in &notifier.field_mapping {
            if let Some(v) = data.get(*field) {
                // Несколько полей формы в одно поле письма — через перевод строки
                let entry = fields.entry(key.to_string()).or_default();
                if !entry.is_empty() {
                    entry.push('\n');
                }
                entry.push_str(&text(v));
            }
        }
        // Письмо — в фоне: медленный SMTP не задерживает ответ формы
        let (db, config, event) = (state.db.clone(), state.config.clone(), notifier.event);
        tokio::spawn(async move {
            if let Err(e) = crate::mail::send_event(&db, &config, event, &fields).await {
                tracing::error!(error = ?e, event, "не удалось отправить письмо формы");
            }
        });
    }
    Ok(success(json!({ "id": id, "data": data })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> FormConfig {
        FormConfig {
            code: "f",
            rules: vec![
                ("name", &[Rule::Required]),
                ("email", &[Rule::Required, Rule::Email]),
                ("note", &[]),
            ],
            writer: None,
            notifier: None,
        }
    }

    #[test]
    fn validation() {
        let ok: Map<String, Value> =
            serde_json::from_str(r#"{"name":" Иван ","email":"a@b.ru","extra":1}"#).unwrap();
        let data = validate(&form(), &ok).unwrap();
        assert_eq!(data["name"], "Иван");
        assert!(!data.contains_key("extra"));
        let bad: Map<String, Value> = serde_json::from_str(r#"{"email":"nope"}"#).unwrap();
        let err = validate(&form(), &bad).unwrap_err();
        assert!(err.message.contains("name is required"));
        assert!(err.message.contains("email must be a valid email"));
    }

    #[test]
    fn list_values() {
        let prop = Field {
            id: 1,
            collection_id: 1,
            code: "consent".into(),
            name: "Согласие".into(),
            kind: "list".into(),
            is_required: false,
            sort: 500,
            multiple: false,
            link_collection_id: None,
            user_type: String::new(),
            in_basket: false,
            offer_tree: false,
        };
        let enums = vec![(759, "Y".to_string(), "Да".to_string())];
        assert_eq!(property_value(&enums, &prop, &json!(true)), json!(759));
        assert_eq!(property_value(&enums, &prop, &json!("Y")), json!(759));
        assert_eq!(property_value(&enums, &prop, &json!(false)), Value::Null);
    }
}
