//! Регистрация посетителя, подтверждение почты, восстановление и сброс пароля.
//!
//! Контракт — как у модуля `mediagroup.bxapi`: прикладные ошибки уходят с HTTP 200
//! и `errors[0].code`, которые различает витрина. Письма отправляются
//! импортированными шаблонами `NEW_USER_CONFIRM` и `USER_PASS_REQUEST`;
//! отсутствие шаблона не ломает сценарий, только пишет предупреждение.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration as StdDuration, Instant},
};

use axum::{body::Bytes, extract::State, response::IntoResponse};
use axum_extra::extract::cookie::CookieJar;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{BxError, BxResult, auth, parse_body, success};
use crate::{auth::hash_password, mail, state::AppState};

/// Подтверждение регистрации живёт неделю, сброс пароля — два часа.
const CONFIRM_TTL_DAYS: i64 = 7;
const RESET_TTL_HOURS: i64 = 2;
const PASSWORD_MIN: usize = 6;
/// Не больше пяти попыток на логин за 15 минут, счётчик в памяти процесса.
const ATTEMPT_LIMIT: u32 = 5;
const ATTEMPT_WINDOW_SECS: u64 = 15 * 60;

static ATTEMPTS: Mutex<Option<HashMap<String, (u32, Instant)>>> = Mutex::new(None);

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn new_token() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

fn text(body: &serde_json::Map<String, Value>, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn bad(code: &str, message: &str) -> BxError {
    BxError::new(code, message)
}

/// Счётчик попыток: подтверждение кода и сброс пароля нельзя перебирать.
fn throttle(login: &str) -> Result<(), BxError> {
    let mut guard = ATTEMPTS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    let entry = map.entry(login.to_lowercase()).or_insert((0, now));
    if now.duration_since(entry.1) > StdDuration::from_secs(ATTEMPT_WINDOW_SECS) {
        *entry = (0, now);
    }
    entry.0 += 1;
    if entry.0 > ATTEMPT_LIMIT {
        return Err(bad(
            "too_many_requests",
            "Слишком много попыток. Попробуйте позже.",
        ));
    }
    Ok(())
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

/// Письмо по шаблону; сбой почты не отменяет регистрацию.
async fn send(state: &AppState, event: &str, fields: HashMap<String, String>) {
    if let Err(e) = mail::send_event(&state.db, &state.config, event, &fields).await {
        tracing::error!(error = ?e, event, "не удалось отправить письмо авторизации");
    }
}

pub async fn register(State(state): State<AppState>, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let name = text(&body, "name");
    let email = text(&body, "email");
    let phone = text(&body, "phone");
    let password = text(&body, "password");
    let confirm = text(&body, "passwordConfirm");
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

    let exists: Option<i64> =
        sqlx::query_scalar("SELECT id FROM users WHERE login = $1 OR lower(email) = $1 LIMIT 1")
            .bind(&email)
            .fetch_optional(&state.db)
            .await?;
    if exists.is_some() {
        return Err(bad("email_exists", "Пользователь с такой почтой уже есть."));
    }

    let hash = hash_password(password).await?;
    let user_id: i64 = sqlx::query_scalar(
        "INSERT INTO users (login, email, password_hash, name, phone, active)
         VALUES ($1, $1, $2, $3, $4, FALSE) RETURNING id",
    )
    .bind(&email)
    .bind(hash)
    .bind(&name)
    .bind(&phone)
    .fetch_one(&state.db)
    .await?;

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
    .execute(&state.db)
    .await?;

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

pub async fn confirm(State(state): State<AppState>, jar: CookieJar, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let login = text(&body, "login");
    let code = text(&body, "confirmCode");
    if login.is_empty() || code.is_empty() {
        return Err(BxError::bad_request(
            "confirm_data_required",
            "Укажите логин и код подтверждения.",
        ));
    }
    throttle(&login)?;

    let user_id: Option<i64> = sqlx::query_scalar(
        "SELECT u.id FROM users u
         JOIN auth_tokens t ON t.user_id = u.id
         WHERE t.kind = 'confirm' AND t.expires_at > now() AND t.token_hash = $2
           AND (u.login = $1 OR lower(u.email) = lower($1))
         LIMIT 1",
    )
    .bind(&login)
    .bind(token_hash(&code))
    .fetch_optional(&state.db)
    .await?;
    let Some(user_id) = user_id else {
        return Err(bad(
            "invalid_confirm_code",
            "Код подтверждения неверен или устарел.",
        ));
    };

    sqlx::query("UPDATE users SET active = TRUE WHERE id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await?;
    sqlx::query("DELETE FROM auth_tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await?;

    let (jar, user) = auth::sign_in(&state, jar, user_id, false).await?;
    Ok((
        jar,
        success(json!({ "user": user, "csrfToken": auth::csrf_token() })),
    )
        .into_response())
}

pub async fn forgot(State(state): State<AppState>, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let login = text(&body, "login");
    if login.is_empty() {
        return Err(BxError::bad_request(
            "auth_login_required",
            "Укажите логин.",
        ));
    }
    throttle(&login)?;

    // Ответ одинаковый для существующего и несуществующего аккаунта.
    let user: Option<(i64, String)> = sqlx::query_as(
        "SELECT id, login FROM users WHERE login = $1 OR lower(email) = lower($1) LIMIT 1",
    )
    .bind(&login)
    .fetch_optional(&state.db)
    .await?;

    if let Some((user_id, user_login)) = user {
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
                ("LOGIN".to_string(), user_login.clone()),
                ("EMAIL".to_string(), user_login),
                ("CHECKWORD".to_string(), word),
            ]),
        )
        .await;
    }

    Ok(success(json!({ "sent": true })))
}

pub async fn reset(State(state): State<AppState>, jar: CookieJar, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let login = text(&body, "login");
    let checkword = text(&body, "checkword");
    let password = text(&body, "password");
    let confirm = text(&body, "passwordConfirm");
    if login.is_empty() || checkword.is_empty() {
        return Err(BxError::bad_request(
            "reset_data_required",
            "Ссылка на сброс пароля неполная.",
        ));
    }
    check_password(&password, &confirm)?;
    throttle(&login)?;

    let user_id: Option<i64> = sqlx::query_scalar(
        "SELECT u.id FROM users u
         JOIN auth_tokens t ON t.user_id = u.id
         WHERE t.kind = 'reset' AND t.expires_at > now() AND t.token_hash = $2
           AND (u.login = $1 OR lower(u.email) = lower($1))
         LIMIT 1",
    )
    .bind(&login)
    .bind(token_hash(&checkword))
    .fetch_optional(&state.db)
    .await?;
    let Some(user_id) = user_id else {
        return Err(bad(
            "invalid_checkword",
            "Ссылка на сброс пароля неверна или устарела.",
        ));
    };

    let hash = hash_password(password).await?;
    sqlx::query("UPDATE users SET password_hash = $2, active = TRUE WHERE id = $1")
        .bind(user_id)
        .bind(hash)
        .execute(&state.db)
        .await?;
    sqlx::query("DELETE FROM auth_tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await?;
    // Пароль сменился — все прежние сессии недействительны.
    sqlx::query("DELETE FROM site_sessions WHERE user_id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await?;

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
