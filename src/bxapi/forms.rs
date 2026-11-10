//! `POST /form/{code}` — формы bxapi: проверка полей, запись в коллекцию, письмо
//! по почтовому событию. Состав форм — реестр [`super::forms_config`]: формы проекта
//! плюс `config/forms.toml`.

use std::collections::HashMap;

use axum::{
    body::{Body, to_bytes},
    extract::{FromRequest, Multipart, Path, Request, State},
    http::{StatusCode, header},
};
use serde_json::{Map, Value, json};

use super::{
    BxError, BxResult,
    forms_config::{FormSpec, NotifierSpec, WriterSpec},
    parse_body,
    project::Rule,
    success, to_snake,
};
use crate::{collection::Field, files, state::AppState, users::is_valid_email};

/// Предел тела формы: файлы вложений не должны съедать память.
pub(crate) const MAX_BODY_BYTES: usize = 20 * 1024 * 1024;
/// Не больше пяти вложений на заявку и 10 МБ на файл.
const MAX_FILES: usize = 5;
const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
/// Сколько заявок с одного IP на форму за окно.
const FORM_LIMIT: u32 = 20;

/// Загруженный файл формы до сохранения.
struct Upload {
    field: String,
    name: String,
    content_type: String,
    data: Vec<u8>,
}

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
fn validate(form: &FormSpec, body: &Map<String, Value>) -> Result<Map<String, Value>, BxError> {
    let mut errors = Vec::new();
    let mut data = Map::new();
    for (field, rules) in &form.rules {
        let value = body.get(field);
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
            // Блоки приходят от формы строкой JSON: проверяем и нормализуем как поле коллекции.
            "blocks" => crate::collection::blocks::from_form(&raw).ok(),
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
    writer: &WriterSpec,
    file_fields: &[(String, i64)],
    data: &Map<String, Value>,
) -> Result<i64, BxError> {
    let snap = state.registry.snapshot(&state.db).await?;
    let schema = snap.by_code(&to_snake(&writer.collection)).ok_or_else(|| {
        BxError::new(
            "form_writer_failed",
            format!("Коллекция {} не найдена", writer.collection),
        )
    })?;
    let mut name = String::new();
    let mut preview = String::new();
    let mut detail = String::new();
    let mut props = Map::new();
    for (field, target) in &writer.mapping {
        let Some(value) = data.get(field) else {
            continue;
        };
        match target.as_str() {
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
    for (code, file_id) in file_fields {
        let Some(prop) = schema.prop(&to_snake(code)) else {
            return Err(BxError::new(
                "file_field_not_found",
                format!("Файловое поле {code} не найдено в коллекции"),
            ));
        };
        if prop.kind != "file" {
            return Err(BxError::new(
                "file_field_not_file",
                format!("Поле {code} не файловое"),
            ));
        }
        if prop.multiple {
            props
                .entry(prop.code.clone())
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .expect("массив файлов")
                .push(json!(file_id));
        } else if props.insert(prop.code.clone(), json!(file_id)).is_some() {
            return Err(BxError::new(
                "file_duplicate",
                format!("В поле {code} можно загрузить только один файл"),
            ));
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
    request: Request,
) -> BxResult {
    let (parts, body) = request.into_parts();
    let ip = parts
        .headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string());
    let is_multipart = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("multipart/form-data"));
    let form = state.forms.get(&code).ok_or_else(|| {
        BxError::with_status(
            StatusCode::NOT_FOUND,
            "form_not_found",
            format!("Форма {code} не найдена"),
        )
    })?;
    let limit_key = format!("form|{}|{code}", ip.as_deref().unwrap_or("unknown"));
    if !crate::throttle::check(&limit_key, FORM_LIMIT) {
        return Err(BxError::with_status(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_requests",
            "Слишком много заявок. Попробуйте позже.",
        ));
    }
    let (body, uploads) = if is_multipart {
        read_multipart(parts, body).await?
    } else {
        let bytes = to_bytes(body, MAX_BODY_BYTES)
            .await
            .map_err(|e| BxError::bad_request("body_too_large", e.to_string()))?;
        (parse_body(&bytes)?, Vec::new())
    };

    for guard in &state.project.form_guards {
        guard.check(&state, &code, &body, ip.as_deref()).await?;
    }

    let extension = state.forms.extension(&code);
    let data = match extension.and_then(|e| e.validator.as_ref()) {
        Some(validator) => validator.validate(&body).await?,
        None => validate(form, &body)?,
    };
    let file_fields = save_files(&state, form, uploads).await?;
    let id = match extension.and_then(|e| e.writer.as_ref()) {
        Some(writer) => writer.write(&state, &data).await?,
        None => match &form.writer {
            Some(writer) => Some(write_element(&state, writer, &file_fields, &data).await?),
            None => None,
        },
    };
    match extension.and_then(|e| e.notifier.as_ref()) {
        Some(notifier) => notifier.notify(&state, &data).await,
        None => {
            if let Some(notifier) = &form.notifier {
                send_notification(&state, notifier, &data);
            }
        }
    }
    Ok(success(json!({ "id": id, "data": data })))
}

/// Разбор `multipart/form-data`: текстовые поля в карту, файлы — отдельно.
async fn read_multipart(
    parts: axum::http::request::Parts,
    body: Body,
) -> Result<(Map<String, Value>, Vec<Upload>), BxError> {
    let request = Request::from_parts(parts, body);
    let mut multipart = Multipart::from_request(request, &())
        .await
        .map_err(|e| BxError::bad_request("invalid_multipart", e.to_string()))?;
    let mut fields = Map::new();
    let mut uploads = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| BxError::bad_request("invalid_multipart", e.to_string()))?
    {
        let name = field.name().unwrap_or_default().to_string();
        match field
            .file_name()
            .map(str::to_string)
            .filter(|n| !n.is_empty())
        {
            Some(file_name) => {
                let content_type = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| BxError::bad_request("invalid_multipart", e.to_string()))?
                    .to_vec();
                uploads.push(Upload {
                    field: name,
                    name: file_name,
                    content_type,
                    data,
                });
            }
            None => {
                let value = field
                    .text()
                    .await
                    .map_err(|e| BxError::bad_request("invalid_multipart", e.to_string()))?;
                fields.insert(name, Value::String(value));
            }
        }
    }
    Ok((fields, uploads))
}

/// Сохраняет заявленные в конфиге файлы и возвращает пары «поле коллекции, id файла».
async fn save_files(
    state: &AppState,
    form: &FormSpec,
    uploads: Vec<Upload>,
) -> Result<Vec<(String, i64)>, BxError> {
    if uploads.len() > MAX_FILES {
        return Err(BxError::bad_request(
            "too_many_files",
            format!("Не больше {MAX_FILES} файлов в заявке."),
        ));
    }
    let mut saved: Vec<(String, i64)> = Vec::new();
    for upload in uploads {
        let Some((_, target)) = form.files.iter().find(|(field, _)| *field == upload.field) else {
            return Err(BxError::bad_request(
                "file_not_allowed",
                format!("Поле файла «{}» не описано в форме.", upload.field),
            ));
        };
        if upload.data.len() > MAX_FILE_BYTES {
            return Err(BxError::bad_request(
                "file_too_large",
                format!("Файл «{}» больше 10 МБ.", upload.name),
            ));
        }
        let record = files::save(
            &state.db,
            &state.config.upload_dir,
            "form",
            &upload.name,
            &upload.content_type,
            &upload.data,
        )
        .await
        .map_err(|e| BxError::bad_request("file_rejected", e.to_string()))?;
        saved.push((target.clone(), record.id));
    }
    Ok(saved)
}

/// Письмо по событию — в фоне: медленный SMTP не задерживает ответ формы.
fn send_notification(state: &AppState, notifier: &NotifierSpec, data: &Map<String, Value>) {
    let mut fields: HashMap<String, String> = HashMap::new();
    for (field, key) in &notifier.mapping {
        if let Some(v) = data.get(field) {
            // Несколько полей формы в одно поле письма — через перевод строки
            let entry = fields.entry(key.clone()).or_default();
            if !entry.is_empty() {
                entry.push('\n');
            }
            entry.push_str(&text(v));
        }
    }
    let (db, config, event) = (
        state.db.clone(),
        state.config.clone(),
        notifier.event.clone(),
    );
    tokio::spawn(async move {
        if let Err(e) = crate::mail::send_event(&db, &config, &event, &fields).await {
            tracing::error!(error = ?e, event, "не удалось отправить письмо формы");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> FormSpec {
        FormSpec {
            code: "f".into(),
            rules: vec![
                ("name".into(), vec![Rule::Required]),
                ("email".into(), vec![Rule::Required, Rule::Email]),
                ("note".into(), vec![]),
            ],
            writer: None,
            notifier: None,
            files: Vec::new(),
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
            xml_id: String::new(),
        };
        let enums = vec![(759, "Y".to_string(), "Да".to_string())];
        assert_eq!(property_value(&enums, &prop, &json!(true)), json!(759));
        assert_eq!(property_value(&enums, &prop, &json!("Y")), json!(759));
        assert_eq!(property_value(&enums, &prop, &json!(false)), Value::Null);
    }
}
