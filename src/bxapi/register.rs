//! Регистрация посетителя, подтверждение почты, восстановление и сброс пароля.
//!
//! Контракт — как у модуля `mediagroup.bxapi`: прикладные ошибки уходят с HTTP 200
//! и `errors[0].code`, которые различает витрина. Письма отправляются
//! импортированными шаблонами `NEW_USER_CONFIRM` и `USER_PASS_REQUEST`;
//! отсутствие шаблона не ломает сценарий, только пишет предупреждение.
//!
//! Подтверждение почты хранится отдельно от `active`: `active = false` — это и
//! «не подтвердил», и «заблокирован администратором», а сброс пароля не должен
//! снимать блокировку.

use std::collections::HashMap;

use axum::{body::Bytes, extract::State, http::HeaderMap, response::IntoResponse};
use axum_extra::extract::cookie::CookieJar;
use chrono::{Duration, Utc};
use serde_json::{Map, Value, json};

use super::{BxError, BxResult, auth, parse_body, success};
use crate::{auth::hash_password, mail, state::AppState};

/// Подтверждение регистрации живёт неделю, сброс пароля — два часа.
const CONFIRM_TTL_DAYS: i64 = 7;
const RESET_TTL_HOURS: i64 = 2;
const PASSWORD_MIN: usize = 6;
/// Длина логина ограничена до записи в счётчик: иначе публичный запрос
/// создаёт многомегабайтный ключ.
const MAX_LOGIN: usize = 254;
/// Не больше пяти попыток на (эндпоинт, IP, логин) за 15 минут.
const ATTEMPT_LIMIT: u32 = 5;

fn token_hash(token: &str) -> String {
    auth::token_hash(token)
}

fn new_token() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

/// Строковое поле с обрезкой пробелов — для имён, почты, логинов.
fn text(body: &Map<String, Value>, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Поле без обрезки — для паролей: вход пароль не обрезает, и регистрация не должна.
fn raw(body: &Map<String, Value>, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn bad(code: &str, message: &str) -> BxError {
    BxError::new(code, message)
}

fn client_ip(headers: &HeaderMap) -> String {
    for name in ["x-forwarded-for", "x-real-ip"] {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let first = value.split(',').next().unwrap_or("").trim();
            if !first.is_empty() {
                return first.to_string();
            }
        }
    }
    "unknown".into()
}

/// Ограничитель перебора: ключ (эндпоинт, IP, логин).
fn throttle(scope: &str, ip: &str, login: &str) -> Result<(), BxError> {
    let key = format!("{scope}|{ip}|{}", login.to_lowercase());
    if crate::throttle::check(&key, ATTEMPT_LIMIT) {
        Ok(())
    } else {
        Err(bad(
            "too_many_requests",
            "Слишком много попыток. Попробуйте позже.",
        ))
    }
}

/// Успешная попытка не должна расходовать чужой бюджет.
fn forgive(scope: &str, ip: &str, login: &str) {
    crate::throttle::forgive(&format!("{scope}|{ip}|{}", login.to_lowercase()));
}

fn normalize_email(value: &str) -> Option<String> {
    let email = value.trim().to_lowercase();
    let (local, domain) = email.split_once('@')?;
    if local.is_empty() || !domain.contains('.') || email.contains(char::is_whitespace) {
        return None;
    }
    Some(email)
}

fn check_password(password: &str, confirm: &str) -> Result<(), BxError> {
    if password.chars().count() < PASSWORD_MIN {
        return Err(bad(
            "weak_password",
            "Пароль должен быть не менее 6 символов.",
        ));
    }
    if password != confirm {
        return Err(bad("password_mismatch", "Пароли не совпадают."));
    }
    Ok(())
}

/// Письмо по шаблону; сбой почты не отменяет сценарий.
async fn send(state: &AppState, event: &str, fields: HashMap<String, String>) {
    if let Err(e) = mail::send_event(&state.db, &state.config, event, &fields).await {
        tracing::error!(error = ?e, event, "не удалось отправить письмо авторизации");
    }
}

/// Забирает токен из БД атомарно: повторный запрос с тем же кодом получит пусто.
async fn consume(
    db: &sqlx::PgPool,
    kind: &str,
    code: &str,
    login: &str,
) -> Result<Option<i64>, BxError> {
    Ok(sqlx::query_scalar(
        "DELETE FROM auth_tokens t USING users u
         WHERE t.user_id = u.id
           AND t.kind = $1
           AND t.token_hash = $2
           AND t.expires_at > now()
           AND (lower(u.login) = lower($3) OR lower(u.email) = lower($3))
         RETURNING t.user_id",
    )
    .bind(kind)
    .bind(token_hash(code))
    .bind(login)
    .fetch_optional(db)
    .await?)
}

pub async fn register(State(state): State<AppState>, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let name = text(&body, "name");
    let email = text(&body, "email");
    let phone = text(&body, "phone");
    let password = raw(&body, "password");
    let confirm = raw(&body, "passwordConfirm");
    let personal_data = body
        .get("personalData")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if name.is_empty() {
        return Err(bad("name_required", "Укажите имя."));
    }
    let Some(email) = normalize_email(&email) else {
        return Err(bad("invalid_email", "Некорректный адрес почты."));
    };
    check_password(&password, &confirm)?;
    if !personal_data {
        return Err(bad(
            "personal_data_required",
            "Нужно согласие на обработку персональных данных.",
        ));
    }

    // Импортированный пользователь может иметь другой регистр логина и пустой email.
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM users
         WHERE lower(login) = $1 OR lower(email) = $1
         LIMIT 1",
    )
    .bind(&email)
    .fetch_optional(&state.db)
    .await?;
    if exists.is_some() {
        return Err(bad("email_exists", "Пользователь с такой почтой уже есть."));
    }

    let hash = hash_password(password).await?;
    let mut tx = state.db.begin().await?;
    let user_id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO users (login, email, password_hash, name, phone, active, confirmed)
         VALUES ($1, $1, $2, $3, $4, FALSE, FALSE)
         ON CONFLICT (login) DO NOTHING
         RETURNING id",
    )
    .bind(&email)
    .bind(hash)
    .bind(&name)
    .bind(&phone)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user_id) = user_id else {
        return Err(bad("email_exists", "Пользователь с такой почтой уже есть."));
    };

    let code = new_token();
    sqlx::query(
        "INSERT INTO auth_tokens (user_id, kind, token_hash, expires_at)
         VALUES ($1, 'confirm', $2, $3)
         ON CONFLICT (kind, user_id) DO UPDATE
             SET token_hash = EXCLUDED.token_hash, expires_at = EXCLUDED.expires_at,
                 created_at = now()",
    )
    .bind(user_id)
    .bind(token_hash(&code))
    .bind(Utc::now() + Duration::days(CONFIRM_TTL_DAYS))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    send(
        &state,
        "NEW_USER_CONFIRM",
        HashMap::from([
            ("USER_ID".to_string(), user_id.to_string()),
            ("LOGIN".to_string(), email.clone()),
            ("EMAIL".to_string(), email.clone()),
            ("NAME".to_string(), name),
            ("CONFIRM_CODE".to_string(), code),
        ]),
    )
    .await;

    Ok(success(json!({
        "user": Value::Null,
        "csrfToken": auth::csrf_token(),
        "pending": true,
    })))
}

pub async fn confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    body: Bytes,
) -> BxResult {
    let body = parse_body(&body)?;
    let login = text(&body, "login");
    let code = text(&body, "confirmCode");
    if login.is_empty() || code.is_empty() {
        return Err(BxError::bad_request(
            "confirm_data_required",
            "Укажите логин и код подтверждения.",
        ));
    }
    if login.chars().count() > MAX_LOGIN {
        return Err(bad("invalid_confirm_code", "Код подтверждения неверен."));
    }
    let ip = client_ip(&headers);
    throttle("confirm", &ip, &login)?;

    let Some(user_id) = consume(&state.db, "confirm", &code, &login).await? else {
        return Err(bad(
            "invalid_confirm_code",
            "Код подтверждения неверен или устарел.",
        ));
    };

    sqlx::query(
        "UPDATE users SET confirmed = TRUE, active = active OR NOT confirmed WHERE id = $1",
    )
    .bind(user_id)
    .execute(&state.db)
    .await?;
    forgive("confirm", &ip, &login);

    let (jar, user) = auth::sign_in(&state, jar, user_id, false).await?;
    Ok((
        jar,
        success(json!({ "user": user, "csrfToken": auth::csrf_token() })),
    )
        .into_response())
}

pub async fn forgot(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let login = text(&body, "login");
    if login.is_empty() {
        return Err(BxError::bad_request(
            "auth_login_required",
            "Укажите логин.",
        ));
    }
    if login.chars().count() > MAX_LOGIN {
        return Err(bad("login_too_long", "Слишком длинный логин."));
    }
    let ip = client_ip(&headers);
    throttle("forgot", &ip, &login)?;

    // Ответ одинаковый для существующего и несуществующего аккаунта.
    // Адрес — из профиля: логин импортированного пользователя почтой не является,
    // а смена email не меняет логин.
    let user: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT id, login, COALESCE(NULLIF(email, ''), login)
         FROM users
         WHERE lower(login) = lower($1) OR lower(email) = lower($1)
         ORDER BY (lower(login) = lower($1)) DESC, id
         LIMIT 1",
    )
    .bind(&login)
    .fetch_optional(&state.db)
    .await?;

    if let Some((user_id, user_login, address)) = user {
        let word = new_token();
        sqlx::query(
            "INSERT INTO auth_tokens (user_id, kind, token_hash, expires_at)
             VALUES ($1, 'reset', $2, $3)
             ON CONFLICT (kind, user_id) DO UPDATE
                 SET token_hash = EXCLUDED.token_hash, expires_at = EXCLUDED.expires_at,
                     created_at = now()",
        )
        .bind(user_id)
        .bind(token_hash(&word))
        .bind(Utc::now() + Duration::hours(RESET_TTL_HOURS))
        .execute(&state.db)
        .await?;
        send(
            &state,
            "USER_PASS_REQUEST",
            HashMap::from([
                ("USER_ID".to_string(), user_id.to_string()),
                ("LOGIN".to_string(), user_login),
                ("EMAIL".to_string(), address),
                ("CHECKWORD".to_string(), word),
            ]),
        )
        .await;
    }

    Ok(success(json!({ "sent": true })))
}

pub async fn reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    body: Bytes,
) -> BxResult {
    let body = parse_body(&body)?;
    let login = text(&body, "login");
    let checkword = text(&body, "checkword");
    let password = raw(&body, "password");
    let confirm = raw(&body, "passwordConfirm");
    if login.is_empty() || checkword.is_empty() {
        return Err(BxError::bad_request(
            "reset_data_required",
            "Ссылка на сброс пароля неполная.",
        ));
    }
    let ip = client_ip(&headers);
    throttle("reset", &ip, &login)?;

    let Some(user_id) = consume(&state.db, "reset", &checkword, &login).await? else {
        return Err(bad(
            "invalid_checkword",
            "Ссылка на сброс пароля неверна или устарела.",
        ));
    };
    // Пароль проверяем после токена: по мёртвой ссылке политика не утекает.
    check_password(&password, &confirm)?;

    let hash = hash_password(password).await?;
    sqlx::query(
        "UPDATE users
         SET password_hash = $2, confirmed = TRUE, active = active OR NOT confirmed
         WHERE id = $1",
    )
    .bind(user_id)
    .bind(hash)
    .execute(&state.db)
    .await?;
    sqlx::query("DELETE FROM auth_tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await?;
    // Пароль сменился — все прежние сессии недействительны, включая админские.
    sqlx::query("DELETE FROM site_sessions WHERE user_id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await?;
    crate::auth::delete_user_sessions(&state.db, user_id, None).await?;
    forgive("reset", &ip, &login);

    let active: bool = sqlx::query_scalar("SELECT active FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&state.db)
        .await?;
    if !active {
        // Заблокированного администратором не авторизуем, но пароль уже сменён.
        return Ok(success(json!({
            "user": Value::Null,
            "csrfToken": auth::csrf_token(),
            "changed": true,
            "authorized": false,
        })));
    }

    let (jar, user) = auth::sign_in(&state, jar, user_id, false).await?;
    Ok((
        jar,
        success(json!({
            "user": user,
            "csrfToken": auth::csrf_token(),
            "changed": true,
            "authorized": true,
        })),
    )
        .into_response())
}
