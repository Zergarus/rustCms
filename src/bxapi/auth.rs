//! Вход посетителей сайта: `auth/login`, `auth/session`, `auth/logout`.
//! Сессия — cookie [`COOKIE`] с случайным токеном, в БД только его sha256.
//! Ограничений витрины по группам нет, поэтому флаги `canSee*` всегда `true`.

use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::FromRow;

use super::{BxError, BxResult, cart::BUYER_COOKIE, parse_body, success};
use crate::{auth::hash_password, passwords, state::AppState};

pub const COOKIE: &str = "CMS_SID";
/// Сессия без «запомнить меня» — сутки, с ним — 30 дней.
const SESSION_HOURS: i64 = 24;
const REMEMBER_DAYS: i64 = 30;

#[derive(FromRow)]
struct SiteUser {
    id: i64,
    login: String,
    email: Option<String>,
    name: String,
    last_name: String,
}

impl SiteUser {
    fn json(&self) -> Value {
        let full_name = [self.name.trim(), self.last_name.trim()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        json!({
            "id": self.id,
            "login": self.login,
            "email": self.email.clone().unwrap_or_default(),
            "name": full_name,
            "canSeePrices": true,
            "canSeeStocks": true,
            "canAddToCart": true,
        })
    }
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn csrf_token() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

fn session_cookie(state: &AppState, token: String, remember: bool) -> Cookie<'static> {
    let mut cookie = Cookie::build((COOKIE, token))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.config.cookie_secure);
    if remember {
        cookie = cookie.max_age(time::Duration::days(REMEMBER_DAYS));
    }
    cookie.build()
}

async fn current_user(state: &AppState, jar: &CookieJar) -> Result<Option<SiteUser>, BxError> {
    let Some(cookie) = jar.get(COOKIE) else {
        return Ok(None);
    };
    Ok(sqlx::query_as(
        "SELECT u.id, u.login, u.email, u.name, u.last_name
         FROM site_sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = $1 AND s.expires_at > now() AND u.active",
    )
    .bind(token_hash(cookie.value()))
    .fetch_optional(&state.db)
    .await?)
}

/// Id авторизованного посетителя по cookie сессии.
pub(super) async fn current_user_id(
    state: &AppState,
    jar: &CookieJar,
) -> Result<Option<i64>, BxError> {
    Ok(current_user(state, jar).await?.map(|u| u.id))
}

fn auth_failed() -> BxError {
    BxError::with_status(
        StatusCode::UNAUTHORIZED,
        "auth_failed",
        "Неверный логин или пароль.",
    )
}

pub async fn login(State(state): State<AppState>, jar: CookieJar, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let login = body
        .get("login")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let password = body
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let remember = body
        .get("remember")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if login.is_empty() {
        return Err(BxError::bad_request(
            "auth_login_required",
            "Укажите логин.",
        ));
    }
    if password.is_empty() {
        return Err(BxError::bad_request(
            "auth_password_required",
            "Укажите пароль.",
        ));
    }

    // Логин или e-mail; совпадение по логину важнее
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT id, password_hash FROM users
         WHERE active AND (login = $1 OR lower(email) = lower($1))
         ORDER BY (login = $1) DESC, id LIMIT 1",
    )
    .bind(&login)
    .fetch_optional(&state.db)
    .await?;
    let Some((user_id, hash)) = row else {
        return Err(auth_failed());
    };
    let (verified, password) =
        tokio::task::spawn_blocking(move || (passwords::verify(&password, &hash), password))
            .await
            .map_err(anyhow::Error::from)?;
    if !verified.ok {
        return Err(auth_failed());
    }
    if verified.rehash {
        let hash = hash_password(password).await?;
        sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
            .bind(user_id)
            .bind(hash)
            .execute(&state.db)
            .await?;
    }

    // Новая сессия на каждый вход (старую из cookie закрываем — защита от фиксации)
    if let Some(old) = jar.get(COOKIE) {
        sqlx::query("DELETE FROM site_sessions WHERE token_hash = $1")
            .bind(token_hash(old.value()))
            .execute(&state.db)
            .await?;
    }
    sqlx::query("DELETE FROM site_sessions WHERE expires_at < now()")
        .execute(&state.db)
        .await?;
    let token = hex::encode(rand::random::<[u8; 32]>());
    let ttl = if remember {
        Duration::days(REMEMBER_DAYS)
    } else {
        Duration::hours(SESSION_HOURS)
    };
    sqlx::query("INSERT INTO site_sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)")
        .bind(token_hash(&token))
        .bind(user_id)
        .bind(Utc::now() + ttl)
        .execute(&state.db)
        .await?;
    sqlx::query("UPDATE users SET last_login_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await?;

    let user: SiteUser =
        sqlx::query_as("SELECT id, login, email, name, last_name FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&state.db)
            .await?;
    let mut jar = jar.add(session_cookie(&state, token, remember));
    // Корзина гостя переходит пользователю, как в Битриксе; сбой не мешает входу
    if let Some(guest) = jar.get(BUYER_COOKIE).map(|c| c.value().to_string()) {
        if let Err(e) = crate::cart::repo::merge_guest_into_user(&state.db, &guest, user_id).await {
            tracing::error!(error = ?e, user_id, "не удалось объединить корзину гостя");
        }
        jar = jar.remove(Cookie::build(BUYER_COOKIE).path("/"));
    }
    let data = json!({ "user": user.json(), "csrfToken": csrf_token() });
    Ok((jar, success(data)).into_response())
}

pub async fn session(State(state): State<AppState>, jar: CookieJar) -> BxResult {
    let user = current_user(&state, &jar).await?;
    Ok(success(json!({
        "authorized": user.is_some(),
        "user": user.map(|u| u.json()),
        "canSeePrices": true,
        "canSeeStocks": true,
        "canAddToCart": true,
    })))
}

pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> Response {
    if let Some(cookie) = jar.get(COOKIE)
        && let Err(e) = sqlx::query("DELETE FROM site_sessions WHERE token_hash = $1")
            .bind(token_hash(cookie.value()))
            .execute(&state.db)
            .await
    {
        return BxError::from(e).into_response();
    }
    let jar = jar.remove(Cookie::build(COOKIE).path("/"));
    (jar, success(json!({ "loggedOut": true }))).into_response()
}
