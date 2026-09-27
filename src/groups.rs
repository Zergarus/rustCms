//! Группы пользователей: запросы к БД.

use std::collections::HashMap;

use serde::Serialize;
use sqlx::{FromRow, PgPool};

use crate::access::Level;

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Group {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub description: String,
    pub sort: i32,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct GroupSummary {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub group: Group,
    pub member_count: i64,
    pub permissions: Vec<String>,
}

#[derive(Debug)]
pub struct GroupInput {
    pub code: String,
    pub name: String,
    pub description: String,
    pub sort: i32,
    pub permissions: Vec<String>,
    /// Уровни доступа к инфоблокам; `Level::None` не сохраняется.
    pub iblock_levels: Vec<(i64, Level)>,
}

pub async fn list(db: &PgPool) -> sqlx::Result<Vec<GroupSummary>> {
    sqlx::query_as(
        "SELECT g.id, g.code, g.name, g.description, g.sort,
                (SELECT count(*) FROM user_groups ug WHERE ug.group_id = g.id) AS member_count,
                ARRAY(SELECT gp.permission FROM group_permissions gp
                      WHERE gp.group_id = g.id ORDER BY gp.permission) AS permissions
         FROM groups g ORDER BY g.sort, g.id",
    )
    .fetch_all(db)
    .await
}

pub async fn get(db: &PgPool, id: i64) -> sqlx::Result<Option<Group>> {
    sqlx::query_as("SELECT id, code, name, description, sort FROM groups WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await
}

pub async fn permissions(db: &PgPool, id: i64) -> sqlx::Result<Vec<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT permission FROM group_permissions WHERE group_id = $1")
            .bind(id)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().map(|(p,)| p).collect())
}

/// Уровни доступа группы по инфоблокам: iblock_id → "read" | "write".
pub async fn iblock_levels(db: &PgPool, id: i64) -> sqlx::Result<HashMap<String, String>> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT iblock_id, level FROM iblock_group_access WHERE group_id = $1")
            .bind(id)
            .fetch_all(db)
            .await?;
    // ключи-строки — чтобы в шаблоне обращаться как levels[iblock.id|string]
    Ok(rows.into_iter().map(|(i, l)| (i.to_string(), l)).collect())
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Member {
    pub id: i64,
    pub login: String,
    pub name: String,
    pub active: bool,
}

pub async fn members(db: &PgPool, id: i64) -> sqlx::Result<Vec<Member>> {
    sqlx::query_as(
        "SELECT u.id, u.login, u.name, u.active
         FROM users u JOIN user_groups ug ON ug.user_id = u.id
         WHERE ug.group_id = $1 ORDER BY u.login",
    )
    .bind(id)
    .fetch_all(db)
    .await
}

/// Создаёт (`id` = None) или обновляет группу вместе с правами — в одной транзакции.
pub async fn save(db: &PgPool, id: Option<i64>, input: &GroupInput) -> sqlx::Result<i64> {
    let mut tx = db.begin().await?;
    let (id,): (i64,) = match id {
        None => {
            sqlx::query_as(
                "INSERT INTO groups (code, name, description, sort) VALUES ($1, $2, $3, $4)
                 RETURNING id",
            )
            .bind(&input.code)
            .bind(&input.name)
            .bind(&input.description)
            .bind(input.sort)
            .fetch_one(&mut *tx)
            .await?
        }
        Some(id) => {
            sqlx::query_as(
                "UPDATE groups SET code = $2, name = $3, description = $4, sort = $5
                 WHERE id = $1 RETURNING id",
            )
            .bind(id)
            .bind(&input.code)
            .bind(&input.name)
            .bind(&input.description)
            .bind(input.sort)
            .fetch_one(&mut *tx)
            .await?
        }
    };

    sqlx::query("DELETE FROM group_permissions WHERE group_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO group_permissions (group_id, permission) SELECT $1, unnest($2::text[])",
    )
    .bind(id)
    .bind(&input.permissions)
    .execute(&mut *tx)
    .await?;

    sqlx::query("DELETE FROM iblock_group_access WHERE group_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let (iblock_ids, levels): (Vec<i64>, Vec<&str>) = input
        .iblock_levels
        .iter()
        .filter_map(|(iblock_id, level)| level.as_db().map(|l| (*iblock_id, l)))
        .unzip();
    sqlx::query(
        "INSERT INTO iblock_group_access (group_id, iblock_id, level)
         SELECT $1, i.id, l.level
         FROM unnest($2::bigint[], $3::text[]) AS l(iblock_id, level)
         JOIN iblocks i ON i.id = l.iblock_id",
    )
    .bind(id)
    .bind(&iblock_ids)
    .bind(&levels)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(id)
}

pub async fn delete(db: &PgPool, id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM groups WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Id групп пользователя.
pub async fn user_group_ids(db: &PgPool, user_id: i64) -> sqlx::Result<Vec<i64>> {
    let rows: Vec<(i64,)> = sqlx::query_as("SELECT group_id FROM user_groups WHERE user_id = $1")
        .bind(user_id)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(|(g,)| g).collect())
}

/// Полностью заменяет набор групп пользователя.
pub async fn set_user_groups(db: &PgPool, user_id: i64, group_ids: &[i64]) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM user_groups WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO user_groups (user_id, group_id)
         SELECT $1, g.id FROM groups g WHERE g.id = ANY($2)",
    )
    .bind(user_id)
    .bind(group_ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
