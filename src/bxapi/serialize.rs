//! Элементы инфоблока → JSON так, как их отдаёт bxapi.
//!
//! Правила сверены с живым Битриксом (формат пустых значений у Битрикса неочевиден:
//! пустой одиночный список — `0`, пустая одиночная строка — `""`, пустое
//! множественное — `null`).

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
};

use chrono::{DateTime, FixedOffset, Utc};
use serde_json::{Map, Value, json};
use sqlx::{FromRow, types::Json};

use super::{
    BxError, images,
    project::Project,
    query::Select,
    registry::{Currency, Schema, Snapshot},
    to_camel, to_snake,
};
use crate::{
    files::{self, FileRecord},
    iblock::Property,
    state::AppState,
};

/// Колонки строки элемента; алиас таблицы — `e`.
pub const ROW_COLS: &str = "e.id, e.iblock_id, e.section_id, e.code, e.xml_id, e.name, e.active, \
     e.sort, e.preview_text, e.detail_text, e.preview_picture_id, e.detail_picture_id, \
     e.published_at, e.created_at, e.updated_at, e.created_by, e.properties";

#[derive(Debug, Clone, FromRow)]
pub struct Row {
    pub id: i64,
    pub iblock_id: i64,
    pub section_id: Option<i64>,
    pub code: String,
    pub xml_id: String,
    pub name: String,
    pub active: bool,
    pub sort: i32,
    pub preview_text: String,
    pub detail_text: String,
    pub preview_picture_id: Option<i64>,
    pub detail_picture_id: Option<i64>,
    pub published_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: Option<i64>,
    pub properties: Json<Map<String, Value>>,
}

impl Row {
    fn values(&self, code: &str) -> Vec<Value> {
        match self.properties.get(code) {
            Some(Value::Array(items)) => items.clone(),
            Some(Value::Null) | None => Vec::new(),
            Some(v) => vec![v.clone()],
        }
    }

    fn ids(&self, code: &str) -> Vec<i64> {
        self.values(code).iter().filter_map(Value::as_i64).collect()
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    List,
    Detail,
    /// Связанный элемент (`prop.element.*`): только запрошенные поля.
    Related,
}

pub struct Env<'a> {
    pub state: &'a AppState,
    pub snap: &'a Snapshot,
    pub project: &'a Project,
    pub image_resize: Option<&'a [u32]>,
}

/// Базовые поля, которые Битрикс отдаёт у элемента всегда.
const BASE_FIELDS: &[&str] = &[
    "id",
    "name",
    "code",
    "active",
    "sort",
    "dateCreate",
    "previewText",
    "detailText",
    "iblockSectionId",
];

pub fn format_date(dt: DateTime<Utc>) -> String {
    let msk = FixedOffset::east_opt(3 * 3600).expect("valid offset");
    dt.with_timezone(&msk)
        .format("%d.%m.%Y %H:%M:%S")
        .to_string()
}

pub async fn load_rows(state: &AppState, ids: &[i64]) -> Result<Vec<Row>, BxError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROW_COLS} FROM iblock_elements e WHERE e.id = ANY($1)"
    )))
    .bind(ids)
    .fetch_all(&state.db)
    .await?)
}

// ---------------------------------------------------------------------------
// Точка входа
// ---------------------------------------------------------------------------

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Сериализует строки одного инфоблока. В `Detail` добавляет характеристики,
/// в `List`/`Detail` — поля декораторов проекта.
pub fn serialize<'a>(
    env: &'a Env<'a>,
    schema: &'a Schema,
    rows: &'a [Row],
    select: &'a Select,
    mode: Mode,
    depth: usize,
) -> BoxFut<'a, Result<Vec<Map<String, Value>>, BxError>> {
    Box::pin(async move {
        let code = schema.iblock.code.as_str();
        let decorators = match mode {
            Mode::List => env.project.list_decorators(code),
            Mode::Detail => env.project.detail_decorators(code),
            Mode::Related => &[][..],
        };
        // Поля, нужные декораторам и деталке, добавляются к запрошенным
        let mut select = select.clone();
        for d in decorators {
            for path in d.required_select() {
                select.add(path, env.project, code);
            }
        }
        let characteristics = if mode == Mode::Detail {
            env.project.characteristics(code)
        } else {
            &[]
        };
        if mode == Mode::Detail {
            for prop in characteristics {
                select.add(&to_camel(prop), env.project, code);
            }
        }
        if schema.iblock.is_catalog && mode != Mode::Related && !select.has("stocks") {
            select.add("stocks", env.project, code);
        }

        let data = Loaded::load(env, schema, rows, &select, characteristics, depth).await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let mut item = Map::new();
            for field in &select.fields {
                if let Some(value) = data.field(env, schema, row, &field.root, &field.rests, mode) {
                    item.insert(field.root.clone(), value);
                }
            }
            if mode != Mode::Related {
                for base in BASE_FIELDS {
                    if !item.contains_key(*base) {
                        item.insert(
                            base.to_string(),
                            base_value(schema, row, base).unwrap_or(Value::Null),
                        );
                    }
                }
            }
            if !characteristics.is_empty() {
                item.insert(
                    "characteristics".into(),
                    data.characteristics(env, schema, row, characteristics),
                );
            }
            items.push(item);
        }
        for d in decorators {
            d.decorate(env.state, &mut items).await?;
        }
        Ok(items)
    })
}

fn base_value(schema: &Schema, row: &Row, field: &str) -> Option<Value> {
    Some(match field {
        "id" => Value::from(row.id),
        "name" => Value::from(row.name.clone()),
        "code" => Value::from(row.code.clone()),
        "xmlId" | "externalId" => Value::from(row.xml_id.clone()),
        "active" => Value::from(row.active),
        "sort" => Value::from(row.sort),
        "dateCreate" => Value::from(format_date(row.created_at)),
        "timestampX" => Value::from(format_date(row.updated_at)),
        "activeFrom" => row
            .published_at
            .map_or(Value::Null, |d| Value::from(format_date(d))),
        "previewText" => Value::from(row.preview_text.clone()),
        "detailText" => Value::from(row.detail_text.clone()),
        "iblockSectionId" => Value::from(row.section_id.unwrap_or(0)),
        "iblockId" => Value::from(row.iblock_id),
        "createdBy" => row.created_by.map_or(Value::Null, Value::from),
        "sectionName" => Value::from(
            row.section_id
                .and_then(|id| schema.sections.get(&id))
                .map(|s| s.name.clone())
                .unwrap_or_default(),
        ),
        "sectionCode" => Value::from(
            row.section_id
                .and_then(|id| schema.sections.get(&id))
                .map(|s| s.code.clone())
                .unwrap_or_default(),
        ),
        "detailPageUrl" => Value::from(detail_page_url(schema, row)),
        _ => return None,
    })
}

/// URL по шаблону инфоблока, как `CIBlock::ReplaceDetailUrl`.
pub fn detail_page_url(schema: &Schema, row: &Row) -> String {
    let template = &schema.iblock.detail_page_url;
    if template.is_empty() {
        return String::new();
    }
    let chain = row
        .section_id
        .map(|id| schema.section_chain(id))
        .unwrap_or_default();
    let section_path: Vec<&str> = chain.iter().map(|s| s.code.as_str()).collect();
    let section = chain.last();
    let url = template
        .replace("#SITE_DIR#", "")
        .replace("#SECTION_CODE_PATH#", &section_path.join("/"))
        .replace("#SECTION_CODE#", section.map_or("", |s| s.code.as_str()))
        .replace(
            "#SECTION_ID#",
            &section.map_or(String::new(), |s| s.id.to_string()),
        )
        .replace("#ELEMENT_ID#", &row.id.to_string())
        .replace("#ID#", &row.id.to_string())
        .replace("#ELEMENT_CODE#", &row.code)
        .replace("#CODE#", &row.code)
        .replace("#EXTERNAL_ID#", &row.xml_id)
        .replace("#IBLOCK_CODE#", &schema.iblock.code);
    collapse_slashes(&url)
}

fn collapse_slashes(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for ch in url.chars() {
        if ch == '/' && out.ends_with('/') {
            continue;
        }
        out.push(ch);
    }
    out
}

// ---------------------------------------------------------------------------
// Пакетная загрузка связанных данных
// ---------------------------------------------------------------------------

#[derive(FromRow)]
struct Price {
    element_id: i64,
    type_id: i64,
    type_name: String,
    is_base: bool,
    price: f64,
    currency: String,
    quantity_from: Option<i32>,
    quantity_to: Option<i32>,
}

#[derive(Default)]
struct Loaded {
    files: HashMap<i64, FileRecord>,
    /// Связанные элементы (привязки и справочники) по id.
    linked: HashMap<i64, Row>,
    /// Сериализованные связанные элементы по корню свойства: id → поля.
    related: HashMap<String, HashMap<i64, Map<String, Value>>>,
    prices: HashMap<i64, Vec<Price>>,
    stocks: HashMap<i64, Vec<(i64, f64)>>,
    quantities: HashMap<i64, f64>,
}

impl Loaded {
    async fn load(
        env: &Env<'_>,
        schema: &Schema,
        rows: &[Row],
        select: &Select,
        characteristics: &[&str],
        depth: usize,
    ) -> Result<Loaded, BxError> {
        let mut loaded = Loaded::default();
        let row_ids: Vec<i64> = rows.iter().map(|r| r.id).collect();

        // Файлы: картинки элемента и файловые свойства
        let mut file_ids: HashSet<i64> = HashSet::new();
        let wants_images = select.has("image") || select.has("imageExt");
        let image_prop = env
            .project
            .image_source(&schema.iblock.code)
            .map(|s| s.property);
        for row in rows {
            if wants_images {
                file_ids.extend(row.preview_picture_id);
                file_ids.extend(row.detail_picture_id);
                if let Some(p) = image_prop {
                    file_ids.extend(row.ids(p).first());
                }
            }
            for field in &select.fields {
                if let Some(prop) = schema
                    .prop(&to_snake(&field.root))
                    .filter(|p| p.kind == "file")
                {
                    file_ids.extend(row.ids(&prop.code));
                }
            }
        }
        let file_ids: Vec<i64> = file_ids.into_iter().collect();
        loaded.files = files::get_many(&env.state.db, &file_ids)
            .await?
            .into_iter()
            .map(|f| (f.id, f))
            .collect();

        // Связанные строки: справочники, привязки в характеристиках и `.element.*`
        let mut linked_ids: HashSet<i64> = HashSet::new();
        let mut related_selects: Vec<(&Property, Select)> = Vec::new();
        for field in &select.fields {
            let Some(prop) = schema
                .prop(&to_snake(&field.root))
                .filter(|p| p.kind == "element")
            else {
                continue;
            };
            let deep: Vec<&Vec<String>> = field
                .rests
                .iter()
                .filter(|r| {
                    r.first().map(String::as_str) == Some("element")
                        && r.len() > 1
                        && r[1..] != ["id".to_string()]
                })
                .collect();
            if prop.user_type == "directory" || !deep.is_empty() {
                for row in rows {
                    linked_ids.extend(row.ids(&prop.code));
                }
            }
            if !deep.is_empty() {
                let mut sub = Select::default();
                for rest in deep {
                    sub.add(&rest[1..].join("."), env.project, "");
                }
                related_selects.push((prop, sub));
            }
        }
        for code in characteristics {
            if let Some(prop) = schema.prop(code).filter(|p| p.kind == "element") {
                for row in rows {
                    linked_ids.extend(row.ids(&prop.code));
                }
            }
        }
        let linked_ids: Vec<i64> = linked_ids.into_iter().collect();
        loaded.linked = load_rows(env.state, &linked_ids)
            .await?
            .into_iter()
            .map(|r| (r.id, r))
            .collect();

        // Вложенные цепочки `prop.element.<поля>` — рекурсивно, до потолка глубины
        if depth < super::filter::MAX_DEPTH {
            for (prop, sub) in related_selects {
                let ids: HashSet<i64> = rows.iter().flat_map(|r| r.ids(&prop.code)).collect();
                let mut by_iblock: HashMap<i64, Vec<Row>> = HashMap::new();
                for id in ids {
                    if let Some(row) = loaded.linked.get(&id) {
                        by_iblock
                            .entry(row.iblock_id)
                            .or_default()
                            .push(row.clone());
                    }
                }
                let mut out = HashMap::new();
                for (iblock_id, group) in by_iblock {
                    let Some(linked_schema) = env.snap.get(iblock_id).cloned() else {
                        continue;
                    };
                    let items =
                        serialize(env, &linked_schema, &group, &sub, Mode::Related, depth + 1)
                            .await?;
                    for (row, item) in group.iter().zip(items) {
                        out.insert(row.id, item);
                    }
                }
                loaded.related.insert(to_camel(&prop.code), out);
            }
        }

        // Каталог: цены, склады, остатки
        if schema.iblock.is_catalog && !row_ids.is_empty() {
            if select.has("catalogPrice") {
                let prices: Vec<Price> = sqlx::query_as(
                    "SELECT p.element_id, p.price_type_id AS type_id, t.name AS type_name, t.is_base,
                            p.price::float8 AS price, p.currency, p.quantity_from, p.quantity_to
                     FROM catalog_prices p JOIN catalog_price_types t ON t.id = p.price_type_id
                     WHERE p.element_id = ANY($1)
                     ORDER BY p.element_id, t.sort, p.price_type_id, p.quantity_from NULLS FIRST",
                )
                .bind(&row_ids)
                .fetch_all(&env.state.db)
                .await?;
                for price in prices {
                    loaded
                        .prices
                        .entry(price.element_id)
                        .or_default()
                        .push(price);
                }
            }
            if select.has("stocks") {
                let amounts: Vec<(i64, i64, f64)> = sqlx::query_as(
                    "SELECT element_id, store_id, amount::float8 FROM catalog_store_amounts
                     WHERE element_id = ANY($1)",
                )
                .bind(&row_ids)
                .fetch_all(&env.state.db)
                .await?;
                for (el, store, amount) in amounts {
                    loaded.stocks.entry(el).or_default().push((store, amount));
                }
            }
            if select.has("catalogQuantity") {
                let qty: Vec<(i64, f64)> = sqlx::query_as(
                    "SELECT element_id, quantity::float8 FROM catalog_products WHERE element_id = ANY($1)",
                )
                .bind(&row_ids)
                .fetch_all(&env.state.db)
                .await?;
                loaded.quantities = qty.into_iter().collect();
            }
        }
        Ok(loaded)
    }

    // -----------------------------------------------------------------------
    // Поля
    // -----------------------------------------------------------------------

    fn field(
        &self,
        env: &Env,
        schema: &Schema,
        row: &Row,
        root: &str,
        rests: &[Vec<String>],
        mode: Mode,
    ) -> Option<Value> {
        match root {
            "previewPicture" | "detailPicture" => None,
            "image" => Some(self.main_image(env, schema, row, mode)),
            "imageExt" => Some(json!({
                "small": self.image(env, row.preview_picture_id),
                "large": self.image(env, row.detail_picture_id),
            })),
            "catalogPrice" if schema.iblock.is_catalog => {
                Some(self.catalog_price(env.snap, row.id))
            }
            "stocks" if schema.iblock.is_catalog => Some(self.stocks_value(env.snap, row.id)),
            "catalogQuantity" if schema.iblock.is_catalog => Some(
                self.quantities
                    .get(&row.id)
                    .map_or(Value::Null, |q| number_value(*q)),
            ),
            "catalogPrice" | "stocks" | "catalogQuantity" => None,
            _ => {
                if let Some(v) = base_value(schema, row, root) {
                    return Some(v);
                }
                match schema.prop(&to_snake(root)) {
                    Some(prop) if rests.is_empty() => Some(self.raw_value(env, prop, row)),
                    Some(prop) => Some(self.path_value(env, prop, row, root, rests)),
                    // Неизвестное поле — null, как у Битрикса
                    None => Some(Value::Null),
                }
            }
        }
    }

    fn image(&self, env: &Env, id: Option<i64>) -> Value {
        images::image_value(
            id.and_then(|id| self.files.get(&id)),
            env.image_resize,
            &env.project.image_widths,
        )
    }

    /// `image`: анонс; у проектов с источником из свойства — по его правилу.
    fn main_image(&self, env: &Env, schema: &Schema, row: &Row, mode: Mode) -> Value {
        let code = schema.iblock.code.as_str();
        let from_prop = env
            .project
            .image_source(code)
            .and_then(|s| row.ids(s.property).first().copied());
        let fallback = env.project.image_source(code).is_some_and(|s| s.fallback);
        let mut candidates: Vec<Option<i64>> = Vec::new();
        if mode == Mode::Detail && env.project.detail_image_first(code) {
            candidates.push(row.detail_picture_id);
        }
        if fallback {
            candidates.extend([row.preview_picture_id, row.detail_picture_id, from_prop]);
        } else if from_prop.is_some() {
            candidates.extend([from_prop, row.preview_picture_id]);
        } else {
            candidates.push(row.preview_picture_id);
        }
        let id = candidates
            .into_iter()
            .flatten()
            .find(|id| self.files.contains_key(id));
        self.image(env, id)
    }

    /// Значение свойства без пути — как `CODE` в select Битрикса.
    fn raw_value(&self, env: &Env, prop: &Property, row: &Row) -> Value {
        let values = row.values(&prop.code);
        let directory = prop.user_type == "directory";
        let items: Vec<Value> = values
            .iter()
            .filter_map(|v| match prop.kind.as_str() {
                "list" => v
                    .as_i64()
                    .and_then(|id| env.snap.enum_value(id))
                    .map(|e| Value::from(e.value.clone())),
                "element" if directory => v
                    .as_i64()
                    .and_then(|id| self.linked.get(&id))
                    .map(|r| Value::from(r.xml_id.clone())),
                "file" => v
                    .as_i64()
                    .and_then(|id| self.files.get(&id))
                    .map(|f| json!({ "src": images::url(f), "alt": "" })),
                _ => Some(v.clone()),
            })
            .collect();
        if prop.multiple {
            return if items.is_empty() {
                Value::Null
            } else {
                Value::Array(items)
            };
        }
        let first = items.into_iter().next();
        match (prop.kind.as_str(), first) {
            (_, Some(v)) => v,
            ("string" | "text", None) => Value::from(""),
            ("list", None) => Value::from(0),
            ("element", None) if !directory => Value::from(0),
            ("boolean", None) => Value::Bool(false),
            _ => Value::Null,
        }
    }

    /// Значение по путям (`.item.xmlId`, `.element.name`...): один путь — скаляр,
    /// несколько — объект по последнему сегменту (у `.element.*` — по корню подпути).
    fn path_value(
        &self,
        env: &Env,
        prop: &Property,
        row: &Row,
        root: &str,
        rests: &[Vec<String>],
    ) -> Value {
        let directory = prop.user_type == "directory";
        let per_item = |stored: &Value| -> Value {
            let mut obj = Map::new();
            let mut single = None;
            for rest in rests {
                let (key, value) = self.rest_value(env, prop, directory, root, stored, rest);
                if rests.len() == 1 {
                    single = Some(value);
                } else if !obj.contains_key(&key) {
                    obj.insert(key, value);
                }
            }
            single.unwrap_or(Value::Object(obj))
        };
        let values = row.values(&prop.code);
        let items: Vec<Value> = values.iter().map(per_item).collect();
        if prop.multiple {
            if items.is_empty() {
                Value::Null
            } else {
                Value::Array(items)
            }
        } else {
            items.into_iter().next().unwrap_or(Value::Null)
        }
    }

    fn rest_value(
        &self,
        env: &Env,
        prop: &Property,
        directory: bool,
        root: &str,
        stored: &Value,
        rest: &[String],
    ) -> (String, Value) {
        let id = stored.as_i64();
        let segs: Vec<&str> = rest.iter().map(String::as_str).collect();
        match (prop.kind.as_str(), segs.as_slice()) {
            ("list", ["item", field]) => {
                let e = id.and_then(|id| env.snap.enum_value(id));
                let v = match (*field, e) {
                    ("value", Some(e)) => Value::from(e.value.clone()),
                    ("xmlId", Some(e)) => Value::from(e.xml_id.clone()),
                    ("id", Some(e)) => Value::from(e.id),
                    ("sort", Some(e)) => Value::from(e.sort),
                    _ => Value::Null,
                };
                (field.to_string(), v)
            }
            ("element", ["item", field]) if directory => {
                let linked = id.and_then(|id| self.linked.get(&id));
                let v = linked.map_or(Value::Null, |r| match *field {
                    "ufXmlId" => Value::from(r.xml_id.clone()),
                    "id" => Value::from(r.id),
                    f => match r.properties.get(&to_snake(f)) {
                        Some(v) => v.clone(),
                        None if f == "ufName" => Value::from(r.name.clone()),
                        None => Value::Null,
                    },
                });
                (field.to_string(), v)
            }
            ("element", ["element"] | ["element", "id"]) => {
                ("id".into(), id.map_or(Value::Null, Value::from))
            }
            ("element", ["element", sub @ ..]) => {
                let key = sub[0].to_string();
                let v = id
                    .and_then(|id| self.related.get(root).and_then(|m| m.get(&id)))
                    .and_then(|m| m.get(&key).cloned())
                    .unwrap_or(Value::Null);
                (key, v)
            }
            (_, ["value"]) => ("value".into(), stored.clone()),
            (_, [.., last]) => (last.to_string(), Value::Null),
            (_, []) => (String::new(), stored.clone()),
        }
    }

    fn catalog_price(&self, snap: &Snapshot, element_id: i64) -> Value {
        let Some(prices) = self.prices.get(&element_id).filter(|p| !p.is_empty()) else {
            return Value::Array(Vec::new()); // Битрикс отдаёт пустой массив
        };
        let main = prices.iter().find(|p| p.is_base).unwrap_or(&prices[0]);
        let by_type: Vec<Value> = prices
            .iter()
            .map(|p| {
                json!({
                    "priceTypeId": p.type_id,
                    "name": p.type_name,
                    "value": number_value(p.price),
                    "currency": p.currency,
                    "formatted": format_price(p.price, snap.currencies.get(&p.currency)),
                    "quantityFrom": p.quantity_from,
                    "quantityTo": p.quantity_to,
                })
            })
            .collect();
        json!({
            "value": number_value(main.price),
            "currency": main.currency,
            "formatted": format_price(main.price, snap.currencies.get(&main.currency)),
            "byType": by_type,
        })
    }

    /// Остатки по активным складам, где у товара есть запись; без записей — `[]`.
    fn stocks_value(&self, snap: &Snapshot, element_id: i64) -> Value {
        let Some(amounts) = self.stocks.get(&element_id) else {
            return Value::Array(Vec::new());
        };
        let items: Vec<Value> = snap
            .stores
            .iter()
            .filter(|s| s.active)
            .filter_map(|s| {
                let (_, amount) = amounts.iter().find(|(id, _)| *id == s.id)?;
                Some(json!({ "storeName": s.name, "amount": number_value(*amount) }))
            })
            .collect();
        Value::Array(items)
    }

    fn characteristics(&self, env: &Env, schema: &Schema, row: &Row, codes: &[&str]) -> Value {
        let mut out = Vec::new();
        for code in codes {
            let Some(prop) = schema.prop(code) else {
                continue;
            };
            let values = row.values(&prop.code);
            let texts: Vec<String> = values
                .iter()
                .filter_map(|v| match prop.kind.as_str() {
                    "list" => v
                        .as_i64()
                        .and_then(|id| env.snap.enum_value(id))
                        .map(|e| e.value.clone()),
                    "element" => v
                        .as_i64()
                        .and_then(|id| self.linked.get(&id))
                        .map(|r| r.name.clone()),
                    "file" => None,
                    _ => match v {
                        Value::String(s) => Some(s.clone()),
                        Value::Null => None,
                        other => Some(other.to_string()),
                    },
                })
                .filter(|s| !s.trim().is_empty())
                .collect();
            if !texts.is_empty() {
                out.push(json!({ "label": prop.name, "value": texts.join(", ") }));
            }
        }
        Value::Array(out)
    }
}

/// Число в JSON: целое без дробной части, иначе дробное.
fn number_value(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9e15 {
        Value::from(n as i64)
    } else {
        Value::from((n * 100.0).round() / 100.0)
    }
}

/// Цена по формату валюты: `597.40 &#8381;`, `3&nbsp;949 &#8381;`.
pub fn format_price(value: f64, currency: Option<&Currency>) -> String {
    let Some(c) = currency else {
        return format!("{value:.2}");
    };
    let decimals = if c.hide_zero && value.fract() == 0.0 {
        0
    } else {
        c.decimals.max(0) as usize
    };
    let fixed = format!("{:.*}", decimals, value.abs());
    let (int_part, frac_part) = fixed.split_once('.').unwrap_or((&fixed, ""));
    let mut grouped = String::new();
    for (i, ch) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i) % 3 == 0 {
            grouped.push_str(&c.thousands_sep);
        }
        grouped.push(ch);
    }
    let mut number = if value < 0.0 {
        format!("-{grouped}")
    } else {
        grouped
    };
    if !frac_part.is_empty() {
        number.push_str(&c.dec_point);
        number.push_str(frac_part);
    }
    // «#» — место числа; «&#8381;» и прочие HTML-сущности не трогаем
    let mut out = String::with_capacity(c.format_string.len() + number.len());
    let mut prev = '\0';
    for ch in c.format_string.chars() {
        if ch == '#' && prev != '&' {
            out.push_str(&number);
        } else {
            out.push(ch);
        }
        prev = ch;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rub() -> Currency {
        Currency {
            code: "RUB".into(),
            format_string: "# &#8381;".into(),
            dec_point: ".".into(),
            thousands_sep: "&nbsp;".into(),
            decimals: 2,
            hide_zero: true,
        }
    }

    #[test]
    fn prices() {
        assert_eq!(format_price(597.4, Some(&rub())), "597.40 &#8381;");
        assert_eq!(format_price(3949.0, Some(&rub())), "3&nbsp;949 &#8381;");
        assert_eq!(
            format_price(1234567.5, Some(&rub())),
            "1&nbsp;234&nbsp;567.50 &#8381;"
        );
        assert_eq!(number_value(597.4), json!(597.4));
        assert_eq!(number_value(35595.0), json!(35595));
    }

    #[test]
    fn dates_and_urls() {
        let dt = DateTime::parse_from_rfc3339("2026-07-24T08:45:07Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(format_date(dt), "24.07.2026 11:45:07");
        assert_eq!(collapse_slashes("//catalog//a/1/"), "/catalog/a/1/");
    }
}
