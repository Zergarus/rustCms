//! Почтовые шаблоны (`mail_templates`): список и правка из админки.
//!
//! Шаблоны переносятся из Битрикса импортом; здесь их можно менять без SQL.
//! Подстановки `#ПОЛЕ#` не валидируются: макросов десятки, ложные срабатывания
//! дороже пользы.

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};

use crate::error::{AppError, AppResult};

const MAX_EVENT: usize = 64;

#[derive(Clone, Debug, FromRow, Serialize)]
pub struct Template {
    pub id: i64,
    pub event_name: String,
    pub active: bool,
    pub email_from: String,
    pub email_to: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub body_type: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TemplateForm {
    #[serde(default)]
    pub event_name: String,
    #[serde(default)]
    pub email_from: String,
    #[serde(default)]
    pub email_to: String,
    #[serde(default)]
    pub bcc: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub body_type: String,
    #[serde(default)]
    pub active: Option<String>,
}

impl TemplateForm {
    pub fn validate(&self) -> Result<(), String> {
        let event = self.event_name.trim();
        if event.is_empty() {
            return Err("Укажите код события".into());
        }
        if event.chars().count() > MAX_EVENT
            || !event
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            return Err("Код события: заглавные латинские буквы, цифры и «_»".into());
        }
        if self.subject.trim().is_empty() {
            return Err("Тема письма не может быть пустой".into());
        }
        if self.body.trim().is_empty() {
            return Err("Тело письма не может быть пустым".into());
        }
        let body_type = self.body_type_or_default();
        if !["text", "html"].contains(&body_type.as_str()) {
            return Err("Тип письма: text или html".into());
        }
        Ok(())
    }

    pub fn body_type_or_default(&self) -> String {
        match self.body_type.trim() {
            "" => "text".to_string(),
            other => other.to_string(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }
}

pub async fn list(db: &PgPool) -> AppResult<Vec<Template>> {
    Ok(sqlx::query_as(
        "SELECT id, event_name, active, email_from, email_to, bcc, subject, body, body_type
         FROM mail_templates ORDER BY event_name, id",
    )
    .fetch_all(db)
    .await?)
}

pub async fn get(db: &PgPool, id: i64) -> AppResult<Template> {
    sqlx::query_as(
        "SELECT id, event_name, active, email_from, email_to, bcc, subject, body, body_type
         FROM mail_templates WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn create(db: &PgPool, form: &TemplateForm) -> AppResult<i64> {
    Ok(sqlx::query_scalar(
        "INSERT INTO mail_templates
             (event_name, active, email_from, email_to, bcc, subject, body, body_type)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(form.event_name.trim())
    .bind(form.is_active())
    .bind(form.email_from.trim())
    .bind(form.email_to.trim())
    .bind(form.bcc.trim())
    .bind(form.subject.trim())
    .bind(&form.body)
    .bind(form.body_type_or_default())
    .fetch_one(db)
    .await?)
}

pub async fn update(db: &PgPool, id: i64, form: &TemplateForm) -> AppResult<()> {
    let updated = sqlx::query(
        "UPDATE mail_templates
         SET event_name = $2, active = $3, email_from = $4, email_to = $5, bcc = $6,
             subject = $7, body = $8, body_type = $9
         WHERE id = $1",
    )
    .bind(id)
    .bind(form.event_name.trim())
    .bind(form.is_active())
    .bind(form.email_from.trim())
    .bind(form.email_to.trim())
    .bind(form.bcc.trim())
    .bind(form.subject.trim())
    .bind(&form.body)
    .bind(form.body_type_or_default())
    .execute(db)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

pub async fn delete(db: &PgPool, id: i64) -> AppResult<()> {
    let deleted = sqlx::query("DELETE FROM mail_templates WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(crate::error::AppError::NotFound);
    }
    Ok(())
}
