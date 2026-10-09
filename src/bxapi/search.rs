//! `POST /iblock/{apiCode}/search`. В Битриксе — индекс модуля search (заголовок +
//! свойства с флагом «участвует в поиске»); здесь — все слова запроса должны
//! встретиться в названии или поисковых свойствах. Совпадения в названии выше.
//! Как и в индексе Битрикса, активность раздела не учитывается — только элемента.
//! Если ничего не нашлось — повтор в другой раскладке / с латинскими двойниками
//! кириллицы (`queryCorrection`).

use serde_json::{Map, Value, json};
use sqlx::{Postgres, QueryBuilder};

use super::{
    BxError, BxResult,
    elements::schema_for,
    query::{Select, image_resize, number},
    registry::Schema,
    serialize::{Env, Mode, ROW_COLS, Row, detail_page_url, serialize},
    success,
};
use crate::{collection::is_valid_code, state::AppState};

const MAX_LIMIT: i64 = 200;

pub async fn search(state: &AppState, api_code: &str, body: Map<String, Value>) -> BxResult {
    let (snap, schema) = schema_for(state, api_code).await?;
    let query = body
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let limit = number(body.get("limit"))
        .filter(|l| *l > 0)
        .map_or(MAX_LIMIT, |l| l.min(MAX_LIMIT));
    let resize = image_resize(body.get("imageResize"));
    let props = state.project.search_props(&schema.collection.code);

    let mut found = find(state, &schema, props, &query, limit).await?;
    let mut correction = None;
    if found.0.is_empty() && found.1.is_empty() && !query.is_empty() {
        for variant in corrections(&query) {
            let attempt = find(state, &schema, props, &variant, limit).await?;
            if !attempt.0.is_empty() || !attempt.1.is_empty() {
                found = attempt;
                correction = Some(variant);
                break;
            }
        }
    }
    let (rows, sections) = found;

    // description: запрошенные поля элемента с «плоскими» camelCase-ключами
    let description_paths: Vec<String> = body
        .get("itemDescription")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut select = Select::default();
    for path in &description_paths {
        select.add(path, &state.project, &schema.collection.code);
    }
    select.add("image", &state.project, &schema.collection.code);
    let env = Env {
        state,
        snap: &snap,
        project: &state.project,
        image_resize: resize.as_deref(),
    };
    let serialized = serialize(&env, &schema, &rows, &select, Mode::Related, 0).await?;

    let items: Vec<Value> = rows
        .iter()
        .zip(serialized)
        .map(|(row, fields)| {
            let mut item = json!({
                "name": row.name,
                "slug": detail_page_url(&schema, row),
                "img": fields.get("image").cloned().unwrap_or(Value::Null),
            });
            if body.contains_key("itemDescription") {
                let mut description = Map::new();
                for path in &description_paths {
                    let root = path.split('.').next().unwrap_or(path);
                    let mut value = fields.get(root).cloned().unwrap_or(Value::Null);
                    let prop = schema.prop(&super::to_snake(root));
                    if value.is_null() && prop.is_some_and(|p| p.multiple) {
                        value = Value::Array(Vec::new());
                    }
                    description.insert(flat_key(path), value);
                }
                item["description"] = Value::Object(description);
            }
            item
        })
        .collect();

    let section_items: Vec<Value> = sections
        .iter()
        .map(|s| json!({ "name": s.1, "slug": s.2 }))
        .collect();
    let mut data = json!({ "items": items, "sections": section_items });
    for group in &state.project.search_groups {
        let rows = group.build(state, &query).await?;
        data[group.key()] = Value::Array(rows);
    }
    if let Some(c) = correction {
        data["queryCorrection"] = Value::from(c);
    }
    Ok(success(data))
}

/// `relatedBrands.item.ufName` → `relatedBrandsItemUfName`.
fn flat_key(path: &str) -> String {
    let mut out = String::new();
    for (i, seg) in path.split('.').enumerate() {
        if i == 0 {
            out.push_str(seg);
        } else {
            let mut chars = seg.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
        }
    }
    out
}

type Found = (Vec<Row>, Vec<(i64, String, String)>);

async fn find(
    state: &AppState,
    schema: &Schema,
    props: &[&str],
    query: &str,
    limit: i64,
) -> Result<Found, BxError> {
    let words: Vec<String> = query
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| !w.is_empty())
        .take(10)
        .collect();
    if words.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let patterns: Vec<String> = words
        .iter()
        .map(|w| {
            format!(
                "%{}%",
                w.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )
        })
        .collect();

    let mut qb: QueryBuilder<Postgres> = {
        let mut qb = QueryBuilder::new(format!(
            "SELECT {ROW_COLS} FROM collection_items e WHERE e.collection_id = "
        ));
        qb.push_bind(schema.collection.id).push(" AND e.active");
        let mut haystack = String::from("e.name");
        for p in props.iter().filter(|p| is_valid_code(p)) {
            haystack.push_str(&format!(
                " || ' ' || COALESCE(e.field_values ->> '{p}', '')"
            ));
        }
        for pattern in &patterns {
            qb.push(format!(" AND ({haystack}) ILIKE "))
                .push_bind(pattern.clone());
        }
        // Сначала совпадения в названии, потом новые
        qb.push(" ORDER BY (e.name ILIKE ALL(")
            .push_bind(patterns.clone())
            .push(")) DESC, e.id DESC");
        qb.push(" LIMIT ").push_bind(limit);
        qb
    };
    let rows: Vec<Row> = qb.build_query_as().fetch_all(&state.db).await?;

    // Разделы: активные, все слова в названии; лимит — общий с элементами
    let rest = (limit as usize).saturating_sub(rows.len());
    let mut sections: Vec<(i64, String, String)> = schema
        .sections
        .values()
        .filter(|s| s.active)
        .filter(|s| {
            let name = s.name.to_lowercase();
            words.iter().all(|w| name.contains(w.as_str()))
        })
        .map(|s| (s.id, s.name.clone(), section_url(schema, s.id)))
        .collect();
    sections.sort_by_key(|s| s.0);
    sections.truncate(rest);
    Ok((rows, sections))
}

fn section_url(schema: &Schema, id: i64) -> String {
    let template = &schema.collection.section_page_url;
    if template.is_empty() {
        return String::new();
    }
    let chain = schema.section_chain(id);
    let path: Vec<&str> = chain.iter().map(|s| s.code.as_str()).collect();
    let url = template
        .replace("#SITE_DIR#", "")
        .replace("#SECTION_CODE_PATH#", &path.join("/"))
        .replace("#SECTION_ID#", &id.to_string());
    let mut out = String::new();
    for ch in url.chars() {
        if !(ch == '/' && out.ends_with('/')) {
            out.push(ch);
        }
    }
    out
}

const RU: &str = "йцукенгшщзхъфывапролджэячсмитьбю.ё";
const EN: &str = "qwertyuiop[]asdfghjkl;'zxcvbnm,./`";

/// Варианты исправления запроса: латиница ↔ кириллица по раскладке и
/// кириллические буквы-двойники латинских (А100 → A100).
fn corrections(query: &str) -> Vec<String> {
    let map = |from: &str, to: &str, q: &str| -> String {
        let from: Vec<char> = from.chars().collect();
        let to: Vec<char> = to.chars().collect();
        q.chars()
            .map(|c| {
                let lower = c.to_lowercase().next().unwrap_or(c);
                match from.iter().position(|f| *f == lower) {
                    Some(i) if c.is_uppercase() => to[i].to_uppercase().next().unwrap_or(to[i]),
                    Some(i) => to[i],
                    None => c,
                }
            })
            .collect()
    };
    let lookalike: String = query
        .chars()
        .map(|c| match c {
            'А' => 'A',
            'В' => 'B',
            'Е' => 'E',
            'К' => 'K',
            'М' => 'M',
            'Н' => 'H',
            'О' => 'O',
            'Р' => 'P',
            'С' => 'C',
            'Т' => 'T',
            'Х' => 'X',
            'а' => 'a',
            'е' => 'e',
            'о' => 'o',
            'р' => 'p',
            'с' => 'c',
            'х' => 'x',
            'у' => 'y',
            other => other,
        })
        .collect();
    let mut out = Vec::new();
    for v in [map(RU, EN, query), map(EN, RU, query), lookalike] {
        if v != query && !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_layouts() {
        assert_eq!(
            flat_key("relatedBrands.item.ufName"),
            "relatedBrandsItemUfName"
        );
        assert_eq!(flat_key("oem"), "oem");
        let c = corrections("ф100");
        assert!(c.contains(&"a100".to_string()));
        assert!(corrections("cnjk").contains(&"стол".to_string()));
        assert!(corrections("А100").contains(&"A100".to_string()));
    }
}
