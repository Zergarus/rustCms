//! Профиль пользователя (`/profile`, `ProfileService` модуля bxapi).

use axum::{
    body::Bytes,
    extract::{FromRequest, Multipart, Request, State},
    http::header::CONTENT_TYPE,
};
use axum_extra::extract::cookie::CookieJar;
use serde_json::{Map, Value, json};

use super::{BxError, BxResult, auth::require_user, parse_body, success};
use crate::{
    files,
    sale::history,
    state::AppState,
    users::{self, Column, ProfileRow, profile_column},
};

/// Поле Битрикса в camelCase: `PERSONAL_PHONE` → `personalPhone`.
pub(super) fn camel(field: &str) -> String {
    let mut out = String::new();
    for word in field
        .to_lowercase()
        .split(['_', '.'])
        .filter(|w| !w.is_empty())
    {
        if out.is_empty() {
            out.push_str(word);
        } else {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
        }
    }
    out
}

fn input_value<'a>(input: &'a Map<String, Value>, field: &str) -> Option<&'a Value> {
    input.get(&camel(field)).or_else(|| input.get(field))
}

/// Изменения из ввода: только поля из `editable`, camelCase или UPPER_SNAKE; строки
/// обрезаются, числа — строкой. Аватар — файл, его здесь нет.
pub(super) fn pick_updates(editable: &[&str], input: &Map<String, Value>) -> Vec<(Column, String)> {
    editable
        .iter()
        .filter_map(|field| {
            let column = profile_column(field).filter(|c| *c != Column::Photo)?;
            let value = match input_value(input, field)? {
                Value::String(s) => s.trim().to_string(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => if *b { "Y" } else { "N" }.to_string(),
                Value::Null => String::new(),
                _ => return None,
            };
            Some((column, value))
        })
        .collect()
}

/// Минимальная длина пароля (политика паролей Битрикса по умолчанию).
const MIN_PASSWORD: usize = 6;

/// Новый пароль из `password` и `passwordRepeat`; оба пусты — пароль не меняется.
/// Ошибка — (поле, код или текст).
pub(super) fn check_password(
    password: &str,
    repeat: &str,
) -> Result<Option<String>, (String, String)> {
    if password.is_empty() && repeat.is_empty() {
        return Ok(None);
    }
    if password != repeat {
        return Err(("passwordRepeat".into(), "password_confirm_mismatch".into()));
    }
    if password.chars().count() < MIN_PASSWORD {
        return Err((
            "password".into(),
            format!("Пароль должен быть не менее {MIN_PASSWORD} символов."),
        ));
    }
    Ok(Some(password.to_string()))
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(s)) => {
            matches!(
                s.trim().to_lowercase().as_str(),
                "1" | "y" | "yes" | "true" | "on"
            )
        }
        _ => false,
    }
}

/// Профиль в формате `GET /profile`.
async fn profile_json(state: &AppState, row: &ProfileRow) -> Result<Value, BxError> {
    let cfg = &state.project.profile;
    let mut out = Map::new();
    out.insert("id".into(), json!(row.id));
    out.insert("login".into(), json!(row.login));
    out.insert("email".into(), json!(row.email.clone().unwrap_or_default()));
    for field in &cfg.fields {
        let Some(column) = profile_column(field) else {
            continue;
        };
        let value = match column {
            Column::Name => json!(row.name),
            Column::LastName => json!(row.last_name),
            Column::SecondName => json!(row.second_name),
            Column::Email => json!(row.email.clone().unwrap_or_default()),
            Column::Phone => json!(row.phone),
            Column::City => json!(row.city),
            Column::WorkPosition => json!(row.work_position),
            Column::Photo => json!(row.photo),
            Column::Extra(key) => row.extra.get(&key).cloned().unwrap_or(Value::Null),
        };
        out.insert(camel(field), value);
    }
    let editable: Vec<String> = cfg.editable_fields.iter().map(|f| camel(f)).collect();
    out.insert("editable".into(), json!(editable));
    if cfg.include_orders_count {
        out.insert(
            "ordersCount".into(),
            json!(history::count(&state.db, row.id).await?),
        );
    }
    Ok(Value::Object(out))
}

async fn current(state: &AppState, jar: &CookieJar) -> Result<ProfileRow, BxError> {
    let id = require_user(state, jar).await?;
    // Сессия есть, а пользователь выключен или удалён — как без сессии
    users::load_profile(&state.db, id).await?.ok_or_else(|| {
        BxError::with_status(
            axum::http::StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Требуется авторизация",
        )
    })
}

/// `GET /profile`
pub async fn get_profile(State(state): State<AppState>, jar: CookieJar) -> BxResult {
    let row = current(&state, &jar).await?;
    Ok(success(profile_json(&state, &row).await?))
}

/// Загруженный файл аватара.
struct Upload {
    name: String,
    content_type: String,
    data: Bytes,
}

/// Расширения аватара.
const PHOTO_EXTENSIONS: [&str; 5] = ["jpg", "jpeg", "png", "gif", "webp"];

/// Ввод `POST /profile`: поля и файл аватара (из multipart).
async fn read_input(
    state: &AppState,
    request: Request,
) -> Result<(Map<String, Value>, Option<Upload>), BxError> {
    let multipart = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("multipart/form-data"));
    if !multipart {
        let body = Bytes::from_request(request, state)
            .await
            .map_err(|e| BxError::bad_request("invalid_body", e.body_text()))?;
        return Ok((parse_body(&body)?, None));
    }
    let bad = |e: axum::extract::multipart::MultipartError| {
        BxError::bad_request("invalid_body", e.body_text())
    };
    let mut form = Multipart::from_request(request, state)
        .await
        .map_err(|e| BxError::bad_request("invalid_body", e.body_text()))?;
    let mut input = Map::new();
    let mut upload = None;
    while let Some(field) = form.next_field().await.map_err(bad)? {
        let name = field.name().unwrap_or_default().to_string();
        if name == "personalPhoto" || name == "PERSONAL_PHOTO" {
            let file_name = field.file_name().unwrap_or_default().to_string();
            let content_type = field.content_type().unwrap_or_default().to_string();
            let data = field.bytes().await.map_err(bad)?;
            if !data.is_empty() {
                upload = Some(Upload {
                    name: file_name,
                    content_type,
                    data,
                });
            }
        } else {
            let text = field.text().await.map_err(bad)?;
            input.insert(name, Value::String(text));
        }
    }
    Ok((input, upload))
}

/// `POST /profile`
pub async fn update_profile(
    State(state): State<AppState>,
    jar: CookieJar,
    request: Request,
) -> BxResult {
    let row = current(&state, &jar).await?;
    let (input, upload) = read_input(&state, request).await?;
    let editable = &state.project.profile.editable_fields;
    let photo_editable = editable
        .iter()
        .any(|f| profile_column(f) == Some(Column::Photo));
    let mut errors = Map::new();

    let updates = pick_updates(editable, &input);
    if let Some((_, email)) = updates.iter().find(|(c, _)| *c == Column::Email)
        && !email.is_empty()
    {
        if !users::is_valid_email(email) {
            errors.insert("email".into(), json!("Неверный email"));
        } else if users::email_taken(&state.db, email, row.id).await? {
            errors.insert(
                "email".into(),
                json!("Пользователь с таким email уже существует"),
            );
        }
    }
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let password = match check_password(&text("password"), &text("passwordRepeat")) {
        Ok(p) => p,
        Err((field, code)) => {
            errors.insert(field, json!(code));
            None
        }
    };
    let upload = upload.filter(|_| photo_editable);
    if let Some(u) = &upload {
        let ext = u.name.rsplit_once('.').map(|(_, e)| e.to_lowercase());
        let is_image = ext.is_some_and(|e| PHOTO_EXTENSIONS.contains(&e.as_str()))
            && files::image_size(&u.data).is_some();
        if !is_image {
            errors.insert("personalPhoto".into(), json!("upload_failed"));
        }
    }
    if !errors.is_empty() {
        return Err(
            BxError::bad_request("profile_update_failed", "Не удалось обновить профиль")
                .with_custom(json!({ "fields": errors })),
        );
    }

    let photo = match upload {
        Some(u) => {
            match files::save(
                &state.db,
                &state.config.upload_dir,
                "main",
                &u.name,
                &u.content_type,
                &u.data,
            )
            .await
            {
                Ok(f) => Some(Some(f.id)),
                Err(e) => {
                    tracing::warn!(error = %e, "аватар не сохранён");
                    return Err(BxError::bad_request(
                        "profile_update_failed",
                        "Не удалось обновить профиль",
                    )
                    .with_custom(json!({ "fields": { "personalPhoto": "upload_failed" } })));
                }
            }
        }
        None if photo_editable
            && truthy(
                input
                    .get("personalPhotoDelete")
                    .or(input.get("PERSONAL_PHOTO_DELETE")),
            ) =>
        {
            Some(None)
        }
        None => None,
    };
    let hash = match password {
        Some(p) => Some(crate::auth::hash_password(p).await.map_err(|e| {
            tracing::error!(error = %e, "хеш пароля");
            BxError::numeric(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error",
            )
        })?),
        None => None,
    };
    users::update_profile(&state.db, row.id, &updates, hash, photo).await?;
    let row = current(&state, &jar).await?;
    Ok(success(profile_json(&state, &row).await?))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::users::Column;

    #[test]
    fn pick_updates_only_editable() {
        let input = json!({
            "name": " Иван ",
            "PERSONAL_PHONE": 79990001122u64,
            "login": "x",
            "isAdmin": true,
            "email": "a@b.ru",
        });
        let updates = pick_updates(&["NAME", "PERSONAL_PHONE"], input.as_object().unwrap());
        assert_eq!(
            updates,
            vec![
                (Column::Name, "Иван".to_string()),
                (Column::Phone, "79990001122".to_string())
            ]
        );
        // Фото — файл, текстом не принимается
        let photo = json!({"personalPhoto": "/etc/passwd"});
        assert!(pick_updates(&["PERSONAL_PHOTO"], photo.as_object().unwrap()).is_empty());
    }

    #[test]
    fn password_rules() {
        assert_eq!(check_password("", ""), Ok(None));
        assert_eq!(
            check_password("abc", "abd"),
            Err(("passwordRepeat".into(), "password_confirm_mismatch".into()))
        );
        assert_eq!(
            check_password("abc", "abc"),
            Err((
                "password".into(),
                "Пароль должен быть не менее 6 символов.".into()
            ))
        );
        assert_eq!(
            check_password("secret1", "secret1"),
            Ok(Some("secret1".into()))
        );
    }

    #[test]
    fn camel_names() {
        assert_eq!(camel("PERSONAL_PHONE"), "personalPhone");
        assert_eq!(camel("UF_CITY_ID"), "ufCityId");
        assert_eq!(camel("NAME"), "name");
    }
}
