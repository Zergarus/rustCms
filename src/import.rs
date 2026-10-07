//! Перенос инфоблоков из базы 1С-Битрикс (MySQL/MariaDB).
//!
//! Переносятся инфоблоки, свойства, варианты списков, разделы, элементы со значениями
//! свойств и файлы. HL-блоки становятся обычными инфоблоками: строка → элемент
//! (`UF_XML_ID` → внешний код), а свойства-справочники — привязками к этим элементам.
//!
//! Id инфоблоков, свойств, вариантов, разделов, элементов и файлов сохраняются как в
//! Битриксе: фронт и старые ссылки опираются на них. Элементы HL-блоков получают id
//! после максимального id элемента Битрикса, сами HL-инфоблоки — `HL_IBLOCK_ID_BASE + id`.
//!
//! Файлы из `upload` Битрикса жёстко линкуются (или копируются) в `UPLOAD_DIR/bitrix/`.
//! Записи о файлах, которых нет на диске, всё равно переносятся — их можно докачать.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Instant,
};

use anyhow::{Context, bail};
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use serde_json::{Map, Value};
use sqlx::{MySqlPool, PgPool, Postgres, QueryBuilder, Row, mysql::MySqlPoolOptions};

use crate::{
    bxapi::to_snake,
    iblock::{is_valid_code, is_valid_slug, slugify},
};

/// Id HL-инфоблока = база + id HL-блока (у Битрикса инфоблоков заметно меньше).
const HL_IBLOCK_ID_BASE: i64 = 100;
/// Даты в Битриксе хранятся во времени сервера — у проектов это Москва.
const BITRIX_UTC_OFFSET_SECS: i32 = 3 * 3600;
/// Строк в одном INSERT (Postgres ограничивает число параметров 65535).
const BATCH: usize = 1000;

pub struct Options {
    pub mysql_url: String,
    /// Каталог `upload` сайта на Битриксе.
    pub bitrix_upload: Option<PathBuf>,
    pub upload_dir: PathBuf,
    /// Удалить уже существующие инфоблоки перед переносом.
    pub replace: bool,
    /// apiCode для инфоблоков, у которых API_CODE в базе не заполнен (`--api-code ID=код`).
    pub api_codes: Vec<(i64, String)>,
}

// ---------------------------------------------------------------------------
// Промежуточные структуры
// ---------------------------------------------------------------------------

struct IblockRow {
    id: i64,
    /// Доступен в API: у инфоблока есть API_CODE (HL-блоки и прочие — только для связей).
    api_enabled: bool,
    code: String,
    name: String,
    description: String,
    sort: i32,
    detail_page_url: String,
    section_page_url: String,
    list_page_url: String,
    is_catalog: bool,
}

struct PropertyRow {
    id: i64,
    iblock_id: i64,
    code: String,
    name: String,
    kind: &'static str,
    multiple: bool,
    is_required: bool,
    sort: i32,
    link_iblock_id: Option<i64>,
    /// Справочник: значения — UF_XML_ID строк HL-блока, их надо превратить в id элементов.
    directory: bool,
    /// USER_TYPE свойства Битрикса (`directory`, `UserID`, `DateTime`...).
    user_type: String,
}

struct EnumRow {
    id: i64,
    property_id: i64,
    value: String,
    xml_id: String,
    sort: i32,
    is_default: bool,
}

struct SectionRow {
    id: i64,
    iblock_id: i64,
    parent_id: Option<i64>,
    code: String,
    xml_id: String,
    name: String,
    active: bool,
    sort: i32,
    depth_level: i32,
    description: String,
    picture_id: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

struct ElementRow {
    id: i64,
    iblock_id: i64,
    section_id: Option<i64>,
    code: String,
    xml_id: String,
    name: String,
    active: bool,
    sort: i32,
    preview_text: String,
    detail_text: String,
    preview_picture_id: Option<i64>,
    detail_picture_id: Option<i64>,
    published_at: Option<DateTime<Utc>>,
    created_by: Option<i64>,
    /// Сырые значения свойств по id свойства: строки в порядке хранения.
    raw: HashMap<i64, Vec<String>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

struct FileRow {
    id: i64,
    path: String,
    original_name: String,
    content_type: String,
    size: i64,
    width: Option<i32>,
    height: Option<i32>,
    created_at: DateTime<Utc>,
}

#[derive(Default)]
struct Data {
    iblocks: Vec<IblockRow>,
    properties: Vec<PropertyRow>,
    enums: Vec<EnumRow>,
    sections: Vec<SectionRow>,
    elements: Vec<ElementRow>,
    files: Vec<FileRow>,
    /// Исходные пути файлов в `upload` Битрикса.
    file_sources: HashMap<i64, String>,
    catalog: Catalog,
    users: Vec<UserRow>,
}

/// Пользователь сайта из `b_user`.
struct UserRow {
    bitrix_id: i64,
    login: String,
    email: String,
    name: String,
    last_name: String,
    /// Хеш как храним у себя (см. [`crate::passwords::from_bitrix`]).
    password_hash: String,
    active: bool,
}

/// Цена: (элемент, тип цены, цена, валюта, количество от, до).
type PriceRow = (i64, i64, f64, String, Option<i32>, Option<i32>);

/// Торговый каталог: цены, склады, остатки, форматы валют.
#[derive(Default)]
struct Catalog {
    /// (id, код, название, базовый)
    price_types: Vec<(i64, String, String, bool)>,
    prices: Vec<PriceRow>,
    /// (id, название, активен, сортировка)
    stores: Vec<(i64, String, bool, i32)>,
    /// (элемент, склад, количество)
    amounts: Vec<(i64, i64, f64)>,
    /// (элемент, общий остаток)
    products: Vec<(i64, f64)>,
    /// (валюта, формат, десятичный разделитель, разделитель тысяч, знаков, скрывать нули)
    currencies: Vec<(String, String, String, String, i32, bool)>,
}

// ---------------------------------------------------------------------------
// Точка входа
// ---------------------------------------------------------------------------

pub async fn run(db: &PgPool, opts: Options) -> anyhow::Result<()> {
    let started = Instant::now();
    let (existing,): (i64,) = sqlx::query_as("SELECT count(*) FROM iblocks")
        .fetch_one(db)
        .await?;
    if existing > 0 && !opts.replace {
        bail!("в CMS уже есть инфоблоки ({existing}); запустите с --replace, чтобы заменить их");
    }

    let my = MySqlPoolOptions::new()
        .max_connections(4)
        .connect(&opts.mysql_url)
        .await
        .context("не удалось подключиться к MySQL Битрикса")?;

    let mut data = Data::default();
    stage(
        "инфоблоки и свойства",
        read_iblocks(&my, &mut data, &opts.api_codes),
    )
    .await?;
    stage("HL-блоки", read_hlblocks(&my, &mut data)).await?;
    stage("разделы", read_sections(&my, &mut data)).await?;
    stage("элементы", read_elements(&my, &mut data)).await?;
    stage("значения свойств", read_property_values(&my, &mut data)).await?;
    stage("файлы", read_files(&my, &mut data)).await?;
    stage("торговый каталог", read_catalog(&my, &mut data)).await?;
    stage("пользователи", read_users(&my, &mut data)).await?;

    let props = resolve_properties(&mut data);
    let (linked, missing) = link_files(&data, &opts)?;

    let mut tx = db.begin().await?;
    if opts.replace {
        sqlx::query("DELETE FROM iblocks").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM files WHERE source = 'bitrix' OR path LIKE 'bitrix/%'")
            .execute(&mut *tx)
            .await?;
    }
    stage("запись в PostgreSQL", write_all(&mut tx, &data, &props)).await?;
    tx.commit().await?;

    tracing::info!(
        iblocks = data.iblocks.len(),
        properties = data.properties.len(),
        enums = data.enums.len(),
        sections = data.sections.len(),
        elements = data.elements.len(),
        files = data.files.len(),
        users = data.users.len(),
        prices = data.catalog.prices.len(),
        store_amounts = data.catalog.amounts.len(),
        files_linked = linked,
        files_missing = missing,
        seconds = started.elapsed().as_secs_f32(),
        "перенос завершён"
    );
    Ok(())
}

async fn stage<T>(name: &str, fut: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    let started = Instant::now();
    let result = fut.await.with_context(|| format!("этап «{name}»"))?;
    tracing::info!(ms = started.elapsed().as_millis() as u64, "{name}: готово");
    Ok(result)
}

// ---------------------------------------------------------------------------
// Чтение из MySQL. Всё приводится к CHAR/SIGNED в запросе, чтобы не зависеть
// от конкретных типов колонок Битрикса.
// ---------------------------------------------------------------------------

fn str_col(row: &sqlx::mysql::MySqlRow, i: usize) -> String {
    row.try_get::<Option<String>, _>(i)
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn int_col(row: &sqlx::mysql::MySqlRow, i: usize) -> Option<i64> {
    row.try_get::<Option<i64>, _>(i).ok().flatten()
}

/// Id > 0 (в Битриксе 0 и NULL одинаково означают «нет»).
fn id_col(row: &sqlx::mysql::MySqlRow, i: usize) -> Option<i64> {
    int_col(row, i).filter(|id| *id > 0)
}

fn bitrix_date(raw: &str) -> Option<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S").ok()?;
    let offset = FixedOffset::east_opt(BITRIX_UTC_OFFSET_SECS)?;
    Some(
        naive
            .and_local_timezone(offset)
            .single()?
            .with_timezone(&Utc),
    )
}

/// Код в формате CMS; если из исходного не получается — `fallback`.
fn clean_code(raw: &str, fallback: String) -> String {
    let code = to_snake(raw.trim());
    if is_valid_code(&code) {
        code
    } else {
        match slugify(raw).replace('-', "_") {
            s if is_valid_code(&s) => s,
            _ => fallback,
        }
    }
}

async fn read_iblocks(
    my: &MySqlPool,
    data: &mut Data,
    api_codes: &[(i64, String)],
) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(IFNULL(API_CODE, '') AS CHAR), CAST(IFNULL(CODE, '') AS CHAR),
                CAST(NAME AS CHAR), CAST(SORT AS SIGNED), CAST(IFNULL(DESCRIPTION, '') AS CHAR),
                CAST(IFNULL(DETAIL_PAGE_URL, '') AS CHAR), CAST(IFNULL(SECTION_PAGE_URL, '') AS CHAR),
                CAST(IFNULL(LIST_PAGE_URL, '') AS CHAR),
                CAST(ID IN (SELECT IBLOCK_ID FROM b_catalog_iblock) AS SIGNED)
         FROM b_iblock ORDER BY ID",
    )
    .fetch_all(my)
    .await?;
    let mut used = HashSet::new();
    for row in rows {
        let id = int_col(&row, 0).context("b_iblock.ID")?;
        let api_code = str_col(&row, 1);
        let override_code = api_codes
            .iter()
            .find(|(oid, _)| *oid == id)
            .map(|(_, c)| c.clone());
        let api_enabled = !api_code.is_empty() || override_code.is_some();
        let source = [Some(api_code), override_code, Some(str_col(&row, 2))]
            .into_iter()
            .flatten()
            .find(|c| !c.is_empty())
            .unwrap_or_default();
        let mut code = clean_code(&source, format!("iblock_{id}"));
        if !used.insert(code.clone()) {
            code = format!("{code}_{id}");
            used.insert(code.clone());
        }
        data.iblocks.push(IblockRow {
            id,
            api_enabled,
            code,
            name: str_col(&row, 3),
            description: str_col(&row, 5),
            sort: int_col(&row, 4).unwrap_or(500) as i32,
            detail_page_url: str_col(&row, 6),
            section_page_url: str_col(&row, 7),
            list_page_url: str_col(&row, 8),
            is_catalog: int_col(&row, 9) == Some(1),
        });
    }

    let hl_tables = hl_table_ids(my).await?;
    let rows = sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(IBLOCK_ID AS SIGNED), CAST(IFNULL(CODE, '') AS CHAR),
                CAST(NAME AS CHAR), CAST(PROPERTY_TYPE AS CHAR), CAST(IFNULL(USER_TYPE, '') AS CHAR),
                CAST(MULTIPLE AS CHAR), CAST(IS_REQUIRED AS CHAR), CAST(SORT AS SIGNED),
                CAST(IFNULL(LINK_IBLOCK_ID, 0) AS SIGNED), CAST(IFNULL(USER_TYPE_SETTINGS, '') AS CHAR)
         FROM b_iblock_property ORDER BY IBLOCK_ID, SORT, ID",
    )
    .fetch_all(my)
    .await?;
    let mut used: HashSet<(i64, String)> = HashSet::new();
    for row in rows {
        let id = int_col(&row, 0).context("b_iblock_property.ID")?;
        let iblock_id = int_col(&row, 1).unwrap_or_default();
        let mut code = clean_code(&str_col(&row, 2), format!("property_{id}"));
        if !used.insert((iblock_id, code.clone())) {
            code = format!("{code}_{id}");
            used.insert((iblock_id, code.clone()));
        }
        let (ptype, user_type) = (str_col(&row, 4), str_col(&row, 5));
        let mut link_iblock_id = id_col(&row, 9);
        let mut directory = false;
        let kind = match (ptype.as_str(), user_type.as_str()) {
            ("S", "directory") => {
                // Привязка к HL-блоку по имени таблицы из настроек свойства
                let settings = str_col(&row, 10);
                link_iblock_id = hl_tables
                    .iter()
                    .find(|(table, _)| settings.contains(&format!("\"{table}\"")))
                    .map(|(_, hl_id)| HL_IBLOCK_ID_BASE + hl_id);
                directory = true;
                "element"
            }
            ("S", "DateTime" | "Date") => "date",
            ("S", "HTML") => "text",
            ("N", _) => "number",
            ("L", _) => "list",
            ("E", _) => "element",
            ("F", _) => "file",
            _ => "string",
        };
        if kind != "element" {
            link_iblock_id = None;
        }
        data.properties.push(PropertyRow {
            id,
            iblock_id,
            code,
            name: str_col(&row, 3),
            kind,
            multiple: str_col(&row, 6) == "Y",
            is_required: str_col(&row, 7) == "Y",
            sort: int_col(&row, 8).unwrap_or(500) as i32,
            link_iblock_id,
            directory,
            user_type,
        });
    }

    let rows = sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(PROPERTY_ID AS SIGNED), CAST(VALUE AS CHAR),
                CAST(IFNULL(XML_ID, '') AS CHAR), CAST(SORT AS SIGNED), CAST(DEF AS CHAR)
         FROM b_iblock_property_enum ORDER BY PROPERTY_ID, SORT, ID",
    )
    .fetch_all(my)
    .await?;
    let mut used: HashSet<(i64, String)> = HashSet::new();
    for row in rows {
        let id = int_col(&row, 0).context("b_iblock_property_enum.ID")?;
        let property_id = int_col(&row, 1).unwrap_or_default();
        let mut xml_id = str_col(&row, 3);
        if xml_id.is_empty() || !used.insert((property_id, xml_id.clone())) {
            xml_id = format!("{xml_id}_{id}");
            used.insert((property_id, xml_id.clone()));
        }
        data.enums.push(EnumRow {
            id,
            property_id,
            value: str_col(&row, 2),
            xml_id,
            sort: int_col(&row, 4).unwrap_or(500) as i32,
            is_default: str_col(&row, 5) == "Y",
        });
    }
    Ok(())
}

/// Таблица HL-блока → id HL-блока.
async fn hl_table_ids(my: &MySqlPool) -> anyhow::Result<Vec<(String, i64)>> {
    let rows = sqlx::query(
        "SELECT CAST(TABLE_NAME AS CHAR), CAST(ID AS SIGNED) FROM b_hlblock_entity ORDER BY ID",
    )
    .fetch_all(my)
    .await?;
    Ok(rows
        .iter()
        .map(|r| (str_col(r, 0), int_col(r, 1).unwrap_or_default()))
        .collect())
}

/// HL-блоки → инфоблоки. `UF_NAME` (или первое строковое поле) — ещё и название элемента,
/// `UF_XML_ID` — внешний код, `UF_SORT` — сортировка, остальные `UF_*` — свойства
/// с кодом `uf_<поле>` (в API — `ufПоле`, как отдаёт Битрикс).
async fn read_hlblocks(my: &MySqlPool, data: &mut Data) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(NAME AS CHAR), CAST(TABLE_NAME AS CHAR)
         FROM b_hlblock_entity ORDER BY ID",
    )
    .fetch_all(my)
    .await?;
    let mut next_element_id: i64 =
        sqlx::query_scalar("SELECT CAST(IFNULL(MAX(ID), 0) AS SIGNED) FROM b_iblock_element")
            .fetch_one(my)
            .await?;
    let mut next_property_id: i64 =
        sqlx::query_scalar("SELECT CAST(IFNULL(MAX(ID), 0) AS SIGNED) FROM b_iblock_property")
            .fetch_one(my)
            .await?;

    for row in rows {
        let hl_id = int_col(&row, 0).context("b_hlblock_entity.ID")?;
        let (name, table) = (str_col(&row, 1), str_col(&row, 2));
        let iblock_id = HL_IBLOCK_ID_BASE + hl_id;
        data.iblocks.push(IblockRow {
            id: iblock_id,
            api_enabled: false,
            code: clean_code(table.trim_start_matches("b_"), format!("hl_{hl_id}")),
            name: format!("{name} (HL)"),
            description: format!("HL-блок {hl_id}, таблица {table}"),
            sort: 1000 + hl_id as i32,
            detail_page_url: String::new(),
            section_page_url: String::new(),
            list_page_url: String::new(),
            is_catalog: false,
        });

        let fields = sqlx::query(
            "SELECT CAST(FIELD_NAME AS CHAR), CAST(USER_TYPE_ID AS CHAR), CAST(MULTIPLE AS CHAR),
                    CAST(SORT AS SIGNED)
             FROM b_user_field WHERE ENTITY_ID = ? ORDER BY SORT, ID",
        )
        .bind(format!("HLBLOCK_{hl_id}"))
        .fetch_all(my)
        .await?;
        let fields: Vec<(String, String, bool, i32)> = fields
            .iter()
            .map(|f| {
                let sort = int_col(f, 3).unwrap_or(500) as i32;
                (str_col(f, 0), str_col(f, 1), str_col(f, 2) == "Y", sort)
            })
            .filter(|(name, ..)| is_safe_column(name))
            .collect();
        let name_field = ["UF_NAME", "UF_MARKING", "UF_FAMILY"]
            .into_iter()
            .find(|n| fields.iter().any(|(f, ..)| f == n))
            .map(str::to_string);

        // Поля кроме названия, внешнего кода и сортировки становятся свойствами
        let mut prop_ids = HashMap::new();
        for (field, user_type, multiple, sort) in &fields {
            // Поле-название остаётся и свойством: справочники читаются единообразно через uf_*
            if matches!(field.as_str(), "UF_XML_ID" | "UF_SORT") {
                continue;
            }
            next_property_id += 1;
            let kind = match user_type.as_str() {
                "integer" | "double" => "number",
                "boolean" => "boolean",
                "file" => "file",
                "date" => "date",
                _ => "string",
            };
            data.properties.push(PropertyRow {
                id: next_property_id,
                iblock_id,
                code: field.to_ascii_lowercase(),
                name: field.clone(),
                kind,
                multiple: *multiple && kind != "boolean",
                is_required: false,
                sort: *sort,
                link_iblock_id: None,
                directory: false,
                user_type: String::new(),
            });
            prop_ids.insert(field.clone(), next_property_id);
        }

        let columns: Vec<String> = fields
            .iter()
            .map(|(f, ..)| format!("CAST(`{f}` AS CHAR)"))
            .collect();
        let sql = format!(
            "SELECT CAST(ID AS SIGNED){}{} FROM `{table}` ORDER BY ID",
            if columns.is_empty() { "" } else { ", " },
            columns.join(", ")
        );
        if !is_safe_column(&table) {
            bail!("подозрительное имя таблицы HL-блока: {table}");
        }
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(my).await?;
        for row in rows {
            let hl_row_id = int_col(&row, 0).unwrap_or_default();
            let value = |field: &str| {
                fields
                    .iter()
                    .position(|(f, ..)| f == field)
                    .map(|i| str_col(&row, i + 1))
                    .unwrap_or_default()
            };
            next_element_id += 1;
            let mut raw = HashMap::new();
            for (field, prop_id) in &prop_ids {
                let v = value(field);
                if !v.is_empty() {
                    raw.insert(*prop_id, vec![v]);
                }
            }
            let name = name_field
                .as_deref()
                .map(value)
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| format!("#{hl_row_id}"));
            let sort = value("UF_SORT").parse().unwrap_or(500);
            let now = Utc::now();
            data.elements.push(ElementRow {
                id: next_element_id,
                iblock_id,
                section_id: None,
                code: String::new(),
                xml_id: value("UF_XML_ID"),
                name,
                active: true,
                sort,
                preview_text: String::new(),
                detail_text: String::new(),
                preview_picture_id: None,
                detail_picture_id: None,
                published_at: None,
                created_by: None,
                raw,
                created_at: now,
                updated_at: now,
            });
        }
    }
    Ok(())
}

/// Имя колонки/таблицы можно подставлять в SQL как есть.
fn is_safe_column(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

async fn read_sections(my: &MySqlPool, data: &mut Data) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(IBLOCK_ID AS SIGNED), CAST(IFNULL(IBLOCK_SECTION_ID, 0) AS SIGNED),
                CAST(IFNULL(CODE, '') AS CHAR), CAST(IFNULL(XML_ID, '') AS CHAR), CAST(NAME AS CHAR),
                CAST(ACTIVE AS CHAR), CAST(SORT AS SIGNED), CAST(DEPTH_LEVEL AS SIGNED),
                CAST(IFNULL(DESCRIPTION, '') AS CHAR), CAST(IFNULL(PICTURE, 0) AS SIGNED),
                CAST(DATE_CREATE AS CHAR), CAST(TIMESTAMP_X AS CHAR)
         FROM b_iblock_section ORDER BY DEPTH_LEVEL, ID",
    )
    .fetch_all(my)
    .await?;
    for row in rows {
        let id = int_col(&row, 0).context("b_iblock_section.ID")?;
        let code = str_col(&row, 3);
        let updated_at = bitrix_date(&str_col(&row, 12)).unwrap_or_else(Utc::now);
        data.sections.push(SectionRow {
            id,
            iblock_id: int_col(&row, 1).unwrap_or_default(),
            parent_id: id_col(&row, 2),
            code: if code.is_empty() || is_valid_slug(&code) {
                code
            } else {
                slugify(&code)
            },
            xml_id: str_col(&row, 4),
            name: str_col(&row, 5),
            active: str_col(&row, 6) == "Y",
            sort: int_col(&row, 7).unwrap_or(500) as i32,
            depth_level: int_col(&row, 8).unwrap_or(1) as i32,
            description: str_col(&row, 9),
            picture_id: id_col(&row, 10),
            created_at: bitrix_date(&str_col(&row, 11)).unwrap_or(updated_at),
            updated_at,
        });
    }
    Ok(())
}

async fn read_elements(my: &MySqlPool, data: &mut Data) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(IBLOCK_ID AS SIGNED), CAST(IFNULL(IBLOCK_SECTION_ID, 0) AS SIGNED),
                CAST(IFNULL(CODE, '') AS CHAR), CAST(IFNULL(XML_ID, '') AS CHAR), CAST(NAME AS CHAR),
                CAST(ACTIVE AS CHAR), CAST(SORT AS SIGNED),
                CAST(IFNULL(PREVIEW_TEXT, '') AS CHAR), CAST(IFNULL(DETAIL_TEXT, '') AS CHAR),
                CAST(IFNULL(PREVIEW_PICTURE, 0) AS SIGNED), CAST(IFNULL(DETAIL_PICTURE, 0) AS SIGNED),
                CAST(ACTIVE_FROM AS CHAR), CAST(DATE_CREATE AS CHAR), CAST(TIMESTAMP_X AS CHAR),
                CAST(IFNULL(CREATED_BY, 0) AS SIGNED)
         FROM b_iblock_element
         WHERE WF_PARENT_ELEMENT_ID IS NULL
         ORDER BY IBLOCK_ID, ID",
    )
    .fetch_all(my)
    .await?;

    let mut used: HashSet<(i64, String)> = HashSet::new();
    for row in rows {
        let id = int_col(&row, 0).context("b_iblock_element.ID")?;
        let iblock_id = int_col(&row, 1).unwrap_or_default();
        let mut code = str_col(&row, 3);
        if !code.is_empty() && !is_valid_slug(&code) {
            code = slugify(&code);
        }
        // Коды в Битриксе не обязаны быть уникальными, у нас непустой — уникален
        if !code.is_empty() && !used.insert((iblock_id, code.clone())) {
            code = format!("{code}-{id}");
            used.insert((iblock_id, code.clone()));
        }
        let updated_at = bitrix_date(&str_col(&row, 14)).unwrap_or_else(Utc::now);
        data.elements.push(ElementRow {
            id,
            iblock_id,
            section_id: id_col(&row, 2),
            code,
            xml_id: str_col(&row, 4),
            name: str_col(&row, 5),
            active: str_col(&row, 6) == "Y",
            sort: int_col(&row, 7).unwrap_or(500) as i32,
            preview_text: str_col(&row, 8),
            detail_text: str_col(&row, 9),
            preview_picture_id: id_col(&row, 10),
            detail_picture_id: id_col(&row, 11),
            published_at: bitrix_date(&str_col(&row, 12)),
            created_by: id_col(&row, 15),
            raw: HashMap::new(),
            created_at: bitrix_date(&str_col(&row, 13)).unwrap_or(updated_at),
            updated_at,
        });
    }
    Ok(())
}

/// Значения свойств: инфоблоки v1 — общая таблица `b_iblock_element_property`,
/// v2 — `b_iblock_element_prop_s<ID>` (колонки одиночных свойств) и `_m<ID>` (множественные).
async fn read_property_values(my: &MySqlPool, data: &mut Data) -> anyhow::Result<()> {
    let index: HashMap<i64, usize> = data
        .elements
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id, i))
        .collect();
    let mut push = |element_id: i64, property_id: i64, value: String| {
        if value.is_empty() {
            return;
        }
        if let Some(&i) = index.get(&element_id) {
            data.elements[i]
                .raw
                .entry(property_id)
                .or_default()
                .push(value);
        }
    };

    let v1 = sqlx::query(
        "SELECT CAST(IBLOCK_ELEMENT_ID AS SIGNED), CAST(IBLOCK_PROPERTY_ID AS SIGNED), CAST(VALUE AS CHAR)
         FROM b_iblock_element_property ORDER BY ID",
    )
    .fetch_all(my)
    .await?;
    for row in v1 {
        push(
            int_col(&row, 0).unwrap_or_default(),
            int_col(&row, 1).unwrap_or_default(),
            str_col(&row, 2),
        );
    }

    let v2_iblocks: Vec<i64> =
        sqlx::query_scalar("SELECT CAST(ID AS SIGNED) FROM b_iblock WHERE VERSION = 2 ORDER BY ID")
            .fetch_all(my)
            .await?;
    for iblock_id in v2_iblocks {
        // В s-таблице у множественных свойств лежит сериализованный кеш — берём только одиночные
        let singles: Vec<i64> = data
            .properties
            .iter()
            .filter(|p| p.iblock_id == iblock_id && !p.multiple)
            .map(|p| p.id)
            .collect();
        if !singles.is_empty() {
            let columns: Vec<String> = singles
                .iter()
                .map(|id| format!("CAST(PROPERTY_{id} AS CHAR)"))
                .collect();
            let sql = format!(
                "SELECT CAST(IBLOCK_ELEMENT_ID AS SIGNED), {} FROM b_iblock_element_prop_s{iblock_id}",
                columns.join(", ")
            );
            let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(my).await?;
            for row in rows {
                let element_id = int_col(&row, 0).unwrap_or_default();
                for (i, prop_id) in singles.iter().enumerate() {
                    push(element_id, *prop_id, str_col(&row, i + 1));
                }
            }
        }
        let sql = format!(
            "SELECT CAST(IBLOCK_ELEMENT_ID AS SIGNED), CAST(IBLOCK_PROPERTY_ID AS SIGNED), CAST(VALUE AS CHAR)
             FROM b_iblock_element_prop_m{iblock_id} ORDER BY ID"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(my).await?;
        for row in rows {
            push(
                int_col(&row, 0).unwrap_or_default(),
                int_col(&row, 1).unwrap_or_default(),
                str_col(&row, 2),
            );
        }
    }
    Ok(())
}

/// Записи `b_file`, на которые ссылаются картинки и файловые свойства.
async fn read_files(my: &MySqlPool, data: &mut Data) -> anyhow::Result<()> {
    let file_props: HashSet<i64> = data
        .properties
        .iter()
        .filter(|p| p.kind == "file")
        .map(|p| p.id)
        .collect();
    let mut ids: HashSet<i64> = HashSet::new();
    for s in &data.sections {
        ids.extend(s.picture_id);
    }
    for e in &data.elements {
        ids.extend(e.preview_picture_id);
        ids.extend(e.detail_picture_id);
        for (prop_id, values) in &e.raw {
            if file_props.contains(prop_id) {
                ids.extend(values.iter().filter_map(|v| v.parse::<i64>().ok()));
            }
        }
    }
    let ids: Vec<i64> = ids.into_iter().collect();
    for chunk in ids.chunks(BATCH) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT CAST(ID AS SIGNED), CAST(IFNULL(SUBDIR, '') AS CHAR), CAST(FILE_NAME AS CHAR),
                    CAST(IFNULL(ORIGINAL_NAME, '') AS CHAR), CAST(IFNULL(CONTENT_TYPE, '') AS CHAR),
                    CAST(IFNULL(FILE_SIZE, 0) AS SIGNED), CAST(IFNULL(WIDTH, 0) AS SIGNED),
                    CAST(IFNULL(HEIGHT, 0) AS SIGNED), CAST(TIMESTAMP_X AS CHAR)
             FROM b_file WHERE ID IN ({placeholders})"
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for id in chunk {
            query = query.bind(id);
        }
        for row in query.fetch_all(my).await? {
            let id = int_col(&row, 0).context("b_file.ID")?;
            let source = format!("{}/{}", str_col(&row, 1), str_col(&row, 2));
            let source = source.trim_start_matches('/').to_string();
            // Путь как в upload Битрикса (<SUBDIR>/<FILE_NAME>) — фронт строит ссылки
            // так же; «..» и абсолютные пути не пускаем
            if source.split('/').any(|p| p == ".." || p.is_empty()) {
                tracing::warn!(id, source, "пропущен файл с подозрительным путём");
                continue;
            }
            let dims = |i| int_col(&row, i).filter(|v| *v > 0).map(|v| v as i32);
            data.files.push(FileRow {
                id,
                path: source.clone(),
                original_name: str_col(&row, 3),
                content_type: str_col(&row, 4),
                size: int_col(&row, 5).unwrap_or_default(),
                width: dims(6),
                height: dims(7),
                created_at: bitrix_date(&str_col(&row, 8)).unwrap_or_else(Utc::now),
            });
            data.file_sources.insert(id, source);
        }
    }
    Ok(())
}

/// Пользователи сайта: логин, почта, имя и хеш пароля (входят со своими паролями).
async fn read_users(my: &MySqlPool, data: &mut Data) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(LOGIN AS CHAR), CAST(IFNULL(EMAIL, '') AS CHAR),
                CAST(IFNULL(NAME, '') AS CHAR), CAST(IFNULL(LAST_NAME, '') AS CHAR),
                CAST(IFNULL(PASSWORD, '') AS CHAR), CAST(ACTIVE AS CHAR)
         FROM b_user ORDER BY ID",
    )
    .fetch_all(my)
    .await?;
    for row in rows {
        let login = str_col(&row, 1).trim().to_string();
        let hash = str_col(&row, 5);
        if login.is_empty() || hash.is_empty() {
            continue;
        }
        data.users.push(UserRow {
            bitrix_id: int_col(&row, 0).context("b_user.ID")?,
            login,
            email: str_col(&row, 2).trim().to_string(),
            name: str_col(&row, 3).trim().to_string(),
            last_name: str_col(&row, 4).trim().to_string(),
            password_hash: crate::passwords::from_bitrix(&hash),
            active: str_col(&row, 6) == "Y",
        });
    }
    Ok(())
}

/// Пользователи: обновляются по `external_id` (не удаляются — у них могут быть сессии).
/// Логин, занятый своим пользователем CMS, пропускается. Возвращает id Битрикса → наш id.
async fn write_users(
    tx: &mut sqlx::PgConnection,
    users: &[UserRow],
) -> anyhow::Result<HashMap<i64, i64>> {
    let taken: HashMap<String, Option<String>> =
        sqlx::query_as::<_, (String, Option<String>)>("SELECT login, external_id FROM users")
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect();
    let mut skipped = Vec::new();
    let users: Vec<&UserRow> = users
        .iter()
        .filter(|u| match taken.get(&u.login) {
            Some(ext) if ext.as_deref() != Some(&format!("bitrix:{}", u.bitrix_id)) => {
                skipped.push(u.login.clone());
                false
            }
            _ => true,
        })
        .collect();
    if !skipped.is_empty() {
        tracing::warn!(logins = ?skipped, "пользователи Битрикса пропущены: логин уже занят в CMS");
    }
    let mut map = HashMap::new();
    for chunk in users.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO users (login, email, name, last_name, password_hash, is_admin, active, external_id) ",
        );
        qb.push_values(chunk, |mut b, u| {
            b.push_bind(&u.login)
                .push_bind((!u.email.is_empty()).then_some(&u.email))
                .push_bind(&u.name)
                .push_bind(&u.last_name)
                .push_bind(&u.password_hash)
                .push_bind(false)
                .push_bind(u.active)
                .push_bind(format!("bitrix:{}", u.bitrix_id));
        });
        // Уже вошедший через CMS хранит argon2 — его не затираем старым хешем
        qb.push(
            " ON CONFLICT (external_id) WHERE external_id IS NOT NULL DO UPDATE SET
                login = EXCLUDED.login, email = EXCLUDED.email, name = EXCLUDED.name,
                last_name = EXCLUDED.last_name, active = EXCLUDED.active,
                password_hash = CASE WHEN users.password_hash LIKE '$argon2%'
                                     THEN users.password_hash ELSE EXCLUDED.password_hash END
              RETURNING id, external_id",
        );
        let rows: Vec<(i64, String)> = qb.build_query_as().fetch_all(&mut *tx).await?;
        for (id, ext) in rows {
            if let Some(bitrix_id) = ext.strip_prefix("bitrix:").and_then(|v| v.parse().ok()) {
                map.insert(bitrix_id, id);
            }
        }
    }
    Ok(map)
}

/// Цены, склады и остатки модуля `catalog`, форматы валют.
async fn read_catalog(my: &MySqlPool, data: &mut Data) -> anyhow::Result<()> {
    let elements: HashSet<i64> = data.elements.iter().map(|e| e.id).collect();
    let c = &mut data.catalog;

    for row in sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(NAME AS CHAR), CAST(BASE AS CHAR) FROM b_catalog_group ORDER BY ID",
    )
    .fetch_all(my)
    .await?
    {
        let id = int_col(&row, 0).context("b_catalog_group.ID")?;
        let code = str_col(&row, 1);
        // Название типа цены — из языковой таблицы, если есть
        let name: Option<String> = sqlx::query_scalar(
            "SELECT CAST(NAME AS CHAR) FROM b_catalog_group_lang WHERE CATALOG_GROUP_ID = ? AND LANG = 'ru'",
        )
        .bind(id)
        .fetch_optional(my)
        .await?
        .flatten();
        let name = name.filter(|n| !n.is_empty()).unwrap_or_else(|| code.clone());
        c.price_types.push((id, code, name, str_col(&row, 2) == "Y"));
    }

    for row in sqlx::query(
        "SELECT CAST(PRODUCT_ID AS SIGNED), CAST(CATALOG_GROUP_ID AS SIGNED), CAST(PRICE AS CHAR),
                CAST(CURRENCY AS CHAR), CAST(QUANTITY_FROM AS SIGNED), CAST(QUANTITY_TO AS SIGNED)
         FROM b_catalog_price ORDER BY ID",
    )
    .fetch_all(my)
    .await?
    {
        let element = int_col(&row, 0).unwrap_or_default();
        let Ok(price) = str_col(&row, 2).parse::<f64>() else {
            continue;
        };
        if elements.contains(&element) {
            let qty = |i| int_col(&row, i).map(|v| v as i32);
            c.prices.push((
                element,
                int_col(&row, 1).unwrap_or_default(),
                price,
                str_col(&row, 3),
                qty(4),
                qty(5),
            ));
        }
    }

    for row in sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(TITLE AS CHAR), CAST(ACTIVE AS CHAR), CAST(SORT AS SIGNED)
         FROM b_catalog_store ORDER BY ID",
    )
    .fetch_all(my)
    .await?
    {
        c.stores.push((
            int_col(&row, 0).context("b_catalog_store.ID")?,
            str_col(&row, 1),
            str_col(&row, 2) == "Y",
            int_col(&row, 3).unwrap_or(500) as i32,
        ));
    }

    for row in sqlx::query(
        "SELECT CAST(PRODUCT_ID AS SIGNED), CAST(STORE_ID AS SIGNED), CAST(AMOUNT AS CHAR)
         FROM b_catalog_store_product",
    )
    .fetch_all(my)
    .await?
    {
        let element = int_col(&row, 0).unwrap_or_default();
        if elements.contains(&element) {
            let amount = str_col(&row, 2).parse().unwrap_or(0.0);
            c.amounts
                .push((element, int_col(&row, 1).unwrap_or_default(), amount));
        }
    }

    for row in sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(IFNULL(QUANTITY, 0) AS CHAR) FROM b_catalog_product",
    )
    .fetch_all(my)
    .await?
    {
        let element = int_col(&row, 0).unwrap_or_default();
        if elements.contains(&element) {
            c.products
                .push((element, str_col(&row, 1).parse().unwrap_or(0.0)));
        }
    }

    for row in sqlx::query(
        "SELECT CAST(CURRENCY AS CHAR), CAST(IFNULL(FORMAT_STRING, '#') AS CHAR),
                CAST(IFNULL(DEC_POINT, '.') AS CHAR), CAST(IFNULL(THOUSANDS_SEP, '') AS CHAR),
                CAST(IFNULL(THOUSANDS_VARIANT, '') AS CHAR), CAST(IFNULL(DECIMALS, 2) AS SIGNED),
                CAST(IFNULL(HIDE_ZERO, 'Y') AS CHAR)
         FROM b_catalog_currency_lang WHERE LID = 'ru'",
    )
    .fetch_all(my)
    .await?
    {
        // THOUSANDS_VARIANT: N — нет, D — точка, C — запятая, S — пробел, B — неразрывный пробел
        let thousands = match str_col(&row, 4).as_str() {
            "N" => String::new(),
            "D" => ".".into(),
            "C" => ",".into(),
            "S" => " ".into(),
            "B" => "&nbsp;".into(),
            _ => str_col(&row, 3),
        };
        c.currencies.push((
            str_col(&row, 0),
            str_col(&row, 1),
            str_col(&row, 2),
            thousands,
            int_col(&row, 5).unwrap_or(2) as i32,
            str_col(&row, 6) == "Y",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Преобразование значений
// ---------------------------------------------------------------------------

/// Значения свойств в формате CMS: элемент → код → JSON.
type Props = HashMap<i64, Map<String, Value>>;

fn resolve_properties(data: &mut Data) -> Props {
    let file_ids: HashSet<i64> = data.files.iter().map(|f| f.id).collect();
    let element_ids: HashSet<i64> = data.elements.iter().map(|e| e.id).collect();
    let section_ids: HashSet<i64> = data.sections.iter().map(|s| s.id).collect();
    let enum_ids: HashSet<i64> = data.enums.iter().map(|e| e.id).collect();
    // Справочники: (id инфоблока, внешний код) → id элемента
    let by_xml_id: HashMap<(i64, &str), i64> = data
        .elements
        .iter()
        .filter(|e| !e.xml_id.is_empty())
        .map(|e| ((e.iblock_id, e.xml_id.as_str()), e.id))
        .collect();

    let mut out = Props::new();
    for element in &data.elements {
        let mut values = Map::new();
        for prop in data
            .properties
            .iter()
            .filter(|p| p.iblock_id == element.iblock_id)
        {
            let raws = element.raw.get(&prop.id).map(Vec::as_slice).unwrap_or(&[]);
            let items: Vec<Value> = raws
                .iter()
                .filter_map(|raw| {
                    let raw = raw.trim();
                    let id = || raw.parse::<i64>().ok();
                    match prop.kind {
                        "number" => number(raw),
                        "boolean" => Some(Value::Bool(matches!(raw, "1" | "Y" | "true"))),
                        "list" => id().filter(|i| enum_ids.contains(i)).map(Value::from),
                        "file" => id().filter(|i| file_ids.contains(i)).map(Value::from),
                        "element" if prop.directory => prop
                            .link_iblock_id
                            .and_then(|ib| by_xml_id.get(&(ib, raw)))
                            .map(|i| Value::from(*i)),
                        "element" => id().filter(|i| element_ids.contains(i)).map(Value::from),
                        "date" => raw.get(..10).map(|d| Value::String(d.to_string())),
                        _ => Some(Value::String(raw.to_string())),
                    }
                })
                .collect();
            let value = if prop.multiple {
                Value::Array(items)
            } else if prop.kind == "boolean" {
                items.into_iter().next().unwrap_or(Value::Bool(false))
            } else {
                items.into_iter().next().unwrap_or(Value::Null)
            };
            values.insert(prop.code.clone(), value);
        }
        out.insert(element.id, values);
    }

    // Ссылки на то, чего нет: убираем, чтобы не упереться во внешние ключи
    for e in &mut data.elements {
        e.section_id = e.section_id.filter(|id| section_ids.contains(id));
        e.preview_picture_id = e.preview_picture_id.filter(|id| file_ids.contains(id));
        e.detail_picture_id = e.detail_picture_id.filter(|id| file_ids.contains(id));
    }
    for s in &mut data.sections {
        s.parent_id = s.parent_id.filter(|id| section_ids.contains(id));
        s.picture_id = s.picture_id.filter(|id| file_ids.contains(id));
    }
    let iblock_ids: HashSet<i64> = data.iblocks.iter().map(|i| i.id).collect();
    for p in &mut data.properties {
        p.link_iblock_id = p.link_iblock_id.filter(|id| iblock_ids.contains(id));
    }
    out
}

/// Число из строки Битрикса (`12.0000` → 12).
fn number(raw: &str) -> Option<Value> {
    let f: f64 = raw
        .replace(',', ".")
        .parse()
        .ok()
        .filter(|f: &f64| f.is_finite())?;
    if f.fract() == 0.0 && f.abs() < 9e15 {
        Some(Value::from(f as i64))
    } else {
        Some(Value::from(f))
    }
}

/// Жёсткие ссылки (или копии) файлов Битрикса в наше хранилище.
/// Возвращает (перенесено, нет в исходном каталоге).
fn link_files(data: &Data, opts: &Options) -> anyhow::Result<(usize, usize)> {
    let Some(source_root) = &opts.bitrix_upload else {
        return Ok((0, data.files.len()));
    };
    let (mut linked, mut missing) = (0, 0);
    for file in &data.files {
        let source = source_root.join(&data.file_sources[&file.id]);
        let target = opts.upload_dir.join(&file.path);
        if target.exists() {
            linked += 1;
            continue;
        }
        if !source.is_file() {
            missing += 1;
            continue;
        }
        std::fs::create_dir_all(target.parent().expect("путь с каталогом"))?;
        if std::fs::hard_link(&source, &target).is_err() {
            std::fs::copy(&source, &target)
                .with_context(|| format!("копирование {}", source.display()))?;
        }
        linked += 1;
    }
    Ok((linked, missing))
}

// ---------------------------------------------------------------------------
// Запись в PostgreSQL
// ---------------------------------------------------------------------------

async fn write_all(tx: &mut sqlx::PgConnection, data: &Data, props: &Props) -> anyhow::Result<()> {
    for chunk in data.files.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO files
                (id, path, original_name, content_type, size, width, height, created_at, source) ",
        );
        qb.push_values(chunk, |mut b, f| {
            b.push_bind(f.id)
                .push_bind(&f.path)
                .push_bind(&f.original_name)
                .push_bind(&f.content_type)
                .push_bind(f.size)
                .push_bind(f.width)
                .push_bind(f.height)
                .push_bind(f.created_at)
                .push_bind("bitrix");
        });
        // Файл мог остаться от прошлого переноса без --replace
        qb.push(" ON CONFLICT (id) DO NOTHING");
        qb.build().execute(&mut *tx).await?;
    }

    for chunk in data.iblocks.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO iblocks (id, code, name, description, api_enabled, sort, detail_page_url,
                                  section_page_url, list_page_url, is_catalog) ",
        );
        qb.push_values(chunk, |mut b, i| {
            b.push_bind(i.id)
                .push_bind(&i.code)
                .push_bind(&i.name)
                .push_bind(&i.description)
                .push_bind(i.api_enabled)
                .push_bind(i.sort)
                .push_bind(&i.detail_page_url)
                .push_bind(&i.section_page_url)
                .push_bind(&i.list_page_url)
                .push_bind(i.is_catalog);
        });
        qb.build().execute(&mut *tx).await?;
    }

    for chunk in data.properties.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO iblock_properties
                (id, iblock_id, code, name, kind, is_required, sort, multiple, link_iblock_id,
                 user_type) ",
        );
        qb.push_values(chunk, |mut b, p| {
            b.push_bind(p.id)
                .push_bind(p.iblock_id)
                .push_bind(&p.code)
                .push_bind(&p.name)
                .push_bind(p.kind)
                .push_bind(p.is_required)
                .push_bind(p.sort)
                .push_bind(p.multiple)
                .push_bind(p.link_iblock_id)
                .push_bind(&p.user_type);
        });
        qb.build().execute(&mut *tx).await?;
    }

    for chunk in data.enums.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO iblock_property_enums (id, property_id, value, xml_id, sort, is_default) ",
        );
        qb.push_values(chunk, |mut b, e| {
            b.push_bind(e.id)
                .push_bind(e.property_id)
                .push_bind(&e.value)
                .push_bind(&e.xml_id)
                .push_bind(e.sort)
                .push_bind(e.is_default);
        });
        qb.build().execute(&mut *tx).await?;
    }

    // Разделы отсортированы по глубине — родитель всегда вставлен раньше потомка
    for chunk in data.sections.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO iblock_sections
                (id, iblock_id, parent_id, code, xml_id, name, active, sort, depth_level,
                 description, picture_id, created_at, updated_at) ",
        );
        qb.push_values(chunk, |mut b, s| {
            b.push_bind(s.id)
                .push_bind(s.iblock_id)
                .push_bind(s.parent_id)
                .push_bind(&s.code)
                .push_bind(&s.xml_id)
                .push_bind(&s.name)
                .push_bind(s.active)
                .push_bind(s.sort)
                .push_bind(s.depth_level)
                .push_bind(&s.description)
                .push_bind(s.picture_id)
                .push_bind(s.created_at)
                .push_bind(s.updated_at);
        });
        qb.build().execute(&mut *tx).await?;
    }

    let users = write_users(&mut *tx, &data.users).await?;
    let empty = Map::new();
    for chunk in data.elements.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO iblock_elements
                (id, iblock_id, section_id, code, xml_id, name, active, sort, preview_text,
                 detail_text, preview_picture_id, detail_picture_id, published_at, properties,
                 created_at, updated_at, created_by) ",
        );
        qb.push_values(chunk, |mut b, e| {
            b.push_bind(e.id)
                .push_bind(e.iblock_id)
                .push_bind(e.section_id)
                .push_bind(&e.code)
                .push_bind(&e.xml_id)
                .push_bind(&e.name)
                .push_bind(e.active)
                .push_bind(e.sort)
                .push_bind(&e.preview_text)
                .push_bind(&e.detail_text)
                .push_bind(e.preview_picture_id)
                .push_bind(e.detail_picture_id)
                .push_bind(e.published_at)
                .push_bind(sqlx::types::Json(props.get(&e.id).unwrap_or(&empty)))
                .push_bind(e.created_at)
                .push_bind(e.updated_at)
                .push_bind(e.created_by.and_then(|id| users.get(&id).copied()));
        });
        qb.build().execute(&mut *tx).await?;
    }

    write_catalog(&mut *tx, &data.catalog).await?;

    // Последовательности — после максимальных перенесённых id
    for (table, seq) in [
        ("files", "files_id_seq"),
        ("iblocks", "iblocks_id_seq"),
        ("iblock_properties", "iblock_properties_id_seq"),
        ("iblock_property_enums", "iblock_property_enums_id_seq"),
        ("iblock_sections", "iblock_sections_id_seq"),
        ("iblock_elements", "iblock_elements_id_seq"),
        ("catalog_price_types", "catalog_price_types_id_seq"),
        ("catalog_prices", "catalog_prices_id_seq"),
        ("catalog_stores", "catalog_stores_id_seq"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT setval('{seq}', GREATEST((SELECT MAX(id) FROM {table}), 1))"
        )))
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Справочники каталога (типы цен, склады, валюты) принадлежат переносу целиком
/// и перезаписываются; цены и остатки ушли вместе с элементами (каскад).
async fn write_catalog(tx: &mut sqlx::PgConnection, c: &Catalog) -> anyhow::Result<()> {
    for table in ["catalog_price_types", "catalog_stores", "currencies"] {
        sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
            .execute(&mut *tx)
            .await?;
    }
    for chunk in c.price_types.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO catalog_price_types (id, code, name, is_base) ",
        );
        qb.push_values(chunk, |mut b, (id, code, name, base)| {
            b.push_bind(id)
                .push_bind(code)
                .push_bind(name)
                .push_bind(base);
        });
        qb.build().execute(&mut *tx).await?;
    }
    for chunk in c.prices.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO catalog_prices
                (element_id, price_type_id, price, currency, quantity_from, quantity_to) ",
        );
        qb.push_values(chunk, |mut b, (el, pt, price, cur, from, to)| {
            b.push_bind(el)
                .push_bind(pt)
                .push_bind(price)
                .push_bind(cur)
                .push_bind(from)
                .push_bind(to);
        });
        qb.build().execute(&mut *tx).await?;
    }
    for chunk in c.stores.chunks(BATCH) {
        let mut qb =
            QueryBuilder::<Postgres>::new("INSERT INTO catalog_stores (id, name, active, sort) ");
        qb.push_values(chunk, |mut b, (id, name, active, sort)| {
            b.push_bind(id)
                .push_bind(name)
                .push_bind(active)
                .push_bind(sort);
        });
        qb.build().execute(&mut *tx).await?;
    }
    for chunk in c.amounts.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO catalog_store_amounts (element_id, store_id, amount) ",
        );
        qb.push_values(chunk, |mut b, (el, store, amount)| {
            b.push_bind(el).push_bind(store).push_bind(amount);
        });
        qb.push(" ON CONFLICT DO NOTHING");
        qb.build().execute(&mut *tx).await?;
    }
    for chunk in c.products.chunks(BATCH) {
        let mut qb =
            QueryBuilder::<Postgres>::new("INSERT INTO catalog_products (element_id, quantity) ");
        qb.push_values(chunk, |mut b, (el, qty)| {
            b.push_bind(el).push_bind(qty);
        });
        qb.build().execute(&mut *tx).await?;
    }
    for chunk in c.currencies.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO currencies (code, format_string, dec_point, thousands_sep, decimals, hide_zero) ",
        );
        qb.push_values(chunk, |mut b, (code, fmt, dec, th, decimals, hide)| {
            b.push_bind(code)
                .push_bind(fmt)
                .push_bind(dec)
                .push_bind(th)
                .push_bind(decimals)
                .push_bind(hide);
        });
        qb.build().execute(&mut *tx).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_and_codes() {
        assert_eq!(to_snake("homeTabs"), "home_tabs");
        assert_eq!(to_snake("RELATED_BRANDS"), "related_brands");
        assert_eq!(to_snake("CML2_ARTICLE"), "cml2_article");
        assert_eq!(clean_code("", "iblock_5".into()), "iblock_5");
        assert_eq!(clean_code("Инфо", "x".into()), "info");
    }

    #[test]
    fn numbers_and_dates() {
        assert_eq!(number("12.0000"), Some(Value::from(12)));
        assert_eq!(number("1.5"), Some(Value::from(1.5)));
        assert_eq!(number("abc"), None);
        let d = bitrix_date("2026-09-01 03:00:00").unwrap();
        assert_eq!(d.to_rfc3339(), "2026-09-01T00:00:00+00:00");
        assert!(bitrix_date("").is_none());
    }
}
