//! Запросы к БД для коллекций, разделов, полей и записей.

use sqlx::{PgPool, types::Json};

use super::{
    Collection, CollectionInput, CollectionSummary, Field, FieldInput, FieldOption, Item,
    ItemInput, Section, SectionInput,
};

const COLLECTION_COLS: &str = "id, code, name, description, api_enabled, sort, detail_page_url, \
     section_page_url, list_page_url, is_catalog, product_collection_id, sku_field_id, created_at, \
     updated_at";
const PROPERTY_COLS: &str = "id, collection_id, code, name, kind, is_required, sort, multiple, link_collection_id, user_type, in_basket, offer_tree";
const SECTION_COLS: &str = "id, collection_id, parent_id, code, xml_id, name, active, sort, depth_level, \
     description, picture_id, created_at, updated_at";
/// Колонки записи; `field_values` дополнен ключом связи предложения с товаром.
fn element_cols() -> String {
    format!(
        "id, collection_id, section_id, code, xml_id, name, active, sort, \
         preview_text, detail_text, preview_picture_id, detail_picture_id, published_at, \
         {} AS field_values, product_id, created_at, updated_at",
        super::sku::field_values_sql("collection_items")
    )
}

pub async fn list_collections(db: &PgPool) -> sqlx::Result<Vec<CollectionSummary>> {
    sqlx::query_as(
        "SELECT i.id, i.code, i.name, i.description, i.api_enabled, i.sort, i.detail_page_url,
                i.section_page_url, i.list_page_url, i.is_catalog, i.product_collection_id, i.sku_field_id, i.created_at, i.updated_at,
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
            (collection_id, code, name, kind, is_required, sort, multiple, link_collection_id,
             in_basket, offer_tree)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING id",
    )
    .bind(collection_id)
    .bind(&input.code)
    .bind(&input.name)
    .bind(&input.kind)
    .bind(input.is_required)
    .bind(input.sort)
    .bind(input.multiple)
    .bind(input.link_collection_id)
    .bind(input.in_basket)
    .bind(input.offer_tree)
    .fetch_one(db)
    .await?;
    Ok(id)
}

/// Меняет только то, что не ломает уже сохранённые значения:
/// название, сортировку, обязательность, коллекцию привязки и флаги корзины и выбора предложения.
pub async fn update_field(db: &PgPool, id: i64, input: &FieldInput) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE collection_fields SET name = $2, is_required = $3, sort = $4, link_collection_id = $5,
                in_basket = $6, offer_tree = $7
         WHERE id = $1",
    )
    .bind(id)
    .bind(&input.name)
    .bind(input.is_required)
    .bind(input.sort)
    .bind(input.link_collection_id)
    .bind(input.in_basket)
    .bind(input.offer_tree)
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
    let cols = element_cols();
    let items = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {cols} FROM collection_items {WHERE}
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
    let cols = element_cols();
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {cols} FROM collection_items WHERE id = $1"
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

pub async fn create_item<'e>(
    db: impl sqlx::PgExecutor<'e>,
    collection_id: i64,
    input: &ItemInput,
) -> sqlx::Result<i64> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO collection_items
            (collection_id, section_id, code, xml_id, name, active, sort, preview_text, detail_text,
             preview_picture_id, detail_picture_id, published_at, field_values, product_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) RETURNING id",
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
    .bind(input.product_id)
    .fetch_one(db)
    .await?;
    Ok(id)
}

pub async fn update_item(db: &PgPool, id: i64, input: &ItemInput) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE collection_items SET section_id = $2, code = $3, xml_id = $4, name = $5,
                active = $6, sort = $7, preview_text = $8, detail_text = $9,
                preview_picture_id = $10, detail_picture_id = $11, published_at = $12,
                field_values = $13, product_id = $14, updated_at = now()
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
    .bind(input.product_id)
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

// ---------------------------------------------------------------------------
// Торговые предложения
// ---------------------------------------------------------------------------

/// Создаёт поле связи в коллекции предложений (если его ещё нет) и записывает его
/// в `sku_field_id`; привязывает коллекцию к товарам. Всё в переданной транзакции.
async fn ensure_link(
    tx: &mut sqlx::PgConnection,
    product_id: i64,
    offers_id: i64,
) -> sqlx::Result<()> {
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM collection_fields WHERE collection_id = $1 AND code = $2",
    )
    .bind(offers_id)
    .bind(super::sku::LINK_CODE)
    .fetch_optional(&mut *tx)
    .await?;
    let field_id = match existing {
        Some(id) => id,
        None => {
            sqlx::query_scalar(
                "INSERT INTO collection_fields
                    (collection_id, code, name, kind, is_required, sort, link_collection_id)
                 VALUES ($1, $2, 'Товар', 'element', TRUE, 100, $3) RETURNING id",
            )
            .bind(offers_id)
            .bind(super::sku::LINK_CODE)
            .bind(product_id)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    sqlx::query(
        "UPDATE collection_fields SET kind = 'element', link_collection_id = $2 WHERE id = $1",
    )
    .bind(field_id)
    .bind(product_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE collections SET product_collection_id = $2, sku_field_id = $3, updated_at = now()
         WHERE id = $1",
    )
    .bind(offers_id)
    .bind(product_id)
    .bind(field_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Создаёт коллекцию предложений для коллекции товаров `product`: код `<код>_offers`,
/// название «<название> — предложения», каталог, поле связи и сама связь — в одной транзакции.
pub async fn create_offer_collection(
    db: &PgPool,
    product: &Collection,
) -> sqlx::Result<Collection> {
    let mut tx = db.begin().await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO collections (code, name, api_enabled, sort, is_catalog)
         VALUES ($1, $2, $3, $4, TRUE) RETURNING id",
    )
    .bind(format!("{}_offers", product.code))
    .bind(format!("{} — предложения", product.name))
    .bind(product.api_enabled)
    .bind(product.sort)
    .fetch_one(&mut *tx)
    .await?;
    ensure_link(&mut tx, product.id, id).await?;
    let created = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLLECTION_COLS} FROM collections WHERE id = $1"
    )))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(created)
}

/// Делает `offers_id` коллекцией предложений коллекции товаров `product_id`
/// (id коллекций, не записей); поле связи создаётся, если его нет.
pub async fn link_offers(db: &PgPool, product_id: i64, offers_id: i64) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    ensure_link(&mut tx, product_id, offers_id).await?;
    tx.commit().await
}

#[derive(Debug)]
pub enum UnlinkError {
    /// В коллекции предложений есть записи (их число).
    HasOffers(i64),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for UnlinkError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// Снимает связь: у коллекции предложений коллекции товаров `product_id` (id коллекции)
/// очищается привязка, поле связи удаляется. Только если в ней нет записей.
pub async fn unlink_offers(db: &PgPool, product_id: i64) -> Result<(), UnlinkError> {
    let mut tx = db.begin().await?;
    let offers: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT id, sku_field_id FROM collections WHERE product_collection_id = $1 FOR UPDATE",
    )
    .bind(product_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((offers_id, field_id)) = offers else {
        return Ok(());
    };
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM collection_items WHERE collection_id = $1")
            .bind(offers_id)
            .fetch_one(&mut *tx)
            .await?;
    if count > 0 {
        return Err(UnlinkError::HasOffers(count));
    }
    sqlx::query(
        "UPDATE collections SET product_collection_id = NULL, sku_field_id = NULL, updated_at = now()
         WHERE id = $1",
    )
    .bind(offers_id)
    .execute(&mut *tx)
    .await?;
    if let Some(field_id) = field_id {
        sqlx::query("DELETE FROM collection_fields WHERE id = $1")
            .bind(field_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Коллекция предложений коллекции товаров `product_collection_id` (id коллекции).
pub async fn offers_collection(
    db: &PgPool,
    product_collection_id: i64,
) -> sqlx::Result<Option<Collection>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLLECTION_COLS} FROM collections WHERE product_collection_id = $1
         ORDER BY id LIMIT 1"
    )))
    .bind(product_collection_id)
    .fetch_optional(db)
    .await
}

/// Сколько записей коллекции `offers_id` привязано (`product_id`) к товарам не из
/// коллекции `product_collection_id`.
pub async fn foreign_offers_count(
    db: &PgPool,
    offers_id: i64,
    product_collection_id: i64,
) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM collection_items o JOIN collection_items p ON p.id = o.product_id
         WHERE o.collection_id = $1 AND p.collection_id <> $2",
    )
    .bind(offers_id)
    .bind(product_collection_id)
    .fetch_one(db)
    .await
}

/// Поле — системное поле связи с товаром какой-либо коллекции предложений.
pub async fn is_sku_field(db: &PgPool, field_id: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM collections WHERE sku_field_id = $1)")
        .bind(field_id)
        .fetch_one(db)
        .await
}

/// Строка таблицы предложений на карточке товара.
#[derive(Debug, sqlx::FromRow)]
pub struct OfferRow {
    pub id: i64,
    pub name: String,
    pub active: bool,
    pub field_values: Json<serde_json::Map<String, serde_json::Value>>,
    /// Наименьшая цена базового типа.
    pub price: Option<f64>,
    /// Сумма остатков по складам.
    pub stock: f64,
}

/// Предложения товара `product_id` (запись коллекции товаров) из коллекции `offers_id`
/// одним запросом: с базовой ценой и суммой остатков.
pub async fn list_offer_rows(
    db: &PgPool,
    offers_id: i64,
    product_id: i64,
) -> sqlx::Result<Vec<OfferRow>> {
    sqlx::query_as(
        "SELECT o.id, o.name, o.active, o.field_values,
                (SELECT min(pr.price)::float8 FROM catalog_prices pr
                 JOIN catalog_price_types t ON t.id = pr.price_type_id
                 WHERE pr.item_id = o.id AND t.is_base) AS price,
                COALESCE((SELECT sum(a.amount)::float8 FROM catalog_store_amounts a
                          WHERE a.item_id = o.id), 0) AS stock
         FROM collection_items o
         WHERE o.collection_id = $1 AND o.product_id = $2
         ORDER BY o.sort, o.id",
    )
    .bind(offers_id)
    .bind(product_id)
    .fetch_all(db)
    .await
}
