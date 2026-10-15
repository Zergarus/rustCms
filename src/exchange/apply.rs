//! Запись разобранной выгрузки в БД: классификатор, затем товары и предложения.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use sqlx::PgConnection;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::cml::{
    Catalog, Classifier, Goods, Offer, OfferPackage, Parsed, Price, PropKind, Property,
};
use super::settings::{Action, Settings};
use crate::bxapi::project::ImportHook;
use crate::collection::{Collection, is_valid_code, is_valid_slug, slugify};

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("Ошибка базы данных: {0}")]
    Db(#[from] sqlx::Error),
    #[error("Некорректные данные пакета: {0}")]
    Invalid(String),
    #[error("Ошибка работы с файлом: {0}")]
    File(String),
    #[error("Ошибка обработчика проекта: {0}")]
    Hook(anyhow::Error),
}

pub struct Ctx<'a> {
    pub settings: &'a Settings,
    pub session_id: Option<i64>,
    pub session_dir: &'a Path,
    pub upload_dir: &'a Path,
    /// Обработчик проекта (`Project::import_hook`).
    pub hook: Option<&'a dyn ImportHook>,
    /// Файлы, записанные на диск во время `apply`. Если транзакция откатывается, они
    /// остаются сиротами; `apply` удаляет их сам при своей ошибке, а вызывающий — через
    /// `discard_written`, если ошибка случилась уже после `apply` (например, при коммите).
    written: Mutex<Vec<PathBuf>>,
}

impl<'a> Ctx<'a> {
    pub fn new(
        settings: &'a Settings,
        session_id: Option<i64>,
        session_dir: &'a Path,
        upload_dir: &'a Path,
    ) -> Self {
        Ctx {
            settings,
            session_id,
            session_dir,
            upload_dir,
            hook: None,
            written: Mutex::new(Vec::new()),
        }
    }

    pub fn with_hook(mut self, hook: Option<&'a dyn ImportHook>) -> Self {
        self.hook = hook;
        self
    }

    /// Удаляет файлы, записанные на диск этим контекстом (вызывать, если транзакция не
    /// закоммичена). Повторный вызов ничего не делает.
    pub async fn discard_written(&self) {
        let paths = std::mem::take(&mut *self.written.lock().expect("список файлов"));
        for path in paths {
            if let Err(e) = tokio::fs::remove_file(&path).await {
                tracing::warn!(path = %path.display(), "обмен с 1С: не удалось убрать файл: {e}");
            }
        }
    }
}

/// Счётчики по видам данных: `sections`, `fields`, `options`, `stores`, `price_types`,
/// `items`, `offers`, `prices`, `files`.
#[derive(Debug, Default, Serialize)]
pub struct Stats {
    pub created: BTreeMap<&'static str, u32>,
    pub updated: BTreeMap<&'static str, u32>,
    pub deactivated: BTreeMap<&'static str, u32>,
    pub deleted: BTreeMap<&'static str, u32>,
    pub skipped: BTreeMap<&'static str, u32>,
}

fn bump(map: &mut BTreeMap<&'static str, u32>, key: &'static str) {
    *map.entry(key).or_default() += 1;
}

/// Код, свободный среди `taken`: `base`, затем `base_2`, `base_3`… Занятый код попадает в `taken`.
fn unique_code(base: &str, taken: &mut HashSet<String>) -> String {
    let mut code = base.to_string();
    let mut n = 2;
    while taken.contains(&code) {
        code = format!("{base}_{n}");
        n += 1;
    }
    taken.insert(code.clone());
    code
}

/// Код поля: транслит названия в snake_case; негодный или пустой — `prop_<n>`.
fn field_code_base(name: &str, n: usize) -> String {
    let code = slugify(name).replace('-', "_");
    let code = code.trim_matches('_').to_string();
    if is_valid_code(&code) && code.len() <= 90 {
        code
    } else {
        format!("prop_{n}")
    }
}

/// Коллекция пакета: по `xml_id` любого из `ids` (по порядку), иначе «коллекция по умолчанию»
/// (если у неё нет `xml_id`, ей присваивается первый непустой `id`), иначе новая коллекция каталога.
async fn resolve_any(
    conn: &mut PgConnection,
    settings: &Settings,
    ids: &[&str],
    name: &str,
) -> Result<Collection, ApplyError> {
    let ids: Vec<&str> = ids.iter().copied().filter(|id| !id.is_empty()).collect();
    for id in &ids {
        let found: Option<Collection> =
            sqlx::query_as("SELECT * FROM collections WHERE xml_id = $1")
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?;
        if let Some(c) = found {
            return Ok(c);
        }
    }
    let xml_id = ids.first().copied().unwrap_or("");
    if let Some(default_id) = settings.default_collection_id {
        // Коллекция по умолчанию (не предложений) принимает любой неизвестный каталог;
        // свой `xml_id` она получает, только если его ещё нет.
        let found: Option<Collection> = sqlx::query_as(
            "UPDATE collections
             SET xml_id = CASE WHEN xml_id = '' THEN $2 ELSE xml_id END,
                 updated_at = CASE WHEN xml_id = '' AND $2 <> '' THEN now() ELSE updated_at END
             WHERE id = $1 AND product_collection_id IS NULL RETURNING *",
        )
        .bind(default_id)
        .bind(xml_id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(c) = found {
            return Ok(c);
        }
    }
    let name = if name.trim().is_empty() {
        "Каталог"
    } else {
        name.trim()
    };
    let mut taken: HashSet<String> = sqlx::query_scalar("SELECT code FROM collections")
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .collect();
    let base = slugify(name);
    let base = if is_valid_code(&base) && base.len() <= 90 {
        base
    } else {
        "catalog".to_string()
    };
    let code = unique_code(&base, &mut taken);
    Ok(sqlx::query_as(
        "INSERT INTO collections (code, name, is_catalog, xml_id)
         VALUES ($1, $2, TRUE, $3) RETURNING *",
    )
    .bind(code)
    .bind(name)
    .bind(xml_id)
    .fetch_one(&mut *conn)
    .await?)
}

/// Коллекция по `Ид` каталога или классификатора (см. `resolve_any`).
#[cfg(test)]
async fn resolve_collection(
    conn: &mut PgConnection,
    settings: &Settings,
    classifier_or_catalog_id: &str,
    name: &str,
) -> Result<Collection, ApplyError> {
    resolve_any(conn, settings, &[classifier_or_catalog_id], name).await
}

/// Коллекция для всего пакета: классификатор (его `Ид` становится `xml_id` новой коллекции),
/// каталог, предложения; ищется по любому из их идентификаторов.
async fn collection_for(
    conn: &mut PgConnection,
    settings: &Settings,
    parsed: &Parsed,
) -> Result<Option<Collection>, ApplyError> {
    let mut ids: Vec<&str> = Vec::new();
    let mut name = "";
    if let Some(c) = &parsed.classifier {
        ids.push(&c.id);
        name = &c.name;
    }
    if let Some(c) = &parsed.catalog {
        ids.push(&c.classifier_id);
        ids.push(&c.id);
        if name.is_empty() {
            name = &c.name;
        }
    }
    if let Some(o) = &parsed.offers {
        ids.push(&o.classifier_id);
        ids.push(&o.catalog_id);
    }
    if parsed.classifier.is_none() && parsed.catalog.is_none() && parsed.offers.is_none() {
        return Ok(None);
    }
    Ok(Some(resolve_any(conn, settings, &ids, name).await?))
}

pub async fn apply(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    parsed: &Parsed,
) -> Result<Stats, ApplyError> {
    let result = apply_inner(conn, ctx, parsed).await;
    if result.is_err() {
        // Вызывающий откатит транзакцию, а файлы на диске откат не уберёт
        ctx.discard_written().await;
    }
    result
}

async fn apply_inner(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    parsed: &Parsed,
) -> Result<Stats, ApplyError> {
    let mut stats = Stats::default();
    let Some(collection) = collection_for(conn, ctx.settings, parsed).await? else {
        return Ok(stats);
    };
    if let Some(classifier) = &parsed.classifier {
        apply_sections(conn, ctx, &collection, classifier, &mut stats).await?;
        apply_fields(conn, &collection, classifier, &mut stats).await?;
        apply_stores(conn, classifier, &mut stats).await?;
        apply_price_types(conn, classifier, &mut stats).await?;
    }
    if let Some(catalog) = &parsed.catalog {
        apply_goods(conn, ctx, &collection, catalog, &mut stats).await?;
    }
    if let Some(offers) = &parsed.offers {
        apply_offers(conn, ctx, &collection, offers, &mut stats).await?;
    }
    Ok(stats)
}

/// Полная выгрузка закончена: записи и разделы тех коллекций, до которых дошла сессия
/// (по `exchange_seen`), но не пришедшие в этой сессии, деактивируются или удаляются
/// по настройкам. Записи без `xml_id` (заведены вручную) и предложения не трогаются.
pub async fn deactivate_missing(
    conn: &mut PgConnection,
    settings: &Settings,
    session_id: i64,
) -> Result<Stats, ApplyError> {
    let mut stats = Stats::default();
    for (kind, table, key, action) in [
        ("item", "collection_items", "items", settings.element_action),
        (
            "section",
            "collection_sections",
            "sections",
            settings.section_action,
        ),
    ] {
        if action == Action::None {
            continue;
        }
        let extra = if kind == "item" {
            "AND t.product_id IS NULL"
        } else {
            ""
        };
        let ids: Vec<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT t.id FROM {table} t
             WHERE t.xml_id <> '' {extra}
               AND t.collection_id IN (
                   SELECT t2.collection_id FROM {table} t2
                   JOIN exchange_seen s ON s.session_id = $1 AND s.kind = $2
                                       AND s.xml_id = t2.xml_id)
               AND NOT EXISTS (SELECT 1 FROM exchange_seen s
                               WHERE s.session_id = $1 AND s.kind = $2 AND s.xml_id = t.xml_id)"
        )))
        .bind(session_id)
        .bind(kind)
        .fetch_all(&mut *conn)
        .await?;
        if ids.is_empty() {
            continue;
        }
        let (sql, map) = match action {
            Action::Deactivate => (
                format!(
                    "UPDATE {table} SET active = FALSE, updated_at = now()
                     WHERE id = ANY($1) AND active"
                ),
                &mut stats.deactivated,
            ),
            _ => (
                format!("DELETE FROM {table} WHERE id = ANY($1)"),
                &mut stats.deleted,
            ),
        };
        let n = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&ids)
            .execute(&mut *conn)
            .await?
            .rows_affected();
        if n > 0 {
            *map.entry(key).or_default() += n as u32;
        }
    }
    Ok(stats)
}

type SectionRow = (i64, String, Option<i64>, bool, i32);

async fn load_sections(
    conn: &mut PgConnection,
    collection_id: i64,
) -> Result<HashMap<String, SectionRow>, ApplyError> {
    let rows: Vec<(String, i64, String, Option<i64>, bool, i32)> = sqlx::query_as(
        "SELECT xml_id, id, name, parent_id, active, sort FROM collection_sections
         WHERE collection_id = $1 AND xml_id <> ''",
    )
    .bind(collection_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(x, id, name, parent, active, sort)| (x, (id, name, parent, active, sort)))
        .collect())
}

/// Группы → разделы. Порядок разделов — позиция в пакете, `(n + 1) * 10`.
async fn apply_sections(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    collection: &Collection,
    classifier: &Classifier,
    stats: &mut Stats,
) -> Result<(), ApplyError> {
    let settings = ctx.settings;
    let mut known = load_sections(conn, collection.id).await?;
    let mut moved = false;
    for (n, group) in classifier.groups.iter().enumerate() {
        if group.id.is_empty() {
            continue;
        }
        let sort = (n as i32 + 1) * 10;
        let existing = known.get(&group.id).cloned();
        if group.deleted {
            match (settings.section_action, existing) {
                (_, None) => {}
                (Action::None, Some(_)) => bump(&mut stats.skipped, "sections"),
                (Action::Deactivate, Some((id, _, _, active, _))) => {
                    if active {
                        sqlx::query(
                            "UPDATE collection_sections SET active = FALSE, updated_at = now()
                             WHERE id = $1",
                        )
                        .bind(id)
                        .execute(&mut *conn)
                        .await?;
                        bump(&mut stats.deactivated, "sections");
                    }
                }
                (Action::Delete, Some((id, ..))) => {
                    // Подразделы уходят вместе с разделом (каскад), поэтому карту читаем заново
                    sqlx::query("DELETE FROM collection_sections WHERE id = $1")
                        .bind(id)
                        .execute(&mut *conn)
                        .await?;
                    bump(&mut stats.deleted, "sections");
                    known = load_sections(conn, collection.id).await?;
                }
            }
            continue;
        }
        let parent_id = group
            .parent
            .as_ref()
            .and_then(|p| known.get(p))
            .map(|row| row.0);
        match existing {
            None => {
                let code = if settings.translit_on_add {
                    let c = slugify(&group.name);
                    if is_valid_slug(&c) { c } else { String::new() }
                } else {
                    String::new()
                };
                let (id,): (i64,) = sqlx::query_as(
                    "INSERT INTO collection_sections
                        (collection_id, parent_id, code, xml_id, name, active, sort, depth_level)
                     VALUES ($1, $2, $3, $4, $5, TRUE, $6,
                             COALESCE((SELECT depth_level + 1 FROM collection_sections
                                       WHERE id = $2), 1))
                     RETURNING id",
                )
                .bind(collection.id)
                .bind(parent_id)
                .bind(code)
                .bind(&group.id)
                .bind(&group.name)
                .bind(sort)
                .fetch_one(&mut *conn)
                .await?;
                known.insert(
                    group.id.clone(),
                    (id, group.name.clone(), parent_id, true, sort),
                );
                bump(&mut stats.created, "sections");
            }
            Some((id, name, parent, active, old_sort)) => {
                // Активность и порядок, выставленные в админке, обмен не перезаписывает:
                // порядок берётся из пакета только при полной выгрузке.
                let new_sort = if classifier.only_changes {
                    old_sort
                } else {
                    sort
                };
                if name == group.name && parent == parent_id && old_sort == new_sort {
                    continue;
                }
                if parent != parent_id {
                    moved = true;
                }
                let code = if settings.translit_on_update {
                    let c = slugify(&group.name);
                    if is_valid_slug(&c) { Some(c) } else { None }
                } else {
                    None
                };
                sqlx::query(
                    "UPDATE collection_sections
                     SET name = $2, parent_id = $3, sort = $4,
                         code = COALESCE($5, code), updated_at = now()
                     WHERE id = $1",
                )
                .bind(id)
                .bind(&group.name)
                .bind(parent_id)
                .bind(new_sort)
                .bind(code)
                .execute(&mut *conn)
                .await?;
                known.insert(
                    group.id.clone(),
                    (id, group.name.clone(), parent_id, active, new_sort),
                );
                bump(&mut stats.updated, "sections");
            }
        }
    }
    if moved {
        sqlx::query(
            "WITH RECURSIVE t(id, d) AS (
                 SELECT id, 1 FROM collection_sections
                 WHERE collection_id = $1 AND parent_id IS NULL
                 UNION ALL
                 SELECT s.id, t.d + 1 FROM collection_sections s JOIN t ON s.parent_id = t.id
             )
             UPDATE collection_sections s SET depth_level = t.d
             FROM t WHERE s.id = t.id AND s.depth_level <> t.d",
        )
        .bind(collection.id)
        .execute(&mut *conn)
        .await?;
    }
    if let (Some(session), false) = (ctx.session_id, classifier.only_changes) {
        for group in classifier.groups.iter().filter(|g| !g.deleted) {
            sqlx::query(
                "INSERT INTO exchange_seen (session_id, kind, xml_id)
                 VALUES ($1, 'section', $2) ON CONFLICT DO NOTHING",
            )
            .bind(session)
            .bind(&group.id)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// Вид поля для свойства; существующее строковое `text` остаётся `text`.
fn field_kind(kind: PropKind) -> &'static str {
    match kind {
        PropKind::Directory => "list",
        PropKind::String => "string",
        PropKind::Number => "number",
        PropKind::Date => "date",
    }
}

/// Свойства → поля и варианты. Удалённое в 1С свойство поле не трогает.
async fn apply_fields(
    conn: &mut PgConnection,
    collection: &Collection,
    classifier: &Classifier,
    stats: &mut Stats,
) -> Result<(), ApplyError> {
    let mut taken: HashSet<String> =
        sqlx::query_scalar("SELECT code FROM collection_fields WHERE collection_id = $1")
            .bind(collection.id)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect();
    for (n, prop) in classifier.properties.iter().enumerate() {
        if prop.id.is_empty() {
            continue;
        }
        if prop.deleted {
            bump(&mut stats.skipped, "fields");
            continue;
        }
        let kind = field_kind(prop.kind);
        let existing: Option<(i64, String, String, bool)> = sqlx::query_as(
            "SELECT id, name, kind, multiple FROM collection_fields
             WHERE collection_id = $1 AND xml_id = $2",
        )
        .bind(collection.id)
        .bind(&prop.id)
        .fetch_optional(&mut *conn)
        .await?;
        let field_id = match existing {
            None => {
                let code = unique_code(&field_code_base(&prop.name, n + 1), &mut taken);
                let (id,): (i64,) = sqlx::query_as(
                    "INSERT INTO collection_fields
                        (collection_id, code, name, kind, sort, multiple, xml_id)
                     VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
                )
                .bind(collection.id)
                .bind(code)
                .bind(&prop.name)
                .bind(kind)
                .bind((n as i32 + 1) * 10)
                .bind(prop.multiple)
                .bind(&prop.id)
                .fetch_one(&mut *conn)
                .await?;
                bump(&mut stats.created, "fields");
                id
            }
            Some((id, name, old_kind, multiple)) => {
                // Вид существующего поля не меняется: значения записей типизированы,
                // и никто их не конвертирует. `text` для строки допустим.
                if old_kind != kind && !(old_kind == "text" && kind == "string") {
                    tracing::warn!(
                        field_id = id,
                        xml_id = %prop.id,
                        "обмен с 1С: вид свойства в пакете ({kind}) отличается от поля ({old_kind}), поле не меняется"
                    );
                }
                if multiple != prop.multiple {
                    tracing::warn!(
                        field_id = id,
                        xml_id = %prop.id,
                        "обмен с 1С: множественность свойства в пакете отличается от поля, поле не меняется"
                    );
                }
                if name != prop.name {
                    sqlx::query("UPDATE collection_fields SET name = $2 WHERE id = $1")
                        .bind(id)
                        .bind(&prop.name)
                        .execute(&mut *conn)
                        .await?;
                    bump(&mut stats.updated, "fields");
                }
                id
            }
        };
        if prop.kind == PropKind::Directory {
            apply_options(conn, field_id, prop, stats).await?;
        }
    }
    Ok(())
}

async fn apply_options(
    conn: &mut PgConnection,
    field_id: i64,
    prop: &Property,
    stats: &mut Stats,
) -> Result<(), ApplyError> {
    for (n, (xml_id, value)) in prop.options.iter().enumerate() {
        let (xml_id, value) = (xml_id.trim(), value.trim());
        if xml_id.is_empty() {
            continue;
        }
        let existing: Option<(i64, String)> = sqlx::query_as(
            "SELECT id, value FROM collection_field_options WHERE field_id = $1 AND xml_id = $2",
        )
        .bind(field_id)
        .bind(xml_id)
        .fetch_optional(&mut *conn)
        .await?;
        match existing {
            None => {
                sqlx::query(
                    "INSERT INTO collection_field_options (field_id, value, xml_id, sort)
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(field_id)
                .bind(value)
                .bind(xml_id)
                .bind((n as i32 + 1) * 10)
                .execute(&mut *conn)
                .await?;
                bump(&mut stats.created, "options");
            }
            Some((id, old)) if old != value => {
                sqlx::query("UPDATE collection_field_options SET value = $2 WHERE id = $1")
                    .bind(id)
                    .bind(value)
                    .execute(&mut *conn)
                    .await?;
                bump(&mut stats.updated, "options");
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Вариант поля-списка по внешнему коду; нет такого — добавляется в конец. Возвращает id.
pub async fn ensure_option(
    conn: &mut PgConnection,
    field_id: i64,
    xml_id: &str,
    value: &str,
) -> Result<i64, ApplyError> {
    let xml_id = xml_id.trim();
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO collection_field_options (field_id, value, xml_id, sort)
         VALUES ($1, $2, $3,
                 COALESCE((SELECT max(sort) FROM collection_field_options WHERE field_id = $1), 0)
                 + 10)
         ON CONFLICT (field_id, xml_id) DO UPDATE SET xml_id = EXCLUDED.xml_id
         RETURNING id",
    )
    .bind(field_id)
    .bind(value.trim())
    .bind(xml_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

/// Склады: по `xml_id`; удалённый в 1С склад деактивируется, а новым не создаётся.
async fn apply_stores(
    conn: &mut PgConnection,
    classifier: &Classifier,
    stats: &mut Stats,
) -> Result<(), ApplyError> {
    for store in classifier.stores.iter().filter(|s| !s.id.is_empty()) {
        let existing: Option<(i64, String, bool)> =
            sqlx::query_as("SELECT id, name, active FROM catalog_stores WHERE xml_id = $1")
                .bind(&store.id)
                .fetch_optional(&mut *conn)
                .await?;
        match (existing, store.deleted) {
            (None, true) => bump(&mut stats.skipped, "stores"),
            (None, false) => {
                sqlx::query(
                    "INSERT INTO catalog_stores (name, active, xml_id) VALUES ($1, TRUE, $2)",
                )
                .bind(&store.name)
                .bind(&store.id)
                .execute(&mut *conn)
                .await?;
                bump(&mut stats.created, "stores");
            }
            (Some((id, _, true)), true) => {
                sqlx::query("UPDATE catalog_stores SET active = FALSE WHERE id = $1")
                    .bind(id)
                    .execute(&mut *conn)
                    .await?;
                bump(&mut stats.deactivated, "stores");
            }
            (Some(_), true) => {}
            (Some((id, name, _)), false) => {
                if name != store.name {
                    sqlx::query("UPDATE catalog_stores SET name = $2 WHERE id = $1")
                        .bind(id)
                        .bind(&store.name)
                        .execute(&mut *conn)
                        .await?;
                    bump(&mut stats.updated, "stores");
                }
            }
        }
    }
    Ok(())
}

/// Типы цен: по `xml_id`; новый — не базовый. Валюты у типа цены в схеме нет: она
/// хранится в самих ценах. Удалённый в 1С тип цены не создаётся и не трогается.
async fn apply_price_types(
    conn: &mut PgConnection,
    classifier: &Classifier,
    stats: &mut Stats,
) -> Result<(), ApplyError> {
    let mut taken: Option<HashSet<String>> = None;
    for (n, pt) in classifier
        .price_types
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.id.is_empty())
    {
        if pt.deleted {
            bump(&mut stats.skipped, "price_types");
            continue;
        }
        let existing: Option<(i64, String)> =
            sqlx::query_as("SELECT id, name FROM catalog_price_types WHERE xml_id = $1")
                .bind(&pt.id)
                .fetch_optional(&mut *conn)
                .await?;
        match existing {
            None => {
                if taken.is_none() {
                    taken = Some(
                        sqlx::query_scalar("SELECT code FROM catalog_price_types")
                            .fetch_all(&mut *conn)
                            .await?
                            .into_iter()
                            .collect(),
                    );
                }
                let base = slugify(&pt.name);
                let base = if is_valid_code(&base) {
                    base
                } else {
                    format!("price_{}", n + 1)
                };
                let code = unique_code(&base, taken.as_mut().expect("задано выше"));
                sqlx::query(
                    "INSERT INTO catalog_price_types (code, name, is_base, sort, xml_id)
                     VALUES ($1, $2, FALSE, $3, $4)",
                )
                .bind(code)
                .bind(&pt.name)
                .bind((n as i32 + 1) * 10)
                .bind(&pt.id)
                .execute(&mut *conn)
                .await?;
                bump(&mut stats.created, "price_types");
            }
            Some((id, name)) if name != pt.name => {
                sqlx::query("UPDATE catalog_price_types SET name = $2 WHERE id = $1")
                    .bind(id)
                    .bind(&pt.name)
                    .execute(&mut *conn)
                    .await?;
                bump(&mut stats.updated, "price_types");
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Системные поля CML2: `xml_id`, код, название, вид, множественное.
type SystemField = (&'static str, &'static str, &'static str, &'static str, bool);

const F_ARTICLE: SystemField = ("CML2_ARTICLE", "cml2_article", "Артикул", "string", false);
const F_BAR_CODE: SystemField = (
    "CML2_BAR_CODE",
    "cml2_bar_code",
    "Штрихкод",
    "string",
    false,
);
const F_MANUFACTURER: SystemField = (
    "CML2_MANUFACTURER",
    "cml2_manufacturer",
    "Изготовитель",
    "list",
    false,
);
const F_TRAITS: SystemField = ("CML2_TRAITS", "cml2_traits", "Реквизиты", "string", true);
const F_TAXES: SystemField = ("CML2_TAXES", "cml2_taxes", "Налоги", "string", true);
const F_BASE_UNIT: SystemField = (
    "CML2_BASE_UNIT",
    "cml2_base_unit",
    "Базовая единица",
    "string",
    false,
);
const F_PICTURES: SystemField = ("CML2_PICTURES", "cml2_pictures", "Картинки", "file", true);

#[derive(Debug, Clone)]
struct FieldInfo {
    id: i64,
    code: String,
    kind: String,
    multiple: bool,
}

/// Поля коллекции, варианты и разделы, прочитанные один раз на пакет.
struct GoodsState {
    collection_id: i64,
    fields: HashMap<String, FieldInfo>,
    taken_codes: HashSet<String>,
    options: HashMap<(i64, String), i64>,
    sections: HashMap<String, i64>,
}

impl GoodsState {
    async fn load(conn: &mut PgConnection, collection_id: i64) -> Result<Self, ApplyError> {
        let rows: Vec<(i64, String, String, bool, String)> = sqlx::query_as(
            "SELECT id, code, kind, multiple, xml_id FROM collection_fields
             WHERE collection_id = $1",
        )
        .bind(collection_id)
        .fetch_all(&mut *conn)
        .await?;
        let mut fields = HashMap::new();
        let mut taken_codes = HashSet::new();
        for (id, code, kind, multiple, xml_id) in rows {
            taken_codes.insert(code.clone());
            if !xml_id.is_empty() {
                fields.insert(
                    xml_id,
                    FieldInfo {
                        id,
                        code,
                        kind,
                        multiple,
                    },
                );
            }
        }
        let sections: Vec<(String, i64)> = sqlx::query_as(
            "SELECT xml_id, id FROM collection_sections WHERE collection_id = $1 AND xml_id <> ''",
        )
        .bind(collection_id)
        .fetch_all(&mut *conn)
        .await?;
        Ok(GoodsState {
            collection_id,
            fields,
            taken_codes,
            options: HashMap::new(),
            sections: sections.into_iter().collect(),
        })
    }

    /// Поле коллекции с `xml_id` системного поля; нет — создаётся.
    async fn system_field(
        &mut self,
        conn: &mut PgConnection,
        def: SystemField,
    ) -> Result<FieldInfo, ApplyError> {
        let (xml_id, code, name, kind, multiple) = def;
        if let Some(f) = self.fields.get(xml_id) {
            return Ok(f.clone());
        }
        let code = unique_code(code, &mut self.taken_codes);
        let sort: i32 = sqlx::query_scalar(
            "SELECT COALESCE(max(sort), 0) + 10 FROM collection_fields WHERE collection_id = $1",
        )
        .bind(self.collection_id)
        .fetch_one(&mut *conn)
        .await?;
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO collection_fields (collection_id, code, name, kind, sort, multiple, xml_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(self.collection_id)
        .bind(&code)
        .bind(name)
        .bind(kind)
        .bind(sort)
        .bind(multiple)
        .bind(xml_id)
        .fetch_one(&mut *conn)
        .await?;
        let info = FieldInfo {
            id,
            code,
            kind: kind.to_string(),
            multiple,
        };
        self.fields.insert(xml_id.to_string(), info.clone());
        Ok(info)
    }

    /// Вариант списка по внешнему коду; неизвестный добавляется (`text` — подпись, если есть).
    async fn option(
        &mut self,
        conn: &mut PgConnection,
        field_id: i64,
        xml_id: &str,
        text: &str,
    ) -> Result<i64, ApplyError> {
        let xml_id = xml_id.trim();
        let key = (field_id, xml_id.to_string());
        if let Some(id) = self.options.get(&key) {
            return Ok(*id);
        }
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM collection_field_options WHERE field_id = $1 AND xml_id = $2",
        )
        .bind(field_id)
        .bind(xml_id)
        .fetch_optional(&mut *conn)
        .await?;
        let id = match found {
            Some(id) => id,
            None => {
                let text = if text.trim().is_empty() { xml_id } else { text };
                tracing::info!(
                    field_id,
                    xml_id,
                    "обмен с 1С: новый вариант списка из товара"
                );
                ensure_option(conn, field_id, xml_id, text).await?
            }
        };
        self.options.insert(key, id);
        Ok(id)
    }
}

/// Значение в поле: пусто — ключ убирается, множественное — массив, иначе первое.
fn assign(values: &mut serde_json::Map<String, Value>, field: &FieldInfo, vals: Vec<Value>) {
    if vals.is_empty() {
        values.remove(&field.code);
    } else if field.multiple {
        values.insert(field.code.clone(), Value::Array(vals));
    } else {
        values.insert(
            field.code.clone(),
            vals.into_iter().next().expect("не пусто"),
        );
    }
}

/// Системное поле со строковыми значениями. Пустой набор не создаёт поле,
/// а у существующего убирает значение.
async fn assign_system(
    conn: &mut PgConnection,
    state: &mut GoodsState,
    values: &mut serde_json::Map<String, Value>,
    def: SystemField,
    vals: Vec<Value>,
) -> Result<(), ApplyError> {
    if vals.is_empty() && !state.fields.contains_key(def.0) {
        return Ok(());
    }
    let field = state.system_field(conn, def).await?;
    assign(values, &field, vals);
    Ok(())
}

fn parse_number(raw: &str) -> Option<Value> {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .replace(',', ".");
    if let Ok(i) = cleaned.parse::<i64>() {
        return Some(Value::from(i));
    }
    let f: f64 = cleaned.parse().ok().filter(|f: &f64| f.is_finite())?;
    if f.fract() == 0.0 && f.abs() < 9e15 {
        Some(Value::from(f as i64))
    } else {
        Some(Value::from(f))
    }
}

/// Значение свойства товара → значение поля; `None` — нет значения.
async fn property_value(
    conn: &mut PgConnection,
    state: &mut GoodsState,
    field: &FieldInfo,
    raw: &str,
) -> Result<Option<Value>, ApplyError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(match field.kind.as_str() {
        "string" | "text" => Some(Value::String(raw.to_string())),
        "number" => {
            let n = parse_number(trimmed);
            if n.is_none() {
                tracing::warn!(field = %field.code, "обмен с 1С: «{trimmed}» не число, значение пропущено");
            }
            n
        }
        "boolean" => Some(Value::Bool(matches!(
            trimmed.to_lowercase().as_str(),
            "1" | "y" | "true" | "да"
        ))),
        "date" => match trimmed.get(..10) {
            Some(d) if chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok() => {
                Some(Value::String(d.to_string()))
            }
            _ => {
                tracing::warn!(field = %field.code, "обмен с 1С: «{trimmed}» не дата, значение пропущено");
                None
            }
        },
        "list" => Some(Value::from(
            state.option(conn, field.id, trimmed, trimmed).await?,
        )),
        kind => {
            tracing::warn!(field = %field.code, "обмен с 1С: поле вида {kind} из пакета не заполняется");
            None
        }
    })
}

/// Код записи: транслит названия, уникальный в коллекции (`код`, `код-2`, `код-3`…).
/// Негодный транслит — `None`.
async fn unique_item_code(
    conn: &mut PgConnection,
    collection_id: i64,
    self_id: Option<i64>,
    name: &str,
) -> Result<Option<String>, ApplyError> {
    let base = slugify(name);
    if !is_valid_slug(&base) || base.len() > 200 {
        return Ok(None);
    }
    let mut taken: HashSet<String> = sqlx::query_scalar(
        "SELECT code FROM collection_items
         WHERE collection_id = $1 AND id IS DISTINCT FROM $2
           AND (code = $3 OR left(code, length($3) + 1) = $3 || '-')",
    )
    .bind(collection_id)
    .bind(self_id)
    .bind(&base)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    let mut code = base.clone();
    let mut n = 2;
    while taken.contains(&code) {
        code = format!("{base}-{n}");
        n += 1;
    }
    taken.insert(code.clone());
    Ok(Some(code))
}

fn content_type_of(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("csv") => "text/csv",
        _ => "application/octet-stream",
    }
}

// Используется следующими задачами плана обмена (админка, проектные обработчики).
#[allow(dead_code)]
/// Файл из каталога сессии по относительному пути пакета. Совпал по sha256 с одним из
/// `existing` — его id; нет файла в пакете — `None`; иначе новый файл. Путь вне каталога
/// сессии — ошибка. Новые файлы запоминаются в `ctx` (см. `Ctx::discard_written`).
pub async fn store_file(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    rel_path: &str,
    existing: &[i64],
) -> Result<Option<i64>, ApplyError> {
    Ok(store_file_inner(conn, ctx, rel_path, existing)
        .await?
        .map(|(id, _)| id))
}

/// Как `store_file`; второе значение — файл создан заново.
async fn store_file_inner(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    rel_path: &str,
    existing: &[i64],
) -> Result<Option<(i64, bool)>, ApplyError> {
    let rel = Path::new(rel_path.trim());
    if rel.as_os_str().is_empty()
        || rel
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(ApplyError::Invalid(format!(
            "недопустимый путь файла «{rel_path}»"
        )));
    }
    let data = match tokio::fs::read(ctx.session_dir.join(rel)).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ApplyError::File(format!("{rel_path}: {e}"))),
    };
    let hash = hex::encode(Sha256::digest(&data));
    if !existing.is_empty() {
        let rows: Vec<(i64, String, String)> =
            sqlx::query_as("SELECT id, path, sha256 FROM files WHERE id = ANY($1) ORDER BY id")
                .bind(existing)
                .fetch_all(&mut *conn)
                .await?;
        for (id, path, sha) in rows {
            let sha = if sha.is_empty() {
                // Файл сохранён до появления хеша: считаем с диска и запоминаем
                let Ok(bytes) = tokio::fs::read(ctx.upload_dir.join(&path)).await else {
                    continue;
                };
                let sha = hex::encode(Sha256::digest(&bytes));
                sqlx::query("UPDATE files SET sha256 = $2 WHERE id = $1")
                    .bind(id)
                    .bind(&sha)
                    .execute(&mut *conn)
                    .await?;
                sha
            } else {
                sha
            };
            if sha == hash {
                return Ok(Some((id, false)));
            }
        }
    }
    let name = rel
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    match crate::files::save_in(
        conn,
        ctx.upload_dir,
        "iblock",
        &name,
        content_type_of(&name),
        &data,
    )
    .await
    {
        Ok(record) => {
            ctx.written
                .lock()
                .expect("список файлов")
                .push(ctx.upload_dir.join(&record.path));
            Ok(Some((record.id, true)))
        }
        Err(crate::files::SaveError::Rejected(msg)) => {
            tracing::warn!("обмен с 1С: файл пропущен: {msg}");
            Ok(None)
        }
        Err(crate::files::SaveError::Failed(e)) => Err(ApplyError::File(format!("{e:#}"))),
    }
}

type ItemRow = (
    i64,
    String,
    Option<i64>,
    String,
    String,
    Option<i64>,
    Option<i64>,
    sqlx::types::Json<serde_json::Map<String, Value>>,
    bool,
);

/// Товары → записи по `xml_id`. Неизменённая запись не пишется.
async fn apply_goods(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    collection: &Collection,
    catalog: &Catalog,
    stats: &mut Stats,
) -> Result<(), ApplyError> {
    let settings = ctx.settings;
    let mut state = GoodsState::load(conn, collection.id).await?;
    for goods in catalog.goods.iter().filter(|g| !g.id.is_empty()) {
        let existing: Option<ItemRow> = sqlx::query_as(
            "SELECT id, name, section_id, code, detail_text, preview_picture_id,
                    detail_picture_id, field_values, active
             FROM collection_items WHERE collection_id = $1 AND xml_id = $2
             ORDER BY id LIMIT 1",
        )
        .bind(collection.id)
        .bind(&goods.id)
        .fetch_optional(&mut *conn)
        .await?;
        if goods.deleted {
            match (settings.element_action, existing) {
                (_, None) => {}
                (Action::None, Some(_)) => bump(&mut stats.skipped, "items"),
                (Action::Deactivate, Some((id, .., active))) => {
                    if active {
                        sqlx::query(
                            "UPDATE collection_items SET active = FALSE, updated_at = now()
                             WHERE id = $1",
                        )
                        .bind(id)
                        .execute(&mut *conn)
                        .await?;
                        bump(&mut stats.deactivated, "items");
                    }
                }
                (Action::Delete, Some((id, ..))) => {
                    sqlx::query("DELETE FROM collection_items WHERE id = $1")
                        .bind(id)
                        .execute(&mut *conn)
                        .await?;
                    bump(&mut stats.deleted, "items");
                }
            }
            continue;
        }
        write_goods(conn, ctx, collection, &mut state, goods, existing, stats).await?;
    }
    if let (Some(session), false) = (ctx.session_id, catalog.only_changes) {
        for goods in catalog
            .goods
            .iter()
            .filter(|g| !g.deleted && !g.id.is_empty())
        {
            sqlx::query(
                "INSERT INTO exchange_seen (session_id, kind, xml_id)
                 VALUES ($1, 'item', $2) ON CONFLICT DO NOTHING",
            )
            .bind(session)
            .bind(&goods.id)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

async fn write_goods(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    collection: &Collection,
    state: &mut GoodsState,
    goods: &Goods,
    existing: Option<ItemRow>,
    stats: &mut Stats,
) -> Result<(), ApplyError> {
    let settings = ctx.settings;
    let (old_values, old_detail, old_preview) = match &existing {
        Some(row) => (row.7.0.clone(), row.6, row.5),
        None => (serde_json::Map::new(), None, None),
    };
    let mut values = old_values.clone();

    // Системные поля
    let text = |s: &str| -> Vec<Value> {
        if s.trim().is_empty() {
            Vec::new()
        } else {
            vec![Value::String(s.trim().to_string())]
        }
    };
    assign_system(conn, state, &mut values, F_ARTICLE, text(&goods.article)).await?;
    assign_system(conn, state, &mut values, F_BAR_CODE, text(&goods.barcode)).await?;
    assign_system(
        conn,
        state,
        &mut values,
        F_BASE_UNIT,
        text(&goods.base_unit),
    )
    .await?;
    let pairs = |list: &[(String, String)]| -> Vec<Value> {
        list.iter()
            .filter(|(_, v)| !v.trim().is_empty())
            .map(|(k, v)| Value::String(format!("{}: {}", k.trim(), v.trim())))
            .collect()
    };
    assign_system(conn, state, &mut values, F_TRAITS, pairs(&goods.traits)).await?;
    assign_system(conn, state, &mut values, F_TAXES, pairs(&goods.taxes)).await?;
    let mut manufacturer = Vec::new();
    if let Some((id, name)) = &goods.manufacturer
        && !id.trim().is_empty()
    {
        let field = state.system_field(conn, F_MANUFACTURER).await?;
        manufacturer.push(Value::from(state.option(conn, field.id, id, name).await?));
    }
    assign_system(conn, state, &mut values, F_MANUFACTURER, manufacturer).await?;

    // Свойства
    for (prop_id, raws) in &goods.properties {
        let Some(field) = state.fields.get(prop_id.trim()).cloned() else {
            tracing::warn!(property = %prop_id, "обмен с 1С: свойство товара {} неизвестно, пропущено", goods.id);
            continue;
        };
        let mut vals = Vec::new();
        for raw in raws {
            if let Some(v) = property_value(conn, state, &field, raw).await?
                && !vals.contains(&v)
            {
                vals.push(v);
            }
        }
        assign(&mut values, &field, vals);
    }

    // Картинки: ячейка без файла в пакете сохраняет прежнее значение
    let (mut detail, mut preview) = (old_detail, old_preview);
    if !goods.pictures.is_empty() {
        let field = state.system_field(conn, F_PICTURES).await?;
        let current = old_values
            .get(&field.code)
            .map(crate::collection::fields::ids)
            .unwrap_or_default();
        let mut known = current.clone();
        known.extend(old_detail);
        known.extend(old_preview);
        let mut resolved = Vec::new();
        for (slot, path) in goods.pictures.iter().enumerate() {
            let stored = store_file_inner(conn, ctx, path, &known).await?;
            let id = match stored {
                Some((id, created)) => {
                    if created {
                        bump(&mut stats.created, "files");
                    }
                    Some(id)
                }
                None => current.get(slot).copied(),
            };
            if let Some(id) = id {
                resolved.push(id);
            }
        }
        if let Some(first) = resolved.first().copied() {
            detail = Some(first);
            preview = Some(first);
        }
        assign(
            &mut values,
            &field,
            resolved.into_iter().map(Value::from).collect(),
        );
    }

    let section = goods
        .groups
        .first()
        .and_then(|g| state.sections.get(g.trim()).copied())
        .or(existing.as_ref().and_then(|r| r.2));

    let (item_id, changed, created) = match existing {
        None => {
            let code = if settings.translit_on_add {
                unique_item_code(conn, collection.id, None, &goods.name)
                    .await?
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let (id,): (i64,) = sqlx::query_as(
                "INSERT INTO collection_items
                    (collection_id, section_id, code, xml_id, name, active, detail_text,
                     preview_picture_id, detail_picture_id, field_values)
                 VALUES ($1, $2, $3, $4, $5, TRUE, $6, $7, $8, $9) RETURNING id",
            )
            .bind(collection.id)
            .bind(section)
            .bind(code)
            .bind(&goods.id)
            .bind(&goods.name)
            .bind(&goods.description)
            .bind(preview)
            .bind(detail)
            .bind(sqlx::types::Json(&values))
            .fetch_one(&mut *conn)
            .await?;
            bump(&mut stats.created, "items");
            (id, true, true)
        }
        Some((id, name, old_section, old_code, old_text, _, _, _, old_active)) => {
            let code = if settings.translit_on_update {
                unique_item_code(conn, collection.id, Some(id), &goods.name)
                    .await?
                    .unwrap_or_else(|| old_code.clone())
            } else {
                old_code.clone()
            };
            let same = name == goods.name
                && old_section == section
                && code == old_code
                && old_text == goods.description
                && old_preview == preview
                && old_detail == detail
                && old_values == values
                && old_active;
            if !same {
                sqlx::query(
                    "UPDATE collection_items
                     SET name = $2, section_id = $3, code = $4, detail_text = $5,
                         preview_picture_id = $6, detail_picture_id = $7, field_values = $8,
                         active = TRUE, updated_at = now()
                     WHERE id = $1",
                )
                .bind(id)
                .bind(&goods.name)
                .bind(section)
                .bind(code)
                .bind(&goods.description)
                .bind(preview)
                .bind(detail)
                .bind(sqlx::types::Json(&values))
                .execute(&mut *conn)
                .await?;
            }
            (id, !same, false)
        }
    };

    // Вес в граммах; строка товарных данных нужна каждой записи каталога
    let weight = goods.weight_grams;
    let row: Option<f64> =
        sqlx::query_scalar("SELECT weight::float8 FROM catalog_products WHERE item_id = $1")
            .bind(item_id)
            .fetch_optional(&mut *conn)
            .await?;
    let weight_changed = match (row, weight) {
        (None, w) => {
            sqlx::query("INSERT INTO catalog_products (item_id, weight) VALUES ($1, $2::float8)")
                .bind(item_id)
                .bind(w.unwrap_or(0.0))
                .execute(&mut *conn)
                .await?;
            !created && w.is_some()
        }
        (Some(old), Some(w)) if (old - w).abs() > 0.0005 => {
            sqlx::query("UPDATE catalog_products SET weight = $2::float8 WHERE item_id = $1")
                .bind(item_id)
                .bind(w)
                .execute(&mut *conn)
                .await?;
            true
        }
        _ => false,
    };
    if !created && (changed || weight_changed) {
        bump(&mut stats.updated, "items");
    }
    if (created || changed || weight_changed)
        && let Some(hook) = ctx.hook
    {
        hook.after_item(conn, item_id, goods)
            .await
            .map_err(ApplyError::Hook)?;
    }
    Ok(())
}

/// Предложения: цены, остатки, количество; `товар#характеристика` — торговые предложения.
async fn apply_offers(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    collection: &Collection,
    package: &OfferPackage,
    stats: &mut Stats,
) -> Result<(), ApplyError> {
    let mut price_types: HashMap<String, i64> = sqlx::query_as::<_, (String, i64)>(
        "SELECT xml_id, id FROM catalog_price_types WHERE xml_id <> ''",
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    let mut stores: HashMap<String, i64> = sqlx::query_as::<_, (String, i64)>(
        "SELECT xml_id, id FROM catalog_stores WHERE xml_id <> ''",
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    let mut sku: Option<SkuState> = None;
    let mut carried_prices = false;
    let mut in_package: Vec<i64> = Vec::new();
    let mut priced: Vec<i64> = Vec::new();

    for offer in package.offers.iter().filter(|o| !o.id.is_empty()) {
        if offer.deleted {
            bump(&mut stats.skipped, "offers");
            continue;
        }
        let item_id = match offer.id.split_once('#') {
            None => {
                let found: Option<i64> = sqlx::query_scalar(
                    "SELECT id FROM collection_items WHERE collection_id = $1 AND xml_id = $2
                     ORDER BY id LIMIT 1",
                )
                .bind(collection.id)
                .bind(&offer.id)
                .fetch_optional(&mut *conn)
                .await?;
                found
            }
            Some((parent_xml, _)) if ctx.settings.use_offers => {
                write_sku_offer(conn, ctx, collection, &mut sku, parent_xml, offer, stats).await?
            }
            Some(_) => None,
        };
        let Some(item_id) = item_id else {
            bump(&mut stats.skipped, "offers");
            continue;
        };
        in_package.push(item_id);

        if let Some(prices) = &offer.prices {
            // Последняя цена типа в предложении побеждает
            let mut by_type: Vec<(&str, &Price)> = Vec::new();
            for price in prices.iter().filter(|p| !p.type_id.is_empty()) {
                match by_type.iter_mut().find(|(t, _)| *t == price.type_id) {
                    Some(slot) => slot.1 = price,
                    None => by_type.push((&price.type_id, price)),
                }
            }
            if !by_type.is_empty() {
                priced.push(item_id);
            }
            for (type_xml, price) in by_type {
                carried_prices = true;
                let type_id = match price_types.get(type_xml) {
                    Some(id) => *id,
                    None => {
                        let id = create_price_type(conn, type_xml).await?;
                        price_types.insert(type_xml.to_string(), id);
                        bump(&mut stats.created, "price_types");
                        id
                    }
                };
                write_price(conn, item_id, type_id, price, stats).await?;
            }
        }

        match (&offer.rests, offer.quantity) {
            (Some(rests), _) if !rests.is_empty() => {
                let mut by_store: Vec<(&str, f64)> = Vec::new();
                for (store_xml, amount) in rests.iter().filter(|(s, _)| !s.is_empty()) {
                    match by_store.iter_mut().find(|(s, _)| s == store_xml) {
                        Some(slot) => slot.1 += amount,
                        None => by_store.push((store_xml, *amount)),
                    }
                }
                for (store_xml, amount) in by_store {
                    let store_id = match stores.get(store_xml) {
                        Some(id) => *id,
                        None => {
                            let id: i64 = sqlx::query_scalar(
                                "INSERT INTO catalog_stores (name, active, xml_id)
                                 VALUES ($1, TRUE, $1) RETURNING id",
                            )
                            .bind(store_xml)
                            .fetch_one(&mut *conn)
                            .await?;
                            stores.insert(store_xml.to_string(), id);
                            bump(&mut stats.created, "stores");
                            id
                        }
                    };
                    write_amount(conn, item_id, store_id, amount, stats).await?;
                }
                let total: f64 = sqlx::query_scalar(
                    "SELECT COALESCE(sum(amount), 0)::float8 FROM catalog_store_amounts
                     WHERE item_id = $1",
                )
                .bind(item_id)
                .fetch_one(&mut *conn)
                .await?;
                write_quantity(conn, item_id, total).await?;
            }
            // Склады не пришли: только общее количество (CommerceML 2.x)
            (None, Some(quantity)) => write_quantity(conn, item_id, quantity).await?,
            _ => {}
        }
    }

    if carried_prices && ctx.settings.deactivate_without_price {
        deactivate_without_price(conn, &in_package, &priced, stats).await?;
    }
    Ok(())
}

async fn create_price_type(conn: &mut PgConnection, xml_id: &str) -> Result<i64, ApplyError> {
    let mut taken: HashSet<String> = sqlx::query_scalar("SELECT code FROM catalog_price_types")
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .collect();
    let base = slugify(xml_id);
    let base = if is_valid_code(&base) {
        base
    } else {
        "price".to_string()
    };
    let code = unique_code(&base, &mut taken);
    Ok(sqlx::query_scalar(
        "INSERT INTO catalog_price_types (code, name, is_base, sort, xml_id)
         VALUES ($1, $2, FALSE, 500, $2) RETURNING id",
    )
    .bind(code)
    .bind(xml_id)
    .fetch_one(&mut *conn)
    .await?)
}

/// Цена типа у записи: строки этого типа заменяются одной; неизменная не пишется.
async fn write_price(
    conn: &mut PgConnection,
    item_id: i64,
    type_id: i64,
    price: &Price,
    stats: &mut Stats,
) -> Result<(), ApplyError> {
    let currency = if price.currency.trim().is_empty() {
        "RUB"
    } else {
        price.currency.trim()
    };
    let (count, same): (i64, bool) = sqlx::query_as(
        "SELECT count(*), COALESCE(bool_and(price = round($3::float8::numeric, 2)
                                            AND currency = $4
                                            AND quantity_from IS NULL
                                            AND quantity_to IS NULL), FALSE)
         FROM catalog_prices WHERE item_id = $1 AND price_type_id = $2",
    )
    .bind(item_id)
    .bind(type_id)
    .bind(price.value)
    .bind(currency)
    .fetch_one(&mut *conn)
    .await?;
    if count == 1 && same {
        return Ok(());
    }
    sqlx::query("DELETE FROM catalog_prices WHERE item_id = $1 AND price_type_id = $2")
        .bind(item_id)
        .bind(type_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO catalog_prices (item_id, price_type_id, price, currency)
         VALUES ($1, $2, round($3::float8::numeric, 2), $4)",
    )
    .bind(item_id)
    .bind(type_id)
    .bind(price.value)
    .bind(currency)
    .execute(&mut *conn)
    .await?;
    bump(
        if count == 0 {
            &mut stats.created
        } else {
            &mut stats.updated
        },
        "prices",
    );
    Ok(())
}

async fn write_amount(
    conn: &mut PgConnection,
    item_id: i64,
    store_id: i64,
    amount: f64,
    stats: &mut Stats,
) -> Result<(), ApplyError> {
    let changed = sqlx::query(
        "INSERT INTO catalog_store_amounts (item_id, store_id, amount)
         VALUES ($1, $2, round($3::float8::numeric, 2))
         ON CONFLICT (item_id, store_id) DO UPDATE SET amount = EXCLUDED.amount
         WHERE catalog_store_amounts.amount <> EXCLUDED.amount",
    )
    .bind(item_id)
    .bind(store_id)
    .bind(amount)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if changed > 0 {
        bump(&mut stats.updated, "rests");
    }
    Ok(())
}

/// Общее количество записи; строка товарных данных создаётся при необходимости.
async fn write_quantity(
    conn: &mut PgConnection,
    item_id: i64,
    quantity: f64,
) -> Result<(), ApplyError> {
    sqlx::query(
        "INSERT INTO catalog_products (item_id, quantity)
         VALUES ($1, round($2::float8::numeric, 2))
         ON CONFLICT (item_id) DO UPDATE SET quantity = EXCLUDED.quantity
         WHERE catalog_products.quantity <> EXCLUDED.quantity",
    )
    .bind(item_id)
    .bind(quantity)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// После пакета с ценами: запись пакета, получившая цену, снова активна; запись пакета
/// без единой цены неактивна. Записи вне пакета не трогаются. У товара с предложениями
/// считаются цены его предложений.
async fn deactivate_without_price(
    conn: &mut PgConnection,
    in_package: &[i64],
    priced: &[i64],
    stats: &mut Stats,
) -> Result<(), ApplyError> {
    let reactivated = sqlx::query(
        "UPDATE collection_items SET active = TRUE, updated_at = now()
         WHERE id = ANY($1) AND NOT active",
    )
    .bind(priced)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if reactivated > 0 {
        *stats.updated.entry("items").or_default() += reactivated as u32;
    }
    let done = sqlx::query(
        "UPDATE collection_items i SET active = FALSE, updated_at = now()
         WHERE i.id = ANY($1) AND i.active
           AND NOT EXISTS (SELECT 1 FROM catalog_prices p WHERE p.item_id = i.id)
           AND NOT EXISTS (SELECT 1 FROM collection_items o
                           JOIN catalog_prices p ON p.item_id = o.id
                           WHERE o.product_id = i.id)",
    )
    .bind(in_package)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if done > 0 {
        *stats.deactivated.entry("items").or_default() += done as u32;
    }
    Ok(())
}

/// Запись предложения: id, название, товар, код, значения полей, активность.
type OfferRow = (
    i64,
    String,
    Option<i64>,
    String,
    sqlx::types::Json<serde_json::Map<String, Value>>,
    bool,
);

/// Коллекция предложений и поля характеристик, прочитанные один раз на пакет.
struct SkuState {
    collection: Collection,
    state: GoodsState,
}

/// Предложение `товар#характеристика` → запись коллекции предложений. `None` — родитель
/// неизвестен (предложение пропускается).
async fn write_sku_offer(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    products: &Collection,
    sku: &mut Option<SkuState>,
    parent_xml: &str,
    offer: &Offer,
    stats: &mut Stats,
) -> Result<Option<i64>, ApplyError> {
    let parent: Option<(i64, String)> = sqlx::query_as(
        "SELECT id, name FROM collection_items WHERE collection_id = $1 AND xml_id = $2
         ORDER BY id LIMIT 1",
    )
    .bind(products.id)
    .bind(parent_xml)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((parent_id, parent_name)) = parent else {
        return Ok(None);
    };
    if sku.is_none() {
        let found: Option<Collection> =
            sqlx::query_as("SELECT * FROM collections WHERE product_collection_id = $1")
                .bind(products.id)
                .fetch_optional(&mut *conn)
                .await?;
        let collection = match found {
            Some(c) => c,
            None => {
                let c = crate::collection::repo::create_offer_collection_in(conn, products).await?;
                bump(&mut stats.created, "offer_collections");
                c
            }
        };
        let state = GoodsState::load(conn, collection.id).await?;
        *sku = Some(SkuState { collection, state });
    }
    let SkuState { collection, state } = sku.as_mut().expect("задано выше");

    let mut values: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut shown: Vec<&str> = Vec::new();
    for (name, value) in &offer.characteristics {
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() || value.is_empty() {
            continue;
        }
        let field = characteristic_field(conn, state, name).await?;
        values.insert(field.code, Value::String(value.to_string()));
        shown.push(value);
    }
    let name = if !offer.name.trim().is_empty() {
        offer.name.trim().to_string()
    } else if shown.is_empty() {
        parent_name
    } else {
        format!("{} ({})", parent_name, shown.join(", "))
    };

    let existing: Option<OfferRow> = sqlx::query_as(
        "SELECT id, name, product_id, code, field_values, active FROM collection_items
             WHERE collection_id = $1 AND xml_id = $2 ORDER BY id LIMIT 1",
    )
    .bind(collection.id)
    .bind(&offer.id)
    .fetch_optional(&mut *conn)
    .await?;
    match existing {
        None => {
            let code = if ctx.settings.translit_on_add {
                unique_item_code(conn, collection.id, None, &name)
                    .await?
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO collection_items
                    (collection_id, code, xml_id, name, active, field_values, product_id)
                 VALUES ($1, $2, $3, $4, TRUE, $5, $6) RETURNING id",
            )
            .bind(collection.id)
            .bind(code)
            .bind(&offer.id)
            .bind(&name)
            .bind(sqlx::types::Json(&values))
            .bind(parent_id)
            .fetch_one(&mut *conn)
            .await?;
            bump(&mut stats.created, "offers");
            Ok(Some(id))
        }
        Some((id, old_name, old_parent, _, old_values, old_active)) => {
            // Значения других полей предложения (админка) сохраняются
            let mut merged = old_values.0.clone();
            merged.extend(values);
            if old_name != name
                || old_parent != Some(parent_id)
                || merged != old_values.0
                || !old_active
            {
                sqlx::query(
                    "UPDATE collection_items
                     SET name = $2, product_id = $3, field_values = $4, active = TRUE,
                         updated_at = now()
                     WHERE id = $1",
                )
                .bind(id)
                .bind(&name)
                .bind(parent_id)
                .bind(sqlx::types::Json(&merged))
                .execute(&mut *conn)
                .await?;
                bump(&mut stats.updated, "offers");
            }
            Ok(Some(id))
        }
    }
}

/// Поле-характеристика коллекции предложений: строка с `offer_tree`, `xml_id` —
/// `CML2_ATTR_` + транслит названия (повторные пакеты находят то же поле).
async fn characteristic_field(
    conn: &mut PgConnection,
    state: &mut GoodsState,
    name: &str,
) -> Result<FieldInfo, ApplyError> {
    let xml_id = format!("CML2_ATTR_{}", slugify(name));
    if let Some(f) = state.fields.get(&xml_id) {
        return Ok(f.clone());
    }
    let code = unique_code(
        &field_code_base(name, state.fields.len() + 1),
        &mut state.taken_codes,
    );
    let sort: i32 = sqlx::query_scalar(
        "SELECT COALESCE(max(sort), 0) + 10 FROM collection_fields WHERE collection_id = $1",
    )
    .bind(state.collection_id)
    .fetch_one(&mut *conn)
    .await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO collection_fields (collection_id, code, name, kind, sort, offer_tree, xml_id)
         VALUES ($1, $2, $3, 'string', $4, TRUE, $5) RETURNING id",
    )
    .bind(state.collection_id)
    .bind(&code)
    .bind(name)
    .bind(sort)
    .bind(&xml_id)
    .fetch_one(&mut *conn)
    .await?;
    let info = FieldInfo {
        id,
        code,
        kind: "string".into(),
        multiple: false,
    };
    state.fields.insert(xml_id, info.clone());
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::cml;
    use crate::exchange::settings::Action;
    use sqlx::PgPool;
    use std::sync::Mutex;

    fn parsed(files: &[&[u8]]) -> Vec<Parsed> {
        files.iter().map(|f| cml::parse(f).unwrap()).collect()
    }

    fn package() -> Vec<Parsed> {
        parsed(&[
            include_bytes!("../../tests/fixtures/1c/groups.xml"),
            include_bytes!("../../tests/fixtures/1c/properties.xml"),
            include_bytes!("../../tests/fixtures/1c/storages.xml"),
            include_bytes!("../../tests/fixtures/1c/price_lists.xml"),
        ])
    }

    async fn run(db: &PgPool, settings: &Settings, p: &Parsed) -> Stats {
        let dir = std::env::temp_dir();
        let ctx = Ctx::new(settings, None, &dir, &dir);
        let mut conn = db.acquire().await.unwrap();
        apply(&mut conn, &ctx, p).await.unwrap()
    }

    async fn collection_id(db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT id FROM collections WHERE xml_id = 'cls-0001'")
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn classifier_creates_and_updates(db: PgPool) {
        let settings = Settings::default();
        let mut created = Stats::default();
        for p in package() {
            created = run(&db, &settings, &p).await;
        }
        let cid = collection_id(&db).await;
        let (name, code, is_catalog): (String, String, bool) =
            sqlx::query_as("SELECT name, code, is_catalog FROM collections WHERE id = $1")
                .bind(cid)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(name, "Тестовый каталог");
        assert_eq!(code, "testovyy-katalog");
        assert!(is_catalog);

        // Разделы деревом; помеченная на удаление g-grand при действии N не создаётся
        let rows: Vec<(String, String, Option<String>, i32, bool, i32)> = sqlx::query_as(
            "SELECT s.xml_id, s.name, p.xml_id, s.depth_level, s.active, s.sort
             FROM collection_sections s LEFT JOIN collection_sections p ON p.id = s.parent_id
             WHERE s.collection_id = $1 ORDER BY s.sort",
        )
        .bind(cid)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["g-root", "g-child", "g-child2", "g-root2"]
        );
        assert_eq!(rows[0].1, "Запчасти & компоненты");
        assert_eq!(rows[0].2, None);
        assert_eq!(rows[0].3, 1);
        assert_eq!(rows[1].2.as_deref(), Some("g-root"));
        assert_eq!(rows[1].3, 2);
        assert!(rows.iter().all(|r| r.4));

        // Поля: справочник → list с вариантами, число, дата; удалённое свойство не создаётся
        let fields: Vec<(String, String, String, bool)> = sqlx::query_as(
            "SELECT xml_id, code, kind, multiple FROM collection_fields
             WHERE collection_id = $1 ORDER BY xml_id",
        )
        .bind(cid)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            fields,
            [
                (
                    "p-date".into(),
                    "data-vypuska".replace('-', "_"),
                    "date".into(),
                    false
                ),
                ("p-dir".into(), "primenimost".into(), "list".into(), false),
                ("p-num".into(), "diametr".into(), "number".into(), false),
            ]
        );
        let opts: Vec<(String, String)> = sqlx::query_as(
            "SELECT o.xml_id, o.value FROM collection_field_options o
             JOIN collection_fields f ON f.id = o.field_id
             WHERE f.xml_id = 'p-dir' ORDER BY o.sort",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            opts,
            [
                ("v-1".into(), "Для автомобилей".into()),
                ("v-2".into(), "Для грузовиков".into())
            ]
        );

        // Склады и типы цен; удалённые не создаются
        let stores: Vec<(String, String, bool)> =
            sqlx::query_as("SELECT xml_id, name, active FROM catalog_stores ORDER BY xml_id")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(stores, [("s-1".into(), "Центральный".into(), true)]);
        let pts: Vec<(String, String, bool)> =
            sqlx::query_as("SELECT xml_id, name, is_base FROM catalog_price_types ORDER BY xml_id")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(pts, [("pt-1".into(), "Розничный".into(), false)]);
        let _ = created;

        // Повтор: ничего не создаётся и не обновляется
        for p in package() {
            let st = run(&db, &settings, &p).await;
            assert!(st.created.is_empty(), "{st:?}");
            assert!(st.updated.is_empty(), "{st:?}");
        }

        // Изменённое название группы → обновление, не дубль
        let mut changed = package().remove(0);
        {
            let c = changed.classifier.as_mut().unwrap();
            c.groups[1].name = "Масла и жидкости".into();
            // перенос g-child2 под g-root2
            c.groups[3].parent = Some("g-root2".into());
        }
        let st = run(&db, &settings, &changed).await;
        assert_eq!(st.updated.get("sections"), Some(&2), "{st:?}");
        assert!(st.created.is_empty());
        let rows: Vec<(String, String, i32)> = sqlx::query_as(
            "SELECT xml_id, name, depth_level FROM collection_sections
             WHERE collection_id = $1 ORDER BY xml_id",
        )
        .bind(cid)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0], ("g-child".into(), "Масла и жидкости".into(), 2));
        assert_eq!(rows[1].0, "g-child2");
        let parent: String = sqlx::query_scalar(
            "SELECT p.xml_id FROM collection_sections s JOIN collection_sections p ON p.id = s.parent_id
             WHERE s.xml_id = 'g-child2'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(parent, "g-root2");
    }

    #[sqlx::test]
    async fn default_collection_is_used(db: PgPool) {
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO collections (code, name) VALUES ('catalog', 'Каталог') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let settings = Settings {
            default_collection_id: Some(id),
            ..Settings::default()
        };
        for p in package() {
            run(&db, &settings, &p).await;
        }
        let xml: String = sqlx::query_scalar("SELECT xml_id FROM collections WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(xml, "cls-0001");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM collections")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let s: i64 =
            sqlx::query_scalar("SELECT count(*) FROM collection_sections WHERE collection_id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(s, 4);

        // Неизвестный Ид при заданной коллекции по умолчанию — она же, её xml_id не меняется
        let mut conn = db.acquire().await.unwrap();
        let c = resolve_collection(&mut conn, &settings, "cat-0001", "X")
            .await
            .unwrap();
        assert_eq!(c.id, id);
        let c = resolve_collection(&mut conn, &settings, "cls-0001", "X")
            .await
            .unwrap();
        assert_eq!(c.id, id);
        drop(conn);

        // Пакет товаров с чужим Ид каталога попадает в коллекцию по умолчанию
        let mut goods = cml::parse(include_bytes!("../../tests/fixtures/1c/goods.xml")).unwrap();
        {
            let cat = goods.catalog.as_mut().unwrap();
            cat.id = "task_s1".into();
            cat.classifier_id = "task_s1".into();
        }
        run(&db, &settings, &goods).await;
        let (cid, xml): (i64, String) = sqlx::query_as(
            "SELECT i.collection_id, c.xml_id FROM collection_items i
             JOIN collections c ON c.id = i.collection_id WHERE i.xml_id = 'good-1'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!((cid, xml.as_str()), (id, "cls-0001"));
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM collections")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[sqlx::test]
    async fn default_offers_collection_is_not_used(db: PgPool) {
        let (products,): (i64,) = sqlx::query_as(
            "INSERT INTO collections (code, name, is_catalog) VALUES ('catalog', 'Каталог', TRUE)
             RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let (offers,): (i64,) = sqlx::query_as(
            "INSERT INTO collections (code, name, is_catalog, product_collection_id)
             VALUES ('offers', 'Предложения', TRUE, $1) RETURNING id",
        )
        .bind(products)
        .fetch_one(&db)
        .await
        .unwrap();
        let settings = Settings {
            default_collection_id: Some(offers),
            ..Settings::default()
        };
        let mut conn = db.acquire().await.unwrap();
        let c = resolve_collection(&mut conn, &settings, "cls-0001", "Новый")
            .await
            .unwrap();
        assert_ne!(c.id, offers);
        assert_ne!(c.id, products);
    }

    #[sqlx::test]
    async fn deleted_group_follows_section_action(db: PgPool) {
        let base = |action| Settings {
            section_action: action,
            ..Settings::default()
        };
        // Сначала группа g-grand приходит неудалённой
        let mut first = package().remove(0);
        first.classifier.as_mut().unwrap().groups[2].deleted = false;
        run(&db, &base(Action::None), &first).await;
        let count = |db: PgPool| async move {
            sqlx::query_as::<_, (i64, i64)>(
                "SELECT count(*), count(*) FILTER (WHERE active) FROM collection_sections
                 WHERE xml_id = 'g-grand'",
            )
            .fetch_one(&db)
            .await
            .unwrap()
        };
        assert_eq!(count(db.clone()).await, (1, 1));

        let deleted = package().remove(0);
        // N — без изменений
        let st = run(&db, &base(Action::None), &deleted).await;
        assert_eq!(count(db.clone()).await, (1, 1));
        assert_eq!(st.skipped.get("sections"), Some(&1));
        // A — деактивирована
        let st = run(&db, &base(Action::Deactivate), &deleted).await;
        assert_eq!(count(db.clone()).await, (1, 0));
        assert_eq!(st.deactivated.get("sections"), Some(&1));
        // D — удалена
        let st = run(&db, &base(Action::Delete), &deleted).await;
        assert_eq!(count(db.clone()).await, (0, 0));
        assert_eq!(st.deleted.get("sections"), Some(&1));
    }

    #[sqlx::test]
    async fn unknown_directory_value_adds_option(db: PgPool) {
        let settings = Settings::default();
        let props = package().remove(1);
        run(&db, &settings, &props).await;
        let field_id: i64 =
            sqlx::query_scalar("SELECT id FROM collection_fields WHERE xml_id = 'p-dir'")
                .fetch_one(&db)
                .await
                .unwrap();
        let mut conn = db.acquire().await.unwrap();
        let known = ensure_option(&mut conn, field_id, "v-1", "Для автомобилей")
            .await
            .unwrap();
        let new = ensure_option(&mut conn, field_id, "v-9", "Новое значение")
            .await
            .unwrap();
        let again = ensure_option(&mut conn, field_id, "v-9", "Другое")
            .await
            .unwrap();
        assert_eq!(new, again);
        assert_ne!(known, new);
        drop(conn);
        let value: String =
            sqlx::query_scalar("SELECT value FROM collection_field_options WHERE id = $1")
                .bind(new)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(value, "Новое значение");
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM collection_field_options WHERE field_id = $1")
                .bind(field_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(n, 3);

        // Удалённое свойство не удаляет и не меняет существующее поле
        let mut gone = props.clone();
        for p in &mut gone.classifier.as_mut().unwrap().properties {
            p.deleted = true;
            p.name = "Переименовано".into();
        }
        run(&db, &settings, &gone).await;
        let (n, name): (i64, String) = sqlx::query_as(
            "SELECT count(*), min(name) FROM collection_fields WHERE xml_id = 'p-dir'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(name, "Применимость");
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM collection_field_options WHERE field_id = $1")
                .bind(field_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(n, 3);
    }

    #[sqlx::test]
    async fn full_export_records_seen_sections(db: PgPool) {
        let (sid,): (i64,) =
            sqlx::query_as("INSERT INTO exchange_sessions (token_hash) VALUES ('h') RETURNING id")
                .fetch_one(&db)
                .await
                .unwrap();
        let settings = Settings::default();
        let dir = std::env::temp_dir();
        let ctx = Ctx::new(&settings, Some(sid), &dir, &dir);
        let mut p = package().remove(0);
        let mut conn = db.acquire().await.unwrap();
        // выгрузка только изменений ничего не отмечает
        apply(&mut conn, &ctx, &p).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM exchange_seen")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(n, 0);
        p.classifier.as_mut().unwrap().only_changes = false;
        apply(&mut conn, &ctx, &p).await.unwrap();
        let seen: Vec<String> = sqlx::query_scalar(
            "SELECT xml_id FROM exchange_seen WHERE kind = 'section' ORDER BY xml_id",
        )
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        assert_eq!(seen, ["g-child", "g-child2", "g-root", "g-root2"]);
    }

    #[sqlx::test]
    async fn existing_field_kind_is_never_changed(db: PgPool) {
        let settings = Settings::default();
        let mut first = package().remove(1);
        run(&db, &settings, &first).await;
        sqlx::query("UPDATE collection_fields SET kind = 'string' WHERE xml_id = 'p-num'")
            .execute(&db)
            .await
            .unwrap();
        // p-num в пакете — Число, p-date — Дата; делаем p-dir строкой в пакете
        first.classifier.as_mut().unwrap().properties[0].kind = cml::PropKind::String;
        let st = run(&db, &settings, &first).await;
        let kinds: Vec<(String, String)> =
            sqlx::query_as("SELECT xml_id, kind FROM collection_fields ORDER BY xml_id")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(
            kinds,
            [
                ("p-date".into(), "date".into()),
                ("p-dir".into(), "list".into()),
                ("p-num".into(), "string".into())
            ]
        );
        assert!(st.updated.is_empty(), "{st:?}");
    }

    #[sqlx::test]
    async fn admin_state_of_sections_survives_exchange(db: PgPool) {
        let settings = Settings::default();
        let p = package().remove(0);
        run(&db, &settings, &p).await;
        sqlx::query(
            "UPDATE collection_sections SET active = FALSE, sort = 777 WHERE xml_id = 'g-child'",
        )
        .execute(&db)
        .await
        .unwrap();
        let state = || async {
            sqlx::query_as::<_, (bool, i32)>(
                "SELECT active, sort FROM collection_sections WHERE xml_id = 'g-child'",
            )
            .fetch_one(&db)
            .await
            .unwrap()
        };
        // тот же пакет (только изменения): ничего не трогается
        let st = run(&db, &settings, &p).await;
        assert_eq!(state().await, (false, 777));
        assert!(st.updated.is_empty(), "{st:?}");
        // изменилось название: обновляется оно, активность и порядок остаются
        let mut renamed = p.clone();
        renamed.classifier.as_mut().unwrap().groups[1].name = "Масла 2".into();
        run(&db, &settings, &renamed).await;
        assert_eq!(state().await, (false, 777));
        // полная выгрузка выставляет порядок, активность по-прежнему не трогает
        let mut full = p.clone();
        full.classifier.as_mut().unwrap().only_changes = false;
        run(&db, &settings, &full).await;
        assert_eq!(state().await, (false, 20));
    }

    fn goods_package() -> Parsed {
        cml::parse(include_bytes!("../../tests/fixtures/1c/goods.xml")).unwrap()
    }

    struct Dirs {
        session: PathBuf,
        upload: PathBuf,
    }

    impl Dirs {
        fn new() -> Dirs {
            let root = std::env::temp_dir().join(format!("cms-ex-{}", rand::random::<u64>()));
            let d = Dirs {
                session: root.join("session"),
                upload: root.join("upload"),
            };
            std::fs::create_dir_all(&d.session).unwrap();
            std::fs::create_dir_all(&d.upload).unwrap();
            d
        }

        fn put(&self, rel: &str, bytes: &[u8]) {
            let path = self.session.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }

        fn disk_files(&self) -> usize {
            fn walk(dir: &Path) -> usize {
                std::fs::read_dir(dir)
                    .map(|rd| {
                        rd.flatten()
                            .map(|e| {
                                if e.path().is_dir() {
                                    walk(&e.path())
                                } else {
                                    1
                                }
                            })
                            .sum()
                    })
                    .unwrap_or(0)
            }
            walk(&self.upload)
        }
    }

    async fn run_in(
        db: &PgPool,
        settings: &Settings,
        dirs: &Dirs,
        p: &Parsed,
    ) -> Result<Stats, ApplyError> {
        let ctx = Ctx::new(settings, None, &dirs.session, &dirs.upload);
        let mut conn = db.acquire().await.unwrap();
        apply(&mut conn, &ctx, p).await
    }

    /// Классификатор и затем товары.
    async fn load_goods(db: &PgPool, settings: &Settings, dirs: &Dirs, goods: &Parsed) -> Stats {
        for p in package() {
            run_in(db, settings, dirs, &p).await.unwrap();
        }
        run_in(db, settings, dirs, goods).await.unwrap()
    }

    async fn item_row(db: &PgPool, xml_id: &str) -> sqlx::postgres::PgRow {
        sqlx::query(
            "SELECT i.*, (SELECT xml_id FROM collection_sections WHERE id = i.section_id) AS sec
             FROM collection_items i WHERE xml_id = $1",
        )
        .bind(xml_id)
        .fetch_one(db)
        .await
        .unwrap()
    }

    fn values(row: &sqlx::postgres::PgRow) -> serde_json::Map<String, serde_json::Value> {
        use sqlx::Row;
        row.get::<sqlx::types::Json<serde_json::Map<String, serde_json::Value>>, _>("field_values")
            .0
    }

    #[sqlx::test]
    async fn goods_create_update_with_fields(db: PgPool) {
        use sqlx::Row;
        let settings = Settings::default();
        let dirs = Dirs::new();
        let st = load_goods(&db, &settings, &dirs, &goods_package()).await;
        assert_eq!(st.created.get("items"), Some(&1), "{st:?}");

        let row = item_row(&db, "good-1").await;
        assert_eq!(row.get::<String, _>("name"), "Ремкомплект тестовый");
        assert_eq!(row.get::<String, _>("code"), "remkomplekt-testovyy");
        assert_eq!(
            row.get::<Option<String>, _>("sec").as_deref(),
            Some("g-child")
        );
        assert!(row.get::<bool, _>("active"));
        assert!(
            row.get::<String, _>("detail_text")
                .starts_with("Первая строка")
        );
        let v = values(&row);
        assert_eq!(v["cml2_article"], "ART-100");
        assert_eq!(v["cml2_bar_code"], "4600000000017");
        assert_eq!(v["cml2_base_unit"], "796");
        assert_eq!(v["cml2_taxes"], serde_json::json!(["НДС: 20"]));
        assert_eq!(v["cml2_traits"][0], "ВидНоменклатуры: Запчасти");
        assert_eq!(v["cml2_traits"][1], "Вес: 1,25");
        assert_eq!(v["cml2_traits"].as_array().unwrap().len(), 2);
        // справочник → id варианта; пустое число и неизвестные поля пропущены
        let opt: i64 = sqlx::query_scalar(
            "SELECT o.id FROM collection_field_options o JOIN collection_fields f ON f.id = o.field_id
             WHERE f.xml_id = 'p-dir' AND o.xml_id = 'v-1'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(v["primenimost"], opt);
        assert!(!v.contains_key("diametr"));
        // изготовитель — вариант системного списка
        let m: String =
            sqlx::query_scalar("SELECT o.value FROM collection_field_options o WHERE o.id = $1")
                .bind(v["cml2_manufacturer"].as_i64().unwrap())
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(m, "ACDelco");
        let w: f64 =
            sqlx::query_scalar("SELECT weight::float8 FROM catalog_products WHERE item_id = $1")
                .bind(row.get::<i64, _>("id"))
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(w, 1250.0);
        // помеченный на удаление и неизвестный не создаётся при действии N
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM collection_items")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 1);

        // Обновление: название и артикул; код без транслита при обновлении не меняется
        let mut changed = goods_package();
        {
            let g = &mut changed.catalog.as_mut().unwrap().goods[0];
            g.name = "Ремкомплект новый".into();
            g.article = "ART-200".into();
            g.groups = vec!["g-root2".into()];
        }
        let st = run_in(&db, &settings, &dirs, &changed).await.unwrap();
        assert_eq!(st.updated.get("items"), Some(&1), "{st:?}");
        let row = item_row(&db, "good-1").await;
        assert_eq!(row.get::<String, _>("name"), "Ремкомплект новый");
        assert_eq!(row.get::<String, _>("code"), "remkomplekt-testovyy");
        assert_eq!(
            row.get::<Option<String>, _>("sec").as_deref(),
            Some("g-root2")
        );
        assert_eq!(values(&row)["cml2_article"], "ART-200");

        // При включённом транслите при обновлении код следует за названием
        let on = Settings {
            translit_on_update: true,
            ..Settings::default()
        };
        run_in(&db, &on, &dirs, &changed).await.unwrap();
        let code: String =
            sqlx::query_scalar("SELECT code FROM collection_items WHERE xml_id = 'good-1'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(code, "remkomplekt-novyy");
    }

    #[sqlx::test]
    async fn item_codes_are_unique_in_collection(db: PgPool) {
        let dirs = Dirs::new();
        let mut p = goods_package();
        {
            let goods = &mut p.catalog.as_mut().unwrap().goods;
            goods[1].deleted = false;
            goods[1].name = goods[0].name.clone();
        }
        load_goods(&db, &Settings::default(), &dirs, &p).await;
        let codes: Vec<String> =
            sqlx::query_scalar("SELECT code FROM collection_items ORDER BY xml_id")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(codes, ["remkomplekt-testovyy", "remkomplekt-testovyy-2"]);
    }

    #[sqlx::test]
    async fn pictures_are_deduplicated_by_sha256(db: PgPool) {
        use sqlx::Row;
        let dirs = Dirs::new();
        dirs.put("import_files/aa/good-1_a.jpeg", b"AAAA");
        dirs.put("import_files/aa/good-1_b.jpeg", b"BBBB");
        let settings = Settings::default();
        let st = load_goods(&db, &settings, &dirs, &goods_package()).await;
        assert_eq!(st.created.get("files"), Some(&2), "{st:?}");
        let row = item_row(&db, "good-1").await;
        let pics = values(&row)["cml2_pictures"].clone();
        let ids = pics.as_array().unwrap().clone();
        assert_eq!(ids.len(), 2);
        assert_eq!(
            row.get::<Option<i64>, _>("detail_picture_id"),
            ids[0].as_i64()
        );
        assert_eq!(
            row.get::<Option<i64>, _>("preview_picture_id"),
            ids[0].as_i64()
        );
        assert_eq!(dirs.disk_files(), 2);

        // Те же байты под другими именами — те же file_id, файлов не больше
        let dirs2 = Dirs {
            session: dirs.session.clone(),
            upload: dirs.upload.clone(),
        };
        dirs2.put("import_files/zz/renamed_1.jpeg", b"AAAA");
        dirs2.put("import_files/zz/renamed_2.jpeg", b"BBBB");
        let mut again = goods_package();
        again.catalog.as_mut().unwrap().goods[0].pictures = vec![
            "import_files/zz/renamed_1.jpeg".into(),
            "import_files/zz/renamed_2.jpeg".into(),
        ];
        let st = run_in(&db, &settings, &dirs, &again).await.unwrap();
        assert!(!st.created.contains_key("files"), "{st:?}");
        let row = item_row(&db, "good-1").await;
        assert_eq!(values(&row)["cml2_pictures"], pics);
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM files")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(dirs.disk_files(), 2);

        // Файл заменён другими байтами — новый file_id, у второй картинки прежний
        dirs.put("import_files/zz/renamed_1.jpeg", b"CCCC");
        run_in(&db, &settings, &dirs, &again).await.unwrap();
        let row = item_row(&db, "good-1").await;
        let now = values(&row)["cml2_pictures"].as_array().unwrap().clone();
        assert_ne!(now[0], ids[0]);
        assert_eq!(now[1], ids[1]);

        // Нет файла в пакете — значение ячейки не трогается
        let mut missing = again.clone();
        missing.catalog.as_mut().unwrap().goods[0].pictures = vec![
            "import_files/none/gone.jpeg".into(),
            "import_files/zz/renamed_2.jpeg".into(),
        ];
        run_in(&db, &settings, &dirs, &missing).await.unwrap();
        let row = item_row(&db, "good-1").await;
        assert_eq!(values(&row)["cml2_pictures"], serde_json::json!(now));
        assert_eq!(
            row.get::<Option<i64>, _>("detail_picture_id"),
            now[0].as_i64()
        );
    }

    #[sqlx::test]
    async fn store_file_matches_existing_by_hash(db: PgPool) {
        let dirs = Dirs::new();
        dirs.put("import_files/a.png", b"PNG1");
        let settings = Settings::default();
        let ctx = Ctx::new(&settings, None, &dirs.session, &dirs.upload);
        let mut conn = db.acquire().await.unwrap();
        let first = store_file(&mut conn, &ctx, "import_files/a.png", &[])
            .await
            .unwrap()
            .unwrap();
        // у старой записи sha256 пуст: считается с диска и дописывается
        sqlx::query("UPDATE files SET sha256 = '' WHERE id = $1")
            .bind(first)
            .execute(&mut *conn)
            .await
            .unwrap();
        let again = store_file(&mut conn, &ctx, "import_files/a.png", &[first])
            .await
            .unwrap();
        assert_eq!(again, Some(first));
        let sha: String = sqlx::query_scalar("SELECT sha256 FROM files WHERE id = $1")
            .bind(first)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(sha.len(), 64);
        // нет файла — None; путь за пределами каталога сессии — ошибка
        assert_eq!(
            store_file(&mut conn, &ctx, "import_files/none.png", &[])
                .await
                .unwrap(),
            None
        );
        assert!(
            store_file(&mut conn, &ctx, "../outside.png", &[])
                .await
                .is_err()
        );
    }

    #[sqlx::test]
    async fn deleted_goods_follow_element_action(db: PgPool) {
        let dirs = Dirs::new();
        let with = |action| Settings {
            element_action: action,
            ..Settings::default()
        };
        let mut live = goods_package();
        live.catalog.as_mut().unwrap().goods[1].deleted = false;
        load_goods(&db, &with(Action::None), &dirs, &live).await;
        let state = || async {
            sqlx::query_as::<_, (i64, i64)>(
                "SELECT count(*), count(*) FILTER (WHERE active) FROM collection_items
                 WHERE xml_id = 'good-2'",
            )
            .fetch_one(&db)
            .await
            .unwrap()
        };
        assert_eq!(state().await, (1, 1));
        let gone = goods_package();
        let st = run_in(&db, &with(Action::None), &dirs, &gone)
            .await
            .unwrap();
        assert_eq!(state().await, (1, 1));
        assert_eq!(st.skipped.get("items"), Some(&1), "{st:?}");
        let st = run_in(&db, &with(Action::Deactivate), &dirs, &gone)
            .await
            .unwrap();
        assert_eq!(state().await, (1, 0));
        assert_eq!(st.deactivated.get("items"), Some(&1), "{st:?}");
        // повтор деактивации ничего не меняет
        let st = run_in(&db, &with(Action::Deactivate), &dirs, &gone)
            .await
            .unwrap();
        assert!(st.deactivated.is_empty(), "{st:?}");
        let st = run_in(&db, &with(Action::Delete), &dirs, &gone)
            .await
            .unwrap();
        assert_eq!(state().await, (0, 0));
        assert_eq!(st.deleted.get("items"), Some(&1), "{st:?}");
    }

    #[sqlx::test]
    async fn repeat_package_is_noop(db: PgPool) {
        let dirs = Dirs::new();
        dirs.put("import_files/aa/good-1_a.jpeg", b"AAAA");
        dirs.put("import_files/aa/good-1_b.jpeg", b"BBBB");
        let settings = Settings::default();
        load_goods(&db, &settings, &dirs, &goods_package()).await;
        sqlx::query("UPDATE collection_items SET updated_at = '2020-01-01'")
            .execute(&db)
            .await
            .unwrap();
        let count = |sql: &'static str| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sql)
                    .fetch_one(&db)
                    .await
                    .unwrap()
            }
        };
        let (f, o) = (
            count("SELECT count(*) FROM files").await,
            count("SELECT count(*) FROM collection_field_options").await,
        );
        let st = run_in(&db, &settings, &dirs, &goods_package())
            .await
            .unwrap();
        assert!(st.created.is_empty() && st.updated.is_empty(), "{st:?}");
        assert_eq!(st.skipped.get("items"), None, "{st:?}");
        assert_eq!(count("SELECT count(*) FROM files").await, f);
        assert_eq!(
            count("SELECT count(*) FROM collection_field_options").await,
            o
        );
        assert_eq!(
            count("SELECT count(*) FROM collection_items WHERE updated_at > '2021-01-01'").await,
            0
        );
        assert_eq!(dirs.disk_files(), 2);
    }

    #[sqlx::test]
    async fn existing_field_with_cml2_xml_id_is_reused(db: PgPool) {
        let dirs = Dirs::new();
        dirs.put("import_files/aa/good-1_a.jpeg", b"AAAA");
        let settings = Settings::default();
        for p in package() {
            run_in(&db, &settings, &dirs, &p).await.unwrap();
        }
        let cid = collection_id(&db).await;
        sqlx::query(
            "INSERT INTO collection_fields (collection_id, code, name, kind, multiple, xml_id)
             VALUES ($1, 'more_photo', 'Фото', 'file', TRUE, 'CML2_PICTURES')",
        )
        .bind(cid)
        .execute(&db)
        .await
        .unwrap();
        run_in(&db, &settings, &dirs, &goods_package())
            .await
            .unwrap();
        let v = values(&item_row(&db, "good-1").await);
        assert_eq!(v["more_photo"].as_array().unwrap().len(), 1);
        assert!(!v.contains_key("cml2_pictures"));
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM collection_fields WHERE code = 'cml2_pictures'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(n, 0);
    }

    #[sqlx::test]
    async fn unknown_directory_value_is_added_and_trimmed(db: PgPool) {
        let dirs = Dirs::new();
        let mut p = goods_package();
        p.catalog.as_mut().unwrap().goods[0].properties =
            vec![("p-dir".into(), vec!["  v-2\n".into()])];
        load_goods(&db, &Settings::default(), &dirs, &p).await;
        let v2: i64 =
            sqlx::query_scalar("SELECT id FROM collection_field_options WHERE xml_id = 'v-2'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(values(&item_row(&db, "good-1").await)["primenimost"], v2);
        p.catalog.as_mut().unwrap().goods[0].properties =
            vec![("p-dir".into(), vec!["v-new".into()])];
        run_in(&db, &Settings::default(), &dirs, &p).await.unwrap();
        let (id, value): (i64, String) =
            sqlx::query_as("SELECT id, value FROM collection_field_options WHERE xml_id = 'v-new'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(value, "v-new");
        assert_eq!(values(&item_row(&db, "good-1").await)["primenimost"], id);
    }

    struct Hook {
        fail_on: Option<&'static str>,
        seen: Mutex<Vec<String>>,
    }

    impl ImportHook for Hook {
        fn after_item<'a>(
            &'a self,
            _conn: &'a mut PgConnection,
            _item_id: i64,
            goods: &'a Goods,
        ) -> crate::bxapi::project::BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                if self.fail_on == Some(goods.id.as_str()) {
                    anyhow::bail!("отказ");
                }
                self.seen.lock().unwrap().push(goods.id.clone());
                Ok(())
            })
        }
    }

    #[sqlx::test]
    async fn hook_runs_after_write_and_failure_leaves_no_files(db: PgPool) {
        let dirs = Dirs::new();
        dirs.put("import_files/aa/good-1_a.jpeg", b"AAAA");
        dirs.put("import_files/aa/good-1_b.jpeg", b"BBBB");
        let settings = Settings::default();
        for p in package() {
            run_in(&db, &settings, &dirs, &p).await.unwrap();
        }
        let ok = Hook {
            fail_on: None,
            seen: Mutex::new(Vec::new()),
        };
        // хук получает только созданные и изменённые товары
        let mut two = goods_package();
        two.catalog.as_mut().unwrap().goods[1].deleted = false;
        let ctx = Ctx::new(&settings, None, &dirs.session, &dirs.upload).with_hook(Some(&ok));
        // первый товар падает: файлы, записанные до отказа, убираются, транзакция откатывается
        let bad = Hook {
            fail_on: Some("good-2"),
            seen: Mutex::new(Vec::new()),
        };
        let failing = Ctx::new(&settings, None, &dirs.session, &dirs.upload).with_hook(Some(&bad));
        let mut tx = db.begin().await.unwrap();
        let err = apply(&mut tx, &failing, &two).await.unwrap_err();
        assert!(matches!(err, ApplyError::Hook(_)), "{err}");
        tx.rollback().await.unwrap();
        assert_eq!(dirs.disk_files(), 0);
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM collection_items")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);

        let mut conn = db.acquire().await.unwrap();
        apply(&mut conn, &ctx, &two).await.unwrap();
        assert_eq!(*ok.seen.lock().unwrap(), ["good-1", "good-2"]);
        assert_eq!(dirs.disk_files(), 2);
        apply(&mut conn, &ctx, &two).await.unwrap();
        assert_eq!(ok.seen.lock().unwrap().len(), 2);

        // Ошибка уже после apply (например, при коммите): вызывающий убирает файлы сам
        dirs.put("import_files/aa/good-1_a.jpeg", b"NEW1");
        dirs.put("import_files/aa/good-1_b.jpeg", b"NEW2");
        let late = Ctx::new(&settings, None, &dirs.session, &dirs.upload);
        let mut tx = db.begin().await.unwrap();
        apply(&mut tx, &late, &two).await.unwrap();
        assert_eq!(dirs.disk_files(), 4);
        tx.rollback().await.unwrap();
        late.discard_written().await;
        assert_eq!(dirs.disk_files(), 2);
    }

    #[sqlx::test]
    async fn full_export_records_seen_items(db: PgPool) {
        let (sid,): (i64,) =
            sqlx::query_as("INSERT INTO exchange_sessions (token_hash) VALUES ('h') RETURNING id")
                .fetch_one(&db)
                .await
                .unwrap();
        let dirs = Dirs::new();
        let settings = Settings::default();
        for p in package() {
            run_in(&db, &settings, &dirs, &p).await.unwrap();
        }
        let ctx = Ctx::new(&settings, Some(sid), &dirs.session, &dirs.upload);
        let mut conn = db.acquire().await.unwrap();
        apply(&mut conn, &ctx, &goods_package()).await.unwrap();
        let seen: Vec<String> = sqlx::query_scalar(
            "SELECT xml_id FROM exchange_seen WHERE kind = 'item' ORDER BY xml_id",
        )
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        // удалённый в 1С товар «виденным» не считается
        assert_eq!(seen, ["good-1"]);
    }

    #[sqlx::test]
    async fn goods_without_delete_mark_are_reactivated(db: PgPool) {
        let dirs = Dirs::new();
        let settings = Settings {
            element_action: Action::Deactivate,
            ..Settings::default()
        };
        load_goods(&db, &settings, &dirs, &goods_package()).await;
        let active = || async {
            sqlx::query_scalar::<_, bool>(
                "SELECT active FROM collection_items WHERE xml_id = 'good-1'",
            )
            .fetch_one(&db)
            .await
            .unwrap()
        };
        let mut gone = goods_package();
        gone.catalog.as_mut().unwrap().goods[0].deleted = true;
        run_in(&db, &settings, &dirs, &gone).await.unwrap();
        assert!(!active().await);
        let st = run_in(&db, &settings, &dirs, &goods_package())
            .await
            .unwrap();
        assert!(active().await);
        assert_eq!(st.updated.get("items"), Some(&1), "{st:?}");
        // уже активная и неизменная запись не пишется
        let st = run_in(&db, &settings, &dirs, &goods_package())
            .await
            .unwrap();
        assert!(st.updated.is_empty() && st.created.is_empty(), "{st:?}");
    }

    fn offers_file(bytes: &[u8]) -> Parsed {
        cml::parse(bytes).unwrap()
    }

    fn prices_package() -> Parsed {
        offers_file(include_bytes!("../../tests/fixtures/1c/prices.xml"))
    }

    fn rests_package() -> Parsed {
        offers_file(include_bytes!("../../tests/fixtures/1c/rests.xml"))
    }

    #[sqlx::test]
    async fn prices_and_rests_applied(db: PgPool) {
        let settings = Settings::default();
        let dirs = Dirs::new();
        load_goods(&db, &settings, &dirs, &goods_package()).await;

        let st = run_in(&db, &settings, &dirs, &prices_package())
            .await
            .unwrap();
        // pt-2 в классификаторе нет: тип цены создаётся
        assert_eq!(st.created.get("price_types"), Some(&1), "{st:?}");
        assert_eq!(st.created.get("prices"), Some(&2), "{st:?}");
        let prices: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT t.xml_id, p.price::text, p.currency FROM catalog_prices p
             JOIN catalog_price_types t ON t.id = p.price_type_id
             JOIN collection_items i ON i.id = p.item_id
             WHERE i.xml_id = 'good-1' ORDER BY t.xml_id",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            prices,
            [
                ("pt-1".into(), "6540.66".into(), "RUB".into()),
                ("pt-2".into(), "80.00".into(), "USD".into())
            ]
        );
        let name: String =
            sqlx::query_scalar("SELECT name FROM catalog_price_types WHERE xml_id = 'pt-2'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(name, "pt-2");

        let st = run_in(&db, &settings, &dirs, &rests_package())
            .await
            .unwrap();
        // s-2 нового склада нет в классификаторе; good-2 (помечен на удаление) и good-3 неизвестны
        assert_eq!(st.created.get("stores"), Some(&1), "{st:?}");
        assert_eq!(st.skipped.get("offers"), Some(&2), "{st:?}");
        let amounts: Vec<(String, String)> = sqlx::query_as(
            "SELECT s.xml_id, a.amount::text FROM catalog_store_amounts a
             JOIN catalog_stores s ON s.id = a.store_id
             JOIN collection_items i ON i.id = a.item_id
             WHERE i.xml_id = 'good-1' ORDER BY s.xml_id",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            amounts,
            [("s-1".into(), "6.00".into()), ("s-2".into(), "2.50".into())]
        );
        let qty: String = sqlx::query_scalar(
            "SELECT c.quantity::text FROM catalog_products c
             JOIN collection_items i ON i.id = c.item_id WHERE i.xml_id = 'good-1'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(qty, "8.50");

        // Повтор ничего не пишет
        for p in [prices_package(), rests_package()] {
            let st = run_in(&db, &settings, &dirs, &p).await.unwrap();
            assert!(st.created.is_empty() && st.updated.is_empty(), "{st:?}");
        }

        // Меняется цена одного типа; второй и типы вне пакета остаются
        let mut changed = prices_package();
        {
            let offer = &mut changed.offers.as_mut().unwrap().offers[0];
            let prices = offer.prices.as_mut().unwrap();
            prices[0].value = 7000.0;
            prices.truncate(1);
        }
        let st = run_in(&db, &settings, &dirs, &changed).await.unwrap();
        assert_eq!(st.updated.get("prices"), Some(&1), "{st:?}");
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM catalog_prices p JOIN collection_items i ON i.id = p.item_id
             WHERE i.xml_id = 'good-1'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(n, 2);
        let p1: String = sqlx::query_scalar(
            "SELECT p.price::text FROM catalog_prices p
             JOIN catalog_price_types t ON t.id = p.price_type_id WHERE t.xml_id = 'pt-1'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(p1, "7000.00");

        // Изменился остаток склада: пересчёт общего количества
        let mut rests = rests_package();
        rests.offers.as_mut().unwrap().offers[0]
            .rests
            .as_mut()
            .unwrap()[0]
            .1 = 1.0;
        run_in(&db, &settings, &dirs, &rests).await.unwrap();
        let qty: String = sqlx::query_scalar(
            "SELECT c.quantity::text FROM catalog_products c
             JOIN collection_items i ON i.id = c.item_id WHERE i.xml_id = 'good-1'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(qty, "3.50");
    }

    #[sqlx::test]
    async fn quantity_without_rests_sets_only_quantity(db: PgPool) {
        let settings = Settings::default();
        let dirs = Dirs::new();
        load_goods(&db, &settings, &dirs, &goods_package()).await;
        let mut p = rests_package();
        {
            let offer = &mut p.offers.as_mut().unwrap().offers[0];
            offer.rests = None;
            offer.quantity = Some(12.0);
        }
        run_in(&db, &settings, &dirs, &p).await.unwrap();
        let qty: String = sqlx::query_scalar(
            "SELECT c.quantity::text FROM catalog_products c
             JOIN collection_items i ON i.id = c.item_id WHERE i.xml_id = 'good-1'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(qty, "12.00");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_store_amounts")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    /// Классификатор и товары 2.x (`import.xml`), затем предложения (`offers.xml`).
    async fn load_2x(db: &PgPool, settings: &Settings, dirs: &Dirs) -> Stats {
        let import = offers_file(include_bytes!("../../tests/fixtures/1c/import.xml"));
        run_in(db, settings, dirs, &import).await.unwrap();
        let offers = offers_file(include_bytes!("../../tests/fixtures/1c/offers.xml"));
        run_in(db, settings, dirs, &offers).await.unwrap()
    }

    #[sqlx::test]
    async fn characteristic_offers_become_sku(db: PgPool) {
        use sqlx::Row;
        let settings = Settings {
            use_offers: true,
            ..Settings::default()
        };
        let dirs = Dirs::new();
        let st = load_2x(&db, &settings, &dirs).await;
        assert_eq!(st.created.get("offers"), Some(&1), "{st:?}");

        let (product, products_cid): (i64, i64) = sqlx::query_as(
            "SELECT id, collection_id FROM collection_items WHERE xml_id = 'g2-good'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let offers_cid: i64 =
            sqlx::query_scalar("SELECT id FROM collections WHERE product_collection_id = $1")
                .bind(products_cid)
                .fetch_one(&db)
                .await
                .unwrap();
        let row = item_row(&db, "g2-good#sku-red-m").await;
        assert_eq!(row.get::<i64, _>("collection_id"), offers_cid);
        assert_eq!(row.get::<Option<i64>, _>("product_id"), Some(product));
        assert_eq!(row.get::<String, _>("name"), "Футболка (красная, M)");
        assert!(row.get::<bool, _>("active"));

        // Поля характеристик: строки с offer_tree, значения в записи
        let fields: Vec<(String, String, bool, String)> = sqlx::query_as(
            "SELECT code, kind, offer_tree, xml_id FROM collection_fields
             WHERE collection_id = $1 AND offer_tree ORDER BY code",
        )
        .bind(offers_cid)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            fields,
            [
                (
                    "razmer".into(),
                    "string".into(),
                    true,
                    "CML2_ATTR_razmer".into()
                ),
                (
                    "tsvet".into(),
                    "string".into(),
                    true,
                    "CML2_ATTR_tsvet".into()
                ),
            ]
        );
        let v = values(&row);
        assert_eq!(v.get("tsvet"), Some(&serde_json::json!("красный")));
        assert_eq!(v.get("razmer"), Some(&serde_json::json!("M")));

        // Цена, склад и количество предложения; товар стал товаром с предложениями
        let (price, cur): (String, String) = sqlx::query_as(
            "SELECT price::text, currency FROM catalog_prices p
             JOIN collection_items i ON i.id = p.item_id WHERE i.xml_id = 'g2-good#sku-red-m'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!((price.as_str(), cur.as_str()), ("999.50", "RUB"));
        let qty: String = sqlx::query_scalar(
            "SELECT c.quantity::text FROM catalog_products c
             JOIN collection_items i ON i.id = c.item_id WHERE i.xml_id = 'g2-good#sku-red-m'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(qty, "4.00");
        let ty: i16 = sqlx::query_scalar("SELECT type FROM catalog_products WHERE item_id = $1")
            .bind(product)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(ty, 3);

        // Повтор: ни записей, ни новой коллекции
        let offers = offers_file(include_bytes!("../../tests/fixtures/1c/offers.xml"));
        let st = run_in(&db, &settings, &dirs, &offers).await.unwrap();
        assert!(st.created.is_empty() && st.updated.is_empty(), "{st:?}");
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM collections WHERE product_collection_id IS NOT NULL",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(n, 1);
    }

    #[sqlx::test]
    async fn characteristic_offers_skipped_without_use_offers(db: PgPool) {
        let settings = Settings::default();
        let dirs = Dirs::new();
        let st = load_2x(&db, &settings, &dirs).await;
        assert_eq!(st.skipped.get("offers"), Some(&1), "{st:?}");
        let n: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM collection_items WHERE xml_id LIKE '%#%')
                  + (SELECT count(*) FROM collections WHERE product_collection_id IS NOT NULL)
                  + (SELECT count(*) FROM catalog_prices)",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(n, 0);
    }

    #[sqlx::test]
    async fn offers_for_unknown_goods_are_skipped(db: PgPool) {
        let settings = Settings {
            use_offers: true,
            ..Settings::default()
        };
        let dirs = Dirs::new();
        load_goods(&db, &settings, &dirs, &goods_package()).await;
        let mut p = prices_package();
        {
            let offers = &mut p.offers.as_mut().unwrap().offers;
            offers[0].id = "nope".into();
            offers[1].id = "nope#x".into();
        }
        let st = run_in(&db, &settings, &dirs, &p).await.unwrap();
        assert_eq!(st.skipped.get("offers"), Some(&2), "{st:?}");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_prices")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM collection_items WHERE xml_id IN ('nope', 'nope#x')",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(n, 0);
    }

    #[sqlx::test]
    async fn goods_without_price_deactivated_when_enabled(db: PgPool) {
        let active = |db: PgPool| async move {
            sqlx::query_as::<_, (String, bool)>(
                "SELECT xml_id, active FROM collection_items ORDER BY xml_id",
            )
            .fetch_all(&db)
            .await
            .unwrap()
        };
        let dirs = Dirs::new();
        // Настройка выключена: все остаются активными
        let off = Settings::default();
        let mut goods = goods_package();
        goods.catalog.as_mut().unwrap().goods[1].deleted = false;
        load_goods(&db, &off, &dirs, &goods).await;
        run_in(&db, &off, &dirs, &prices_package()).await.unwrap();
        assert!(active(db.clone()).await.iter().all(|r| r.1));

        let on = Settings {
            deactivate_without_price: true,
            ..Settings::default()
        };
        // Пакет остатков цен не несёт: ничего не меняется
        run_in(&db, &on, &dirs, &rests_package()).await.unwrap();
        assert!(active(db.clone()).await.iter().all(|r| r.1));

        let st = run_in(&db, &on, &dirs, &prices_package()).await.unwrap();
        assert_eq!(st.deactivated.get("items"), Some(&1), "{st:?}");
        assert_eq!(
            active(db.clone()).await,
            [("good-1".to_string(), true), ("good-2".to_string(), false)]
        );
        // Повтор не считает уже неактивную
        let st = run_in(&db, &on, &dirs, &prices_package()).await.unwrap();
        assert!(st.deactivated.is_empty(), "{st:?}");
    }

    #[sqlx::test]
    async fn deactivation_follows_package_and_reactivates(db: PgPool) {
        let on = Settings {
            deactivate_without_price: true,
            ..Settings::default()
        };
        let dirs = Dirs::new();
        let mut goods = goods_package();
        goods.catalog.as_mut().unwrap().goods[1].deleted = false;
        load_goods(&db, &on, &dirs, &goods).await;
        let active = |db: PgPool| async move {
            sqlx::query_as::<_, (String, bool)>(
                "SELECT xml_id, active FROM collection_items ORDER BY xml_id",
            )
            .fetch_all(&db)
            .await
            .unwrap()
        };
        // Цены раздельными пакетами: сначала good-1, потом good-2
        let price = |id: &str, ty: &str| {
            let mut p = prices_package();
            let offers = &mut p.offers.as_mut().unwrap().offers;
            offers.retain(|o| o.id == "good-1");
            offers[0].id = id.into();
            offers[0]
                .prices
                .as_mut()
                .unwrap()
                .retain(|x| x.type_id == ty);
            p
        };
        run_in(&db, &on, &dirs, &price("good-1", "pt-1"))
            .await
            .unwrap();
        // good-2 вне пакета: не тронут
        assert!(active(db.clone()).await.iter().all(|r| r.1));
        run_in(&db, &on, &dirs, &price("good-2", "pt-1"))
            .await
            .unwrap();
        assert!(active(db.clone()).await.iter().all(|r| r.1));

        // Запись пакета без цены неактивна, запись вне пакета не тронута
        sqlx::query("DELETE FROM catalog_prices p USING collection_items i WHERE i.id = p.item_id AND i.xml_id = 'good-2'")
            .execute(&db)
            .await
            .unwrap();
        let mut p = price("good-1", "pt-2");
        p.offers.as_mut().unwrap().offers.push(cml::Offer {
            id: "good-2".into(),
            prices: Some(Vec::new()),
            ..cml::Offer::default()
        });
        let st = run_in(&db, &on, &dirs, &p).await.unwrap();
        assert_eq!(st.deactivated.get("items"), Some(&1), "{st:?}");
        assert_eq!(
            active(db.clone()).await,
            [("good-1".to_string(), true), ("good-2".to_string(), false)]
        );
        // Получила цену: снова активна
        let st = run_in(&db, &on, &dirs, &price("good-2", "pt-1"))
            .await
            .unwrap();
        assert_eq!(st.updated.get("items"), Some(&1), "{st:?}");
        assert!(active(db.clone()).await.iter().all(|r| r.1));
    }
}
