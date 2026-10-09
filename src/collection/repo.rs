//! Запросы к БД для коллекций, разделов, полей и записей.

use sqlx::{PgPool, types::Json};

use super::{
    Collection, CollectionInput, CollectionSummary, Field, FieldInput, FieldOption, Item,
    ItemInput, Section, SectionInput,
};

const COLLECTION_COLS: &str = "id, code, name, description, api_enabled, sort, detail_page_url, \
     section_page_url, list_page_url, is_catalog, created_at, updated_at";
const PROPERTY_COLS: &str = "id, collection_id, code, name, kind, is_required, sort, multiple, link_collection_id, user_type";
const SECTION_COLS: &str = "id, collection_id, parent_id, code, xml_id, name, active, sort, depth_level, \
     description, picture_id, created_at, updated_at";
const ELEMENT_COLS: &str = "id, collection_id, section_id, code, xml_id, name, active, sort, \
     preview_text, detail_text, preview_picture_id, detail_picture_id, published_at, field_values, \
     created_at, updated_at";

pub async fn list_collections(db: &PgPool) -> sqlx::Result<Vec<CollectionSummary>> {
    sqlx::query_as(
        "SELECT i.id, i.code, i.name, i.description, i.api_enabled, i.sort, i.detail_page_url,
                i.section_page_url, i.list_page_url, i.is_catalog, i.created_at, i.updated_at,
                (SELECT count(*) FROM collection_items e WHERE e.collection_id = i.id) AS item_count
         FROM collections i ORDER BY i.sort, i.id",
    )
    .fetch_all(db)
    .await
}

pub async fn get_collection(db: &PgPool, id: i64) -> sqlx::Result<Option<Collection>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLLECTION_COLS} FROM collections WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await
}

pub async fn get_collection_by_code(db: &PgPool, code: &str) -> sqlx::Result<Option<Collection>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLLECTION_COLS} FROM collections WHERE code = $1"
    )))
    .bind(code)
    .fetch_optional(db)
    .await
}

pub async fn create_collection(db: &PgPool, input: &CollectionInput) -> sqlx::Result<Collection> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO collections (code, name, description, api_enabled, sort, is_catalog)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING {COLLECTION_COLS}"
    )))
    .bind(&input.code)
    .bind(&input.name)
    .bind(&input.description)
    .bind(input.api_enabled)
    .bind(input.sort)
    .bind(input.is_catalog)
    .fetch_one(db)
    .await
}

pub async fn update_collection(
    db: &PgPool,
    id: i64,
    input: &CollectionInput,
) -> sqlx::Result<bool> {
    let res = sqlx::query(
        "UPDATE collections SET code = $2, name = $3, description = $4, api_enabled = $5,
                            sort = $6, is_catalog = $7, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(&input.code)
    .bind(&input.name)
    .bind(&input.description)
    .bind(input.api_enabled)
    .bind(input.sort)
    .bind(input.is_catalog)
    .execute(db)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn delete_collection(db: &PgPool, id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM collections WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Поля
// ---------------------------------------------------------------------------

pub async fn list_fields(db: &PgPool, collection_id: i64) -> sqlx::Result<Vec<Field>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PROPERTY_COLS} FROM collection_fields WHERE collection_id = $1 ORDER BY sort, id"
    )))
    .bind(collection_id)
    .fetch_all(db)
    .await
}

pub async fn get_field(db: &PgPool, id: i64) -> sqlx::Result<Option<Field>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PROPERTY_COLS} FROM collection_fields WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await
}

pub async fn create_field(
    db: &PgPool,
    collection_id: i64,
    input: &FieldInput,
) -> sqlx::Result<i64> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO collection_fields
            (collection_id, code, name, kind, is_required, sort, multiple, link_collection_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(collection_id)
    .bind(&input.code)
    .bind(&input.name)
    .bind(&input.kind)
    .bind(input.is_required)
    .bind(input.sort)
    .bind(input.multiple)
    .bind(input.link_collection_id)
    .fetch_one(db)
    .await?;
    Ok(id)
}

/// Меняет только то, что не ломает уже сохранённые значения:
/// название, сортировку, обязательность и коллекцию привязки.
pub async fn update_field(db: &PgPool, id: i64, input: &FieldInput) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE collection_fields SET name = $2, is_required = $3, sort = $4, link_collection_id = $5
         WHERE id = $1",
    )
    .bind(id)
    .bind(&input.name)
    .bind(input.is_required)
    .bind(input.sort)
    .bind(input.link_collection_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Удаляет поле и возвращает id коллекции, к которой оно относилось.
/// Значения поля у записей тоже вычищаются.
pub async fn delete_field(db: &PgPool, id: i64) -> sqlx::Result<Option<i64>> {
    let mut tx = db.begin().await?;
    let row: Option<(i64, String)> =
        sqlx::query_as("DELETE FROM collection_fields WHERE id = $1 RETURNING collection_id, code")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some((collection_id, code)) = &row {
        sqlx::query(
            "UPDATE collection_items SET field_values = field_values - $2 WHERE collection_id = $1",
        )
        .bind(collection_id)
        .bind(code)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(row.map(|(collection_id, _)| collection_id))
}

/// Варианты всех полей-списков коллекции.
pub async fn list_collection_options(
    db: &PgPool,
    collection_id: i64,
) -> sqlx::Result<Vec<FieldOption>> {
    sqlx::query_as(
        "SELECT e.id, e.field_id, e.value, e.xml_id, e.sort, e.is_default
         FROM collection_field_options e
         JOIN collection_fields p ON p.id = e.field_id
         WHERE p.collection_id = $1
         ORDER BY e.field_id, e.sort, e.id",
    )
    .bind(collection_id)
    .fetch_all(db)
    .await
}

pub async fn list_options(db: &PgPool, property_id: i64) -> sqlx::Result<Vec<FieldOption>> {
    sqlx::query_as(
        "SELECT id, field_id, value, xml_id, sort, is_default
         FROM collection_field_options WHERE field_id = $1 ORDER BY sort, id",
    )
    .bind(property_id)
    .fetch_all(db)
    .await
}

/// Вариант списка из формы; `id` = None — новый.
#[derive(Debug)]
pub struct OptionInput {
    pub id: Option<i64>,
    pub value: String,
    pub xml_id: String,
    pub sort: i32,
    pub is_default: bool,
}

/// Сохраняет набор вариантов списка: обновляет существующие, добавляет новые,
/// удаляет перечисленные в `delete` и вычищает их из значений записей.
pub async fn save_options(
    db: &PgPool,
    property: &Field,
    items: &[OptionInput],
    delete: &[i64],
) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    if !delete.is_empty() {
        sqlx::query("DELETE FROM collection_field_options WHERE field_id = $1 AND id = ANY($2)")
            .bind(property.id)
            .bind(delete)
            .execute(&mut *tx)
            .await?;
        remove_prop_ids(&mut tx, property, delete).await?;
    }
    for item in items {
        match item.id {
            Some(id) => {
                sqlx::query(
                    "UPDATE collection_field_options SET value = $3, xml_id = $4, sort = $5,
                            is_default = $6
                     WHERE field_id = $1 AND id = $2",
                )
                .bind(property.id)
                .bind(id)
                .bind(&item.value)
                .bind(&item.xml_id)
                .bind(item.sort)
                .bind(item.is_default)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO collection_field_options (field_id, value, xml_id, sort, is_default)
                     VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(property.id)
                .bind(&item.value)
                .bind(&item.xml_id)
                .bind(item.sort)
                .bind(item.is_default)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    tx.commit().await
}

/// Убирает id из значения поля у всех записей коллекции
/// (одиночное значение становится null, из массива id вычёркиваются).
async fn remove_prop_ids(
    tx: &mut sqlx::PgConnection,
    property: &Field,
    ids: &[i64],
) -> sqlx::Result<()> {
    let ids: Vec<serde_json::Value> = ids.iter().map(|id| (*id).into()).collect();
    let sql = if property.multiple {
        "UPDATE collection_items
         SET field_values = jsonb_set(field_values, ARRAY[$2], COALESCE(
                (SELECT jsonb_agg(v) FROM jsonb_array_elements(field_values -> $2) v
                 WHERE NOT (v = ANY($3::jsonb[]))), '[]'::jsonb))
         WHERE collection_id = $1 AND jsonb_typeof(field_values -> $2) = 'array'"
    } else {
        "UPDATE collection_items SET field_values = jsonb_set(field_values, ARRAY[$2], 'null')
         WHERE collection_id = $1 AND field_values -> $2 = ANY($3::jsonb[])"
    };
    sqlx::query(sql)
        .bind(property.collection_id)
        .bind(&property.code)
        .bind(ids)
        .execute(tx)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Разделы
// ---------------------------------------------------------------------------

pub async fn list_sections(db: &PgPool, collection_id: i64) -> sqlx::Result<Vec<Section>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SECTION_COLS} FROM collection_sections WHERE collection_id = $1 ORDER BY sort, name, id"
    )))
    .bind(collection_id)
    .fetch_all(db)
    .await
}

pub async fn get_section(db: &PgPool, id: i64) -> sqlx::Result<Option<Section>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SECTION_COLS} FROM collection_sections WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await
}

pub async fn create_section(
    db: &PgPool,
    collection_id: i64,
    input: &SectionInput,
) -> sqlx::Result<i64> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO collection_sections
            (collection_id, parent_id, code, xml_id, name, active, sort, description, picture_id,
             depth_level)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9,
                 COALESCE((SELECT depth_level + 1 FROM collection_sections WHERE id = $2), 1))
         RETURNING id",
    )
    .bind(collection_id)
    .bind(input.parent_id)
    .bind(&input.code)
    .bind(&input.xml_id)
    .bind(&input.name)
    .bind(input.active)
    .bind(input.sort)
    .bind(&input.description)
    .bind(input.picture_id)
    .fetch_one(db)
    .await?;
    Ok(id)
}

/// Обновляет раздел и пересчитывает глубину у него и всех потомков
/// (на случай переноса в другого родителя).
pub async fn update_section(db: &PgPool, id: i64, input: &SectionInput) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "UPDATE collection_sections SET parent_id = $2, code = $3, xml_id = $4, name = $5,
                active = $6, sort = $7, description = $8, picture_id = $9, updated_at = now(),
                depth_level = COALESCE((SELECT depth_level + 1 FROM collection_sections WHERE id = $2), 1)
         WHERE id = $1",
    )
    .bind(id)
    .bind(input.parent_id)
    .bind(&input.code)
    .bind(&input.xml_id)
    .bind(&input.name)
    .bind(input.active)
    .bind(input.sort)
    .bind(&input.description)
    .bind(input.picture_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "WITH RECURSIVE tree AS (
             SELECT id, depth_level FROM collection_sections WHERE id = $1
             UNION ALL
             SELECT s.id, t.depth_level + 1 FROM collection_sections s JOIN tree t ON s.parent_id = t.id
         )
         UPDATE collection_sections s SET depth_level = tree.depth_level
         FROM tree WHERE s.id = tree.id AND s.depth_level <> tree.depth_level",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Удаляет раздел вместе с подразделами; записи остаются без раздела.
pub async fn delete_section(db: &PgPool, id: i64) -> sqlx::Result<Option<i64>> {
    let row: Option<(i64,)> =
        sqlx::query_as("DELETE FROM collection_sections WHERE id = $1 RETURNING collection_id")
            .bind(id)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(collection_id,)| collection_id))
}

// ---------------------------------------------------------------------------
// Записи
// ---------------------------------------------------------------------------

/// Список записей для админки. `section_id` — только записи этого раздела
/// (без подразделов), `Some(0)` — записи без раздела.
pub async fn list_items(
    db: &PgPool,
    collection_id: i64,
    section_id: Option<i64>,
    limit: i64,
    offset: i64,
) -> sqlx::Result<(Vec<Item>, i64)> {
    const WHERE: &str = "WHERE collection_id = $1 AND ($2::bigint IS NULL
                          OR ($2 = 0 AND section_id IS NULL) OR section_id = $2)";
    let items = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ELEMENT_COLS} FROM collection_items {WHERE}
         ORDER BY sort, id DESC LIMIT $3 OFFSET $4"
    )))
    .bind(collection_id)
    .bind(section_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await?;
    let (total,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM collection_items {WHERE}"
    )))
    .bind(collection_id)
    .bind(section_id)
    .fetch_one(db)
    .await?;
    Ok((items, total))
}

pub async fn get_item(db: &PgPool, id: i64) -> sqlx::Result<Option<Item>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ELEMENT_COLS} FROM collection_items WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await
}

/// Названия записей по id (для подписей у привязок). `collection_id` — если задан,
/// учитываются только записи этой коллекции.
pub async fn item_names(
    db: &PgPool,
    ids: &[i64],
    collection_id: Option<i64>,
) -> sqlx::Result<Vec<(i64, String)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as(
        "SELECT id, name FROM collection_items
         WHERE id = ANY($1) AND ($2::bigint IS NULL OR collection_id = $2)",
    )
    .bind(ids)
    .bind(collection_id)
    .fetch_all(db)
    .await
}

pub async fn create_item(db: &PgPool, collection_id: i64, input: &ItemInput) -> sqlx::Result<i64> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO collection_items
            (collection_id, section_id, code, xml_id, name, active, sort, preview_text, detail_text,
             preview_picture_id, detail_picture_id, published_at, field_values)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING id",
    )
    .bind(collection_id)
    .bind(input.section_id)
    .bind(&input.code)
    .bind(&input.xml_id)
    .bind(&input.name)
    .bind(input.active)
    .bind(input.sort)
    .bind(&input.preview_text)
    .bind(&input.detail_text)
    .bind(input.preview_picture_id)
    .bind(input.detail_picture_id)
    .bind(input.published_at)
    .bind(Json(&input.field_values))
    .fetch_one(db)
    .await?;
    Ok(id)
}

pub async fn update_item(db: &PgPool, id: i64, input: &ItemInput) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE collection_items SET section_id = $2, code = $3, xml_id = $4, name = $5,
                active = $6, sort = $7, preview_text = $8, detail_text = $9,
                preview_picture_id = $10, detail_picture_id = $11, published_at = $12,
                field_values = $13, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(input.section_id)
    .bind(&input.code)
    .bind(&input.xml_id)
    .bind(&input.name)
    .bind(input.active)
    .bind(input.sort)
    .bind(&input.preview_text)
    .bind(&input.detail_text)
    .bind(input.preview_picture_id)
    .bind(input.detail_picture_id)
    .bind(input.published_at)
    .bind(Json(&input.field_values))
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete_item(db: &PgPool, id: i64) -> sqlx::Result<Option<i64>> {
    let row: Option<(i64,)> =
        sqlx::query_as("DELETE FROM collection_items WHERE id = $1 RETURNING collection_id")
            .bind(id)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(collection_id,)| collection_id))
}
