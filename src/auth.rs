//! Пользователи, пароли и cookie-сессии админки.

use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use chrono::{Duration, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};

use crate::error::{AppError, AppResult};

pub const SESSION_COOKIE: &str = "cms_session";
const SESSION_DAYS: i64 = 7;

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct User {
    pub id: i64,
    pub login: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub active: bool,
}

pub async fn hash_password(password: String) -> anyhow::Result<String> {
    // argon2 намеренно медленный — не блокируем рантайм
    Ok(tokio::task::spawn_blocking(move || password_auth::generate_hash(password)).await?)
}

pub async fn create_user(
    db: &PgPool,
    login: &str,
    password: String,
    is_admin: bool,
) -> AppResult<User> {
    let hash = hash_password(password).await?;
    let user = sqlx::query_as::<_, User>(
        "INSERT INTO users (login, password_hash, is_admin) VALUES ($1, $2, $3)
         ON CONFLICT (login) DO UPDATE SET password_hash = EXCLUDED.password_hash,
                                           is_admin = EXCLUDED.is_admin
         RETURNING id, login, email, is_admin, active",
    )
    .bind(login)
    .bind(hash)
    .bind(is_admin)
    .fetch_one(db)
    .await?;
    Ok(user)
}

/// Проверяет логин/пароль. `None` — если пользователя нет или пароль неверный.
pub async fn verify_login(db: &PgPool, login: &str, password: String) -> AppResult<Option<User>> {
    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT id, password_hash FROM users WHERE login = $1")
            .bind(login)
            .fetch_optional(db)
            .await?;
    let Some((id, hash)) = row else {
        return Ok(None);
    };
    let ok = tokio::task::spawn_blocking(move || {
        password_auth::verify_password(password, &hash).is_ok()
    })
    .await
    .map_err(anyhow::Error::from)?;
    if !ok {
        return Ok(None);
    }
    let user = sqlx::query_as::<_, User>(
        "SELECT id, login, email, is_admin, active FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_one(db)
    .await?;
    Ok(Some(user))
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Создаёт сессию и возвращает сырой токен для cookie.
pub async fn create_session(db: &PgPool, user_id: i64) -> AppResult<String> {
    sqlx::query("DELETE FROM sessions WHERE expires_at < now()")
        .execute(db)
        .await?;
    let token = hex::encode(rand::random::<[u8; 32]>());
    sqlx::query("INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)")
        .bind(token_hash(&token))
        .bind(user_id)
        .bind(Utc::now() + Duration::days(SESSION_DAYS))
        .execute(db)
        .await?;
    sqlx::query("UPDATE users SET last_login_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(db)
        .await?;
    Ok(token)
}

/// Завершает все сессии пользователя, кроме `keep` (сессии, из которой идёт запрос).
pub async fn delete_user_sessions(
    db: &PgPool,
    user_id: i64,
    keep: Option<&CookieJar>,
) -> sqlx::Result<()> {
    let keep_hash = keep
        .and_then(|jar| jar.get(SESSION_COOKIE))
        .map(|c| token_hash(c.value()));
    sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND token_hash IS DISTINCT FROM $2")
        .bind(user_id)
        .bind(keep_hash)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn current_user(db: &PgPool, jar: &CookieJar) -> Result<Option<User>, sqlx::Error> {
    let Some(cookie) = jar.get(SESSION_COOKIE) else {
        return Ok(None);
    };
    sqlx::query_as::<_, User>(
        "SELECT u.id, u.login, u.email, u.is_admin, u.active
         FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = $1 AND s.expires_at > now()",
    )
    .bind(token_hash(cookie.value()))
    .fetch_optional(db)
    .await
}

pub async fn delete_session(db: &PgPool, jar: &CookieJar) -> Result<(), AppError> {
    if let Some(cookie) = jar.get(SESSION_COOKIE) {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(token_hash(cookie.value()))
            .execute(db)
            .await?;
    }
    Ok(())
}

pub fn session_cookie(token: String, secure: bool) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, token))
        .path("/admin")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(secure)
        .max_age(time::Duration::days(SESSION_DAYS))
        .build()
}

pub fn removal_cookie() -> Cookie<'static> {
    Cookie::build(SESSION_COOKIE).path("/admin").build()
}
