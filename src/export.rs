//! Экспорт данных: JSON по коллекциям и CSV со строками.

use std::path::Path;

use anyhow::Context;
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool, types::Json};

#[derive(FromRow)]
struct CollectionRow {
    id: i64,
    code: String,
    name: String,
    description: String,
    api_enabled: bool,
    is_catalog: bool,
    sort: i32,
}

#[derive(FromRow)]
struct ItemRow {
    id: i64,
    collection_id: i64,
    code: String,
    name: String,
    active: bool,
    sort: i32,
    section_id: Option<i64>,
    published_at: Option<chrono::DateTime<chrono::Utc>>,
    xml_id: String,
    data: Json<Value>,
}

/// Экранирует значение для CSV (RFC 4180).
pub fn csv_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Выгружает все коллекции в `out`: `collections.json` и `<code>.csv` на коллекцию.
/// Возвращает число коллекций.
pub async fn export(db: &PgPool, out: &Path) -> anyhow::Result<usize> {
    tokio::fs::create_dir_all(out)
        .await
        .with_context(|| format!("не удалось создать {}", out.display()))?;
    let collections: Vec<CollectionRow> = sqlx::query_as(
        "SELECT id, code, name, description, api_enabled, is_catalog, sort
         FROM collections ORDER BY sort, id",
    )
    .fetch_all(db)
    .await?;

    let fields: Vec<(i64, Value)> = sqlx::query_as(
        "SELECT collection_id, jsonb_build_object(
             'code', code, 'name', name, 'kind', kind, 'multiple', multiple,
             'required', is_required, 'sort', sort)
         FROM collection_fields ORDER BY collection_id, sort, id",
    )
    .fetch_all(db)
    .await?;
    let sections: Vec<(i64, Value)> = sqlx::query_as(
        "SELECT collection_id, jsonb_build_object(
             'id', id, 'code', code, 'name', name, 'active', active, 'sort', sort,
             'parent_id', parent_id)
         FROM collection_sections ORDER BY collection_id, sort, id",
    )
    .fetch_all(db)
    .await?;
    let items: Vec<ItemRow> = sqlx::query_as(
        "SELECT id, collection_id, code, name, active, sort, section_id, published_at, xml_id,
                field_values AS data
         FROM collection_items ORDER BY collection_id, sort, id",
    )
    .fetch_all(db)
    .await?;

    let mut document = Vec::new();
    for collection in &collections {
        let own_fields: Vec<&Value> = fields
            .iter()
            .filter(|(id, _)| *id == collection.id)
            .map(|(_, value)| value)
            .collect();
        let own_sections: Vec<&Value> = sections
            .iter()
            .filter(|(id, _)| *id == collection.id)
            .map(|(_, value)| value)
            .collect();
        let own_items: Vec<Value> = items
            .iter()
            .filter(|item| item.collection_id == collection.id)
            .map(|item| {
                json!({
                    "id": item.id,
                    "code": item.code,
                    "name": item.name,
                    "active": item.active,
                    "sort": item.sort,
                    "section_id": item.section_id,
                    "published_at": item.published_at.map(|at| at.to_rfc3339()),
                    "xml_id": item.xml_id,
                    "properties": item.data.0,
                })
            })
            .collect();
        document.push(json!({
            "code": collection.code,
            "name": collection.name,
            "description": collection.description,
            "api_enabled": collection.api_enabled,
            "is_catalog": collection.is_catalog,
            "sort": collection.sort,
            "fields": own_fields,
            "sections": own_sections,
            "items": own_items,
        }));

        let csv = csv(&items, collection.id);
        tokio::fs::write(out.join(format!("{}.csv", collection.code)), csv)
            .await
            .with_context(|| format!("не удалось записать CSV коллекции {}", collection.code))?;
    }

    let text = serde_json::to_string_pretty(&document)?;
    tokio::fs::write(out.join("collections.json"), text).await?;
    Ok(collections.len())
}

fn csv(items: &[ItemRow], collection_id: i64) -> String {
    let mut out =
        String::from("id;code;name;active;sort;section_id;published_at;xml_id;properties\n");
    for item in items
        .iter()
        .filter(|item| item.collection_id == collection_id)
    {
        let cells = [
            item.id.to_string(),
            item.code.clone(),
            item.name.clone(),
            item.active.to_string(),
            item.sort.to_string(),
            item.section_id.map(|id| id.to_string()).unwrap_or_default(),
            item.published_at
                .map(|at| at.to_rfc3339())
                .unwrap_or_default(),
            item.xml_id.clone(),
            item.data.0.to_string(),
        ];
        out.push_str(
            &cells
                .iter()
                .map(|cell| csv_field(cell))
                .collect::<Vec<_>>()
                .join(";"),
        );
        out.push('\n');
    }
    out
}
