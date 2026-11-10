//! Настройки установки: сайт и SMTP. Хранятся в `options`
//! (модули `main` и `mail`), как в Битриксе.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};

use crate::error::AppResult;

/// Настройки для формы: пароль не читается из БД и не показывается.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Settings {
    pub site_name: String,
    pub server_name: String,
    pub email_from: String,
    pub smtp_host: String,
    pub smtp_port: String,
    pub smtp_user: String,
    pub smtp_secure: String,
}

/// Данные формы настроек.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SettingsForm {
    #[serde(default)]
    pub site_name: String,
    #[serde(default)]
    pub server_name: String,
    #[serde(default)]
    pub email_from: String,
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default)]
    pub smtp_port: String,
    #[serde(default)]
    pub smtp_user: String,
    #[serde(default)]
    pub smtp_password: String,
    #[serde(default)]
    pub smtp_secure: String,
}

/// Готовое подключение к SMTP из настроек.
#[derive(Clone, Debug)]
pub struct Smtp {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// `auto` | `none` | `starttls` | `tls`
    pub secure: String,
}

impl SettingsForm {
    pub fn validate(&self) -> Result<(), String> {
        if self.smtp_host.trim().chars().any(char::is_whitespace) {
            return Err("Хост SMTP не должен содержать пробелов".into());
        }
        if !self.smtp_port.trim().is_empty() {
            match self.smtp_port.trim().parse::<u16>() {
                Ok(port) if port > 0 => {}
                _ => return Err("Порт SMTP — число от 1 до 65535".into()),
            }
        }
        if !["", "auto", "none", "starttls", "tls"].contains(&self.smtp_secure.as_str()) {
            return Err("Шифрование SMTP: auto, none, starttls или tls".into());
        }
        if !self.smtp_host.trim().is_empty() && self.smtp_port.trim().is_empty() {
            return Err("Укажите порт SMTP".into());
        }
        Ok(())
    }
}

async fn set(
    tx: &mut Transaction<'_, Postgres>,
    module: &str,
    name: &str,
    value: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO options (module, name, value) VALUES ($1, $2, $3)
         ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(module)
    .bind(name)
    .bind(value)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn load(db: &PgPool) -> AppResult<Settings> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT module, name, value FROM options WHERE module IN ('main', 'mail')")
            .fetch_all(db)
            .await?;
    let get = |module: &str, name: &str| {
        rows.iter()
            .find(|(m, n, _)| m == module && n == name)
            .map(|(_, _, value)| value.clone())
            .unwrap_or_default()
    };
    Ok(Settings {
        site_name: get("main", "site_name"),
        server_name: get("main", "server_name"),
        email_from: get("main", "email_from"),
        smtp_host: get("mail", "smtp_host"),
        smtp_port: get("mail", "smtp_port"),
        smtp_user: get("mail", "smtp_user"),
        smtp_secure: get("mail", "smtp_secure"),
    })
}

/// Сохраняет настройки; пустой пароль означает «оставить прежний».
pub async fn save(db: &PgPool, form: &SettingsForm) -> AppResult<()> {
    let mut tx = db.begin().await?;
    set(&mut tx, "main", "site_name", form.site_name.trim()).await?;
    set(&mut tx, "main", "server_name", form.server_name.trim()).await?;
    set(&mut tx, "main", "email_from", form.email_from.trim()).await?;
    set(&mut tx, "mail", "smtp_host", form.smtp_host.trim()).await?;
    set(&mut tx, "mail", "smtp_port", form.smtp_port.trim()).await?;
    set(&mut tx, "mail", "smtp_user", form.smtp_user.trim()).await?;
    set(&mut tx, "mail", "smtp_secure", form.smtp_secure.trim()).await?;
    if !form.smtp_password.trim().is_empty() {
        set(&mut tx, "mail", "smtp_password", &form.smtp_password).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Подключение из настроек; без хоста — `None` (письма сохраняются в `.eml`).
pub async fn smtp(db: &PgPool) -> AppResult<Option<Smtp>> {
    let settings = load(db).await?;
    let host = settings.smtp_host.trim().to_string();
    if host.is_empty() {
        return Ok(None);
    }
    let port = match settings.smtp_port.trim().parse::<u16>() {
        Ok(port) if port > 0 => port,
        _ => {
            tracing::warn!(port = %settings.smtp_port, "некорректный порт SMTP, письма уйдут в .eml");
            return Ok(None);
        }
    };
    let password: String = sqlx::query_scalar(
        "SELECT value FROM options WHERE module = 'mail' AND name = 'smtp_password'",
    )
    .fetch_optional(db)
    .await?
    .unwrap_or_default();
    let secure = match settings.smtp_secure.as_str() {
        "" => "auto".to_string(),
        other => other.to_string(),
    };
    Ok(Some(Smtp {
        host,
        port,
        user: settings.smtp_user,
        password,
        secure,
    }))
}
