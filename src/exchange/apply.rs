//! Запись разобранной выгрузки в БД: классификатор, затем товары и предложения.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;
use sqlx::PgConnection;

use super::cml::{Classifier, Parsed, PropKind, Property};
use super::settings::{Action, Settings};
use crate::collection::{Collection, is_valid_code, is_valid_slug, slugify};

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("Ошибка базы данных: {0}")]
    Db(#[from] sqlx::Error),
}

pub struct Ctx<'a> {
    pub settings: &'a Settings,
    pub session_id: Option<i64>,
    pub session_dir: &'a Path,
    pub upload_dir: &'a Path,
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
/// без своего `xml_id` (ей присваивается первый непустой `id`), иначе новая коллекция каталога.
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
        let adopted: Option<Collection> = sqlx::query_as(
            "UPDATE collections SET xml_id = $2, updated_at = now()
             WHERE id = $1 AND xml_id = '' RETURNING *",
        )
        .bind(default_id)
        .bind(xml_id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(c) = adopted {
            return Ok(c);
        }
        if xml_id.is_empty()
            && let Some(c) = sqlx::query_as("SELECT * FROM collections WHERE id = $1")
                .bind(default_id)
                .fetch_optional(&mut *conn)
                .await?
        {
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
pub async fn resolve_collection(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::cml;
    use crate::exchange::settings::Action;
    use sqlx::PgPool;

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
        let ctx = Ctx {
            settings,
            session_id: None,
            session_dir: &dir,
            upload_dir: &dir,
        };
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

        // Пакет каталога без классификатора находит коллекцию по ИдКлассификатора
        let mut conn = db.acquire().await.unwrap();
        let c = resolve_collection(&mut conn, &settings, "cat-0001", "X")
            .await
            .unwrap();
        // cat-0001 неизвестен, но коллекция по умолчанию уже занята другим xml_id → новая
        assert_ne!(c.id, id);
        let c = resolve_collection(&mut conn, &settings, "cls-0001", "X")
            .await
            .unwrap();
        assert_eq!(c.id, id);
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
        let ctx = Ctx {
            settings: &settings,
            session_id: Some(sid),
            session_dir: &dir,
            upload_dir: &dir,
        };
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
}
