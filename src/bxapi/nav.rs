//! `nav/breadcrumbs` и `nav/legacy-redirect`.

use axum::{body::Bytes, extract::State};
use serde_json::{Map, Value, json};

use super::{BxError, BxResult, parse_body, registry::Schema, success, to_snake};
use crate::state::AppState;

/// Путь из `path` или `url`; без обоих — ошибка с кодом `missing`.
fn input_path(body: &Map<String, Value>, missing: &str) -> Result<String, BxError> {
    let raw = body
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| body.get("url").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| BxError::new(missing, "Не передан path или url"))?;
    // Из полного адреса берётся только путь
    let path = match raw.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
        None => raw,
    };
    let path = path.split(['?', '#']).next().unwrap_or("/");
    Ok(if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    })
}

fn crumb(label: &str, path: &str) -> Value {
    json!({ "label": label, "path": path, "current": false })
}

/// Путь с закрывающим слешем (кроме корня).
fn with_slash(segments: &[&str]) -> String {
    if segments.is_empty() {
        "/".into()
    } else {
        format!("/{}/", segments.join("/"))
    }
}

/// «dsg-direct_shift» → «Dsg Direct Shift» (как ucwords у Битрикса).
fn humanize(slug: &str) -> String {
    slug.split(['-', '_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            chars
                .next()
                .map(|f| f.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Цепочка инфоблока: сегменты после префикса — коды разделов, последний может
/// быть элементом (по коду или id). Не сошлось — `None`.
async fn iblock_chain(
    state: &AppState,
    schema: &Schema,
    root_label: &str,
    prefix: &[&str],
    rest: &[&str],
) -> Result<Option<Vec<Value>>, BxError> {
    let mut crumbs = vec![crumb(root_label, &with_slash(prefix))];
    let mut path: Vec<&str> = prefix.to_vec();
    let mut parent: Option<i64> = None;
    for (i, seg) in rest.iter().enumerate() {
        path.push(seg);
        let section = schema
            .sections
            .values()
            .find(|s| s.code == *seg && s.parent_id == parent)
            .or_else(|| {
                (parent.is_none())
                    .then(|| schema.sections.values().find(|s| s.code == *seg))
                    .flatten()
            });
        if let Some(s) = section {
            crumbs.push(crumb(&s.name, &with_slash(&path)));
            parent = Some(s.id);
            continue;
        }
        if i + 1 != rest.len() {
            return Ok(None);
        }
        let id: i64 = seg.parse().unwrap_or(0);
        let name: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM collection_items
             WHERE collection_id = $1 AND ((code <> '' AND code = $2) OR id = $3)
             ORDER BY (code = $2) DESC LIMIT 1",
        )
        .bind(schema.collection.id)
        .bind(*seg)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
        match name {
            Some((name,)) => crumbs.push(crumb(&name, &with_slash(&path))),
            None => return Ok(None),
        }
    }
    Ok(Some(crumbs))
}

pub async fn breadcrumbs(State(state): State<AppState>, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let path = input_path(&body, "breadcrumbs_missing_input")?;
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let home = state.project.home_label;
    let snap = state.registry.snapshot(&state.db).await?;

    let mut crumbs: Option<Vec<Value>> = None;
    // 1. Явные правила проекта (URL каталога не совпадает с apiCode инфоблока)
    for rule in &state.project.breadcrumb_rules {
        let prefix: Vec<&str> = rule.prefix.split('/').filter(|s| !s.is_empty()).collect();
        if !segments.starts_with(&prefix) {
            continue;
        }
        if let Some(schema) = snap.by_code(&to_snake(rule.api_code)) {
            crumbs = iblock_chain(
                &state,
                schema,
                rule.root_label,
                &prefix,
                &segments[prefix.len()..],
            )
            .await?;
            if crumbs.is_some() {
                break;
            }
        }
    }
    // 2. Первый сегмент — apiCode инфоблока
    if crumbs.is_none()
        && let Some(first) = segments.first()
        && let Some(schema) = snap
            .by_code(&to_snake(first))
            .filter(|s| s.collection.api_enabled)
    {
        crumbs = iblock_chain(
            &state,
            schema,
            &schema.collection.name,
            &segments[..1],
            &segments[1..],
        )
        .await?;
    }
    // 3. Заглушка: главная + названия из сегментов
    let mut items = crumbs.unwrap_or_else(|| {
        let mut items = vec![crumb(home, "/")];
        for i in 0..segments.len() {
            items.push(crumb(&humanize(segments[i]), &with_slash(&segments[..=i])));
        }
        items
    });
    if items.first().and_then(|c| c["path"].as_str()) != Some("/") {
        items.insert(0, crumb(home, "/"));
    }
    if let Some(last) = items.last_mut() {
        last["current"] = Value::Bool(true);
    }
    Ok(success(json!({ "items": items })))
}

/// Числовой хвост пути: `/catalog/a/b/59957/` → 59957.
fn tail_id(path: &str) -> Option<i64> {
    path.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

pub async fn legacy_redirect(State(state): State<AppState>, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let path = input_path(&body, "legacy_redirect_missing_input")?;
    let snap = state.registry.snapshot(&state.db).await?;
    for rule in &state.project.legacy_rules {
        if rule.prefix.is_some_and(|p| !path.starts_with(p)) {
            continue;
        }
        let (Some(id), Some(schema)) = (tail_id(&path), snap.by_code(&to_snake(rule.api_code)))
        else {
            continue;
        };
        let column = |f: &str| match f {
            "code" => "code",
            "xmlId" => "xml_id",
            _ => "id::text",
        };
        let sql = format!(
            "SELECT {}::text FROM collection_items WHERE collection_id = $1 AND active AND {} = $2",
            column(rule.value_field),
            column(rule.match_field)
        );
        let value: Option<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(schema.collection.id)
            .bind(id.to_string())
            .fetch_optional(&state.db)
            .await?;
        if let Some((value,)) = value.filter(|(v,)| !v.is_empty()) {
            let url = rule.template.replace("{value}", &value);
            return Ok(success(json!({ "item": { "url": url } })));
        }
    }
    Ok(success(json!({ "item": null })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(
            humanize("dsg-direct_shiftgearbox"),
            "Dsg Direct Shiftgearbox"
        );
        assert_eq!(with_slash(&["news", "x"]), "/news/x/");
        assert_eq!(tail_id("/catalog/a/b/59957/"), Some(59957));
        assert_eq!(tail_id("/news/abc/"), None);
        let body: Map<String, Value> =
            serde_json::from_str(r#"{"url":"https://site.ru/news/x/?a=1"}"#).unwrap();
        assert_eq!(input_path(&body, "m").unwrap(), "/news/x/");
    }
}
