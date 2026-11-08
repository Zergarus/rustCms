//! Sitemap: конфигурация типов в БД и два эндпоинта в контракте Битрикса.
//!
//! Как в битриксовом модуле `seo` (`b_seo_sitemap` + `b_seo_sitemap_iblock`):
//! тип — это строка `seo_sitemap`, связанная с коллекциями, а адрес страницы
//! берётся из шаблона типа или из шаблона коллекции. XML и имена файлов собирает
//! витрина, поэтому `changefreq`, `priority` и имена файлов здесь не хранятся.
//!
//! См. `docs/superpowers/specs/2026-11-09-sitemap-api-design.md`.

use axum::{
    extract::{Path, State},
    response::Response,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::{FromRow, PgPool};

use crate::{
    error::{AppError, AppResult},
    state::AppState,
};

/// Строка `seo_sitemap`: один тип в API.
#[derive(Clone, Debug, FromRow, Serialize)]
pub struct Type {
    pub id: i64,
    pub name: String,
    pub active: bool,
    pub path_template: String,
    pub include: String,
    pub sort: i32,
}

/// Коллекция типа вместе с шаблонами адресов.
#[derive(Clone, Debug, FromRow, Serialize)]
pub struct Source {
    pub collection_id: i64,
    pub code: String,
    pub detail_page_url: String,
    pub section_page_url: String,
}

/// Тип вместе с коллекциями.
#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    #[serde(flatten)]
    pub sitemap: Type,
    pub sources: Vec<Source>,
}

/// Данные для подстановки в шаблон адреса.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ctx<'a> {
    pub section_path: &'a str,
    pub section_code: &'a str,
    pub element_id: i64,
    pub element_code: &'a str,
}

/// Подставляет в битриксовый шаблон адрес страницы и приводит его к каноническому виду.
pub fn render(template: &str, ctx: &Ctx) -> String {
    let id = ctx.element_id.to_string();
    let code = ctx.element_code;
    let code_or_id = if code.is_empty() {
        id.clone()
    } else {
        ascii_lower(code)
    };
    let path = template
        .replace("#SITE_DIR#", "")
        .replace("#SECTION_CODE_PATH#", ctx.section_path)
        .replace("#SECTION_CODE#", ctx.section_code)
        .replace("#ELEMENT_CODE_OR_ID#", &code_or_id)
        .replace("#ELEMENT_ID#", &id)
        .replace("#ELEMENT_CODE#", code)
        .replace("#ID#", &id)
        .replace("#CODE#", code);
    normalize(&path)
}

/// Канонический вид адреса: нижний регистр, без двойных слэшей, один на конце.
pub fn normalize(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 1);
    for ch in ascii_lower(path).chars() {
        if ch == '/' && out.ends_with('/') {
            continue;
        }
        out.push(ch);
    }
    if !out.starts_with('/') {
        out.insert(0, '/');
    }
    if !out.ends_with('/') {
        out.push('/');
    }
    out
}

fn ascii_lower(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

/// Типы с коллекциями: активные только или все (админка).
pub async fn list(db: &PgPool, active_only: bool) -> AppResult<Vec<Entry>> {
    let rows: Vec<Type> = sqlx::query_as(
        "SELECT id, name, active, path_template, include, sort
         FROM seo_sitemap
         WHERE NOT $1 OR active
         ORDER BY sort, id",
    )
    .bind(active_only)
    .fetch_all(db)
    .await?;
    let links: Vec<(i64, i64, String, String, String)> = sqlx::query_as(
        "SELECT l.sitemap_id, c.id, c.code, c.detail_page_url, c.section_page_url
         FROM seo_sitemap_collection l
         JOIN collections c ON c.id = l.collection_id
         ORDER BY l.sitemap_id, c.id",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|sitemap| Entry {
            sources: links
                .iter()
                .filter(|(id, ..)| *id == sitemap.id)
                .map(|(_, collection_id, code, detail, section)| Source {
                    collection_id: *collection_id,
                    code: code.clone(),
                    detail_page_url: detail.clone(),
                    section_page_url: section.clone(),
                })
                .collect(),
            sitemap,
        })
        .collect())
}

/// Индекс: типы в порядке `sort, id` со счётчиком и последней датой изменения.
pub async fn index(State(state): State<AppState>) -> AppResult<Response> {
    let mut items = Vec::new();
    for entry in list(&state.db, true).await? {
        let (mut count, mut lastmod) = (0i64, None::<DateTime<Utc>>);
        for source in &entry.sources {
            if template(&entry.sitemap, source).is_empty() {
                continue;
            }
            let (source_count, source_lastmod) =
                aggregate(&state.db, &entry.sitemap, source).await?;
            count += source_count;
            if let Some(at) = source_lastmod {
                lastmod = Some(lastmod.map_or(at, |current: DateTime<Utc>| current.max(at)));
            }
        }
        items.push(json!({
            "type": entry.sitemap.name,
            "count": count,
            "lastmod": lastmod.map(format_date),
        }));
    }
    Ok(crate::bxapi::success(json!({ "items": items })))
}

/// Записи одного типа: 404, если тип не активен или не настроен.
pub async fn kind(State(state): State<AppState>, Path(raw): Path<String>) -> AppResult<Response> {
    let entry = list(&state.db, true)
        .await?
        .into_iter()
        .find(|entry| entry.sitemap.name == raw)
        .ok_or(AppError::NotFound)?;
    let mut items = Vec::new();
    for source in &entry.sources {
        let template = template(&entry.sitemap, source);
        if template.is_empty() {
            continue;
        }
        if entry.sitemap.include == "sections" {
            for (section, code, updated_at, path) in
                sections(&state.db, source.collection_id).await?
            {
                let ctx = Ctx {
                    section_path: &path,
                    section_code: &code,
                    element_id: section,
                    element_code: &code,
                };
                items.push(
                    json!({ "path": render(template, &ctx), "lastmod": format_date(updated_at) }),
                );
            }
        } else {
            for (item, code, updated_at, section_code, section_path) in
                items_of(&state.db, source.collection_id).await?
            {
                let ctx = Ctx {
                    section_path: &section_path,
                    section_code: &section_code,
                    element_id: item,
                    element_code: &code,
                };
                items.push(
                    json!({ "path": render(template, &ctx), "lastmod": format_date(updated_at) }),
                );
            }
        }
    }
    Ok(crate::bxapi::success(json!({
        "type": entry.sitemap.name,
        "items": items,
    })))
}

/// Шаблон адреса: свой у типа, иначе шаблон коллекции. Пустой — адресов нет.
fn template<'a>(sitemap: &'a Type, source: &'a Source) -> &'a str {
    if !sitemap.path_template.is_empty() {
        return &sitemap.path_template;
    }
    if sitemap.include == "sections" {
        &source.section_page_url
    } else {
        &source.detail_page_url
    }
}

fn format_date(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// Сколько записей типа видно витрине и когда последняя менялась.
async fn aggregate(
    db: &PgPool,
    sitemap: &Type,
    source: &Source,
) -> AppResult<(i64, Option<DateTime<Utc>>)> {
    if sitemap.include == "sections" {
        return Ok(sqlx::query_as(
            "SELECT count(*), max(s.updated_at)
             FROM collection_sections s
             JOIN collections c ON c.id = s.collection_id
             WHERE s.collection_id = $1 AND s.active AND c.api_enabled",
        )
        .bind(source.collection_id)
        .fetch_one(db)
        .await?);
    }
    Ok(sqlx::query_as(
        "SELECT count(*), max(i.updated_at)
         FROM collection_items i
         JOIN collections c ON c.id = i.collection_id
         WHERE i.collection_id = $1
           AND c.api_enabled
           AND i.active
           AND (i.published_at IS NULL OR i.published_at <= now())",
    )
    .bind(source.collection_id)
    .fetch_one(db)
    .await?)
}

/// Активные опубликованные записи коллекции с кодом и путём их раздела.
async fn items_of(
    db: &PgPool,
    collection_id: i64,
) -> AppResult<Vec<(i64, String, DateTime<Utc>, String, String)>> {
    Ok(sqlx::query_as(
        "WITH RECURSIVE tree AS (
             SELECT id, code, code::text AS path FROM collection_sections WHERE parent_id IS NULL
             UNION ALL
             SELECT s.id, s.code, tree.path || '/' || s.code
             FROM collection_sections s JOIN tree ON s.parent_id = tree.id
         )
         SELECT i.id, i.code, i.updated_at,
                COALESCE(tree.code, ''), COALESCE(tree.path, '')
         FROM collection_items i
         JOIN collections c ON c.id = i.collection_id
         LEFT JOIN tree ON tree.id = i.section_id
         WHERE i.collection_id = $1
           AND c.api_enabled
           AND i.active
           AND (i.published_at IS NULL OR i.published_at <= now())
         ORDER BY i.id",
    )
    .bind(collection_id)
    .fetch_all(db)
    .await?)
}

/// Активные разделы коллекции с полным путём от корня.
async fn sections(
    db: &PgPool,
    collection_id: i64,
) -> AppResult<Vec<(i64, String, DateTime<Utc>, String)>> {
    Ok(sqlx::query_as(
        "WITH RECURSIVE tree AS (
             SELECT id, code, code::text AS path
             FROM collection_sections
             WHERE collection_id = $1 AND parent_id IS NULL
             UNION ALL
             SELECT s.id, s.code, tree.path || '/' || s.code
             FROM collection_sections s JOIN tree ON s.parent_id = tree.id
         )
         SELECT t.id, t.code, s.updated_at, t.path
         FROM tree t
         JOIN collection_sections s ON s.id = t.id
         JOIN collections c ON c.id = s.collection_id
         WHERE s.active AND c.api_enabled
         ORDER BY t.id",
    )
    .bind(collection_id)
    .fetch_all(db)
    .await?)
}

/// Данные формы админки. Значения по умолчанию совпадают с дефолтами таблицы
/// и с поведением Битрикса: новый тип активен, сортировка 500, включаются записи.
#[derive(Clone, Debug, Serialize)]
pub struct Input {
    pub name: String,
    pub active: bool,
    pub sort: i32,
    pub path_template: String,
    pub include: String,
    pub collections: Vec<i64>,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            name: String::new(),
            active: true,
            sort: 500,
            path_template: String::new(),
            include: "items".into(),
            collections: Vec::new(),
        }
    }
}

impl Input {
    pub fn validate(&self) -> Result<(), String> {
        let name_ok = !self.name.is_empty()
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !name_ok {
            return Err("Имя типа: строчные латинские буквы, цифры и дефис".into());
        }
        if self.path_template.chars().count() > 500 {
            return Err("Шаблон адреса — не длиннее 500 символов".into());
        }
        if !["items", "sections"].contains(&self.include.as_str()) {
            return Err("Что включать: записи или разделы".into());
        }
        Ok(())
    }
}

/// Один тип со связями — для формы правки.
pub async fn get(db: &PgPool, id: i64) -> AppResult<Entry> {
    list(db, false)
        .await?
        .into_iter()
        .find(|entry| entry.sitemap.id == id)
        .ok_or(AppError::NotFound)
}

/// Создаёт (`id = None`) или обновляет тип; связи с коллекциями перезаписываются.
pub async fn save(db: &PgPool, id: Option<i64>, input: &Input) -> AppResult<i64> {
    let mut tx = db.begin().await?;
    let id = match id {
        Some(id) => {
            sqlx::query(
                "UPDATE seo_sitemap
                 SET name = $2, active = $3, sort = $4, path_template = $5, include = $6,
                     updated_at = now()
                 WHERE id = $1",
            )
            .bind(id)
            .bind(&input.name)
            .bind(input.active)
            .bind(input.sort)
            .bind(&input.path_template)
            .bind(&input.include)
            .execute(&mut *tx)
            .await?;
            id
        }
        None => {
            sqlx::query_scalar(
                "INSERT INTO seo_sitemap(name, active, sort, path_template, include)
                 VALUES ($1, $2, $3, $4, $5) RETURNING id",
            )
            .bind(&input.name)
            .bind(input.active)
            .bind(input.sort)
            .bind(&input.path_template)
            .bind(&input.include)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    sqlx::query("DELETE FROM seo_sitemap_collection WHERE sitemap_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for collection_id in &input.collections {
        sqlx::query(
            "INSERT INTO seo_sitemap_collection(sitemap_id, collection_id) VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(collection_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(id)
}

pub async fn remove(db: &PgPool, id: i64) -> AppResult<()> {
    sqlx::query("DELETE FROM seo_sitemap WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}
