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
    collection_id: i64,
    code: String,
    name: String,
    kind: &'static str,
    multiple: bool,
    is_required: bool,
    sort: i32,
    link_collection_id: Option<i64>,
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
    collection_id: i64,
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
    collection_id: i64,
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
    groups: Vec<GroupRow>,
    locations: Vec<LocationRow>,
    mail_templates: Vec<MailTemplateRow>,
    options: Vec<(String, String, String)>,
    sale: Sale,
}

/// Настройки оформления заказа (модуль `sale`).
#[derive(Default)]
struct Sale {
    /// (код, название, сортировка, описание, уведомлять)
    statuses: Vec<(String, String, i32, String, bool)>,
    /// (id, код, название, активен, сортировка)
    person_types: Vec<(i64, String, String, bool, i32)>,
    /// (id, тип плательщика, название, сортировка, код блока)
    groups: Vec<(i64, i64, String, i32, String)>,
    properties: Vec<SalePropertyRow>,
    /// (id, свойство, значение, название, сортировка)
    variants: Vec<(i64, i64, String, String, i32)>,
    deliveries: Vec<DeliveryRow>,
    pay_systems: Vec<PaySystemRow>,
    /// (свойство, 'P' | 'D', id платёжки или доставки)
    relations: Vec<(i64, String, i64)>,
}

struct SalePropertyRow {
    id: i64,
    person_type_id: i64,
    group_id: Option<i64>,
    code: String,
    name: String,
    kind: &'static str,
    required: bool,
    util: bool,
    /// is_email, is_phone, is_payer, is_profile_name, is_location, is_address, is_zip
    flags: [bool; 7],
    default_value: String,
    description: String,
    sort: i32,
    active: bool,
    multiple: bool,
}

struct DeliveryRow {
    id: i64,
    code: String,
    name: String,
    description: String,
    active: bool,
    public: bool,
    sort: i32,
    price: f64,
    currency: String,
    stores: Vec<i64>,
}

struct PaySystemRow {
    id: i64,
    code: String,
    name: String,
    description: String,
    active: bool,
    sort: i32,
    api_type: &'static str,
    group_ids: Vec<i64>,
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
    second_name: String,
    phone: String,
    city: String,
    work_position: String,
    photo_id: Option<i64>,
    /// UF-поля: `uf_phone_list` → значение (множественное — массив).
    extra: Map<String, Value>,
    /// Группы Битрикса, в которых состоит пользователь.
    groups: Vec<i64>,
}

/// Группа пользователей Битрикса: (id, код, название, описание, сортировка).
type GroupRow = (i64, String, String, String, i32);

/// Местоположение: (id, код, родитель, тип, название, сортировка, глубина).
type LocationRow = (i64, String, Option<i64>, String, String, i32, i32);

/// Товар как в `b_catalog_product`; флаги `None` — «по умолчанию».
struct ProductRow {
    element_id: i64,
    quantity: f64,
    available: bool,
    quantity_trace: Option<bool>,
    can_buy_zero: Option<bool>,
}

/// Склад как в `b_catalog_store`.
struct StoreRow {
    id: i64,
    name: String,
    active: bool,
    sort: i32,
    address: String,
    phone: String,
    email: String,
    schedule: String,
    /// UF-поля склада (`uf_city_id`...).
    extra: Map<String, Value>,
    image_id: Option<i64>,
}

/// Почтовый шаблон `b_event_message`.
struct MailTemplateRow {
    id: i64,
    event_name: String,
    active: bool,
    email_from: String,
    email_to: String,
    bcc: String,
    subject: String,
    body: String,
    body_type: String,
}

/// Настройки ядра, которые нужны CMS (без паролей и ключей из b_option).
const OPTIONS: &[(&str, &str)] = &[
    ("catalog", "default_quantity_trace"),
    ("catalog", "default_can_buy_zero"),
    ("main", "email_from"),
    ("main", "server_name"),
    ("main", "site_name"),
];

/// Цена: (элемент, тип цены, цена, валюта, количество от, до).
type PriceRow = (i64, i64, f64, String, Option<i32>, Option<i32>);

/// Торговый каталог: цены, склады, остатки, форматы валют.
#[derive(Default)]
struct Catalog {
    /// (id, код, название, базовый)
    price_types: Vec<(i64, String, String, bool)>,
    prices: Vec<PriceRow>,
    stores: Vec<StoreRow>,
    /// (элемент, склад, количество)
    amounts: Vec<(i64, i64, f64)>,
    products: Vec<ProductRow>,
    /// (валюта, формат, десятичный разделитель, разделитель тысяч, знаков, скрывать нули)
    currencies: Vec<(String, String, String, String, i32, bool)>,
}

// ---------------------------------------------------------------------------
// Точка входа
// ---------------------------------------------------------------------------

pub async fn run(db: &PgPool, opts: Options) -> anyhow::Result<()> {
    let started = Instant::now();
    let (existing,): (i64,) = sqlx::query_as("SELECT count(*) FROM collections")
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
    stage("пользователи", read_users(&my, &mut data)).await?;
    stage("файлы", read_files(&my, &mut data)).await?;
    stage("торговый каталог", read_catalog(&my, &mut data)).await?;
    stage("местоположения", read_locations(&my, &mut data)).await?;
    stage("почтовые шаблоны и настройки", read_mail(&my, &mut data)).await?;
    stage("настройки заказов", read_sale(&my, &mut data)).await?;

    let props = resolve_properties(&mut data);
    let (linked, missing) = link_files(&data, &opts)?;

    let mut tx = db.begin().await?;
    if opts.replace {
        sqlx::query("DELETE FROM collections")
            .execute(&mut *tx)
            .await?;
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
        groups = data.groups.len(),
        locations = data.locations.len(),
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
        let collection_id = int_col(&row, 1).unwrap_or_default();
        let mut code = clean_code(&str_col(&row, 2), format!("property_{id}"));
        if !used.insert((collection_id, code.clone())) {
            code = format!("{code}_{id}");
            used.insert((collection_id, code.clone()));
        }
        let (ptype, user_type) = (str_col(&row, 4), str_col(&row, 5));
        let mut link_collection_id = id_col(&row, 9);
        let mut directory = false;
        let kind = match (ptype.as_str(), user_type.as_str()) {
            ("S", "directory") => {
                // Привязка к HL-блоку по имени таблицы из настроек свойства
                let settings = str_col(&row, 10);
                link_collection_id = hl_tables
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
            link_collection_id = None;
        }
        data.properties.push(PropertyRow {
            id,
            collection_id,
            code,
            name: str_col(&row, 3),
            kind,
            multiple: str_col(&row, 6) == "Y",
            is_required: str_col(&row, 7) == "Y",
            sort: int_col(&row, 8).unwrap_or(500) as i32,
            link_collection_id,
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
        let collection_id = HL_IBLOCK_ID_BASE + hl_id;
        data.iblocks.push(IblockRow {
            id: collection_id,
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
                collection_id,
                code: field.to_ascii_lowercase(),
                name: field.clone(),
                kind,
                multiple: *multiple && kind != "boolean",
                is_required: false,
                sort: *sort,
                link_collection_id: None,
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
                collection_id,
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
            collection_id: int_col(&row, 1).unwrap_or_default(),
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
        let collection_id = int_col(&row, 1).unwrap_or_default();
        let mut code = str_col(&row, 3);
        if !code.is_empty() && !is_valid_slug(&code) {
            code = slugify(&code);
        }
        // Коды в Битриксе не обязаны быть уникальными, у нас непустой — уникален
        if !code.is_empty() && !used.insert((collection_id, code.clone())) {
            code = format!("{code}-{id}");
            used.insert((collection_id, code.clone()));
        }
        let updated_at = bitrix_date(&str_col(&row, 14)).unwrap_or_else(Utc::now);
        data.elements.push(ElementRow {
            id,
            collection_id,
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
    for collection_id in v2_iblocks {
        // В s-таблице у множественных свойств лежит сериализованный кеш — берём только одиночные
        let singles: Vec<i64> = data
            .properties
            .iter()
            .filter(|p| p.collection_id == collection_id && !p.multiple)
            .map(|p| p.id)
            .collect();
        if !singles.is_empty() {
            let columns: Vec<String> = singles
                .iter()
                .map(|id| format!("CAST(PROPERTY_{id} AS CHAR)"))
                .collect();
            let sql = format!(
                "SELECT CAST(IBLOCK_ELEMENT_ID AS SIGNED), {} FROM b_iblock_element_prop_s{collection_id}",
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
             FROM b_iblock_element_prop_m{collection_id} ORDER BY ID"
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
    for u in &data.users {
        ids.extend(u.photo_id);
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
                CAST(IFNULL(PASSWORD, '') AS CHAR), CAST(ACTIVE AS CHAR),
                CAST(IFNULL(SECOND_NAME, '') AS CHAR), CAST(IFNULL(PERSONAL_PHONE, '') AS CHAR),
                CAST(IFNULL(PERSONAL_CITY, '') AS CHAR), CAST(IFNULL(WORK_POSITION, '') AS CHAR),
                CAST(IFNULL(PERSONAL_PHOTO, 0) AS SIGNED)
         FROM b_user ORDER BY ID",
    )
    .fetch_all(my)
    .await?;
    let uf = read_user_fields(my).await?;
    let mut memberships: HashMap<i64, Vec<i64>> = HashMap::new();
    for row in
        sqlx::query("SELECT CAST(USER_ID AS SIGNED), CAST(GROUP_ID AS SIGNED) FROM b_user_group")
            .fetch_all(my)
            .await?
    {
        if let (Some(user), Some(group)) = (int_col(&row, 0), int_col(&row, 1)) {
            memberships.entry(user).or_default().push(group);
        }
    }
    for row in rows {
        let login = str_col(&row, 1).trim().to_string();
        let hash = str_col(&row, 5);
        if login.is_empty() || hash.is_empty() {
            continue;
        }
        let bitrix_id = int_col(&row, 0).context("b_user.ID")?;
        data.users.push(UserRow {
            bitrix_id,
            login,
            email: str_col(&row, 2).trim().to_string(),
            name: str_col(&row, 3).trim().to_string(),
            last_name: str_col(&row, 4).trim().to_string(),
            password_hash: crate::passwords::from_bitrix(&hash),
            active: str_col(&row, 6) == "Y",
            second_name: str_col(&row, 7).trim().to_string(),
            phone: str_col(&row, 8).trim().to_string(),
            city: str_col(&row, 9).trim().to_string(),
            work_position: str_col(&row, 10).trim().to_string(),
            photo_id: id_col(&row, 11),
            extra: uf.get(&bitrix_id).cloned().unwrap_or_default(),
            groups: memberships.remove(&bitrix_id).unwrap_or_default(),
        });
    }

    for row in sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(IFNULL(STRING_ID, '') AS CHAR), CAST(NAME AS CHAR),
                CAST(IFNULL(DESCRIPTION, '') AS CHAR), CAST(IFNULL(C_SORT, 100) AS SIGNED)
         FROM b_group ORDER BY ID",
    )
    .fetch_all(my)
    .await?
    {
        let id = int_col(&row, 0).context("b_group.ID")?;
        let string_id = str_col(&row, 1);
        let code = match clean_code(&string_id, String::new()) {
            c if !c.is_empty() && !string_id.is_empty() => format!("bitrix_{c}"),
            _ => format!("bitrix_{id}"),
        };
        data.groups.push((
            id,
            code,
            str_col(&row, 2),
            str_col(&row, 3),
            int_col(&row, 4).unwrap_or(100) as i32,
        ));
    }
    Ok(())
}

/// UF-поля пользователей → id пользователя → { uf_код: значение }.
async fn read_user_fields(my: &MySqlPool) -> anyhow::Result<HashMap<i64, Map<String, Value>>> {
    read_uf(my, "USER", "b_uts_user", "b_utm_user").await
}

/// UF-поля сущности `entity` (`b_uts_*`, множественные — `b_utm_*` или сериализованный
/// массив в `b_uts_*`) → id записи → { uf_код: значение }. Нет таблицы — пусто.
async fn read_uf(
    my: &MySqlPool,
    entity: &str,
    uts_table: &str,
    utm_table: &str,
) -> anyhow::Result<HashMap<i64, Map<String, Value>>> {
    let fields: Vec<(i64, String, String, bool)> = sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(FIELD_NAME AS CHAR), CAST(USER_TYPE_ID AS CHAR),
                CAST(MULTIPLE AS CHAR)
         FROM b_user_field WHERE ENTITY_ID = ? ORDER BY SORT, ID",
    )
    .bind(entity)
    .fetch_all(my)
    .await?
    .iter()
    .map(|r| {
        (
            int_col(r, 0).unwrap_or_default(),
            str_col(r, 1),
            str_col(r, 2),
            str_col(r, 3) == "Y",
        )
    })
    .filter(|(_, name, ..)| is_safe_column(name))
    .collect();
    let mut out: HashMap<i64, Map<String, Value>> = HashMap::new();
    if fields.is_empty()
        || !is_safe_column(uts_table)
        || !is_safe_column(utm_table)
        || !mysql_table_exists(my, uts_table).await?
    {
        return Ok(out);
    }
    let convert = |user_type: &str, raw: &str| -> Option<Value> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        Some(match user_type {
            "boolean" => Value::Bool(matches!(raw, "1" | "Y")),
            "integer" | "double" | "file" | "iblock_element" | "iblock_section" => {
                number(raw).unwrap_or_else(|| Value::from(raw))
            }
            _ => Value::from(raw),
        })
    };
    let columns: Vec<String> = fields
        .iter()
        .map(|(_, n, ..)| format!("CAST(`{n}` AS CHAR)"))
        .collect();
    let sql = format!(
        "SELECT CAST(VALUE_ID AS SIGNED), {} FROM {uts_table}",
        columns.join(", ")
    );
    for row in sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(my).await? {
        let user = int_col(&row, 0).unwrap_or_default();
        let map = out.entry(user).or_default();
        for (i, (_, name, user_type, multiple)) in fields.iter().enumerate() {
            let raw = str_col(&row, i + 1);
            let value = if *multiple {
                let items: Vec<Value> = php_unserialize_list(&raw)
                    .iter()
                    .filter_map(|v| convert(user_type, v))
                    .collect();
                (!items.is_empty()).then_some(Value::Array(items))
            } else {
                convert(user_type, &raw)
            };
            if let Some(v) = value {
                map.insert(name.to_ascii_lowercase(), v);
            }
        }
    }
    // Строки множественных полей, если они есть, точнее сериализованного кеша
    let mut multi: HashMap<(i64, String), Vec<Value>> = HashMap::new();
    let utm_rows = if mysql_table_exists(my, utm_table).await? {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT CAST(VALUE_ID AS SIGNED), CAST(FIELD_ID AS SIGNED), CAST(VALUE AS CHAR)
             FROM {utm_table} ORDER BY ID"
        )))
        .fetch_all(my)
        .await?
    } else {
        Vec::new()
    };
    for row in utm_rows {
        let field_id = int_col(&row, 1).unwrap_or_default();
        let Some((_, name, user_type, _)) = fields.iter().find(|(id, ..)| *id == field_id) else {
            continue;
        };
        if let Some(v) = convert(user_type, &str_col(&row, 2)) {
            multi
                .entry((
                    int_col(&row, 0).unwrap_or_default(),
                    name.to_ascii_lowercase(),
                ))
                .or_default()
                .push(v);
        }
    }
    for ((user, name), values) in multi {
        out.entry(user)
            .or_default()
            .insert(name, Value::Array(values));
    }
    Ok(out)
}

/// Значения из PHP-`serialize` массива скаляров: `a:2:{i:0;s:3:"abc";i:1;i:5;}`.
/// Не массив — само значение как один элемент.
fn php_unserialize_list(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    let Some(body) = raw
        .strip_prefix("a:")
        .and_then(|r| r.split_once(":{"))
        .and_then(|(_, b)| b.strip_suffix('}'))
    else {
        return if raw.is_empty() {
            Vec::new()
        } else {
            vec![raw.to_string()]
        };
    };
    let bytes = body.as_bytes();
    let mut items = Vec::new();
    let mut i = 0;
    let mut is_value = false;
    while i < bytes.len() {
        let kind = bytes[i];
        let rest = &body[i..];
        let (value, consumed) = match kind {
            b's' => {
                // s:<длина в байтах>:"...";
                let Some((len, after)) = rest[2..].split_once(':') else {
                    break;
                };
                let Ok(len) = len.parse::<usize>() else { break };
                let start = i + 2 + len.to_string().len() + 2;
                let end = start + len;
                if end > body.len() || !body.is_char_boundary(start) || !body.is_char_boundary(end)
                {
                    break;
                }
                let _ = after;
                (body[start..end].to_string(), end + 2 - i)
            }
            b'i' | b'd' | b'b' => {
                let Some(end) = rest.find(';') else { break };
                (rest[2..end].to_string(), end + 1)
            }
            b'N' => (String::new(), 2),
            _ => break,
        };
        if is_value {
            items.push(value);
        }
        is_value = !is_value;
        i += consumed;
    }
    items
}

/// Разбор PHP-сериализации с вложенными массивами: массив — объект с ключами-строками
/// (порядок сохраняется), скаляры — строки. `None` — строка не разобрана.
fn php_value(raw: &str) -> Option<Value> {
    fn parse(s: &str, i: &mut usize) -> Option<Value> {
        let b = s.as_bytes();
        let kind = *b.get(*i)?;
        match kind {
            b's' => {
                let rest = &s[*i + 2..];
                let (len, _) = rest.split_once(':')?;
                let len: usize = len.parse().ok()?;
                let start = *i + 2 + len.to_string().len() + 2;
                let end = start + len;
                let v = s.get(start..end)?.to_string();
                *i = end + 2;
                Some(Value::String(v))
            }
            b'i' | b'd' | b'b' => {
                let end = *i + s[*i..].find(';')?;
                let v = s[*i + 2..end].to_string();
                *i = end + 1;
                Some(Value::String(v))
            }
            b'N' => {
                *i += 2;
                Some(Value::Null)
            }
            b'a' => {
                let rest = &s[*i + 2..];
                let (count, _) = rest.split_once(':')?;
                let count: usize = count.parse().ok()?;
                *i += 2 + count.to_string().len() + 2;
                let mut map = Map::new();
                for _ in 0..count {
                    let key = match parse(s, i)? {
                        Value::String(k) => k,
                        _ => return None,
                    };
                    let value = parse(s, i)?;
                    map.insert(key, value);
                }
                // закрывающая `}`
                *i += 1;
                Some(Value::Object(map))
            }
            _ => None,
        }
    }
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let mut i = 0;
    parse(raw, &mut i)
}

/// Код блока формы по названию группы свойств — как эвристика модуля bxapi.
fn sale_block_code(name: &str, id: i64) -> String {
    let lower = name.trim().to_lowercase();
    for (needle, code) in [
        ("покупатель", "buyer"),
        ("плательщик", "buyer"),
        ("получатель", "recipient"),
        ("доставка", "delivery"),
        ("оплата", "payment"),
        ("комментарий", "comment"),
        ("адрес", "address"),
    ] {
        if !lower.is_empty() && lower.contains(needle) {
            return code.to_string();
        }
    }
    let slug: String = lower
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .collect();
    let slug: Vec<&str> = slug.split('_').filter(|p| !p.is_empty()).collect();
    if slug.is_empty() {
        format!("group_{id}")
    } else {
        slug.join("_")
    }
}

/// Вид поля по типу свойства заказа Битрикса.
fn sale_prop_kind(bitrix_type: &str, multiline: bool) -> &'static str {
    match bitrix_type {
        "STRING" if multiline => "textarea",
        "NUMBER" => "number",
        "Y/N" => "checkbox",
        "ENUM" | "RADIO" => "select",
        "DATE" => "date",
        "FILE" => "file",
        "LOCATION" => "location",
        "ADDRESS" => "address",
        _ => "text",
    }
}

/// Цена службы доставки из `CONFIG`: `MAIN.PRICE` (настраиваемая), иначе первый тариф
/// `MAIN[0]` (`SimpleHandler`); служба «Без доставки» — 0.
fn delivery_price(class_name: &str, config: &str) -> f64 {
    if class_name.ends_with("EmptyDeliveryService") {
        return 0.0;
    }
    let Some(main) = php_value(config).and_then(|v| v.get("MAIN").cloned()) else {
        return 0.0;
    };
    main.get("PRICE")
        .or_else(|| main.get("0"))
        .and_then(Value::as_str)
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(0.0)
}

/// Числовые id из массива `PARAMS[key]` ограничения или услуги (`STORES`, `GROUPS`).
fn param_ids(params: &str, key: &str) -> Vec<i64> {
    php_value(params)
        .and_then(|v| v.get(key).cloned())
        .and_then(|v| match v {
            Value::Object(m) => Some(
                m.values()
                    .filter_map(|x| x.as_str().and_then(|s| s.trim().parse().ok()))
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

/// Тип платёжки для API по обработчику: счета — `document`, остальное — `other`.
fn pay_api_type(action_file: &str) -> &'static str {
    let name = action_file.rsplit('/').next().unwrap_or("").to_lowercase();
    if name.starts_with("bill") || name.ends_with("bill") {
        "document"
    } else {
        "other"
    }
}

/// Почтовые шаблоны и нужные CMS настройки ядра.
async fn read_mail(my: &MySqlPool, data: &mut Data) -> anyhow::Result<()> {
    for row in sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(EVENT_NAME AS CHAR), CAST(ACTIVE AS CHAR),
                CAST(IFNULL(EMAIL_FROM, '') AS CHAR), CAST(IFNULL(EMAIL_TO, '') AS CHAR),
                CAST(IFNULL(BCC, '') AS CHAR), CAST(IFNULL(SUBJECT, '') AS CHAR),
                CAST(IFNULL(MESSAGE, '') AS CHAR), CAST(IFNULL(BODY_TYPE, 'text') AS CHAR)
         FROM b_event_message ORDER BY ID",
    )
    .fetch_all(my)
    .await?
    {
        data.mail_templates.push(MailTemplateRow {
            id: int_col(&row, 0).context("b_event_message.ID")?,
            event_name: str_col(&row, 1),
            active: str_col(&row, 2) == "Y",
            email_from: str_col(&row, 3),
            email_to: str_col(&row, 4),
            bcc: str_col(&row, 5),
            subject: str_col(&row, 6),
            body: str_col(&row, 7),
            body_type: str_col(&row, 8),
        });
    }
    for (module, name) in OPTIONS {
        let value: Option<String> = sqlx::query_scalar(
            "SELECT CAST(VALUE AS CHAR) FROM b_option WHERE MODULE_ID = ? AND NAME = ? AND SITE_ID IS NULL",
        )
        .bind(module)
        .bind(name)
        .fetch_optional(my)
        .await?
        .flatten();
        if let Some(value) = value {
            data.options
                .push((module.to_string(), name.to_string(), value));
        }
    }
    Ok(())
}

/// Шаблоны обновляются по external_id; настройки — по (модуль, имя).
async fn write_mail(tx: &mut sqlx::PgConnection, data: &Data) -> anyhow::Result<()> {
    for t in &data.mail_templates {
        sqlx::query(
            "INSERT INTO mail_templates
                (event_name, active, email_from, email_to, bcc, subject, body, body_type, external_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (external_id) WHERE external_id IS NOT NULL DO UPDATE SET
                event_name = EXCLUDED.event_name, active = EXCLUDED.active,
                email_from = EXCLUDED.email_from, email_to = EXCLUDED.email_to, bcc = EXCLUDED.bcc,
                subject = EXCLUDED.subject, body = EXCLUDED.body, body_type = EXCLUDED.body_type",
        )
        .bind(&t.event_name)
        .bind(t.active)
        .bind(&t.email_from)
        .bind(&t.email_to)
        .bind(&t.bcc)
        .bind(&t.subject)
        .bind(&t.body)
        .bind(&t.body_type)
        .bind(format!("bitrix:{}", t.id))
        .execute(&mut *tx)
        .await?;
    }
    for (module, name, value) in &data.options {
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ($1, $2, $3)
             ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(module)
        .bind(name)
        .bind(value)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Местоположения модуля sale с русскими названиями.
async fn read_locations(my: &MySqlPool, data: &mut Data) -> anyhow::Result<()> {
    let exists: Option<String> = sqlx::query_scalar("SHOW TABLES LIKE 'b_sale_location'")
        .fetch_optional(my)
        .await?;
    if exists.is_none() {
        return Ok(());
    }
    for row in sqlx::query(
        "SELECT CAST(l.ID AS SIGNED), CAST(l.CODE AS CHAR), CAST(IFNULL(l.PARENT_ID, 0) AS SIGNED),
                CAST(IFNULL(t.CODE, '') AS CHAR), CAST(IFNULL(n.NAME, l.CODE) AS CHAR),
                CAST(l.SORT AS SIGNED), CAST(IFNULL(l.DEPTH_LEVEL, 1) AS SIGNED)
         FROM b_sale_location l
         LEFT JOIN b_sale_loc_type t ON t.ID = l.TYPE_ID
         LEFT JOIN b_sale_loc_name n ON n.LOCATION_ID = l.ID AND n.LANGUAGE_ID = 'ru'
         ORDER BY l.DEPTH_LEVEL, l.ID",
    )
    .fetch_all(my)
    .await?
    {
        data.locations.push((
            int_col(&row, 0).context("b_sale_location.ID")?,
            str_col(&row, 1),
            id_col(&row, 2),
            str_col(&row, 3),
            str_col(&row, 4),
            int_col(&row, 5).unwrap_or(100) as i32,
            int_col(&row, 6).unwrap_or(1) as i32,
        ));
    }
    Ok(())
}

/// Пользователи: обновляются по `external_id` (не удаляются — у них могут быть сессии).
/// Логин, занятый своим пользователем CMS, пропускается. Возвращает id Битрикса → наш id.
async fn write_users(
    tx: &mut sqlx::PgConnection,
    users: &[UserRow],
    groups: &[GroupRow],
    file_ids: &HashSet<i64>,
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
            "INSERT INTO users (login, email, name, last_name, password_hash, is_admin, active,
                                external_id, second_name, phone, city, work_position, photo_id, extra) ",
        );
        qb.push_values(chunk, |mut b, u| {
            b.push_bind(&u.login)
                .push_bind((!u.email.is_empty()).then_some(&u.email))
                .push_bind(&u.name)
                .push_bind(&u.last_name)
                .push_bind(&u.password_hash)
                .push_bind(false)
                .push_bind(u.active)
                .push_bind(format!("bitrix:{}", u.bitrix_id))
                .push_bind(&u.second_name)
                .push_bind(&u.phone)
                .push_bind(&u.city)
                .push_bind(&u.work_position)
                .push_bind(u.photo_id.filter(|id| file_ids.contains(id)))
                .push_bind(sqlx::types::Json(&u.extra));
        });
        // Уже вошедший через CMS хранит argon2 — его не затираем старым хешем
        qb.push(
            " ON CONFLICT (external_id) WHERE external_id IS NOT NULL DO UPDATE SET
                login = EXCLUDED.login, email = EXCLUDED.email, name = EXCLUDED.name,
                last_name = EXCLUDED.last_name, active = EXCLUDED.active,
                second_name = EXCLUDED.second_name, phone = EXCLUDED.phone, city = EXCLUDED.city,
                work_position = EXCLUDED.work_position, photo_id = EXCLUDED.photo_id,
                extra = users.extra || EXCLUDED.extra,
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

    // Группы: обновляются по external_id; членство в них перезаписывается
    let mut group_map: HashMap<i64, i64> = HashMap::new();
    for (bitrix_id, code, name, description, sort) in groups {
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO groups (code, name, description, sort, external_id) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (external_id) WHERE external_id IS NOT NULL
             DO UPDATE SET name = EXCLUDED.name, description = EXCLUDED.description, sort = EXCLUDED.sort
             RETURNING id",
        )
        .bind(code)
        .bind(name)
        .bind(description)
        .bind(sort)
        .bind(format!("bitrix:{bitrix_id}"))
        .fetch_one(&mut *tx)
        .await?;
        group_map.insert(*bitrix_id, id);
    }
    sqlx::query(
        "DELETE FROM user_groups ug USING groups g
         WHERE g.id = ug.group_id AND g.external_id LIKE 'bitrix:%'",
    )
    .execute(&mut *tx)
    .await?;
    let pairs: Vec<(i64, i64)> = users
        .iter()
        .filter_map(|u| map.get(&u.bitrix_id).map(|uid| (u, *uid)))
        .flat_map(|(u, uid)| {
            let group_map = &group_map;
            u.groups
                .iter()
                .filter_map(move |g| group_map.get(g).map(|gid| (uid, *gid)))
        })
        .collect();
    for chunk in pairs.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new("INSERT INTO user_groups (user_id, group_id) ");
        qb.push_values(chunk, |mut b, (u, g)| {
            b.push_bind(u).push_bind(g);
        });
        qb.push(" ON CONFLICT DO NOTHING");
        qb.build().execute(&mut *tx).await?;
    }
    Ok(map)
}

/// Местоположения перезаписываются целиком; родитель вставлен раньше потомка.
async fn write_locations(
    tx: &mut sqlx::PgConnection,
    locations: &[LocationRow],
) -> anyhow::Result<()> {
    if locations.is_empty() {
        return Ok(());
    }
    sqlx::query("DELETE FROM locations")
        .execute(&mut *tx)
        .await?;
    let ids: HashSet<i64> = locations.iter().map(|l| l.0).collect();
    for chunk in locations.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO locations (id, code, parent_id, type_code, name, sort, depth_level) ",
        );
        qb.push_values(
            chunk,
            |mut b, (id, code, parent, type_code, name, sort, depth)| {
                b.push_bind(id)
                    .push_bind(code)
                    .push_bind(parent.filter(|p| ids.contains(p)))
                    .push_bind(type_code)
                    .push_bind(name)
                    .push_bind(sort)
                    .push_bind(depth);
            },
        );
        qb.build().execute(&mut *tx).await?;
    }
    Ok(())
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
        "SELECT CAST(ID AS SIGNED), CAST(TITLE AS CHAR), CAST(ACTIVE AS CHAR), CAST(SORT AS SIGNED),
                CAST(IFNULL(ADDRESS, '') AS CHAR), CAST(IFNULL(PHONE, '') AS CHAR),
                CAST(IFNULL(EMAIL, '') AS CHAR), CAST(IFNULL(SCHEDULE, '') AS CHAR),
                CAST(IMAGE_ID AS SIGNED)
         FROM b_catalog_store ORDER BY ID",
    )
    .fetch_all(my)
    .await?
    {
        c.stores.push(StoreRow {
            id: int_col(&row, 0).context("b_catalog_store.ID")?,
            name: str_col(&row, 1),
            active: str_col(&row, 2) == "Y",
            sort: int_col(&row, 3).unwrap_or(500) as i32,
            address: str_col(&row, 4),
            phone: str_col(&row, 5),
            email: str_col(&row, 6),
            schedule: str_col(&row, 7),
            extra: Map::new(),
            image_id: id_col(&row, 8),
        });
    }
    let store_uf = read_uf(my, "CAT_STORE", "b_uts_cat_store", "b_utm_cat_store").await?;
    for st in &mut c.stores {
        if let Some(extra) = store_uf.get(&st.id) {
            st.extra = extra.clone();
        }
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
        "SELECT CAST(ID AS SIGNED), CAST(IFNULL(QUANTITY, 0) AS CHAR),
                CAST(IFNULL(AVAILABLE, 'Y') AS CHAR), CAST(IFNULL(QUANTITY_TRACE, 'D') AS CHAR),
                CAST(IFNULL(CAN_BUY_ZERO, 'D') AS CHAR)
         FROM b_catalog_product",
    )
    .fetch_all(my)
    .await?
    {
        let element = int_col(&row, 0).unwrap_or_default();
        if elements.contains(&element) {
            c.products.push(ProductRow {
                element_id: element,
                quantity: str_col(&row, 1).parse().unwrap_or(0.0),
                available: str_col(&row, 2) != "N",
                quantity_trace: bitrix_flag(&str_col(&row, 3)),
                can_buy_zero: bitrix_flag(&str_col(&row, 4)),
            });
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
        .map(|e| ((e.collection_id, e.xml_id.as_str()), e.id))
        .collect();

    let mut out = Props::new();
    for element in &data.elements {
        let mut values = Map::new();
        for prop in data
            .properties
            .iter()
            .filter(|p| p.collection_id == element.collection_id)
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
                            .link_collection_id
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
        p.link_collection_id = p.link_collection_id.filter(|id| iblock_ids.contains(id));
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

/// Есть ли таблица в базе Битрикса (`name` — проверенное имя таблицы).
async fn mysql_table_exists(my: &MySqlPool, name: &str) -> anyhow::Result<bool> {
    let found: Option<String> =
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SHOW TABLES LIKE '{name}'")))
            .fetch_optional(my)
            .await?;
    Ok(found.is_some())
}

/// Флаг товара Битрикса: `Y`/`N`; `D` и пусто — «по умолчанию» из настроек каталога.
fn bitrix_flag(raw: &str) -> Option<bool> {
    match raw.trim() {
        "Y" => Some(true),
        "N" => Some(false),
        _ => None,
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
            "INSERT INTO collections (id, code, name, description, api_enabled, sort, detail_page_url,
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
            "INSERT INTO collection_fields
                (id, collection_id, code, name, kind, is_required, sort, multiple, link_collection_id,
                 user_type) ",
        );
        qb.push_values(chunk, |mut b, p| {
            b.push_bind(p.id)
                .push_bind(p.collection_id)
                .push_bind(&p.code)
                .push_bind(&p.name)
                .push_bind(p.kind)
                .push_bind(p.is_required)
                .push_bind(p.sort)
                .push_bind(p.multiple)
                .push_bind(p.link_collection_id)
                .push_bind(&p.user_type);
        });
        qb.build().execute(&mut *tx).await?;
    }

    for chunk in data.enums.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO collection_field_options (id, field_id, value, xml_id, sort, is_default) ",
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
            "INSERT INTO collection_sections
                (id, collection_id, parent_id, code, xml_id, name, active, sort, depth_level,
                 description, picture_id, created_at, updated_at) ",
        );
        qb.push_values(chunk, |mut b, s| {
            b.push_bind(s.id)
                .push_bind(s.collection_id)
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

    let file_ids: HashSet<i64> = data.files.iter().map(|f| f.id).collect();
    let users = write_users(&mut *tx, &data.users, &data.groups, &file_ids).await?;
    write_locations(&mut *tx, &data.locations).await?;
    write_mail(&mut *tx, data).await?;
    let empty = Map::new();
    for chunk in data.elements.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO collection_items
                (id, collection_id, section_id, code, xml_id, name, active, sort, preview_text,
                 detail_text, preview_picture_id, detail_picture_id, published_at, field_values,
                 created_at, updated_at, created_by) ",
        );
        qb.push_values(chunk, |mut b, e| {
            b.push_bind(e.id)
                .push_bind(e.collection_id)
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
    write_sale(&mut *tx, &data.sale).await?;

    // Последовательности — после максимальных перенесённых id
    for (table, seq) in [
        ("files", "files_id_seq"),
        ("collections", "collections_id_seq"),
        ("collection_fields", "collection_fields_id_seq"),
        (
            "collection_field_options",
            "collection_field_options_id_seq",
        ),
        ("collection_sections", "collection_sections_id_seq"),
        ("collection_items", "collection_items_id_seq"),
        ("catalog_price_types", "catalog_price_types_id_seq"),
        ("catalog_prices", "catalog_prices_id_seq"),
        ("catalog_stores", "catalog_stores_id_seq"),
        ("person_types", "person_types_id_seq"),
        ("order_property_groups", "order_property_groups_id_seq"),
        ("order_properties", "order_properties_id_seq"),
        ("order_property_variants", "order_property_variants_id_seq"),
        ("deliveries", "deliveries_id_seq"),
        ("pay_systems", "pay_systems_id_seq"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT setval('{seq}', GREATEST((SELECT MAX(id) FROM {table}), 1))"
        )))
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Настройки оформления: статусы, типы плательщика, свойства, доставки, платёжки.
async fn read_sale(my: &MySqlPool, data: &mut Data) -> anyhow::Result<()> {
    if !mysql_table_exists(my, "b_sale_person_type").await? {
        return Ok(());
    }
    let sale = &mut data.sale;
    for row in sqlx::query(
        "SELECT CAST(s.ID AS CHAR), CAST(IFNULL(l.NAME, s.ID) AS CHAR), CAST(s.SORT AS SIGNED),
                CAST(IFNULL(l.DESCRIPTION, '') AS CHAR), CAST(s.NOTIFY AS CHAR)
         FROM b_sale_status s LEFT JOIN b_sale_status_lang l ON l.STATUS_ID = s.ID AND l.LID = 'ru'
         WHERE s.TYPE = 'O' ORDER BY s.SORT, s.ID",
    )
    .fetch_all(my)
    .await?
    {
        sale.statuses.push((
            str_col(&row, 0),
            str_col(&row, 1),
            int_col(&row, 2).unwrap_or(100) as i32,
            str_col(&row, 3),
            str_col(&row, 4) == "Y",
        ));
    }
    for row in sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(IFNULL(CODE, '') AS CHAR), CAST(NAME AS CHAR),
                CAST(ACTIVE AS CHAR), CAST(SORT AS SIGNED)
         FROM b_sale_person_type ORDER BY ID",
    )
    .fetch_all(my)
    .await?
    {
        sale.person_types.push((
            int_col(&row, 0).context("b_sale_person_type.ID")?,
            str_col(&row, 1),
            str_col(&row, 2),
            str_col(&row, 3) == "Y",
            int_col(&row, 4).unwrap_or(100) as i32,
        ));
    }
    for row in sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(PERSON_TYPE_ID AS SIGNED), CAST(NAME AS CHAR), CAST(SORT AS SIGNED)
         FROM b_sale_order_props_group ORDER BY ID",
    )
    .fetch_all(my)
    .await?
    {
        let id = int_col(&row, 0).context("b_sale_order_props_group.ID")?;
        let name = str_col(&row, 2);
        sale.groups.push((
            id,
            int_col(&row, 1).unwrap_or(0),
            name.clone(),
            int_col(&row, 3).unwrap_or(100) as i32,
            sale_block_code(&name, id),
        ));
    }
    for row in sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(PERSON_TYPE_ID AS SIGNED), CAST(PROPS_GROUP_ID AS SIGNED),
                CAST(IFNULL(CODE, '') AS CHAR), CAST(NAME AS CHAR), CAST(TYPE AS CHAR),
                CAST(REQUIRED AS CHAR), CAST(UTIL AS CHAR), CAST(IS_EMAIL AS CHAR),
                CAST(IS_PHONE AS CHAR), CAST(IS_PAYER AS CHAR), CAST(IS_PROFILE_NAME AS CHAR),
                CAST(IS_LOCATION AS CHAR), CAST(IS_ADDRESS AS CHAR), CAST(IS_ZIP AS CHAR),
                CAST(IFNULL(DEFAULT_VALUE, '') AS CHAR), CAST(IFNULL(DESCRIPTION, '') AS CHAR),
                CAST(SORT AS SIGNED), CAST(ACTIVE AS CHAR), CAST(IFNULL(SETTINGS, '') AS CHAR),
                CAST(MULTIPLE AS CHAR)
         FROM b_sale_order_props ORDER BY ID",
    )
    .fetch_all(my)
    .await?
    {
        let yes = |i: usize| str_col(&row, i) == "Y";
        let multiline = php_value(&str_col(&row, 19))
            .and_then(|v| v.get("MULTILINE").and_then(Value::as_str).map(|m| m == "Y"))
            .unwrap_or(false);
        sale.properties.push(SalePropertyRow {
            id: int_col(&row, 0).context("b_sale_order_props.ID")?,
            person_type_id: int_col(&row, 1).unwrap_or(0),
            group_id: id_col(&row, 2),
            code: str_col(&row, 3),
            name: str_col(&row, 4),
            kind: sale_prop_kind(&str_col(&row, 5), multiline),
            required: yes(6),
            util: yes(7),
            flags: [yes(8), yes(9), yes(10), yes(11), yes(12), yes(13), yes(14)],
            default_value: str_col(&row, 15),
            description: str_col(&row, 16),
            sort: int_col(&row, 17).unwrap_or(100) as i32,
            active: yes(18),
            multiple: yes(20),
        });
    }
    for row in sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(ORDER_PROPS_ID AS SIGNED), CAST(VALUE AS CHAR),
                CAST(NAME AS CHAR), CAST(SORT AS SIGNED)
         FROM b_sale_order_props_variant ORDER BY ID",
    )
    .fetch_all(my)
    .await?
    {
        sale.variants.push((
            int_col(&row, 0).context("b_sale_order_props_variant.ID")?,
            int_col(&row, 1).unwrap_or(0),
            str_col(&row, 2),
            str_col(&row, 3),
            int_col(&row, 4).unwrap_or(100) as i32,
        ));
    }

    if mysql_table_exists(my, "b_sale_order_props_relation").await? {
        for row in sqlx::query(
            "SELECT CAST(PROPERTY_ID AS SIGNED), CAST(ENTITY_TYPE AS CHAR), CAST(ENTITY_ID AS SIGNED)
             FROM b_sale_order_props_relation WHERE ENTITY_TYPE IN ('P', 'D')",
        )
        .fetch_all(my)
        .await?
        {
            if let (Some(property), Some(entity)) = (int_col(&row, 0), int_col(&row, 2)) {
                sale.relations.push((property, str_col(&row, 1), entity));
            }
        }
    }

    // Ограничения служб: (служба, тип 0 — доставка / 1 — оплата, класс, параметры)
    let restrictions: Vec<(i64, i64, String, String)> = sqlx::query(
        "SELECT CAST(SERVICE_ID AS SIGNED), CAST(SERVICE_TYPE AS SIGNED), CAST(CLASS_NAME AS CHAR),
                CAST(IFNULL(PARAMS, '') AS CHAR)
         FROM b_sale_service_rstr",
    )
    .fetch_all(my)
    .await?
    .iter()
    .map(|r| {
        (
            int_col(r, 0).unwrap_or(0),
            int_col(r, 1).unwrap_or(-1),
            str_col(r, 2),
            str_col(r, 3),
        )
    })
    .collect();
    let mut pickup: HashMap<i64, Vec<i64>> = HashMap::new();
    for row in sqlx::query(
        "SELECT CAST(DELIVERY_ID AS SIGNED), CAST(IFNULL(PARAMS, '') AS CHAR)
         FROM b_sale_delivery_es WHERE ACTIVE = 'Y' AND CLASS_NAME LIKE '%ExtraServices%Store'",
    )
    .fetch_all(my)
    .await?
    {
        pickup
            .entry(int_col(&row, 0).unwrap_or(0))
            .or_default()
            .extend(param_ids(&str_col(&row, 1), "STORES"));
    }
    for row in sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(IFNULL(CODE, '') AS CHAR), CAST(NAME AS CHAR),
                CAST(IFNULL(DESCRIPTION, '') AS CHAR), CAST(ACTIVE AS CHAR), CAST(SORT AS SIGNED),
                CAST(CLASS_NAME AS CHAR), CAST(IFNULL(CONFIG, '') AS CHAR), CAST(IFNULL(CURRENCY, '') AS CHAR)
         FROM b_sale_delivery_srv ORDER BY ID",
    )
    .fetch_all(my)
    .await?
    {
        let id = int_col(&row, 0).context("b_sale_delivery_srv.ID")?;
        let class = str_col(&row, 6);
        // Группы служб — папки в админке Битрикса, не способы доставки
        if class.ends_with(r"\Group") {
            continue;
        }
        let hidden = restrictions.iter().any(|(sid, kind, cls, params)| {
            *sid == id
                && *kind == 0
                && cls.ends_with("ByPublicMode")
                && php_value(params)
                    .and_then(|v| v.get("PUBLIC_SHOW").and_then(Value::as_str).map(|p| p == "N"))
                    .unwrap_or(false)
        });
        let currency = str_col(&row, 8);
        sale.deliveries.push(DeliveryRow {
            id,
            code: str_col(&row, 1),
            name: str_col(&row, 2),
            description: str_col(&row, 3),
            active: str_col(&row, 4) == "Y",
            public: !hidden,
            sort: int_col(&row, 5).unwrap_or(100) as i32,
            price: delivery_price(&class, &str_col(&row, 7)),
            currency: if currency.is_empty() { "RUB".into() } else { currency },
            stores: pickup.remove(&id).unwrap_or_default(),
        });
    }
    for row in sqlx::query(
        "SELECT CAST(ID AS SIGNED), CAST(IFNULL(CODE, '') AS CHAR), CAST(NAME AS CHAR),
                CAST(IFNULL(DESCRIPTION, '') AS CHAR), CAST(ACTIVE AS CHAR), CAST(SORT AS SIGNED),
                CAST(IFNULL(ACTION_FILE, '') AS CHAR)
         FROM b_sale_pay_system_action ORDER BY ID",
    )
    .fetch_all(my)
    .await?
    {
        let id = int_col(&row, 0).context("b_sale_pay_system_action.ID")?;
        let group_ids = restrictions
            .iter()
            .filter(|(sid, kind, _, _)| *sid == id && *kind == 1)
            .flat_map(|(_, _, _, params)| param_ids(params, "GROUPS"))
            .collect();
        sale.pay_systems.push(PaySystemRow {
            id,
            code: str_col(&row, 1),
            name: str_col(&row, 2),
            description: str_col(&row, 3),
            active: str_col(&row, 4) == "Y",
            sort: int_col(&row, 5).unwrap_or(100) as i32,
            api_type: pay_api_type(&str_col(&row, 6)),
            group_ids,
        });
    }
    Ok(())
}

/// Настройки оформления обновляются по id (на них могут ссылаться заказы), варианты
/// свойств и склады доставок — перезаписываются.
async fn write_sale(tx: &mut sqlx::PgConnection, sale: &Sale) -> anyhow::Result<()> {
    for (code, name, sort, description, notify) in &sale.statuses {
        sqlx::query(
            "INSERT INTO order_statuses (code, name, sort, description, notify) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (code) DO UPDATE SET name = EXCLUDED.name, sort = EXCLUDED.sort,
                 description = EXCLUDED.description, notify = EXCLUDED.notify",
        )
        .bind(code)
        .bind(name)
        .bind(sort)
        .bind(description)
        .bind(notify)
        .execute(&mut *tx)
        .await?;
    }
    for (id, code, name, active, sort) in &sale.person_types {
        sqlx::query(
            "INSERT INTO person_types (id, code, name, active, sort) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (id) DO UPDATE SET code = EXCLUDED.code, name = EXCLUDED.name,
                 active = EXCLUDED.active, sort = EXCLUDED.sort",
        )
        .bind(id)
        .bind(code)
        .bind(name)
        .bind(active)
        .bind(sort)
        .execute(&mut *tx)
        .await?;
    }
    let person_types: HashSet<i64> = sale.person_types.iter().map(|p| p.0).collect();
    for (id, person_type, name, sort, block) in &sale.groups {
        if !person_types.contains(person_type) {
            continue;
        }
        sqlx::query(
            "INSERT INTO order_property_groups (id, person_type_id, name, sort, block_code)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (id) DO UPDATE SET person_type_id = EXCLUDED.person_type_id,
                 name = EXCLUDED.name, sort = EXCLUDED.sort, block_code = EXCLUDED.block_code",
        )
        .bind(id)
        .bind(person_type)
        .bind(name)
        .bind(sort)
        .bind(block)
        .execute(&mut *tx)
        .await?;
    }
    let groups: HashSet<i64> = sale.groups.iter().map(|g| g.0).collect();
    for p in sale
        .properties
        .iter()
        .filter(|p| person_types.contains(&p.person_type_id))
    {
        let [
            is_email,
            is_phone,
            is_payer,
            is_profile_name,
            is_location,
            is_address,
            is_zip,
        ] = p.flags;
        sqlx::query(
            "INSERT INTO order_properties (id, person_type_id, group_id, code, name, kind, required, util,
                 is_email, is_phone, is_payer, is_profile_name, is_location, is_address, is_zip,
                 default_value, description, sort, active, multiple)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)
             ON CONFLICT (id) DO UPDATE SET person_type_id = EXCLUDED.person_type_id,
                 group_id = EXCLUDED.group_id, code = EXCLUDED.code, name = EXCLUDED.name,
                 kind = EXCLUDED.kind, required = EXCLUDED.required, util = EXCLUDED.util,
                 is_email = EXCLUDED.is_email, is_phone = EXCLUDED.is_phone, is_payer = EXCLUDED.is_payer,
                 is_profile_name = EXCLUDED.is_profile_name, is_location = EXCLUDED.is_location,
                 is_address = EXCLUDED.is_address, is_zip = EXCLUDED.is_zip,
                 default_value = EXCLUDED.default_value, description = EXCLUDED.description,
                 sort = EXCLUDED.sort, active = EXCLUDED.active, multiple = EXCLUDED.multiple",
        )
        .bind(p.id)
        .bind(p.person_type_id)
        .bind(p.group_id.filter(|g| groups.contains(g)))
        .bind(&p.code)
        .bind(&p.name)
        .bind(p.kind)
        .bind(p.required)
        .bind(p.util)
        .bind(is_email)
        .bind(is_phone)
        .bind(is_payer)
        .bind(is_profile_name)
        .bind(is_location)
        .bind(is_address)
        .bind(is_zip)
        .bind(&p.default_value)
        .bind(&p.description)
        .bind(p.sort)
        .bind(p.active)
        .bind(p.multiple)
        .execute(&mut *tx)
        .await?;
    }
    let properties: Vec<i64> = sale
        .properties
        .iter()
        .filter(|p| person_types.contains(&p.person_type_id))
        .map(|p| p.id)
        .collect();
    sqlx::query("DELETE FROM order_property_variants WHERE property_id = ANY($1)")
        .bind(&properties)
        .execute(&mut *tx)
        .await?;
    for (id, property, value, name, sort) in &sale.variants {
        if !properties.contains(property) {
            continue;
        }
        sqlx::query(
            "INSERT INTO order_property_variants (id, property_id, value, name, sort) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(property)
        .bind(value)
        .bind(name)
        .bind(sort)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM order_property_relations WHERE property_id = ANY($1)")
        .bind(&properties)
        .execute(&mut *tx)
        .await?;
    for (property, kind, entity) in &sale.relations {
        if properties.contains(property) {
            sqlx::query(
                "INSERT INTO order_property_relations (property_id, entity_type, entity_id)
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(property)
            .bind(kind)
            .bind(entity)
            .execute(&mut *tx)
            .await?;
        }
    }
    for d in &sale.deliveries {
        sqlx::query(
            "INSERT INTO deliveries (id, code, name, description, active, public, sort, price, currency)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (id) DO UPDATE SET code = EXCLUDED.code, name = EXCLUDED.name,
                 description = EXCLUDED.description, active = EXCLUDED.active, public = EXCLUDED.public,
                 sort = EXCLUDED.sort, price = EXCLUDED.price, currency = EXCLUDED.currency",
        )
        .bind(d.id)
        .bind(&d.code)
        .bind(&d.name)
        .bind(&d.description)
        .bind(d.active)
        .bind(d.public)
        .bind(d.sort)
        .bind(d.price)
        .bind(&d.currency)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM delivery_stores WHERE delivery_id = $1")
            .bind(d.id)
            .execute(&mut *tx)
            .await?;
        // Склад мог не перенестись (удалён в Битриксе) — такие пропускаются
        sqlx::query(
            "INSERT INTO delivery_stores (delivery_id, store_id)
             SELECT $1, id FROM catalog_stores WHERE id = ANY($2) ON CONFLICT DO NOTHING",
        )
        .bind(d.id)
        .bind(&d.stores)
        .execute(&mut *tx)
        .await?;
    }
    for p in &sale.pay_systems {
        sqlx::query(
            "INSERT INTO pay_systems (id, code, name, description, active, sort, api_type, group_ids)
             VALUES ($1, $2, $3, $4,
                     -- ограничение по группам, которых нет в CMS: выключаем, а не открываем всем
                     $5 AND (cardinality($8::text[]) = 0
                             OR EXISTS (SELECT 1 FROM groups WHERE external_id = ANY($8))),
                     $6, $7, ARRAY(SELECT id FROM groups WHERE external_id = ANY($8) ORDER BY id))
             ON CONFLICT (id) DO UPDATE SET code = EXCLUDED.code, name = EXCLUDED.name,
                 description = EXCLUDED.description, active = EXCLUDED.active, sort = EXCLUDED.sort,
                 api_type = EXCLUDED.api_type, group_ids = EXCLUDED.group_ids",
        )
        .bind(p.id)
        .bind(&p.code)
        .bind(&p.name)
        .bind(&p.description)
        .bind(p.active)
        .bind(p.sort)
        .bind(p.api_type)
        // Группы CMS — по внешнему коду группы Битрикса
        .bind(
            p.group_ids
                .iter()
                .map(|g| format!("bitrix:{g}"))
                .collect::<Vec<_>>(),
        )
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
                (item_id, price_type_id, price, currency, quantity_from, quantity_to) ",
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
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO catalog_stores (id, name, active, sort, address, phone, email, schedule, extra) ",
        );
        qb.push_values(chunk, |mut b, st| {
            b.push_bind(st.id)
                .push_bind(&st.name)
                .push_bind(st.active)
                .push_bind(st.sort)
                .push_bind(&st.address)
                .push_bind(&st.phone)
                .push_bind(&st.email)
                .push_bind(&st.schedule)
                .push_bind(sqlx::types::Json(&st.extra));
        });
        qb.build().execute(&mut *tx).await?;
    }
    // Картинка склада — только если файл перенесён
    for st in c.stores.iter().filter(|st| st.image_id.is_some()) {
        sqlx::query(
            "UPDATE catalog_stores SET image_id = $2
             WHERE id = $1 AND EXISTS (SELECT 1 FROM files WHERE id = $2)",
        )
        .bind(st.id)
        .bind(st.image_id)
        .execute(&mut *tx)
        .await?;
    }
    for chunk in c.amounts.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO catalog_store_amounts (item_id, store_id, amount) ",
        );
        qb.push_values(chunk, |mut b, (el, store, amount)| {
            b.push_bind(el).push_bind(store).push_bind(amount);
        });
        qb.push(" ON CONFLICT DO NOTHING");
        qb.build().execute(&mut *tx).await?;
    }
    for chunk in c.products.chunks(BATCH) {
        let mut qb = QueryBuilder::<Postgres>::new(
            "INSERT INTO catalog_products (item_id, quantity, available, quantity_trace, can_buy_zero) ",
        );
        qb.push_values(chunk, |mut b, p| {
            b.push_bind(p.element_id)
                .push_bind(p.quantity)
                .push_bind(p.available)
                .push_bind(p.quantity_trace)
                .push_bind(p.can_buy_zero);
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

    /// Минимальный перенос контента: инфоблок, свойство-список с вариантом, раздел, элемент.
    #[sqlx::test]
    async fn import_writes_collections(db: PgPool) {
        let now = Utc::now();
        let mut data = Data::default();
        data.iblocks.push(IblockRow {
            id: 5,
            api_enabled: true,
            code: "news".into(),
            name: "Новости".into(),
            description: String::new(),
            sort: 500,
            detail_page_url: String::new(),
            section_page_url: String::new(),
            list_page_url: String::new(),
            is_catalog: false,
        });
        data.properties.push(PropertyRow {
            id: 7,
            collection_id: 5,
            code: "color".into(),
            name: "Цвет".into(),
            kind: "list",
            multiple: false,
            is_required: false,
            sort: 500,
            link_collection_id: None,
            directory: false,
            user_type: String::new(),
        });
        data.enums.push(EnumRow {
            id: 9,
            property_id: 7,
            value: "Красный".into(),
            xml_id: "red".into(),
            sort: 500,
            is_default: false,
        });
        data.sections.push(SectionRow {
            id: 3,
            collection_id: 5,
            parent_id: None,
            code: "a".into(),
            xml_id: String::new(),
            name: "Раздел".into(),
            active: true,
            sort: 500,
            depth_level: 1,
            description: String::new(),
            picture_id: None,
            created_at: now,
            updated_at: now,
        });
        data.elements.push(ElementRow {
            id: 11,
            collection_id: 5,
            section_id: Some(3),
            code: "n".into(),
            xml_id: String::new(),
            name: "Новость".into(),
            active: true,
            sort: 500,
            preview_text: String::new(),
            detail_text: String::new(),
            preview_picture_id: None,
            detail_picture_id: None,
            published_at: None,
            created_by: None,
            raw: HashMap::from([(7, vec!["9".to_string()])]),
            created_at: now,
            updated_at: now,
        });
        let props = resolve_properties(&mut data);
        let mut tx = db.begin().await.unwrap();
        write_all(&mut tx, &data, &props).await.unwrap();
        tx.commit().await.unwrap();
        for table in [
            "collections",
            "collection_fields",
            "collection_field_options",
            "collection_sections",
            "collection_items",
        ] {
            let n: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert_eq!(n, 1, "{table}");
        }
        let values: Value =
            sqlx::query_scalar("SELECT field_values FROM collection_items WHERE id = 11")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(values, serde_json::json!({"color": 9}));
    }

    #[sqlx::test]
    async fn pay_system_with_unmapped_groups_is_disabled(db: PgPool) {
        sqlx::query("INSERT INTO groups (code, name, external_id) VALUES ('cashless', 'Безнал', 'bitrix:21')")
            .execute(&db)
            .await
            .unwrap();
        let pay = |id: i64, groups: Vec<i64>| PaySystemRow {
            id,
            code: String::new(),
            name: format!("Платёжка {id}"),
            description: String::new(),
            active: true,
            sort: 100,
            api_type: "other",
            group_ids: groups,
        };
        let sale = Sale {
            pay_systems: vec![pay(5, vec![21]), pay(6, vec![99]), pay(7, vec![])],
            ..Default::default()
        };
        let mut conn = db.acquire().await.unwrap();
        write_sale(&mut conn, &sale).await.unwrap();
        let rows: Vec<(i64, bool, i32)> = sqlx::query_as(
            "SELECT id, active, cardinality(group_ids) FROM pay_systems ORDER BY id",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        // 5 — группа сопоставлена; 6 — группы нет в CMS: не открываем всем, а выключаем
        assert_eq!(rows, vec![(5, true, 1), (6, false, 0), (7, true, 0)]);
    }

    #[test]
    fn sale_block_code_by_group_name() {
        assert_eq!(sale_block_code("Покупатель", 1), "buyer");
        assert_eq!(sale_block_code("Данные покупателя", 9), "group_9");
        assert_eq!(sale_block_code("Плательщик", 2), "buyer");
        assert_eq!(sale_block_code("Получатель", 3), "recipient");
        assert_eq!(sale_block_code("Комментарий", 4), "comment");
        assert_eq!(sale_block_code("Свойства заказа", 7), "group_7");
        assert_eq!(sale_block_code("Main info", 8), "main_info");
    }

    #[test]
    fn sale_prop_kinds() {
        assert_eq!(sale_prop_kind("STRING", false), "text");
        assert_eq!(sale_prop_kind("STRING", true), "textarea");
        assert_eq!(sale_prop_kind("Y/N", false), "checkbox");
        assert_eq!(sale_prop_kind("ENUM", false), "select");
        assert_eq!(sale_prop_kind("LOCATION", false), "location");
        assert_eq!(sale_prop_kind("WHATEVER", false), "text");
    }

    #[test]
    fn delivery_price_from_config() {
        let configurable = r#"a:1:{s:4:"MAIN";a:3:{s:8:"CURRENCY";s:3:"RUB";s:5:"PRICE";s:3:"300";s:6:"PERIOD";a:0:{}}}"#;
        assert_eq!(
            delivery_price(r"\Bitrix\Sale\Delivery\Services\Configurable", configurable),
            300.0
        );
        let simple = r#"a:1:{s:4:"MAIN";a:4:{s:8:"CURRENCY";s:3:"RUB";i:0;s:3:"300";i:1;s:0:"";i:2;s:0:"";}}"#;
        assert_eq!(
            delivery_price(r"\Sale\Handlers\Delivery\SimpleHandler", simple),
            300.0
        );
        assert_eq!(
            delivery_price(r"\Bitrix\Sale\Delivery\Services\EmptyDeliveryService", ""),
            0.0
        );
    }

    #[test]
    fn param_ids_from_serialized() {
        assert_eq!(
            param_ids(
                r#"a:1:{s:6:"STORES";a:3:{i:0;s:1:"3";i:1;s:1:"1";i:2;s:1:"4";}}"#,
                "STORES"
            ),
            vec![3, 1, 4]
        );
        assert_eq!(
            param_ids(r#"a:1:{s:6:"GROUPS";a:1:{i:0;s:2:"21";}}"#, "GROUPS"),
            vec![21]
        );
        assert!(param_ids("a:0:{}", "GROUPS").is_empty());
    }

    #[test]
    fn pay_api_types() {
        assert_eq!(pay_api_type("custombill"), "document");
        assert_eq!(pay_api_type("bill"), "document");
        assert_eq!(pay_api_type("billkz"), "document");
        assert_eq!(pay_api_type("yandexcheckout"), "other");
    }

    #[test]
    fn bitrix_flag_values() {
        assert_eq!(bitrix_flag("Y"), Some(true));
        assert_eq!(bitrix_flag("N"), Some(false));
        assert_eq!(bitrix_flag("D"), None);
        assert_eq!(bitrix_flag(""), None);
    }

    #[test]
    fn php_arrays() {
        assert_eq!(
            php_unserialize_list(r#"a:2:{i:0;s:12:"+7 900 1-2-3";i:1;s:8:"Тест";}"#),
            ["+7 900 1-2-3", "Тест"]
        );
        assert_eq!(php_unserialize_list("a:1:{i:0;i:5;}"), ["5"]);
        assert!(php_unserialize_list("a:0:{}").is_empty());
        assert_eq!(php_unserialize_list("plain"), ["plain"]);
        assert!(php_unserialize_list("").is_empty());
    }

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
