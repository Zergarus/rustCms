//! Запросы к БД для инфоблоков, свойств и элементов.

use sqlx::{PgPool, types::Json};

use super::{Element, ElementInput, Iblock, IblockInput, IblockSummary, Property, PropertyInput};

const IBLOCK_COLS: &str = "id, code, name, description, api_enabled, sort, created_at, updated_at";
const ELEMENT_COLS: &str = "id, iblock_id, code, name, active, sort, preview_text, detail_text, \
     published_at, properties, created_at, updated_at";

pub async fn list_iblocks(db: &PgPool) -> sqlx::Result<Vec<IblockSummary>> {
    sqlx::query_as(
        "SELECT i.id, i.code, i.name, i.description, i.api_enabled, i.sort,
                i.created_at, i.updated_at,
                (SELECT count(*) FROM iblock_elements e WHERE e.iblock_id = i.id) AS element_count
         FROM iblocks i ORDER BY i.sort, i.id",
    )
    .fetch_all(db)
    .await
}

pub async fn get_iblock(db: &PgPool, id: i64) -> sqlx::Result<Option<Iblock>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {IBLOCK_COLS} FROM iblocks WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await
}

pub async fn get_iblock_by_code(db: &PgPool, code: &str) -> sqlx::Result<Option<Iblock>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {IBLOCK_COLS} FROM iblocks WHERE code = $1"
    )))
    .bind(code)
    .fetch_optional(db)
    .await
}

pub async fn create_iblock(db: &PgPool, input: &IblockInput) -> sqlx::Result<Iblock> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO iblocks (code, name, description, api_enabled, sort)
         VALUES ($1, $2, $3, $4, $5) RETURNING {IBLOCK_COLS}"
    )))
    .bind(&input.code)
    .bind(&input.name)
    .bind(&input.description)
    .bind(input.api_enabled)
    .bind(input.sort)
    .fetch_one(db)
    .await
}

pub async fn update_iblock(db: &PgPool, id: i64, input: &IblockInput) -> sqlx::Result<bool> {
    let res = sqlx::query(
        "UPDATE iblocks SET code = $2, name = $3, description = $4, api_enabled = $5,
                            sort = $6, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(&input.code)
    .bind(&input.name)
    .bind(&input.description)
    .bind(input.api_enabled)
    .bind(input.sort)
    .execute(db)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn delete_iblock(db: &PgPool, id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM iblocks WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn list_properties(db: &PgPool, iblock_id: i64) -> sqlx::Result<Vec<Property>> {
    sqlx::query_as(
        "SELECT id, iblock_id, code, name, kind, is_required, sort
         FROM iblock_properties WHERE iblock_id = $1 ORDER BY sort, id",
    )
    .bind(iblock_id)
    .fetch_all(db)
    .await
}

pub async fn create_property(
    db: &PgPool,
    iblock_id: i64,
    input: &PropertyInput,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO iblock_properties (iblock_id, code, name, kind, is_required, sort)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(iblock_id)
    .bind(&input.code)
    .bind(&input.name)
    .bind(&input.kind)
    .bind(input.is_required)
    .bind(input.sort)
    .execute(db)
    .await?;
    Ok(())
}

/// Удаляет свойство и возвращает id инфоблока, к которому оно относилось.
/// Значения свойства у элементов тоже вычищаются.
pub async fn delete_property(db: &PgPool, id: i64) -> sqlx::Result<Option<i64>> {
    let mut tx = db.begin().await?;
    let row: Option<(i64, String)> =
        sqlx::query_as("DELETE FROM iblock_properties WHERE id = $1 RETURNING iblock_id, code")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some((iblock_id, code)) = &row {
        sqlx::query("UPDATE iblock_elements SET properties = properties - $2 WHERE iblock_id = $1")
            .bind(iblock_id)
            .bind(code)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(row.map(|(iblock_id, _)| iblock_id))
}

pub async fn list_elements(
    db: &PgPool,
    iblock_id: i64,
    limit: i64,
    offset: i64,
) -> sqlx::Result<(Vec<Element>, i64)> {
    let items = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ELEMENT_COLS} FROM iblock_elements WHERE iblock_id = $1
         ORDER BY sort, id DESC LIMIT $2 OFFSET $3"
    )))
    .bind(iblock_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await?;
    let (total,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM iblock_elements WHERE iblock_id = $1")
            .bind(iblock_id)
            .fetch_one(db)
            .await?;
    Ok((items, total))
}

pub async fn get_element(db: &PgPool, id: i64) -> sqlx::Result<Option<Element>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ELEMENT_COLS} FROM iblock_elements WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await
}

pub async fn create_element(
    db: &PgPool,
    iblock_id: i64,
    input: &ElementInput,
) -> sqlx::Result<i64> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO iblock_elements
            (iblock_id, code, name, active, sort, preview_text, detail_text, published_at, properties)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
    )
    .bind(iblock_id)
    .bind(&input.code)
    .bind(&input.name)
    .bind(input.active)
    .bind(input.sort)
    .bind(&input.preview_text)
    .bind(&input.detail_text)
    .bind(input.published_at)
    .bind(Json(&input.properties))
    .fetch_one(db)
    .await?;
    Ok(id)
}

pub async fn update_element(db: &PgPool, id: i64, input: &ElementInput) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE iblock_elements SET code = $2, name = $3, active = $4, sort = $5,
                preview_text = $6, detail_text = $7, published_at = $8, properties = $9,
                updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(&input.code)
    .bind(&input.name)
    .bind(input.active)
    .bind(input.sort)
    .bind(&input.preview_text)
    .bind(&input.detail_text)
    .bind(input.published_at)
    .bind(Json(&input.properties))
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete_element(db: &PgPool, id: i64) -> sqlx::Result<Option<i64>> {
    let row: Option<(i64,)> =
        sqlx::query_as("DELETE FROM iblock_elements WHERE id = $1 RETURNING iblock_id")
            .bind(id)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(iblock_id,)| iblock_id))
}
