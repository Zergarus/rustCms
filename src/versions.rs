//! Версии записей: снимок после сохранения, история и восстановление.
//!
//! Снимок — это `to_jsonb` строки `collection_items`: восстановление не зависит
//! от набора полей, который был в форме.

use serde::Serialize;
use sqlx::{FromRow, PgPool, types::Json};

use crate::{
    access::Access,
    error::{AppError, AppResult},
    state::AppState,
};

#[derive(Clone, Debug, FromRow, Serialize)]
pub struct Revision {
    pub id: i64,
    pub item_id: i64,
    /// Отформатировано для показа (`ДД.ММ.ГГГГ ЧЧ:ММ`).
    pub created_at: String,
    pub user_login: String,
    pub action: String,
    pub name: String,
    pub code: String,
}

/// Пишет снимок текущего состояния записи. Ошибка не ломает сохранение.
pub async fn snapshot(state: &AppState, item_id: i64, action: &str, user_id: i64, login: &str) {
    let result = sqlx::query(
        "INSERT INTO item_revisions (item_id, collection_id, user_id, user_login, action, data)
         SELECT i.id, i.collection_id, $2, $3, $4, to_jsonb(i)
         FROM collection_items i WHERE i.id = $1",
    )
    .bind(item_id)
    .bind(user_id)
    .bind(login)
    .bind(action)
    .execute(&state.db)
    .await;
    if let Err(error) = result {
        tracing::warn!(?error, item_id, "снимок версии записи не сохранён");
    }
}

pub async fn history(db: &PgPool, item_id: i64) -> AppResult<Vec<Revision>> {
    Ok(sqlx::query_as(
        "SELECT id, item_id, to_char(created_at, 'DD.MM.YYYY HH24:MI') AS created_at,
                user_login, action,
                COALESCE(data->>'name', '') AS name, COALESCE(data->>'code', '') AS code
         FROM item_revisions WHERE item_id = $1 ORDER BY id DESC",
    )
    .bind(item_id)
    .fetch_all(db)
    .await?)
}

/// Возвращает запись к снимку и пишет версию восстановления.
pub async fn restore(state: &AppState, user: &Access, revision_id: i64) -> AppResult<i64> {
    let row: Option<(i64, Json<serde_json::Value>)> =
        sqlx::query_as("SELECT item_id, data FROM item_revisions WHERE id = $1")
            .bind(revision_id)
            .fetch_optional(&state.db)
            .await?;
    let (item_id, data) = row.ok_or(AppError::NotFound)?;
    let collection_id: i64 =
        sqlx::query_scalar("SELECT collection_id FROM collection_items WHERE id = $1")
            .bind(item_id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?;
    user.require_collection(collection_id, crate::access::Level::Write)?;

    sqlx::query(
        "UPDATE collection_items SET
             name = data->>'name',
             code = data->>'code',
             active = COALESCE((data->>'active')::boolean, TRUE),
             sort = COALESCE((data->>'sort')::int, 500),
             section_id = NULLIF(data->>'section_id', '')::bigint,
             preview_text = COALESCE(data->>'preview_text', ''),
             detail_text = COALESCE(data->>'detail_text', ''),
             published_at = (data->>'published_at')::timestamptz,
             field_values = COALESCE(data->'field_values', '{}'::jsonb),
             updated_at = now()
         FROM (SELECT $2::jsonb AS data) AS snapshot
         WHERE collection_items.id = $1",
    )
    .bind(item_id)
    .bind(&data.0)
    .execute(&state.db)
    .await?;

    snapshot(state, item_id, "restore", user.user.id, &user.user.login).await;
    crate::audit::record(
        state,
        user.user.id,
        &user.user.login,
        "restore",
        "item",
        item_id,
        format!("восстановлена версия {revision_id}"),
    )
    .await;
    Ok(item_id)
}
