//! `section/list`. Разделы уже в памяти (снимок реестра) — фильтр, сортировка и
//! страницы считаются без БД; в БД ходит только `hasElements`.

use std::{cell::Cell, cmp::Ordering, collections::HashSet};

use serde_json::{Map, Value, json};
use sqlx::{Postgres, QueryBuilder};

use super::{
    BxError, BxResult, collate_key,
    elements::schema_for,
    filter::{Ctx, Op, push_filter, split_op, value_matches},
    images,
    query::{ListRequest, number},
    registry::{Schema, Snapshot},
    success,
};
use crate::{collection::Section, files, state::AppState};

fn section_field(schema: &Schema, s: &Section, field: &str) -> Option<Value> {
    Some(match field {
        "id" => json!(s.id),
        "name" => json!(s.name),
        "code" => json!(s.code),
        "xmlId" | "externalId" => json!(s.xml_id),
        "active" => json!(if s.active { "Y" } else { "N" }),
        "globalActive" => json!(if schema.globally_active.contains(&s.id) {
            "Y"
        } else {
            "N"
        }),
        "sort" => json!(s.sort),
        "depthLevel" => json!(s.depth_level),
        "iblockSectionId" => json!(s.parent_id.unwrap_or(0)),
        "description" => json!(s.description),
        _ => return None,
    })
}

/// Фильтр раздела в памяти: те же операторы и группы `logic`, что у элементов.
fn matches(schema: &Schema, s: &Section, filter: &Map<String, Value>) -> Result<bool, BxError> {
    let is_or = filter
        .get("logic")
        .and_then(Value::as_str)
        .is_some_and(|l| l.eq_ignore_ascii_case("or"));
    let mut results = Vec::new();
    for (key, value) in filter {
        if key.eq_ignore_ascii_case("logic") {
            continue;
        }
        let ok = match value {
            Value::Object(sub) if key.bytes().all(|b| b.is_ascii_digit()) => {
                matches(schema, s, sub)?
            }
            _ => {
                let (op, path) = split_op(key);
                let field = section_field(schema, s, path).ok_or_else(|| {
                    BxError::new("invalid_filter", format!("Unknown filter field: {path}"))
                })?;
                let values: Vec<Value> = match value {
                    Value::Array(items) => items.iter().map(bool_marker).collect(),
                    other => vec![bool_marker(other)],
                };
                let op = match (op, value.is_array()) {
                    (Op::Eq, true) => Op::In,
                    (Op::Ne, true) => Op::NotIn,
                    (op, _) => op,
                };
                // «Без родителя» — 0 или null
                let values: Vec<Value> = values
                    .into_iter()
                    .map(|v| {
                        if v.is_null() && path == "iblockSectionId" {
                            json!(0)
                        } else {
                            v
                        }
                    })
                    .collect();
                value_matches(op, &field, &values)
            }
        };
        results.push(ok);
    }
    Ok(if results.is_empty() {
        true
    } else if is_or {
        results.iter().any(|r| *r)
    } else {
        results.iter().all(|r| *r)
    })
}

fn bool_marker(v: &Value) -> Value {
    match v {
        Value::Bool(true) => json!("Y"),
        Value::Bool(false) => json!("N"),
        other => other.clone(),
    }
}

fn compare(schema: &Schema, a: &Section, b: &Section, order: &[super::query::Order]) -> Ordering {
    for o in order {
        let (x, y) = (
            section_field(schema, a, &o.field).unwrap_or(Value::Null),
            section_field(schema, b, &o.field).unwrap_or(Value::Null),
        );
        let ord = match (&x, &y) {
            (Value::Number(p), Value::Number(q)) => p
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&q.as_f64().unwrap_or(0.0)),
            _ => collate_key(x.as_str().unwrap_or("")).cmp(&collate_key(y.as_str().unwrap_or(""))),
        };
        let ord = if o.desc { ord.reverse() } else { ord };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.id.cmp(&b.id)
}

/// Разделы, где прямо лежат элементы под фильтром `hasElements` (как в Битриксе,
/// предки не добавляются). `rollupToDepth: N` — вместо раздела глубже N берётся его
/// предок на уровне N, разделы выше уровня N не попадают.
async fn sections_with_elements(
    state: &AppState,
    snap: &Snapshot,
    schema: &Schema,
    has_elements: &Map<String, Value>,
) -> Result<HashSet<i64>, BxError> {
    let filter = match has_elements.get("filter") {
        Some(Value::Object(f)) => f.clone(),
        _ => Map::new(),
    };
    let mut qb: QueryBuilder<Postgres> = {
        let counter = Cell::new(0);
        let ctx = Ctx::root(snap, schema, &state.project, &counter);
        let mut qb = QueryBuilder::new(
            "SELECT DISTINCT e.section_id FROM collection_items e WHERE e.section_id IS NOT NULL AND e.collection_id = ",
        );
        qb.push_bind(schema.collection.id).push(" AND ");
        push_filter(&mut qb, &ctx, &filter)?;
        qb
    };
    let direct: Vec<(i64,)> = qb.build_query_as().fetch_all(&state.db).await?;

    let rollup = number(has_elements.get("rollupToDepth")).filter(|d| *d > 0);
    let mut out = HashSet::new();
    for (section_id,) in direct {
        match rollup {
            Some(depth) => {
                let chain = schema.section_chain(section_id);
                if let Some(s) = chain.iter().find(|s| i64::from(s.depth_level) == depth) {
                    out.insert(s.id);
                }
            }
            None => {
                out.insert(section_id);
            }
        }
    }
    Ok(out)
}

pub async fn list(state: &AppState, api_code: &str, body: Map<String, Value>) -> BxResult {
    let (snap, schema) = schema_for(state, api_code).await?;
    let req = ListRequest::parse(&body, &state.project, &schema.collection.code)?;
    let allowed = match body.get("hasElements") {
        Some(Value::Object(h)) => Some(sections_with_elements(state, &snap, &schema, h).await?),
        _ => None,
    };

    let mut sections: Vec<&Section> = Vec::new();
    for s in schema.sections.values() {
        if allowed.as_ref().is_some_and(|a| !a.contains(&s.id)) {
            continue;
        }
        if matches(&schema, s, &req.filter)? {
            sections.push(s);
        }
    }
    let order = if req.order.is_empty() {
        vec![super::query::Order {
            field: "sort".into(),
            desc: false,
        }]
    } else {
        req.order.clone()
    };
    sections.sort_by(|a, b| compare(&schema, a, b, &order));
    let page: Vec<&Section> = sections
        .into_iter()
        .skip(req.offset as usize)
        .take(req.limit.unwrap_or(i64::MAX) as usize)
        .collect();

    let mut fields: Vec<String> = req.select.fields.iter().map(|f| f.root.clone()).collect();
    if fields.is_empty() {
        fields = [
            "id",
            "name",
            "code",
            "active",
            "sort",
            "depthLevel",
            "iblockSectionId",
        ]
        .map(String::from)
        .to_vec();
    }
    let picture_ids: Vec<i64> = page.iter().filter_map(|s| s.picture_id).collect();
    let pictures: std::collections::HashMap<i64, files::FileRecord> =
        files::get_many(&state.db, &picture_ids)
            .await?
            .into_iter()
            .map(|f| (f.id, f))
            .collect();
    let resize = req.image_resize.as_deref();
    let widths = &state.project.image_widths;

    let items: Vec<Value> = page
        .into_iter()
        .map(|s| {
            let mut item = Map::new();
            for field in &fields {
                let value = match field.as_str() {
                    "image" | "picture" => images::image_value(s.picture_id.and_then(|id| pictures.get(&id)), resize, widths),
                    "imageExt" => json!({
                        "small": images::image_value(s.picture_id.and_then(|id| pictures.get(&id)), resize, widths),
                        "large": null,
                    }),
                    "sectionPageUrl" => json!(section_page_url(&schema, s)),
                    // В ответе элементы-поля у разделов — числа/строки как в Битриксе
                    "active" => json!(s.active),
                    other => section_field(&schema, s, other).unwrap_or(Value::Null),
                };
                item.insert(field.clone(), value);
            }
            Value::Object(item)
        })
        .collect();
    Ok(success(json!({ "items": items })))
}

fn section_page_url(schema: &Schema, s: &Section) -> String {
    let template = &schema.collection.section_page_url;
    if template.is_empty() {
        return String::new();
    }
    let path: Vec<&str> = schema
        .section_chain(s.id)
        .iter()
        .map(|c| c.code.as_str())
        .collect();
    let url = template
        .replace("#SITE_DIR#", "")
        .replace("#SECTION_CODE_PATH#", &path.join("/"))
        .replace("#SECTION_CODE#", &s.code)
        .replace("#SECTION_ID#", &s.id.to_string())
        .replace("#ID#", &s.id.to_string())
        .replace("#CODE#", &s.code);
    let mut out = String::with_capacity(url.len());
    for ch in url.chars() {
        if !(ch == '/' && out.ends_with('/')) {
            out.push(ch);
        }
    }
    out
}
