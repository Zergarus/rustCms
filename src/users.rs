//! Управление пользователями: запросы к БД для раздела «Пользователи» админки.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder};

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

/// Поле профиля (`b_user`) → колонка `users`.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    Name,
    LastName,
    SecondName,
    Email,
    Phone,
    City,
    WorkPosition,
    Photo,
    /// `UF_*` — ключ в `extra` (snake_case).
    Extra(String),
}

/// Колонка для поля Битрикса (`PERSONAL_PHONE`, `UF_CITY_ID`); неизвестное — `None`.
pub fn profile_column(field: &str) -> Option<Column> {
    let upper = field.to_uppercase();
    Some(match upper.as_str() {
        "NAME" => Column::Name,
        "LAST_NAME" => Column::LastName,
        "SECOND_NAME" => Column::SecondName,
        "EMAIL" => Column::Email,
        "PERSONAL_PHONE" => Column::Phone,
        "PERSONAL_CITY" => Column::City,
        "WORK_POSITION" => Column::WorkPosition,
        "PERSONAL_PHOTO" => Column::Photo,
        f if f.starts_with("UF_") && f.len() > 3 => Column::Extra(f.to_lowercase()),
        _ => return None,
    })
}

/// Пользователь для личного кабинета.
#[derive(Debug, Clone, FromRow)]
pub struct ProfileRow {
    pub id: i64,
    pub login: String,
    pub email: Option<String>,
    pub name: String,
    pub last_name: String,
    pub second_name: String,
    pub phone: String,
    pub city: String,
    pub work_position: String,
    /// `/upload/...` аватара.
    pub photo: Option<String>,
    pub extra: sqlx::types::Json<Map<String, Value>>,
}

pub async fn load_profile(db: &PgPool, id: i64) -> sqlx::Result<Option<ProfileRow>> {
    sqlx::query_as(
        "SELECT u.id, u.login, u.email, u.name, u.last_name, u.second_name, u.phone, u.city,
                u.work_position, '/upload/' || f.path AS photo, u.extra
         FROM users u LEFT JOIN files f ON f.id = u.photo_id
         WHERE u.id = $1 AND u.active",
    )
    .bind(id)
    .fetch_optional(db)
    .await
}

/// Email занят другим пользователем (без учёта регистра).
pub async fn email_taken(db: &PgPool, email: &str, except: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE lower(email) = lower($1) AND id <> $2)",
    )
    .bind(email.trim())
    .bind(except)
    .fetch_one(db)
    .await
}

/// Изменение профиля одним запросом: поля, хеш пароля, аватар (`Some(None)` — убрать).
pub async fn update_profile(
    db: &PgPool,
    id: i64,
    updates: &[(Column, String)],
    password_hash: Option<String>,
    photo: Option<Option<i64>>,
) -> sqlx::Result<()> {
    let mut qb = QueryBuilder::<Postgres>::new("UPDATE users SET id = id");
    for (column, value) in updates {
        let name = match column {
            Column::Name => "name",
            Column::LastName => "last_name",
            Column::SecondName => "second_name",
            Column::Email => {
                qb.push(", email = NULLIF(")
                    .push_bind(value.clone())
                    .push(", '')");
                continue;
            }
            Column::Phone => "phone",
            Column::City => "city",
            Column::WorkPosition => "work_position",
            Column::Photo => continue,
            Column::Extra(key) => {
                qb.push(", extra = extra || jsonb_build_object(")
                    .push_bind(key.clone())
                    .push("::text, ")
                    .push_bind(value.clone())
                    .push("::text)");
                continue;
            }
        };
        qb.push(format_args!(", {name} = "))
            .push_bind(value.clone());
    }
    if let Some(hash) = password_hash {
        qb.push(", password_hash = ").push_bind(hash);
    }
    if let Some(photo) = photo {
        qb.push(", photo_id = ").push_bind(photo);
    }
    qb.push(" WHERE id = ").push_bind(id);
    qb.build().execute(db).await?;
    Ok(())
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

    #[test]
    fn columns() {
        assert_eq!(profile_column("PERSONAL_CITY"), Some(Column::City));
        assert_eq!(profile_column("personal_phone"), Some(Column::Phone));
        assert_eq!(
            profile_column("UF_CITY_ID"),
            Some(Column::Extra("uf_city_id".into()))
        );
        assert_eq!(profile_column("FOO"), None);
    }

    async fn file(db: &PgPool) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO files (path, original_name, content_type, size) VALUES ('main/a/b.jpg', 'b.jpg', 'image/jpeg', 1) RETURNING id",
        )
        .fetch_one(db)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn update_profile_writes(db: PgPool) {
        let me: i64 = sqlx::query_scalar(
            "INSERT INTO users (login, email, password_hash) VALUES ('me', 'Me@x.ru', 'old') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO users (login, email, password_hash) VALUES ('other', 'a@x.ru', '') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let photo = file(&db).await;
        let updates = [
            (Column::Name, "Иван".to_string()),
            (Column::Phone, "+7999".to_string()),
            (Column::Extra("uf_inn".into()), "123".to_string()),
        ];
        update_profile(
            &db,
            me,
            &updates,
            Some("new-hash".into()),
            Some(Some(photo)),
        )
        .await
        .unwrap();
        let row = load_profile(&db, me).await.unwrap().unwrap();
        assert_eq!((row.name.as_str(), row.phone.as_str()), ("Иван", "+7999"));
        assert_eq!(row.extra.get("uf_inn"), Some(&serde_json::json!("123")));
        assert_eq!(row.photo.as_deref(), Some("/upload/main/a/b.jpg"));
        let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
            .bind(me)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(hash, "new-hash");
        // Без пароля и фото — не трогаются; удаление фото — Some(None)
        update_profile(&db, me, &[], None, Some(None))
            .await
            .unwrap();
        let row = load_profile(&db, me).await.unwrap().unwrap();
        assert!(row.photo.is_none());
        assert!(email_taken(&db, "A@X.ru", me).await.unwrap());
        assert!(!email_taken(&db, "me@x.ru", me).await.unwrap());
        assert!(!email_taken(&db, "a@x.ru", other).await.unwrap());
        assert!(load_profile(&db, other + 100).await.unwrap().is_none());
    }
}
