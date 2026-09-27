//! Управление пользователями: запросы к БД для раздела «Пользователи» админки.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgPool};

const USER_COLS: &str = "id, login, name, email, is_admin, active, created_at, last_login_at, \
     ARRAY(SELECT g.name FROM user_groups ug JOIN groups g ON g.id = ug.group_id \
           WHERE ug.user_id = users.id ORDER BY g.sort, g.id) AS groups";

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct UserRow {
    pub id: i64,
    pub login: String,
    pub name: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
    /// Названия групп пользователя.
    pub groups: Vec<String>,
}

#[derive(Debug)]
pub struct UserInput {
    pub login: String,
    pub name: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub active: bool,
}

/// Логин: 3–50 символов, латиница, цифры и `_ . - @`.
pub fn is_valid_login(login: &str) -> bool {
    (3..=50).contains(&login.chars().count())
        && login
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '@'))
}

pub fn is_valid_email(email: &str) -> bool {
    match email.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !email.contains(char::is_whitespace)
        }
        None => false,
    }
}

pub async fn list(db: &PgPool) -> sqlx::Result<Vec<UserRow>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {USER_COLS} FROM users ORDER BY id"
    )))
    .fetch_all(db)
    .await
}

pub async fn get(db: &PgPool, id: i64) -> sqlx::Result<Option<UserRow>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {USER_COLS} FROM users WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await
}

pub async fn create(db: &PgPool, input: &UserInput, password_hash: &str) -> sqlx::Result<i64> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO users (login, name, email, is_admin, active, password_hash)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(&input.login)
    .bind(&input.name)
    .bind(&input.email)
    .bind(input.is_admin)
    .bind(input.active)
    .bind(password_hash)
    .fetch_one(db)
    .await?;
    Ok(id)
}

/// Обновляет профиль; `password_hash` = None — пароль не меняется.
pub async fn update(
    db: &PgPool,
    id: i64,
    input: &UserInput,
    password_hash: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE users SET login = $2, name = $3, email = $4, is_admin = $5, active = $6,
                password_hash = COALESCE($7, password_hash)
         WHERE id = $1",
    )
    .bind(id)
    .bind(&input.login)
    .bind(&input.name)
    .bind(&input.email)
    .bind(input.is_admin)
    .bind(input.active)
    .bind(password_hash)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete(db: &PgPool, id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins() {
        assert!(is_valid_login("admin"));
        assert!(is_valid_login("ivan.petrov@corp"));
        assert!(!is_valid_login("ab"));
        assert!(!is_valid_login("иван"));
        assert!(!is_valid_login("with space"));
    }

    #[test]
    fn emails() {
        assert!(is_valid_email("a@b.ru"));
        assert!(!is_valid_email("a@b"));
        assert!(!is_valid_email("@b.ru"));
        assert!(!is_valid_email("a b@c.ru"));
        assert!(!is_valid_email("a@.ru"));
    }
}
