//! Журнал действий администраторов: кто, что и когда изменил.
//!
//! Запись не должна ломать основное действие: ошибка журнала только логируется.

use sqlx::{FromRow, PgPool};

use crate::error::AppResult;

#[derive(Clone, Debug, FromRow, serde::Serialize)]
pub struct Entry {
    pub id: i64,
    /// Уже отформатировано для показа (`ДД.ММ.ГГГГ ЧЧ:ММ`).
    pub created_at: String,
    pub user_login: String,
    pub action: String,
    pub entity: String,
    pub entity_id: String,
    pub summary: String,
}

/// Пишет запись журнала. `action` — `create` | `update` | `delete` | `status` |
/// `paid` | `cancel` | `settings`, `entity` — `item`, `order`, `settings`, …
pub async fn record(
    db: &PgPool,
    user_id: i64,
    login: &str,
    action: &str,
    entity: &str,
    entity_id: impl ToString,
    summary: impl Into<String>,
) {
    let result = sqlx::query(
        "INSERT INTO audit_log (user_id, user_login, action, entity, entity_id, summary)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(user_id)
    .bind(login)
    .bind(action)
    .bind(entity)
    .bind(entity_id.to_string())
    .bind(summary.into())
    .execute(db)
    .await;
    if let Err(error) = result {
        tracing::warn!(?error, entity, action, "запись журнала действий не удалась");
    }
}

/// Последние записи журнала с необязательными фильтрами.
pub async fn list(
    db: &PgPool,
    entity: Option<&str>,
    login: Option<&str>,
    limit: i64,
) -> AppResult<Vec<Entry>> {
    Ok(sqlx::query_as(
        "SELECT id, to_char(created_at, 'DD.MM.YYYY HH24:MI') AS created_at,
                user_login, action, entity, entity_id, summary
         FROM audit_log
         WHERE ($1::text IS NULL OR entity = $1)
           AND ($2::text IS NULL OR user_login = $2)
         ORDER BY id DESC
         LIMIT $3",
    )
    .bind(entity.filter(|value| !value.is_empty()))
    .bind(login.filter(|value| !value.is_empty()))
    .bind(limit.clamp(1, 500))
    .fetch_all(db)
    .await?)
}
