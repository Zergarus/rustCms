use std::collections::HashMap;

use axum::{
    Extension,
    extract::{Multipart, Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::NaiveDateTime;
use minijinja::context;
use serde::{Deserialize, Serialize};
use serde_json::Map;

use super::{parse_sort, read_upload_form, render};
use crate::{
    access::{Access, Level},
    catalog::{self, PurchaseInput},
    collection::{
        Collection, Field, FieldOption, Item, ItemInput, Section, fields, is_valid_slug, repo,
        section_tree, sku, slugify,
    },
    error::{AppError, AppResult, is_unique_violation},
    files::{self, FileRecord},
    state::AppState,
};

const PER_PAGE: i64 = 50;
const DATETIME_FORMAT: &str = "%Y-%m-%dT%H:%M";
/// Картинки записи — одиночные файловые поля.
const PICTURE_FIELDS: [&str; 2] = ["preview_picture_id", "detail_picture_id"];

/// Значения формы записи: name, code, xml_id, section_id, active, sort, preview_text,
/// detail_text, published_at, preview_picture_id, detail_picture_id и `prop_<код>`.
/// Ключ может повторяться (множественный список, файлы).
#[derive(Debug, Default)]
struct FormValues(HashMap<String, Vec<String>>);

impl FormValues {
    fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut form = Self::default();
        for (key, value) in pairs {
            form.0.entry(key).or_default().push(value);
        }
        form
    }

    fn get(&self, key: &str) -> &str {
        self.0
            .get(key)
            .and_then(|v| v.first())
            .map(|s| s.trim())
            .unwrap_or("")
    }

    fn all(&self, key: &str) -> Vec<&str> {
        self.0
            .get(key)
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    fn set(&mut self, key: &str, value: impl Into<String>) {
        self.0.insert(key.to_string(), vec![value.into()]);
    }

    /// Первые значения — для обычных полей в шаблоне.
    fn first_values(&self) -> HashMap<&str, &str> {
        self.0
            .iter()
            .filter_map(|(k, v)| Some((k.as_str(), v.first()?.as_str())))
            .collect()
    }

    /// Раскладывает загруженные файлы по полям: `upload_<поле>` → `<поле>`.
    /// В одиночное поле новый файл встаёт вместо старого, в множественное — добавляется.
    fn apply_uploads(&mut self, uploads: Vec<(String, FileRecord)>, properties: &[Field]) {
        for (field, file) in uploads {
            let Some(key) = field.strip_prefix("upload_") else {
                continue;
            };
            let prop = properties
                .iter()
                .find(|p| p.kind == "file" && key == format!("prop_{}", p.code));
            if prop.is_none() && !PICTURE_FIELDS.contains(&key) {
                continue;
            }
            let values = self.0.entry(key.to_string()).or_default();
            if !prop.is_some_and(|p| p.multiple) {
                values.clear();
            }
            values.push(file.id.to_string());
        }
    }
}

fn parse_optional_id(raw: &str, what: &str, errors: &mut Vec<String>) -> Option<i64> {
    match raw {
        "" => None,
        raw => match raw.parse::<i64>() {
            Ok(id) if id > 0 => Some(id),
            _ => {
                errors.push(format!("{what}: неверный id"));
                None
            }
        },
    }
}

fn build_input(
    form: &FormValues,
    properties: &[Field],
    enums: &[FieldOption],
    sections: &[Section],
) -> Result<ItemInput, String> {
    let get = |key: &str| form.get(key);
    let mut errors = Vec::new();

    let name = get("name");
    if name.is_empty() {
        errors.push("Укажите название".to_string());
    }
    let code = match get("code") {
        "" => slugify(name),
        code => code.to_string(),
    };
    if !code.is_empty() && !is_valid_slug(&code) {
        errors.push("Код: только латиница, цифры, «_» и «-»".into());
    }
    let section_id = parse_optional_id(get("section_id"), "Раздел", &mut errors);
    if section_id.is_some_and(|id| !sections.iter().any(|s| s.id == id)) {
        errors.push("Раздел не найден в этой коллекции".into());
    }
    let preview_picture_id =
        parse_optional_id(get("preview_picture_id"), "Картинка анонса", &mut errors);
    let detail_picture_id =
        parse_optional_id(get("detail_picture_id"), "Детальная картинка", &mut errors);
    let published_at = match get("published_at") {
        "" => None,
        raw => match NaiveDateTime::parse_from_str(raw, DATETIME_FORMAT) {
            Ok(dt) => Some(dt.and_utc()),
            Err(_) => {
                errors.push("Неверная дата публикации".into());
                None
            }
        },
    };

    let mut values = Map::new();
    for prop in properties {
        let prop_enums: Vec<FieldOption> = enums
            .iter()
            .filter(|e| e.field_id == prop.id)
            .cloned()
            .collect();
        let raws = form.all(&format!("prop_{}", prop.code));
        match fields::parse_value(prop, &raws, &prop_enums) {
            Ok(value) => {
                values.insert(prop.code.clone(), value);
            }
            Err(e) => errors.push(e),
        }
    }

    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(ItemInput {
        section_id,
        code,
        xml_id: get("xml_id").to_string(),
        name: name.to_string(),
        active: form.0.contains_key("active"),
        sort: parse_sort(get("sort")),
        preview_text: get("preview_text").to_string(),
        detail_text: get("detail_text").to_string(),
        preview_picture_id,
        detail_picture_id,
        published_at,
        field_values: values,
        product_id: None,
    })
}

/// Проверки, которым нужна БД: привязанные записи и файлы существуют.
async fn check_references(
    state: &AppState,
    properties: &[Field],
    input: &ItemInput,
) -> AppResult<Result<(), String>> {
    let mut errors = Vec::new();
    let mut file_ids: Vec<i64> = [input.preview_picture_id, input.detail_picture_id]
        .into_iter()
        .flatten()
        .collect();
    for prop in properties {
        let Some(value) = input.field_values.get(&prop.code) else {
            continue;
        };
        let ids = fields::ids(value);
        match prop.kind.as_str() {
            "element" if !ids.is_empty() => {
                let found = repo::item_names(&state.db, &ids, prop.link_collection_id).await?;
                let missing: Vec<String> = ids
                    .iter()
                    .filter(|id| !found.iter().any(|(f, _)| f == *id))
                    .map(i64::to_string)
                    .collect();
                if !missing.is_empty() {
                    errors.push(format!(
                        "«{}»: нет записей с id {}",
                        prop.name,
                        missing.join(", ")
                    ));
                }
            }
            "file" => file_ids.extend(ids),
            _ => {}
        }
    }
    let found = files::get_many(&state.db, &file_ids).await?;
    if file_ids.iter().any(|id| !found.iter().any(|f| f.id == *id)) {
        errors.push("Часть файлов не найдена — загрузите их заново".into());
    }
    Ok(if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    })
}

fn element_to_form(element: &Item, properties: &[Field]) -> FormValues {
    let mut form = FormValues::default();
    form.set("name", &element.name);
    form.set("code", &element.code);
    form.set("xml_id", &element.xml_id);
    form.set("sort", element.sort.to_string());
    form.set("preview_text", &element.preview_text);
    form.set("detail_text", &element.detail_text);
    if element.active {
        form.set("active", "on");
    }
    for (key, id) in [
        ("section_id", element.section_id),
        ("preview_picture_id", element.preview_picture_id),
        ("detail_picture_id", element.detail_picture_id),
    ] {
        if let Some(id) = id {
            form.set(key, id.to_string());
        }
    }
    if let Some(dt) = element.published_at {
        form.set("published_at", dt.format(DATETIME_FORMAT).to_string());
    }
    for prop in properties {
        if let Some(value) = element.field_values.get(&prop.code) {
            form.0.insert(
                format!("prop_{}", prop.code),
                fields::to_form_values(prop, value),
            );
        }
    }
    form
}

/// Всё, что нужно форме записи кроме самих значений.
struct CollectionContext {
    collection: Collection,
    properties: Vec<Field>,
    enums: Vec<FieldOption>,
    /// Разделы в порядке дерева.
    sections: Vec<Section>,
    /// Для торгового каталога — типы цен и склады вкладки «Торговый каталог».
    catalog: Option<CatalogContext>,
    /// Для коллекции товаров — её коллекция предложений.
    offers: Option<Collection>,
    /// Для коллекции предложений — коллекция товаров.
    product_collection: Option<Collection>,
}

#[derive(Serialize)]
struct CatalogContext {
    /// (id, название)
    price_types: Vec<(i64, String)>,
    /// (id, название, активен)
    stores: Vec<(i64, String, bool)>,
}

/// Вкладка «Торговый каталог» из полей формы: `price_<тип>`, `amount_<склад>`,
/// `available`, `quantity_trace` / `can_buy_zero` (`Y` | `N` | `default`).
fn purchase_input_from_form(
    form: &FormValues,
    price_types: &[i64],
    stores: &[i64],
) -> Result<PurchaseInput, String> {
    let number = |raw: &str| {
        raw.trim()
            .replace(',', ".")
            .replace(' ', "")
            .parse::<f64>()
            .ok()
    };
    let mut prices = Vec::new();
    for t in price_types {
        let raw = form.get(&format!("price_{t}"));
        if raw.is_empty() {
            prices.push((*t, None));
        } else {
            let v = number(raw)
                .filter(|v| *v >= 0.0)
                .ok_or("Цена: ожидается число")?;
            prices.push((*t, Some(v)));
        }
    }
    let quantity = match form.get("quantity") {
        "" => None,
        raw => Some(
            number(raw)
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or("Доступное количество: ожидается число не меньше нуля")?,
        ),
    };
    let mut amounts = Vec::new();
    for st in stores {
        let raw = form.get(&format!("amount_{st}"));
        if !raw.is_empty() {
            amounts.push((*st, number(raw).ok_or("Остаток: ожидается число")?));
        }
    }
    let weight = match form.get("weight") {
        "" => 0.0,
        raw => number(raw)
            .filter(|v| v.is_finite() && *v >= 0.0)
            .ok_or("Вес: ожидается число не меньше нуля")?,
    };
    let flag = |key: &str| match form.get(key) {
        "Y" => Some(true),
        "N" => Some(false),
        _ => None,
    };
    Ok(PurchaseInput {
        prices,
        amounts,
        quantity,
        available: form.0.contains_key("available"),
        weight,
        quantity_trace: flag("quantity_trace"),
        can_buy_zero: flag("can_buy_zero"),
    })
}

/// Текущие цены, остатки и флаги товара — в поля формы.
async fn purchase_to_form(state: &AppState, item_id: i64, form: &mut FormValues) -> AppResult<()> {
    let flag = |v: Option<bool>| match v {
        Some(true) => "Y",
        Some(false) => "N",
        None => "default",
    };
    match catalog::raw_flags(&state.db, item_id).await? {
        Some((available, trace, zero)) => {
            if available {
                form.set("available", "on");
            }
            form.set("quantity_trace", flag(trace));
            form.set("can_buy_zero", flag(zero));
        }
        None => {
            form.set("available", "on");
            form.set("quantity_trace", "default");
            form.set("can_buy_zero", "default");
        }
    }
    let prices = catalog::load_prices(&state.db, &[item_id])
        .await?
        .remove(&item_id)
        .unwrap_or_default();
    let tiered = catalog::tiered_price_types(&prices);
    for p in &prices {
        if tiered.contains(&p.type_id) {
            // Диапазоны по количеству показываются только для чтения
            let key = format!("tiered_{}", p.type_id);
            let range = match (p.quantity_from, p.quantity_to) {
                (Some(f), Some(t)) => format!("{f}–{t} шт.: "),
                (Some(f), None) => format!("от {f} шт.: "),
                (None, Some(t)) => format!("до {t} шт.: "),
                (None, None) => String::new(),
            };
            let text = [form.get(&key), &format!("{range}{}", p.price)]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            form.set(&key, text);
        } else {
            form.set(&format!("price_{}", p.type_id), p.price.to_string());
        }
    }
    let stock: Option<(f64, f64)> = sqlx::query_as(
        "SELECT quantity::float8, weight::float8 FROM catalog_products WHERE item_id = $1",
    )
    .bind(item_id)
    .fetch_optional(&state.db)
    .await?;
    if let Some((q, weight)) = stock {
        form.set("quantity", q.to_string());
        form.set("weight", weight.to_string());
    }
    let amounts: Vec<(i64, f64)> = sqlx::query_as(
        "SELECT store_id, amount::float8 FROM catalog_store_amounts WHERE item_id = $1",
    )
    .bind(item_id)
    .fetch_all(&state.db)
    .await?;
    for (store, amount) in amounts {
        form.set(&format!("amount_{store}"), amount.to_string());
    }
    Ok(())
}

async fn load_collection(state: &AppState, id: i64) -> AppResult<CollectionContext> {
    let collection = repo::get_collection(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let catalog = if collection.is_catalog {
        Some(CatalogContext {
            price_types: sqlx::query_as(
                "SELECT id, name FROM catalog_price_types ORDER BY sort, id",
            )
            .fetch_all(&state.db)
            .await?,
            stores: sqlx::query_as("SELECT id, name, active FROM catalog_stores ORDER BY sort, id")
                .fetch_all(&state.db)
                .await?,
        })
    } else {
        None
    };
    let product_collection = match collection.product_collection_id {
        Some(p) => repo::get_collection(&state.db, p).await?,
        None => None,
    };
    Ok(CollectionContext {
        offers: repo::offers_collection(&state.db, id).await?,
        product_collection,
        properties: repo::list_fields(&state.db, id).await?,
        enums: repo::list_collection_options(&state.db, id).await?,
        sections: section_tree(repo::list_sections(&state.db, id).await?),
        catalog,
        collection,
    })
}

#[derive(Serialize)]
struct FileView {
    id: i64,
    url: String,
    name: String,
    is_image: bool,
}

async fn render_form(
    state: &AppState,
    user: Access,
    ctx: CollectionContext,
    item_id: Option<i64>,
    form: FormValues,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let CollectionContext {
        collection,
        catalog,
        properties,
        enums,
        sections,
        offers,
        product_collection,
    } = ctx;

    // Файлы и подписи привязанных записей для текущих значений формы
    let mut file_ids: Vec<i64> = PICTURE_FIELDS
        .iter()
        .filter_map(|key| form.get(key).parse().ok())
        .collect();
    let mut linked = HashMap::new();
    for prop in &properties {
        let key = format!("prop_{}", prop.code);
        match prop.kind.as_str() {
            "file" => file_ids.extend(form.all(&key).iter().filter_map(|s| s.parse::<i64>().ok())),
            "element" => {
                let ids: Vec<i64> = form
                    .get(&key)
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let names = repo::item_names(&state.db, &ids, prop.link_collection_id).await?;
                linked.insert(key, names);
            }
            _ => {}
        }
    }
    let files: HashMap<String, FileView> = files::get_many(&state.db, &file_ids)
        .await?
        .into_iter()
        .map(|f| {
            let view = FileView {
                id: f.id,
                url: f.url(),
                is_image: f.is_image(),
                name: f.original_name,
            };
            (f.id.to_string(), view)
        })
        .collect();

    let enums: HashMap<String, Vec<FieldOption>> = properties
        .iter()
        .map(|p| {
            let items = enums
                .iter()
                .filter(|e| e.field_id == p.id)
                .cloned()
                .collect();
            (p.code.clone(), items)
        })
        .collect();
    // Все значения по ключу (для списков и файлов); пустые — чтобы шаблону не проверять наличие
    let mut multi: HashMap<String, Vec<String>> = properties
        .iter()
        .map(|p| (format!("prop_{}", p.code), Vec::new()))
        .collect();
    multi.extend(form.0.iter().map(|(k, v)| (k.clone(), v.clone())));

    let can_write = user.collection_level(collection.id) >= Level::Write;
    // У товара с предложениями цены и остатки задаются у предложений
    let offers_priced = match item_id {
        Some(id) => matches!(
            product_type(&state.db, id).await?,
            Some(catalog::TYPE_SKU | catalog::TYPE_EMPTY_SKU)
        ),
        None => false,
    };
    let offers_tab = match (&offers, item_id) {
        (Some(o), Some(id)) if user.collection_level(o.id) >= Level::Read => {
            Some(offers_tab(state, o, id).await?)
        }
        _ => None,
    };
    let can_write_offers = offers_tab.is_some()
        && can_write
        && offers
            .as_ref()
            .is_some_and(|o| user.collection_level(o.id) >= Level::Write);
    let offer_product = product_collection.as_ref().map(|c| c.name.clone());
    render(
        state,
        "item_form.html",
        context! {
            user, collection, properties, item_id, error, can_write, sections, enums, files, linked,
            multi, catalog, offers_priced, offers, offers_tab, can_write_offers, offer_product,
            form => form.first_values(),
        },
    )
}

/// Тип товара из `catalog_products`; `None` — записи каталога нет.
async fn product_type(db: &sqlx::PgPool, item_id: i64) -> sqlx::Result<Option<i16>> {
    sqlx::query_scalar("SELECT type FROM catalog_products WHERE item_id = $1")
        .bind(item_id)
        .fetch_optional(db)
        .await
}

#[derive(Serialize)]
struct OfferView {
    id: i64,
    name: String,
    active: bool,
    /// Значения полей выбора предложения, по порядку `OffersTab::fields`.
    tree: Vec<String>,
    price: Option<f64>,
    stock: f64,
}

#[derive(Serialize)]
struct OffersTab {
    /// Названия полей выбора предложения (`offer_tree`).
    fields: Vec<String>,
    rows: Vec<OfferView>,
}

/// Таблица предложений товара `product_id`: одна выборка на вкладку, подписи значений
/// полей выбора — по одному запросу на все предложения.
async fn offers_tab(
    state: &AppState,
    offers: &Collection,
    product_id: i64,
) -> AppResult<OffersTab> {
    let rows = repo::list_offer_rows(&state.db, offers.id, product_id).await?;
    let tree: Vec<Field> = repo::list_fields(&state.db, offers.id)
        .await?
        .into_iter()
        .filter(|f| f.offer_tree)
        .collect();
    let options = repo::list_collection_options(&state.db, offers.id).await?;
    let mut linked_ids = Vec::new();
    for f in tree.iter().filter(|f| f.kind == "element") {
        for row in &rows {
            if let Some(v) = row.field_values.0.get(&f.code) {
                linked_ids.extend(fields::ids(v));
            }
        }
    }
    let names = repo::item_names(&state.db, &linked_ids, None).await?;
    let label = |f: &Field, v: &serde_json::Value| -> String {
        match f.kind.as_str() {
            "list" => fields::ids(v)
                .iter()
                .filter_map(|id| options.iter().find(|o| o.id == *id))
                .map(|o| o.value.clone())
                .collect::<Vec<_>>()
                .join(", "),
            "element" => fields::ids(v)
                .iter()
                .filter_map(|id| names.iter().find(|(n, _)| n == id))
                .map(|(_, name)| name.clone())
                .collect::<Vec<_>>()
                .join(", "),
            _ => match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => String::new(),
            },
        }
    };
    let rows = rows
        .iter()
        .map(|row| OfferView {
            id: row.id,
            name: row.name.clone(),
            active: row.active,
            tree: tree
                .iter()
                .map(|f| {
                    row.field_values
                        .0
                        .get(&f.code)
                        .map(|v| label(f, v))
                        .unwrap_or_default()
                })
                .collect(),
            price: row.price,
            stock: row.stock,
        })
        .collect();
    Ok(OffersTab {
        fields: tree.into_iter().map(|f| f.name).collect(),
        rows,
    })
}

#[derive(Deserialize)]
pub struct ListQuery {
    page: Option<i64>,
    section: Option<i64>,
}

/// Список записей по разделам, как в Битриксе: подразделы текущего раздела,
/// затем его записи. В корне — разделы верхнего уровня и записи без раздела.
pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Query(q): Query<ListQuery>,
) -> AppResult<Html<String>> {
    user.require_collection(id, Level::Read)?;
    let collection = repo::get_collection(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let all_sections = repo::list_sections(&state.db, id).await?;
    let current = match q.section {
        Some(sid) => Some(
            all_sections
                .iter()
                .find(|s| s.id == sid)
                .cloned()
                .ok_or(AppError::NotFound)?,
        ),
        None => None,
    };
    let current_id = current.as_ref().map(|s| s.id);

    // Цепочка родителей для хлебных крошек
    let mut chain = Vec::new();
    let mut cursor = current.as_ref();
    while let Some(s) = cursor {
        chain.push(s.clone());
        cursor = s
            .parent_id
            .and_then(|p| all_sections.iter().find(|x| x.id == p));
    }
    chain.reverse();

    let subsections: Vec<Section> = all_sections
        .iter()
        .filter(|s| s.parent_id == current_id)
        .cloned()
        .collect();

    let page = q.page.unwrap_or(1).clamp(1, 1_000_000);
    let (items, total) = repo::list_items(
        &state.db,
        id,
        Some(current_id.unwrap_or(0)),
        PER_PAGE,
        (page - 1) * PER_PAGE,
    )
    .await?;
    let pages = ((total + PER_PAGE - 1) / PER_PAGE).max(1);
    render(
        &state,
        "items.html",
        context! {
            can_write => user.collection_level(id) >= Level::Write,
            user, collection, items, total, page, pages, subsections, chain,
        },
    )
}

#[derive(Deserialize)]
pub struct NewQuery {
    section: Option<i64>,
    /// Для предложения: товар (запись коллекции товаров), к которому оно добавляется.
    product: Option<i64>,
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Query(q): Query<NewQuery>,
) -> AppResult<Html<String>> {
    user.require_collection(id, Level::Write)?;
    let ctx = load_collection(&state, id).await?;
    let mut form = FormValues::default();
    form.set("active", "on");
    form.set("sort", "500");
    if ctx.catalog.is_some() {
        form.set("available", "on");
        form.set("quantity_trace", "default");
        form.set("can_buy_zero", "default");
    }
    if let Some(section) = q.section {
        form.set("section_id", section.to_string());
    }
    if let (Some(product), Some(field)) = (q.product, ctx.collection.sku_field_id)
        && let Some(f) = ctx.properties.iter().find(|f| f.id == field)
    {
        form.set(&format!("prop_{}", f.code), product.to_string());
    }
    // Варианты списков «по умолчанию»
    for prop in ctx.properties.iter().filter(|p| p.kind == "list") {
        let defaults: Vec<String> = ctx
            .enums
            .iter()
            .filter(|e| e.field_id == prop.id && e.is_default)
            .map(|e| e.id.to_string())
            .collect();
        form.0.insert(format!("prop_{}", prop.code), defaults);
    }
    render_form(&state, user, ctx, None, form, None).await
}

/// Разбирает отправленную форму и сохраняет запись (`item_id` = None — создание).
async fn save(
    state: &AppState,
    user: Access,
    ctx: CollectionContext,
    item_id: Option<i64>,
    multipart: Multipart,
) -> AppResult<Response> {
    let upload = read_upload_form(state, multipart, "iblock").await?;
    let mut form = FormValues::from_pairs(upload.fields);
    form.apply_uploads(upload.uploads, &ctx.properties);

    // У товара с предложениями цены, остатки и доступность принадлежат предложениям
    let offers_priced = match item_id {
        Some(id) => matches!(
            product_type(&state.db, id).await?,
            Some(catalog::TYPE_SKU | catalog::TYPE_EMPTY_SKU)
        ),
        None => false,
    };
    // Вкладка каталога проверяется вместе с формой — до любой записи
    let purchase = match &ctx.catalog {
        Some(_) if offers_priced => None,
        Some(c) => {
            // Цены с диапазонами по количеству вкладка не меняет
            let tiered = match item_id {
                Some(id) => catalog::tiered_price_types(
                    &catalog::load_prices(&state.db, &[id])
                        .await?
                        .remove(&id)
                        .unwrap_or_default(),
                ),
                None => Default::default(),
            };
            let types: Vec<i64> = c
                .price_types
                .iter()
                .map(|t| t.0)
                .filter(|t| !tiered.contains(t))
                .collect();
            let stores: Vec<i64> = c.stores.iter().map(|s| s.0).collect();
            Some(purchase_input_from_form(&form, &types, &stores))
        }
        None => None,
    };
    let purchase_error = purchase.as_ref().and_then(|p| p.as_ref().err().cloned());
    // Поле связи с товаром проверяется отдельно (build_input не знает про «Укажите товар»)
    let sku_field = ctx.collection.sku_field_id;
    let parse_props: Vec<Field> = ctx
        .properties
        .iter()
        .cloned()
        .map(|mut p| {
            if Some(p.id) == sku_field {
                p.is_required = false;
            }
            p
        })
        .collect();
    let result = match build_input(&form, &parse_props, &ctx.enums, &ctx.sections) {
        Ok(_) if purchase_error.is_some() => Err(purchase_error.clone().unwrap_or_default()),
        Ok(input) if upload.rejected.is_empty() => {
            match offer_link_error(state, &ctx, &input).await? {
                Some(e) => Err(e),
                None => match check_references(state, &ctx.properties, &input).await? {
                    Ok(()) => Ok(input),
                    Err(e) => Err(e),
                },
            }
        }
        Ok(_) => Err(upload.rejected.join("; ")),
        Err(e) if upload.rejected.is_empty() => Err(e),
        Err(e) => Err(format!("{e}; {}", upload.rejected.join("; "))),
    };
    let error = match result {
        Ok(mut input) => {
            // Связь предложения с товаром хранится в колонке, а не в field_values
            input.product_id =
                sku::take_link(&ctx.collection, &ctx.properties, &mut input.field_values);
            let saved = match item_id {
                Some(id) => repo::update_item(&state.db, id, &input).await.map(|_| id),
                None => repo::create_item(&state.db, ctx.collection.id, &input).await,
            };
            match saved {
                Ok(saved_id) => {
                    if let Some(Ok(p)) = &purchase {
                        catalog::save_purchase(&state.db, saved_id, p).await?;
                    }
                    let url = match input.product_id {
                        // Предложение: назад в карточку товара, на вкладку предложений
                        Some(product) => format!("/admin/items/{product}#offers"),
                        None => {
                            let mut url = format!("/admin/collections/{}/items", ctx.collection.id);
                            if let Some(section) = input.section_id {
                                url.push_str(&format!("?section={section}"));
                            }
                            url
                        }
                    };
                    return Ok(Redirect::to(&url).into_response());
                }
                Err(e) if is_unique_violation(&e) => "Запись с таким кодом уже есть".into(),
                Err(e) => return Err(e.into()),
            }
        }
        Err(msg) => msg,
    };
    let page = render_form(state, user, ctx, item_id, form, Some(error)).await?;
    Ok(page.into_response())
}

/// Предложение обязано ссылаться на существующий товар из коллекции товаров,
/// независимо от флага «обязательное» у поля связи.
async fn offer_link_error(
    state: &AppState,
    ctx: &CollectionContext,
    input: &ItemInput,
) -> AppResult<Option<String>> {
    let (Some(_), Some(parent)) = (
        ctx.collection.sku_field_id,
        ctx.collection.product_collection_id,
    ) else {
        return Ok(None);
    };
    let mut values = input.field_values.clone();
    let linked = sku::take_link(&ctx.collection, &ctx.properties, &mut values);
    let exists = match linked {
        Some(id) => !repo::item_names(&state.db, &[id], Some(parent))
            .await?
            .is_empty(),
        None => false,
    };
    if exists {
        return Ok(None);
    }
    let name = ctx
        .product_collection
        .as_ref()
        .map(|c| c.name.as_str())
        .unwrap_or_default();
    Ok(Some(format!("Укажите товар из коллекции {name}")))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    multipart: Multipart,
) -> AppResult<Response> {
    user.require_collection(id, Level::Write)?;
    let ctx = load_collection(&state, id).await?;
    save(&state, user, ctx, None, multipart).await
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let element = repo::get_item(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    user.require_collection(element.collection_id, Level::Read)?;
    let ctx = load_collection(&state, element.collection_id).await?;
    let mut form = element_to_form(&element, &ctx.properties);
    if ctx.catalog.is_some() {
        purchase_to_form(&state, id, &mut form).await?;
    }
    render_form(&state, user, ctx, Some(id), form, None).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    multipart: Multipart,
) -> AppResult<Response> {
    let element = repo::get_item(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    user.require_collection(element.collection_id, Level::Write)?;
    let ctx = load_collection(&state, element.collection_id).await?;
    save(&state, user, ctx, Some(id), multipart).await
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    let element = repo::get_item(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    user.require_collection(element.collection_id, Level::Write)?;
    let collection_id = repo::delete_item(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if let Some(product) = element.product_id {
        // Предложение: назад на вкладку предложений товара
        return Ok(Redirect::to(&format!("/admin/items/{product}#offers")));
    }
    let mut url = format!("/admin/collections/{collection_id}/items");
    if let Some(section) = element.section_id {
        url.push_str(&format!("?section={section}"));
    }
    Ok(Redirect::to(&url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purchase_input_from_form_fields() {
        let form = FormValues::from_pairs(
            [
                ("price_3", "597,40"),
                ("price_1", ""),
                ("amount_5", "3"),
                ("quantity_trace", "default"),
                ("can_buy_zero", "N"),
                ("available", "on"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        let input = purchase_input_from_form(&form, &[3, 1], &[5]).unwrap();
        assert_eq!(input.prices, vec![(3, Some(597.4)), (1, None)]);
        assert_eq!(input.amounts, vec![(5, 3.0)]);
        assert_eq!(input.quantity_trace, None);
        assert_eq!(input.can_buy_zero, Some(false));
        assert!(input.available);
        let bad = FormValues::from_pairs([("price_3".to_string(), "abc".to_string())]);
        assert_eq!(
            purchase_input_from_form(&bad, &[3], &[]).unwrap_err(),
            "Цена: ожидается число"
        );
    }

    #[test]
    fn purchase_input_total_quantity() {
        let form = FormValues::from_pairs([("quantity".to_string(), "50".to_string())]);
        assert_eq!(
            purchase_input_from_form(&form, &[], &[]).unwrap().quantity,
            Some(50.0)
        );
        let empty = FormValues::from_pairs([("quantity".to_string(), String::new())]);
        assert_eq!(
            purchase_input_from_form(&empty, &[], &[]).unwrap().quantity,
            None
        );
        let bad = FormValues::from_pairs([("quantity".to_string(), "-1".to_string())]);
        assert!(purchase_input_from_form(&bad, &[], &[]).is_err());
    }

    fn prop(code: &str, kind: &str, required: bool) -> Field {
        Field {
            id: 1,
            collection_id: 1,
            code: code.into(),
            name: code.into(),
            kind: kind.into(),
            is_required: required,
            sort: 500,
            multiple: false,
            link_collection_id: None,
            user_type: String::new(),
            in_basket: false,
            offer_tree: false,
        }
    }

    fn form(pairs: &[(&str, &str)]) -> FormValues {
        FormValues::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
    }

    #[test]
    fn autogenerates_code_and_parses_props() {
        let form = form(&[
            ("name", "Первая новость"),
            ("active", "on"),
            ("published_at", "2026-09-27T10:30"),
            ("prop_price", "99.5"),
        ]);
        let input = build_input(
            &form,
            &[prop("price", "number", true), prop("hot", "boolean", false)],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(input.code, "pervaya-novost");
        assert!(input.active);
        assert_eq!(input.field_values["price"], serde_json::json!(99.5));
        assert_eq!(input.field_values["hot"], serde_json::json!(false));
        assert!(input.published_at.is_some());
        assert_eq!(input.section_id, None);
    }

    #[test]
    fn collects_errors() {
        let form = form(&[("name", ""), ("section_id", "7")]);
        let err = build_input(&form, &[prop("price", "number", true)], &[], &[]).unwrap_err();
        assert!(err.contains("Укажите название"));
        assert!(err.contains("обязательное"));
        assert!(err.contains("Раздел"));
    }

    #[test]
    fn uploads_replace_single_and_append_multiple() {
        let mut photos = prop("photos", "file", false);
        photos.multiple = true;
        let props = [photos, prop("doc", "file", false)];
        let mut form = form(&[
            ("preview_picture_id", "1"),
            ("prop_photos", "2"),
            ("prop_doc", "3"),
        ]);
        let file = |id: i64| FileRecord {
            id,
            path: String::new(),
            original_name: String::new(),
            content_type: String::new(),
            size: 0,
            width: None,
            height: None,
            created_at: chrono::Utc::now(),
        };
        form.apply_uploads(
            vec![
                ("upload_preview_picture_id".into(), file(10)),
                ("upload_prop_photos".into(), file(11)),
                ("upload_prop_doc".into(), file(12)),
                ("upload_prop_unknown".into(), file(13)),
            ],
            &props,
        );
        assert_eq!(form.all("preview_picture_id"), ["10"]);
        assert_eq!(form.all("prop_photos"), ["2", "11"]);
        assert_eq!(form.all("prop_doc"), ["12"]);
        assert!(form.all("prop_unknown").is_empty());
    }
}
