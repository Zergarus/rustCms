//! `iblock/list`, `iblock/info/{apiCode}`, `element/list`, `element/{id}`, `element/slug/{slug}`.

use std::{cell::Cell, sync::Arc};

use serde_json::{Map, Value, json};
use sqlx::{Postgres, QueryBuilder};

use super::{
    BxError, BxResult, collate_key,
    filter::{Ctx, push_filter, push_order},
    query::{ListRequest, Select, image_resize},
    registry::{Schema, Snapshot},
    serialize::{Env, Mode, ROW_COLS, Row, serialize},
    success, to_camel, to_snake,
};
use crate::state::AppState;

pub enum Key<'a> {
    Id(i64),
    Slug(&'a str),
}

/// Схема инфоблока по apiCode (camelCase из URL → snake_case в CMS).
pub async fn schema_for(
    state: &AppState,
    api_code: &str,
) -> Result<(Arc<Snapshot>, Arc<Schema>), BxError> {
    let snap = state.registry.snapshot(&state.db).await?;
    let schema = snap
        .by_code(&to_snake(api_code))
        .filter(|s| s.collection.api_enabled)
        .cloned()
        .ok_or_else(|| {
            BxError::new(
                "iblock_not_found",
                format!("Iblock with API_CODE=\"{api_code}\" not found"),
            )
        })?;
    Ok((snap, schema))
}

fn property_type(kind: &str) -> &'static str {
    match kind {
        "number" => "number",
        "list" => "list",
        "file" => "file",
        "element" => "element",
        "date" => "datetime",
        "boolean" => "boolean",
        _ => "string",
    }
}

fn iblock_json(snap: &Snapshot, schema: &Schema) -> Value {
    let mut props: Vec<&crate::collection::Field> = schema.props.iter().collect();
    props.sort_by(|a, b| {
        a.sort
            .cmp(&b.sort)
            .then_with(|| collate_key(&a.name).cmp(&collate_key(&b.name)))
    });
    let properties: Vec<Value> = props
        .into_iter()
        .map(|p| {
            let mut item = Map::new();
            item.insert("id".into(), json!(p.id));
            item.insert("name".into(), json!(p.name));
            item.insert("code".into(), json!(to_camel(&p.code)));
            let kind = if p.user_type == "directory" { "directory" } else { property_type(&p.kind) };
            item.insert("type".into(), json!(kind));
            item.insert("multiple".into(), json!(p.multiple));
            item.insert("sort".into(), json!(p.sort));
            if !p.user_type.is_empty() {
                item.insert("userType".into(), json!(p.user_type));
            }
            if p.kind == "element" && p.user_type.is_empty() {
                item.insert("linkIblockId".into(), json!(p.link_collection_id.unwrap_or(0)));
            }
            if p.kind == "list" {
                let values: Vec<Value> = snap
                    .property_enums(p.id)
                    .into_iter()
                    .map(|e| {
                        json!({ "id": e.id, "value": e.value, "xmlId": e.xml_id, "sort": e.sort, "default": e.is_default })
                    })
                    .collect();
                item.insert("values".into(), Value::Array(values));
            }
            Value::Object(item)
        })
        .collect();
    json!({
        "id": schema.collection.id,
        "apiCode": to_camel(&schema.collection.code),
        "name": schema.collection.name,
        "properties": properties,
    })
}

pub async fn iblock_list(axum::extract::State(state): axum::extract::State<AppState>) -> BxResult {
    let snap = state.registry.snapshot(&state.db).await?;
    let mut schemas: Vec<&Arc<Schema>> = snap
        .by_id
        .values()
        .filter(|s| s.collection.api_enabled)
        .collect();
    schemas.sort_by_key(|s| collate_key(&s.collection.name));
    let items: Vec<Value> = schemas.into_iter().map(|s| iblock_json(&snap, s)).collect();
    Ok(success(json!({ "items": items })))
}

pub async fn iblock_info(state: &AppState, api_code: &str) -> BxResult {
    let (snap, schema) = schema_for(state, api_code).await?;
    Ok(success(json!({ "item": iblock_json(&snap, &schema) })))
}

/// SQL выборки элементов по фильтру. Строится целиком до первого `await`:
/// контекст компиляции не `Send`.
pub fn build_list_query(
    snap: &Snapshot,
    schema: &Schema,
    state: &AppState,
    req: &ListRequest,
) -> Result<QueryBuilder<Postgres>, BxError> {
    let counter = Cell::new(0);
    let ctx = Ctx::root(snap, schema, &state.project, &counter);
    let mut qb = QueryBuilder::new(format!(
        "SELECT {ROW_COLS} FROM collection_items e WHERE e.collection_id = "
    ));
    qb.push_bind(schema.collection.id).push(" AND ");
    push_filter(&mut qb, &ctx, &req.filter)?;
    push_order(&mut qb, &ctx, &req.order)?;
    if let Some(limit) = req.limit {
        qb.push(" LIMIT ").push_bind(limit);
    }
    qb.push(" OFFSET ").push_bind(req.offset);
    Ok(qb)
}

pub async fn list(state: &AppState, api_code: &str, body: Map<String, Value>) -> BxResult {
    let (snap, schema) = schema_for(state, api_code).await?;
    let req = ListRequest::parse(&body, &state.project, &schema.collection.code)?;
    let mut qb = build_list_query(&snap, &schema, state, &req)?;
    let rows: Vec<Row> = qb.build_query_as().fetch_all(&state.db).await?;
    let env = Env {
        state,
        snap: &snap,
        project: &state.project,
        image_resize: req.image_resize.as_deref(),
    };
    let items = serialize(&env, &schema, &rows, &req.select, Mode::List, 0).await?;
    Ok(success(json!({ "items": items })))
}

/// Деталь элемента. `filter`/`order` тела, как у Битрикса, не используются.
pub async fn detail(
    state: &AppState,
    api_code: &str,
    key: Key<'_>,
    body: Map<String, Value>,
) -> BxResult {
    let (snap, schema) = schema_for(state, api_code).await?;
    let select = Select::parse(body.get("select"), &state.project, &schema.collection.code);
    let resize = image_resize(body.get("imageResize"));
    let sql = format!("SELECT {ROW_COLS} FROM collection_items e WHERE e.collection_id = $1 AND ");
    let row: Option<Row> = match key {
        Key::Id(id) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!("{sql} e.id = $2")))
                .bind(schema.collection.id)
                .bind(id)
                .fetch_optional(&state.db)
                .await?
        }
        Key::Slug(slug) => {
            let slug = percent_decode(slug);
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{sql} e.code = $2 AND e.code <> ''"
            )))
            .bind(schema.collection.id)
            .bind(slug)
            .fetch_optional(&state.db)
            .await?
        }
    };
    let Some(row) = row else {
        return Ok(success(json!({ "item": null })));
    };
    let env = Env {
        state,
        snap: &snap,
        project: &state.project,
        image_resize: resize.as_deref(),
    };
    let item = serialize(
        &env,
        &schema,
        std::slice::from_ref(&row),
        &select,
        Mode::Detail,
        0,
    )
    .await?
    .pop();
    Ok(success(json!({ "item": item })))
}

/// `rawurldecode` для сегмента пути.
fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%D0%B0"), "а");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%Яx"), "%Яx");
    }
}
