use std::collections::HashMap;

use axum::{
    Extension,
    extract::{Multipart, Path, Query, RawQuery, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{NaiveDate, NaiveDateTime};
use minijinja::context;
use serde::{Deserialize, Serialize};
use serde_json::Map;

use super::{
    card::{TabErrors, tab_context},
    flash::{self, FlashKind},
    parse_sort, read_upload_form, render,
};
use crate::{
    access::{Access, COLLECTIONS_MANAGE, Level},
    admin::list::{
        collection::{collection_spec, section_scope},
        render::{Action, ListPage, run_list},
        state::{self as list_state, SectionSel},
        tree, views,
    },
    catalog::{self, PurchaseInput},
    collection::{
        Collection, Field, FieldOption, Item, ItemInput, Section, fields, is_valid_slug, repo,
        section_tree, sku, slugify,
    },
    error::{AppError, AppResult, is_unique_violation},
    files::{self, FileRecord},
    state::AppState,
    stock::{self, StockError},
};

/// Вкладки карточки записи, на которые можно вернуться после сохранения.
const CARD_TABS: [&str; 6] = ["main", "text", "fields", "catalog", "offers", "seo"];
const DATETIME_FORMAT: &str = "%Y-%m-%dT%H:%M";
/// Картинки записи — одиночные файловые поля.
const PICTURE_FIELDS: [&str; 2] = ["preview_picture_id", "detail_picture_id"];

/// Значения формы записи: name, code, xml_id, section_id, active, sort, preview_text,
/// detail_text, published_at, preview_picture_id, detail_picture_id и `prop_<код>`.
/// Ключ может повторяться (множественный список, файлы).
#[derive(Debug, Default)]
pub(super) struct FormValues(HashMap<String, Vec<String>>);

impl FormValues {
    pub(super) fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut form = Self::default();
        for (key, value) in pairs {
            form.0.entry(key).or_default().push(value);
        }
        form
    }

    pub(super) fn get(&self, key: &str) -> &str {
        self.0
            .get(key)
            .and_then(|v| v.first())
            .map(|s| s.trim())
            .unwrap_or("")
    }

    pub(super) fn all(&self, key: &str) -> Vec<&str> {
        self.0
            .get(key)
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    pub(super) fn set(&mut self, key: &str, value: impl Into<String>) {
        self.0.insert(key.to_string(), vec![value.into()]);
    }

    /// Первые значения — для обычных полей в шаблоне.
    pub(super) fn first_values(&self) -> HashMap<&str, &str> {
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

fn parse_optional_id(
    raw: &str,
    what: &str,
    tab: &'static str,
    errors: &mut TabErrors,
) -> Option<i64> {
    match raw {
        "" => None,
        raw => match raw.parse::<i64>() {
            Ok(id) if id > 0 => Some(id),
            _ => {
                errors.push(tab, format!("{what}: неверный id"));
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
) -> Result<ItemInput, TabErrors> {
    let get = |key: &str| form.get(key);
    let mut errors = TabErrors::new();

    let name = get("name");
    if name.is_empty() {
        errors.push("main", "Укажите название");
    }
    let code = match get("code") {
        "" => slugify(name),
        code => code.to_string(),
    };
    if !code.is_empty() && !is_valid_slug(&code) {
        errors.push("main", "Код: только латиница, цифры, «_» и «-»");
    }
    let section_id = parse_optional_id(get("section_id"), "Раздел", "main", &mut errors);
    if section_id.is_some_and(|id| !sections.iter().any(|s| s.id == id)) {
        errors.push("main", "Раздел не найден в этой коллекции");
    }
    let preview_picture_id = parse_optional_id(
        get("preview_picture_id"),
        "Картинка анонса",
        "text",
        &mut errors,
    );
    let detail_picture_id = parse_optional_id(
        get("detail_picture_id"),
        "Детальная картинка",
        "text",
        &mut errors,
    );
    let published_at = match get("published_at") {
        "" => None,
        raw => match parse_datetime(raw) {
            Some(dt) => Some(dt.and_utc()),
            None => {
                errors.push("main", "Дата публикации: ожидается ДД.ММ.ГГГГ ЧЧ:ММ");
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
            Err(e) => errors.push("fields", e),
        }
    }

    if !errors.is_empty() {
        return Err(errors);
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
) -> AppResult<Result<(), TabErrors>> {
    let mut errors = TabErrors::new();
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
                    errors.push(
                        "fields",
                        format!("«{}»: нет записей с id {}", prop.name, missing.join(", ")),
                    );
                }
            }
            "file" => file_ids.extend(ids),
            _ => {}
        }
    }
    let found = files::get_many(&state.db, &file_ids).await?;
    if file_ids.iter().any(|id| !found.iter().any(|f| f.id == *id)) {
        errors.push("fields", "Часть файлов не найдена — загрузите их заново");
    }
    Ok(if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
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
        form.set("published_at", dt.format("%d.%m.%Y %H:%M").to_string());
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
    /// Коды валют справочника (пусто — выбор не показывается)
    currencies: Vec<String>,
    /// Валюта сайта — по умолчанию у новой цены
    site_currency: String,
    /// (id, название, активен)
    stores: Vec<(i64, String, bool)>,
}

/// Вкладка «Торговый каталог» из полей формы: `price_<тип>`, `amount_<склад>`,
/// `available`, `quantity_trace` / `can_buy_zero` (`Y` | `N` | `default`).
pub(super) fn purchase_input_from_form(
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
    let mut currencies = Vec::new();
    for t in price_types {
        let code = form.get(&format!("currency_{t}")).trim().to_uppercase();
        if !code.is_empty() {
            currencies.push((*t, code));
        }
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
        currencies,
        amounts,
        quantity,
        available: form.0.contains_key("available"),
        weight,
        quantity_trace: flag("quantity_trace"),
        can_buy_zero: flag("can_buy_zero"),
    })
}

/// Валюты цен из формы должны быть в справочнике (пустой справочник — не проверяется).
pub(super) async fn check_price_currencies(
    db: &sqlx::PgPool,
    input: &PurchaseInput,
) -> sqlx::Result<Result<(), String>> {
    if input.currencies.is_empty() {
        return Ok(Ok(()));
    }
    let known: Vec<String> = sqlx::query_scalar("SELECT code FROM currencies")
        .fetch_all(db)
        .await?;
    if known.is_empty() {
        return Ok(Ok(()));
    }
    Ok(
        match input.currencies.iter().find(|(_, c)| !known.contains(c)) {
            Some((_, c)) => Err(format!("Неизвестная валюта {c}")),
            None => Ok(()),
        },
    )
}

/// Сохраняет вкладку «Торговый каталог». При складском учёте остатки по складам и доступное
/// количество форма не меняет — их ведут складские документы и резервы.
async fn save_catalog_tab(
    db: &sqlx::PgPool,
    item_id: i64,
    input: &catalog::PurchaseInput,
    barcodes: Option<&BarcodeInput>,
    user_id: i64,
) -> Result<(), StockError> {
    let mut tx = db.begin().await?;
    if crate::stock::settings(&mut tx).await?.store_control {
        // Строка товара заблокирована, пока читаем и возвращаем прежнее количество
        catalog::load_locked(&mut tx, &[item_id]).await?;
        let quantity: Option<f64> =
            sqlx::query_scalar("SELECT quantity::float8 FROM catalog_products WHERE item_id = $1")
                .bind(item_id)
                .fetch_optional(&mut *tx)
                .await?;
        let mut input = input.clone();
        input.amounts.clear();
        input.quantity = quantity;
        catalog::save_purchase_in(&mut tx, item_id, &input).await?;
    } else {
        catalog::save_purchase_in(&mut tx, item_id, input).await?;
    }
    // Штрихкоды — в той же транзакции, что и вкладка: отказ откатывает обе части
    if let Some(b) = barcodes {
        stock::barcodes::set_multi(&mut tx, item_id, b.multi).await?;
        if let Some(codes) = b.common.as_ref().filter(|_| !b.multi) {
            stock::barcodes::set_common(&mut tx, item_id, codes, Some(user_id)).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Штрихкоды из формы: флажок поштучного учёта и общие коды (`None` — поле не отправлялось).
struct BarcodeInput {
    multi: bool,
    common: Option<Vec<String>>,
}

fn barcodes_from_form(form: &FormValues) -> Option<BarcodeInput> {
    // Скрытое поле-маркер: формы без блока штрихкодов (предложения, старые клиенты) не затрагиваются
    form.0.contains_key("barcode_form").then(|| BarcodeInput {
        multi: form.0.contains_key("barcode_multi"),
        common: form.0.get("barcodes").map(|v| {
            v.iter()
                .flat_map(|t| t.lines())
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .collect()
        }),
    })
}

/// Проверка штрихкодов до записи товара: чужой код и снятие флажка при кодах на складах.
async fn check_barcodes(
    db: &sqlx::PgPool,
    item_id: Option<i64>,
    name: &str,
    b: &BarcodeInput,
) -> sqlx::Result<Result<(), String>> {
    if let Some(id) = item_id
        && !b.multi
    {
        let has: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM catalog_barcodes WHERE item_id = $1 AND store_id IS NOT NULL)",
        )
        .bind(id)
        .fetch_one(db)
        .await?;
        if has {
            return Ok(Err("У товара есть штрихкоды единиц на складах".into()));
        }
    }
    if let Some(codes) = b.common.as_ref().filter(|_| !b.multi) {
        for code in codes {
            let other: Option<i64> = sqlx::query_scalar(
                "SELECT item_id FROM catalog_barcodes WHERE barcode = $1 AND item_id IS DISTINCT FROM $2 LIMIT 1",
            )
            .bind(code)
            .bind(item_id)
            .fetch_optional(db)
            .await?;
            if other.is_some() {
                return Ok(Err(format!(
                    "Штрихкод {code}, введенный для товара {name}, уже существует."
                )));
            }
        }
    }
    Ok(Ok(()))
}

/// Текущие цены, остатки и флаги товара — в поля формы.
async fn purchase_to_form(
    state: &AppState,
    item_id: i64,
    form: &mut FormValues,
    can_purchase_info: bool,
) -> AppResult<()> {
    let multi: Option<bool> =
        sqlx::query_scalar("SELECT barcode_multi FROM catalog_products WHERE item_id = $1")
            .bind(item_id)
            .fetch_optional(&state.db)
            .await?;
    if multi == Some(true) {
        form.set("barcode_multi", "on");
    }
    let common: Vec<String> = stock::barcodes::list(&mut *state.db.acquire().await?, item_id)
        .await?
        .into_iter()
        .filter(|b| b.store_id.is_none() && b.order_id.is_none())
        .map(|b| b.barcode)
        .collect();
    form.set("barcodes", common.join("\n"));
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
            form.set(&format!("currency_{}", p.type_id), p.currency.clone());
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
    // `allow_negative_amount = N`: при складском учёте отрицательный остаток виден как 0
    // (поля при учёте только для чтения, поэтому показанное значение не сохраняется)
    let stock = crate::stock::settings(&mut *state.db.acquire().await?).await?;
    let hide_negative = stock.store_control && !stock.allow_negative;
    for (store, amount) in amounts {
        let amount = if hide_negative {
            amount.max(0.0)
        } else {
            amount
        };
        form.set(&format!("amount_{store}"), amount.to_string());
    }
    if stock.store_control && can_purchase_info {
        let price: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT purchasing_price::text, purchasing_currency FROM catalog_products
             WHERE item_id = $1 AND purchasing_price IS NOT NULL",
        )
        .bind(item_id)
        .fetch_optional(&state.db)
        .await?;
        if let Some((price, currency)) = price {
            form.set(
                "purchasing_price",
                format!("{price} {}", currency.unwrap_or_default())
                    .trim()
                    .to_string(),
            );
        }
    }
    Ok(())
}

async fn load_collection(state: &AppState, id: i64) -> AppResult<CollectionContext> {
    let collection = repo::get_collection(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let catalog = if collection.is_catalog {
        let site_currency = crate::currency::site_currency(&mut *state.db.acquire().await?).await?;
        Some(CatalogContext {
            currencies: sqlx::query_scalar("SELECT code FROM currencies ORDER BY sort, code")
                .fetch_all(&state.db)
                .await?,
            site_currency,
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
        offers: if collection.is_catalog {
            repo::offers_collection(&state.db, id).await?
        } else {
            None
        },
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

#[allow(clippy::too_many_arguments)] // контекст формы, ошибки по вкладкам, уведомление и вкладка
async fn render_form(
    state: &AppState,
    user: Access,
    ctx: CollectionContext,
    item_id: Option<i64>,
    form: FormValues,
    errors: TabErrors,
    notice: Option<String>,
    tab: Option<&str>,
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
    let store_control = user.store_control;
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
    let can_purchase_info = user.can(crate::access::PURCHASE_INFO);
    let can_shop = user.can(crate::access::SHOP_MANAGE);
    // У поштучного товара: склад → число штрихкодов (только чтение)
    let barcode_stores: Vec<(String, i64)> = match item_id {
        Some(id) if catalog.is_some() => {
            sqlx::query_as(
                "SELECT s.name, count(*) FROM catalog_barcodes b
                 JOIN catalog_stores s ON s.id = b.store_id
                 WHERE b.item_id = $1 GROUP BY s.id, s.name ORDER BY s.sort, s.id",
            )
            .bind(id)
            .fetch_all(&state.db)
            .await?
        }
        _ => Vec::new(),
    };
    // Вкладки: «Основное» и «Описание» всегда, остальные — по данным коллекции и записи
    let mut card_tabs: Vec<(&'static str, &'static str, Option<usize>)> =
        vec![("main", "Основное", None), ("text", "Описание", None)];
    if !properties.is_empty() {
        card_tabs.push(("fields", "Поля", None));
    }
    if catalog.is_some() {
        card_tabs.push(("catalog", "Каталог", None));
    }
    if let Some(t) = &offers_tab {
        card_tabs.push(("offers", "Предложения", Some(t.rows.len())));
    }
    if item_id.is_some() {
        card_tabs.push(("seo", "SEO", None));
    }
    let visible: Vec<&'static str> = card_tabs.iter().map(|t| t.0).collect();
    let tabctx = tab_context(&errors, &visible, tab);
    let seo = match item_id {
        Some(id) => match crate::seo::load(&state.db, crate::seo::Kind::Item, id).await {
            Ok(target) => crate::seo::settings(&state.db, &target).await.ok(),
            Err(_) => None,
        },
        None => None,
    };
    render(
        state,
        "item_form.html",
        context! {
            user, collection, properties, item_id, card_tabs, tabctx, can_write, sections, enums, files, linked, seo,
            multi, catalog, store_control, offers_priced, offers, offers_tab, can_write_offers, offer_product, notice,
            can_purchase_info, can_shop, barcode_stores,
            form => form.first_values(),
        },
    )
}

/// SEO-поля из общей формы записи; пустые — наследование, как на отдельной странице.
fn seo_settings(form: &FormValues) -> crate::seo::Settings {
    crate::seo::Settings {
        title: form.get("seo_title").trim().to_string(),
        description: form.get("seo_description").trim().to_string(),
        h1: form.get("seo_h1").trim().to_string(),
        canonical: form.get("seo_canonical").trim().to_string(),
        robots: form.get("seo_robots").trim().to_string(),
    }
}

async fn save_seo(db: &sqlx::PgPool, item_id: i64, form: &FormValues) {
    let settings = seo_settings(form);
    let empty = settings.title.is_empty()
        && settings.description.is_empty()
        && settings.h1.is_empty()
        && settings.canonical.is_empty()
        && settings.robots.is_empty();
    if empty {
        return;
    }
    let target = match crate::seo::load(db, crate::seo::Kind::Item, item_id).await {
        Ok(target) => target,
        Err(error) => {
            tracing::warn!(item_id, ?error, "SEO записи не сохранено");
            return;
        }
    };
    if let Err(error) = crate::seo::save(db, &target, settings).await {
        // Карточка уже сохранена: SEO-ошибка не должна откатывать основную запись
        tracing::warn!(item_id, ?error, "SEO записи не сохранено");
    }
}

/// Дата из поля: `ДД.ММ.ГГГГ ЧЧ:ММ`, `ДД.ММ.ГГГГ` или старый ISO `ГГГГ-ММ-ДДTЧЧ:ММ`.
fn parse_datetime(raw: &str) -> Option<NaiveDateTime> {
    let raw = raw.trim();
    NaiveDateTime::parse_from_str(raw, "%d.%m.%Y %H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(raw, DATETIME_FORMAT))
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(raw, "%d.%m.%Y")
                .ok()
                .and_then(|date| date.and_hms_opt(0, 0, 0))
        })
}

/// Работа с предложениями требует записи и в коллекции товаров.
pub(super) fn require_parent_write(user: &Access, collection: &Collection) -> AppResult<()> {
    match collection.product_collection_id {
        Some(parent) => user.require_collection(parent, Level::Write),
        None => Ok(()),
    }
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

/// Список записей на движке списков, слева — дерево разделов.
pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> AppResult<Response> {
    list_inner(state, user, id, raw, headers, false, None).await
}

/// `after_action` — перерисовка после действия с видом: показывает запомненное и ничего не запоминает.
pub(crate) async fn list_inner(
    state: AppState,
    user: Access,
    id: i64,
    raw: Option<String>,
    headers: HeaderMap,
    after_action: bool,
    bulk: Option<crate::admin::list::bulk::BulkSummary>,
) -> AppResult<Response> {
    user.require_collection(id, Level::Read)?;
    let collection = repo::get_collection(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let spec = collection_spec(&state.db, &user, &collection).await?;
    let resolved = views::resolve(
        &state.db,
        user.user.id,
        &spec,
        raw.as_deref().unwrap_or(""),
        views::restores(&headers, after_action),
    )
    .await?;
    let (mut parsed, notices) = (resolved.parsed, resolved.notices);
    let sections = repo::list_sections(&state.db, id).await?;
    // Удалённый или чужой раздел в адресе отбрасывается вместе с «только этот раздел»
    if let Some(SectionSel::Id(sid)) = parsed.state.section
        && !sections.iter().any(|s| s.id == sid)
    {
        parsed.state.section = None;
        parsed.state.only = false;
        parsed.changed = true;
    }
    let (scope, current) =
        section_scope(&state.db, id, &parsed.state.section, parsed.state.only).await?;
    let can_write = user.collection_level(id) >= Level::Write;
    let path = format!("/admin/collections/{id}/items");

    // Крошки: цепочка родителей текущего раздела
    let mut chain: Vec<&Section> = Vec::new();
    let mut cursor = current.as_ref();
    while let Some(s) = cursor {
        if chain.iter().any(|c| c.id == s.id) {
            break;
        }
        chain.push(s);
        cursor = s
            .parent_id
            .and_then(|p| sections.iter().find(|x| x.id == p));
    }
    chain.reverse();
    let selected_none = parsed.state.section == Some(SectionSel::Without);
    let mut crumbs = vec![("Коллекции".to_string(), "/admin/collections".to_string())];
    if chain.is_empty() && !selected_none {
        // заголовок и есть коллекция
    } else {
        crumbs.push((collection.name.clone(), path.clone()));
        for s in chain.iter().take(chain.len().saturating_sub(1)) {
            crumbs.push((s.name.clone(), format!("{path}?section={}", s.id)));
        }
    }
    let title = match (&current, selected_none) {
        (Some(s), _) => s.name.clone(),
        (None, true) => "Без раздела".to_string(),
        (None, false) => collection.name.clone(),
    };

    let new_item = match &current {
        Some(s) => format!("/admin/collections/{id}/items/new?section={}", s.id),
        None => format!("/admin/collections/{id}/items/new"),
    };
    let mut actions = Vec::new();
    if user.can(COLLECTIONS_MANAGE) && current.is_none() && !selected_none {
        actions.push(Action {
            label: "Настройки".into(),
            url: format!("/admin/collections/{id}"),
            primary: false,
            hotkey: None,
        });
    }
    if can_write {
        actions.push(Action {
            label: "Добавить запись".into(),
            url: new_item,
            primary: true,
            hotkey: Some('c'),
        });
    }

    let own = tree::own_counts(&state.db, id).await?;
    let total: i64 = own.values().sum();
    let without = own.get(&None).copied().unwrap_or(0);
    let counts = tree::subtree_counts(&sections, &own);
    let builder = tree::TreeBuilder {
        collection_id: id,
        spec: &spec,
        state: &parsed.state,
        sections: &sections,
        counts,
        path: path.clone(),
        current: current.as_ref().map(|s| s.id),
        ancestors: current
            .as_ref()
            .map(|s| tree::ancestors_of(&sections, s.id))
            .unwrap_or_default(),
    };
    let tree_value = builder.panel(total, without, selected_none, can_write);
    let page = ListPage {
        title,
        crumbs,
        actions,
        tree: Some(tree_value),
        section_has_kids: current
            .as_ref()
            .is_some_and(|c| sections.iter().any(|s| s.parent_id == Some(c.id))),
        section_name: current.as_ref().map(|s| s.name.clone()),
        bulk,
    };
    run_list(
        &state,
        &user,
        &headers,
        &path,
        &spec,
        parsed,
        scope,
        page,
        notices,
        !after_action,
    )
    .await
}

#[derive(Deserialize)]
pub struct TreeQuery {
    parent: i64,
}

/// Дети узла дерева разделов: `GET /admin/collections/{id}/tree?parent=<id>`.
pub async fn tree(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Query(q): Query<TreeQuery>,
    RawQuery(raw): RawQuery,
) -> AppResult<Response> {
    user.require_collection(id, Level::Read)?;
    let collection = repo::get_collection(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let sections = repo::list_sections(&state.db, id).await?;
    if !sections.iter().any(|s| s.id == q.parent) {
        return Err(AppError::NotFound);
    }
    let spec = collection_spec(&state.db, &user, &collection).await?;
    let parsed = list_state::parse(&spec, raw.as_deref().unwrap_or(""));
    let own = tree::own_counts(&state.db, id).await?;
    let builder = tree::TreeBuilder {
        collection_id: id,
        spec: &spec,
        state: &parsed.state,
        sections: &sections,
        counts: tree::subtree_counts(&sections, &own),
        path: format!("/admin/collections/{id}/items"),
        current: None,
        ancestors: Default::default(),
    };
    let can_write = user.collection_level(id) >= Level::Write;
    let html = render(
        &state,
        "_tree.html",
        context! { tree => builder.children(q.parent, can_write) },
    )?;
    Ok(html.into_response())
}

#[derive(Deserialize)]
pub struct CollapsedForm {
    value: String,
}

/// Запоминает свёрнутость дерева разделов: `POST /admin/collections/{id}/tree/collapsed`.
pub async fn tree_collapsed(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    axum::Form(form): axum::Form<CollapsedForm>,
) -> AppResult<axum::http::StatusCode> {
    user.require_collection(id, Level::Read)?;
    crate::admin::shell::merge_list_prefs(
        &state.db,
        user.user.id,
        &format!("collection:{id}"),
        serde_json::json!({"tree_collapsed": form.value == "1"}),
    )
    .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
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
    require_parent_write(&user, &ctx.collection)?;
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
    render_form(&state, user, ctx, None, form, TabErrors::new(), None, None).await
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
            let mut input = purchase_input_from_form(&form, &types, &stores);
            if let Ok(p) = &input
                && let Err(e) = check_price_currencies(&state.db, p).await?
            {
                input = Err(e);
            }
            Some(input)
        }
        None => None,
    };
    let barcodes = purchase.as_ref().and_then(|_| barcodes_from_form(&form));
    let mut purchase = purchase;
    if let (Some(Ok(_)), Some(b)) = (&purchase, &barcodes)
        && let Err(e) = check_barcodes(&state.db, item_id, form.get("name"), b).await?
    {
        purchase = Some(Err(e));
    }
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
    // Ошибки всех вкладок собираются сразу: карточка открывается на первой из них
    let mut errors = TabErrors::new();
    let built = match build_input(&form, &parse_props, &ctx.enums, &ctx.sections) {
        Ok(input) => Some(input),
        Err(e) => {
            errors.append(e);
            None
        }
    };
    if let Some(e) = purchase_error {
        errors.push("catalog", e);
    }
    for r in &upload.rejected {
        errors.push("fields", r.clone());
    }
    // Проверки с обращением к БД — только когда форма сама по себе верна
    let result = match built {
        Some(input) if errors.is_empty() => {
            match offer_link_error(state, &ctx, &input).await? {
                Some(e) => errors.push("fields", e),
                None => {
                    if let Err(e) = check_references(state, &ctx.properties, &input).await? {
                        errors.append(e);
                    }
                }
            }
            errors.is_empty().then_some(input)
        }
        _ => None,
    };
    let requested_tab = Some(form.get("tab").to_string()).filter(|t| !t.is_empty());
    let errors = match result {
        Some(mut input) => {
            // Связь предложения с товаром хранится в колонке, а не в field_values
            input.product_id =
                sku::take_link(&ctx.collection, &ctx.properties, &mut input.field_values);
            let saved = match item_id {
                Some(id) => repo::update_item(&state.db, id, &input).await.map(|_| id),
                None => repo::create_item(&state.db, ctx.collection.id, &input).await,
            };
            match saved {
                Ok(saved_id) => {
                    crate::versions::snapshot(
                        &state.db,
                        saved_id,
                        if item_id.is_some() {
                            "update"
                        } else {
                            "create"
                        },
                        user.user.id,
                        &user.user.login,
                    )
                    .await;
                    save_seo(&state.db, saved_id, &form).await;
                    if let Some(Ok(p)) = &purchase
                        && let Err(e) = save_catalog_tab(
                            &state.db,
                            saved_id,
                            p,
                            barcodes.as_ref(),
                            user.user.id,
                        )
                        .await
                    {
                        match e {
                            // Запись уже сохранена: форма показывает отказ и ведёт на её правку
                            StockError::Rejected(m) => {
                                let mut errors = TabErrors::new();
                                for msg in m {
                                    errors.push("catalog", msg);
                                }
                                let page = render_form(
                                    state,
                                    user,
                                    ctx,
                                    Some(saved_id),
                                    form,
                                    errors,
                                    None,
                                    requested_tab.as_deref(),
                                )
                                .await?;
                                return Ok(page.into_response());
                            }
                            StockError::Db(e) => return Err(e.into()),
                        }
                    }
                    return Ok(match (input.product_id, requested_tab.as_deref()) {
                        // Предложение: назад в карточку товара, на вкладку предложений
                        (Some(product), _) => {
                            Redirect::to(&format!("/admin/items/{product}?tab=offers"))
                                .into_response()
                        }
                        // Карточка с вкладкой: назад на ту же вкладку
                        (None, Some(tab)) if CARD_TABS.contains(&tab) => flash::redirect(
                            &format!("/admin/items/{saved_id}?tab={tab}"),
                            "Сохранено",
                            FlashKind::Ok,
                        ),
                        (None, _) => {
                            let mut url = format!("/admin/collections/{}/items", ctx.collection.id);
                            if let Some(section) = input.section_id {
                                url.push_str(&format!("?section={section}"));
                            }
                            Redirect::to(&url).into_response()
                        }
                    });
                }
                Err(e) if is_unique_violation(&e) => {
                    let mut errors = TabErrors::new();
                    errors.push("main", "Запись с таким кодом уже есть");
                    errors
                }
                Err(e) => return Err(e.into()),
            }
        }
        None => errors,
    };
    let page = render_form(
        state,
        user,
        ctx,
        item_id,
        form,
        errors,
        None,
        requested_tab.as_deref(),
    )
    .await?;
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
    require_parent_write(&user, &ctx.collection)?;
    save(&state, user, ctx, None, multipart).await
}

/// Итог генератора предложений, переданный редиректом (`?generated=N&skipped=M`).
#[derive(Deserialize)]
pub struct EditQuery {
    generated: Option<usize>,
    skipped: Option<usize>,
    /// Открываемая вкладка карточки.
    tab: Option<String>,
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Query(q): Query<EditQuery>,
) -> AppResult<Html<String>> {
    let element = repo::get_item(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    user.require_collection(element.collection_id, Level::Read)?;
    let ctx = load_collection(&state, element.collection_id).await?;
    let mut form = element_to_form(&element, &ctx.properties);
    if ctx.catalog.is_some() {
        purchase_to_form(
            &state,
            id,
            &mut form,
            user.can(crate::access::PURCHASE_INFO),
        )
        .await?;
    }
    let notice = q.generated.map(|n| {
        format!(
            "Создано {n}, пропущено {} (уже есть)",
            q.skipped.unwrap_or(0)
        )
    });
    render_form(
        &state,
        user,
        ctx,
        Some(id),
        form,
        TabErrors::new(),
        notice,
        q.tab.as_deref(),
    )
    .await
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
    require_parent_write(&user, &ctx.collection)?;
    let (actor_id, actor_login) = (user.user.id, user.user.login.clone());
    let response = save(&state, user, ctx, Some(id), multipart).await?;
    // Ошибки валидации возвращают 200 с формой: повода для записи нет.
    if response.status().is_redirection() {
        crate::audit::record(
            &state.db,
            actor_id,
            &actor_login,
            "update",
            "item",
            id,
            "запись изменена",
        )
        .await;
    }
    Ok(response)
}

/// Запрос из панели быстрого просмотра: HTMX с целью `#peek`.
fn is_peek(headers: &axum::http::HeaderMap) -> bool {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    header("hx-request") == Some("true") && header("hx-target") == Some("peek")
}

#[derive(Serialize)]
struct PeekPrice {
    price: f64,
    currency: String,
    type_name: String,
}

#[derive(Serialize)]
struct StoreStock {
    name: String,
    amount: f64,
}

/// Панель быстрого просмотра товара (фрагмент); `toast` — уведомление в `HX-Trigger`,
/// `list_changed` просит страницу перезапросить список (метка `data-list-changed`). Права проверяет вызывающий.
async fn peek_panel(
    state: &AppState,
    user: &Access,
    item: Item,
    toast: Option<(&str, FlashKind)>,
    list_changed: bool,
) -> AppResult<Response> {
    let id = item.id;
    let section = match item.section_id {
        Some(sid) => repo::list_sections(&state.db, item.collection_id)
            .await?
            .into_iter()
            .find(|s| s.id == sid)
            .map(|s| s.name),
        None => None,
    };
    let picture = match item.preview_picture_id.or(item.detail_picture_id) {
        Some(fid) => files::get_many(&state.db, &[fid])
            .await?
            .into_iter()
            .find(|f| f.is_image())
            .map(|f| f.url()),
        None => None,
    };
    let offers_priced = matches!(
        product_type(&state.db, id).await?,
        Some(catalog::TYPE_SKU | catalog::TYPE_EMPTY_SKU)
    );
    let price = catalog::load_prices(&state.db, &[id])
        .await?
        .remove(&id)
        .and_then(|prices| catalog::main_price(&prices).cloned())
        .map(|p| PeekPrice {
            price: p.price,
            currency: p.currency,
            type_name: p.type_name,
        });
    // Остатки и доступное количество — как на вкладке «Каталог» карточки
    let quantity: f64 =
        sqlx::query_scalar("SELECT quantity::float8 FROM catalog_products WHERE item_id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .unwrap_or(0.0);
    let stock = crate::stock::settings(&mut *state.db.acquire().await?).await?;
    let hide_negative = stock.store_control && !stock.allow_negative;
    let rows: Vec<(String, f64)> = sqlx::query_as(
        "SELECT s.name, a.amount::float8 FROM catalog_store_amounts a
         JOIN catalog_stores s ON s.id = a.store_id
         WHERE a.item_id = $1 ORDER BY s.sort, s.id",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let stores: Vec<StoreStock> = rows
        .into_iter()
        .map(|(name, amount)| StoreStock {
            name,
            amount: if hide_negative {
                amount.max(0.0)
            } else {
                amount
            },
        })
        .collect();
    let can_write = user.collection_level(item.collection_id) >= Level::Write;
    let html = render(
        state,
        "item_peek.html",
        context! { item, section, picture, offers_priced, price, quantity, stores, can_write, list_changed },
    )?;
    Ok(match toast {
        Some((text, kind)) => (flash::toast(text, kind), html).into_response(),
        None => html.into_response(),
    })
}

/// Запись каталога, к которой у пользователя есть доступ `level`; иначе 404 (нет записи
/// или коллекция не каталог) или 403.
async fn peek_item(state: &AppState, user: &Access, id: i64, level: Level) -> AppResult<Item> {
    let item = repo::get_item(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    user.require_collection(item.collection_id, level)?;
    let collection = repo::get_collection(&state.db, item.collection_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if !collection.is_catalog {
        return Err(AppError::NotFound);
    }
    if level == Level::Write {
        require_parent_write(user, &collection)?;
    }
    Ok(item)
}

/// Быстрый просмотр товара: `GET /admin/items/{id}/peek`.
pub async fn peek(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let item = peek_item(&state, &user, id, Level::Read).await?;
    peek_panel(&state, &user, item, None, false).await
}

#[derive(Deserialize)]
pub struct ActiveForm {
    /// `on` — включить, иначе выключить.
    active: String,
}

/// Переключатель «Активен» из панели: `POST /admin/items/{id}/active`. Как и сохранение
/// карточки, меняет только `active` и `updated_at` (других побочных эффектов у сохранения нет).
pub async fn set_active(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<ActiveForm>,
) -> AppResult<Response> {
    peek_item(&state, &user, id, Level::Write).await?;
    let active = form.active == "on";
    repo::set_item_active(&state.db, id, active).await?;
    crate::versions::snapshot(&state.db, id, "update", user.user.id, &user.user.login).await;
    crate::audit::record(
        &state.db,
        user.user.id,
        &user.user.login,
        "update",
        "item",
        id,
        if active {
            "запись включена"
        } else {
            "запись отключена"
        },
    )
    .await;
    if !is_peek(&headers) {
        return Ok(flash::redirect(
            &format!("/admin/items/{id}"),
            "Сохранено",
            FlashKind::Ok,
        ));
    }
    let item = repo::get_item(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let text = if active {
        "Запись включена"
    } else {
        "Запись отключена"
    };
    peek_panel(&state, &user, item, Some((text, FlashKind::Ok)), true).await
}

/// Удаление записи с проверкой прав; возвращает удалённую запись (для редиректа).
pub async fn delete_one(
    db: &sqlx::PgPool,
    user: &Access,
    id: i64,
) -> AppResult<crate::collection::Item> {
    let element = repo::get_item(db, id).await?.ok_or(AppError::NotFound)?;
    user.require_collection(element.collection_id, Level::Write)?;
    if let Some(collection) = repo::get_collection(db, element.collection_id).await? {
        require_parent_write(user, &collection)?;
    }
    repo::delete_item(db, id).await?.ok_or(AppError::NotFound)?;
    Ok(element)
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    let element = delete_one(&state.db, &user, id).await?;
    crate::audit::record(
        &state.db,
        user.user.id,
        &user.user.login,
        "delete",
        "item",
        element.id,
        format!("удалена запись «{}»", element.name),
    )
    .await;
    if let Some(product) = element.product_id {
        // Предложение: назад на вкладку предложений товара
        return Ok(Redirect::to(&format!("/admin/items/{product}?tab=offers")));
    }
    let mut url = format!("/admin/collections/{}/items", element.collection_id);
    if let Some(section) = element.section_id {
        url.push_str(&format!("?section={section}"));
    }
    Ok(Redirect::to(&url))
}

/// Массовое действие над записью коллекции (реестр действий — `list::bulk`).
pub(crate) async fn bulk_run(
    ctx: &crate::admin::list::bulk::BulkCtx<'_>,
    action: &str,
    param: &str,
    id: i64,
) -> Result<(), String> {
    const NOT_FOUND: &str = "запись не найдена";
    let db = &ctx.state.db;
    let db_err = |e: sqlx::Error| {
        tracing::error!("массовое действие над записью {id}: {e}");
        "ошибка сервера".to_string()
    };
    let collection_id = ctx.collection_id.ok_or_else(|| NOT_FOUND.to_string())?;
    let item = repo::get_item(db, id)
        .await
        .map_err(db_err)?
        .filter(|i| i.collection_id == collection_id)
        .ok_or_else(|| NOT_FOUND.to_string())?;
    let collection = ctx
        .collection
        .get_or_try_init(|| repo::get_collection(db, collection_id))
        .await
        .map_err(db_err)?
        .as_ref()
        .ok_or_else(|| NOT_FOUND.to_string())?;
    require_parent_write(ctx.user, collection).map_err(|_| "нет права".to_string())?;
    match action {
        "activate" | "deactivate" => {
            ctx.user
                .require_collection(collection_id, Level::Write)
                .map_err(|_| "нет права".to_string())?;
            let changed = repo::set_item_active(db, item.id, action == "activate")
                .await
                .map_err(db_err)?;
            if changed {
                crate::versions::snapshot(
                    db,
                    item.id,
                    "update",
                    ctx.user.user.id,
                    &ctx.user.user.login,
                )
                .await;
                crate::audit::record(
                    db,
                    ctx.user.user.id,
                    &ctx.user.user.login,
                    "update",
                    "item",
                    item.id,
                    if action == "activate" {
                        "массово: запись включена".to_string()
                    } else {
                        "массово: запись отключена".to_string()
                    },
                )
                .await;
            }
            changed.then_some(()).ok_or_else(|| NOT_FOUND.to_string())
        }
        "move" => {
            ctx.user
                .require_collection(collection_id, Level::Write)
                .map_err(|_| "нет права".to_string())?;
            let section = if param == "none" {
                None
            } else {
                let sid: i64 = param.parse().map_err(|_| "раздел не найден".to_string())?;
                let found = repo::get_section(db, sid).await.map_err(db_err)?;
                if found.is_none_or(|s| s.collection_id != collection_id) {
                    return Err("раздел не найден".into());
                }
                Some(sid)
            };
            let changed = repo::move_item(db, item.id, section)
                .await
                .map_err(db_err)?;
            if changed {
                crate::versions::snapshot(
                    db,
                    item.id,
                    "update",
                    ctx.user.user.id,
                    &ctx.user.user.login,
                )
                .await;
                crate::audit::record(
                    db,
                    ctx.user.user.id,
                    &ctx.user.user.login,
                    "update",
                    "item",
                    item.id,
                    "массово: запись перемещена",
                )
                .await;
            }
            changed.then_some(()).ok_or_else(|| NOT_FOUND.to_string())
        }
        "delete" => match delete_one(db, ctx.user, item.id).await {
            Ok(element) => {
                crate::audit::record(
                    db,
                    ctx.user.user.id,
                    &ctx.user.user.login,
                    "delete",
                    "item",
                    element.id,
                    format!("массово: удалена запись «{}»", element.name),
                )
                .await;
                Ok(())
            }
            Err(AppError::NotFound) => Err(NOT_FOUND.into()),
            Err(AppError::Forbidden) => Err("нет права".into()),
            Err(AppError::Db(e)) => Err(db_err(e)),
            Err(e) => {
                tracing::error!("массовое удаление записи {id}: {e}");
                Err("ошибка сервера".into())
            }
        },
        _ => Err("неизвестное действие".into()),
    }
}

/// История версий записи.
pub async fn history(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<axum::response::Html<String>> {
    let element = repo::get_item(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    user.require_collection(element.collection_id, Level::Read)?;
    let revisions = crate::versions::history(&state.db, id).await?;
    super::render(
        &state,
        "item_history.html",
        context! { user, id, name => element.name, revisions },
    )
}

/// Восстановление версии записи.
pub async fn restore_version(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path((id, revision)): Path<(i64, i64)>,
) -> AppResult<Redirect> {
    crate::versions::restore(&state.db, &user, revision).await?;
    Ok(Redirect::to(&format!("/admin/items/{id}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// «Открыть карточку» в панели должна менять адрес: у #peek нет унаследованного
    /// `hx-push-url="false"`, а формы внутри панели (hx-post) отключают push явно.
    #[test]
    fn peek_panel_pushes_url_only_for_open_link() {
        let base = include_str!("../../templates/admin/base.html");
        let peek_tag = base.lines().find(|l| l.contains("id=\"peek\"")).unwrap();
        assert!(!peek_tag.contains("hx-push-url"), "{peek_tag}");
        for tpl in [
            include_str!("../../templates/admin/item_peek.html"),
            include_str!("../../templates/admin/shop/order_peek.html"),
        ] {
            for form in tpl.lines().filter(|l| l.contains("hx-post=")) {
                assert!(form.contains("hx-push-url=\"false\""), "{form}");
            }
        }
    }

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
            xml_id: String::new(),
        }
    }

    fn form(pairs: &[(&str, &str)]) -> FormValues {
        FormValues::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
    }

    #[test]
    fn published_date_accepts_ru_and_iso() {
        assert_eq!(
            super::parse_datetime("27.09.2026 10:30")
                .unwrap()
                .to_string(),
            "2026-09-27 10:30:00"
        );
        assert_eq!(
            super::parse_datetime("27.09.2026").unwrap().to_string(),
            "2026-09-27 00:00:00"
        );
        assert_eq!(
            super::parse_datetime("2026-09-27T10:30")
                .unwrap()
                .to_string(),
            "2026-09-27 10:30:00"
        );
        assert!(super::parse_datetime("вчера").is_none());
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
        let all = err.iter().map(|(_, m)| m).collect::<Vec<_>>().join("; ");
        assert!(all.contains("Укажите название"));
        assert!(all.contains("обязательное"));
        assert!(all.contains("Раздел"));
    }

    #[test]
    fn item_errors_tagged() {
        type Case<'a> = (&'a [(&'a str, &'a str)], &'a str, &'a str);
        let cases: [Case; 7] = [
            (&[("name", "")], "Укажите название", "main"),
            (&[("name", "А"), ("code", "плохой код")], "Код:", "main"),
            (
                &[("name", "А"), ("section_id", "x")],
                "Раздел: неверный id",
                "main",
            ),
            (
                &[("name", "А"), ("section_id", "7")],
                "Раздел не найден",
                "main",
            ),
            (
                &[("name", "А"), ("published_at", "вчера")],
                "Дата публикации: ожидается",
                "main",
            ),
            (
                &[("name", "А"), ("preview_picture_id", "x")],
                "Картинка анонса",
                "text",
            ),
            (&[("name", "А")], "обязательное", "fields"),
        ];
        for (pairs, needle, tab) in cases {
            let err =
                build_input(&form(pairs), &[prop("price", "number", true)], &[], &[]).unwrap_err();
            let hit = err.iter().find(|(_, m)| m.contains(needle));
            assert_eq!(hit.map(|h| h.0), Some(tab), "{needle}: {err:?}");
        }
    }

    #[sqlx::test]
    async fn item_references_errors_tagged(db: sqlx::PgPool) {
        let state = crate::test_support::test_state(db);
        let input = ItemInput {
            section_id: None,
            code: "a".into(),
            xml_id: String::new(),
            name: "А".into(),
            active: true,
            sort: 500,
            preview_text: String::new(),
            detail_text: String::new(),
            preview_picture_id: Some(987654),
            detail_picture_id: None,
            published_at: None,
            field_values: Map::new(),
            product_id: None,
        };
        let err = check_references(&state, &[], &input)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(
            err.iter().map(|(t, _)| t).collect::<Vec<_>>(),
            ["fields"],
            "{err:?}"
        );
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
            sha256: String::new(),
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

    #[sqlx::test]
    async fn item_price_currency_saved(db: sqlx::PgPool) {
        use crate::{
            smoke_tests::{get, post_multipart},
            test_support::{admin_cookie, content_fixture, seed_usd, test_state},
        };
        let c = content_fixture(&db).await;
        sqlx::query("UPDATE collections SET is_catalog = TRUE WHERE id = $1")
            .bind(c.collection_id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO catalog_price_types (id, code, name, is_base, sort) VALUES (1, 'BASE', 'Розница', TRUE, 100)")
            .execute(&db)
            .await
            .unwrap();
        seed_usd(&db, None).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let path = format!("/admin/items/{}", c.item_id);
        let currency = || {
            let db = db.clone();
            let id = c.item_id;
            async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT currency FROM catalog_prices WHERE item_id = $1 AND price_type_id = 1",
                )
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };
        let base = [("name", "Первая новость"), ("sort", "500")];
        // Валюта из формы
        let mut fields = base.to_vec();
        fields.extend([("price_1", "10"), ("currency_1", "USD")]);
        let (s, _) = post_multipart(&app, &path, &cookie, &fields).await;
        assert_eq!(s, axum::http::StatusCode::SEE_OTHER);
        assert_eq!(currency().await, "USD");
        // Форма показывает select с выбранной валютой
        let (_, html) = get(&app, &path, &cookie).await;
        assert!(html.contains("<select name=\"currency_1\""), "{html}");
        assert!(html.contains("<option value=\"USD\" selected>"), "{html}");
        // Без поля валюты прежняя сохраняется
        let mut fields = base.to_vec();
        fields.push(("price_1", "11"));
        post_multipart(&app, &path, &cookie, &fields).await;
        assert_eq!(currency().await, "USD");
        // Новая цена без валюты — валюта сайта
        sqlx::query("DELETE FROM catalog_prices WHERE item_id = $1")
            .bind(c.item_id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO currencies (code) VALUES ('EUR')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ('sale', 'default_currency', 'EUR')",
        )
        .execute(&db)
        .await
        .unwrap();
        post_multipart(&app, &path, &cookie, &fields).await;
        assert_eq!(currency().await, "EUR");
        // Неизвестная валюта — ошибка формы, цена не меняется
        let mut fields = base.to_vec();
        fields.extend([("price_1", "99"), ("currency_1", "ZZZ")]);
        let (s, html) = post_multipart(&app, &path, &cookie, &fields).await;
        assert_eq!(s, axum::http::StatusCode::OK);
        assert!(html.contains("Неизвестная валюта ZZZ"), "{html}");
        let price: f64 = sqlx::query_scalar(
            "SELECT price::float8 FROM catalog_prices WHERE item_id = $1 AND price_type_id = 1",
        )
        .bind(c.item_id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(price, 11.0);
    }

    mod barcode_tab {
        use sqlx::PgPool;

        use crate::{
            smoke_tests::{get, post_multipart},
            stock::set_store_control,
            test_support::{admin_cookie, order_fixture, permitted_cookie, test_state},
        };

        async fn post(
            app: &axum::Router,
            cookie: &str,
            id: i64,
            extra: &[(&str, &str)],
        ) -> (axum::http::StatusCode, String) {
            let mut fields = vec![
                ("name", "Товар"),
                ("code", "tovar"),
                ("sort", "500"),
                ("available", "on"),
                ("quantity_trace", "default"),
                ("can_buy_zero", "default"),
                ("barcode_form", "1"),
            ];
            fields.extend_from_slice(extra);
            post_multipart(app, &format!("/admin/items/{id}"), cookie, &fields).await
        }

        async fn codes(db: &PgPool, id: i64) -> Vec<String> {
            sqlx::query_scalar(
                "SELECT barcode FROM catalog_barcodes WHERE item_id = $1 ORDER BY barcode",
            )
            .bind(id)
            .fetch_all(db)
            .await
            .unwrap()
        }

        #[sqlx::test]
        async fn item_saves_barcodes(db: PgPool) {
            let f = order_fixture(&db).await;
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let (s, _) = post(
                &app,
                &cookie,
                f.product_id,
                &[("barcodes", "111\n 222 \n\n111")],
            )
            .await;
            assert!(s.is_redirection(), "{s}");
            assert_eq!(codes(&db, f.product_id).await, ["111", "222"]);
            let (_, html) = get(&app, &format!("/admin/items/{}", f.product_id), &cookie).await;
            assert!(html.contains("111\n222"), "{html}");
            // поштучный учёт включается флажком
            let (s, _) = post(&app, &cookie, f.product_id, &[("barcode_multi", "on")]).await;
            assert!(s.is_redirection(), "{s}");
            let multi: bool =
                sqlx::query_scalar("SELECT barcode_multi FROM catalog_products WHERE item_id = $1")
                    .bind(f.product_id)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert!(multi);
            assert_eq!(codes(&db, f.product_id).await, ["111", "222"]);
        }

        #[sqlx::test]
        async fn item_barcode_conflict_shows_error(db: PgPool) {
            let f = order_fixture(&db).await;
            let other: i64 = sqlx::query_scalar(
                "INSERT INTO collection_items (collection_id, code, name)
                 SELECT collection_id, 'drugoy', 'Другой' FROM collection_items WHERE id = $1
                 RETURNING id",
            )
            .bind(f.product_id)
            .fetch_one(&db)
            .await
            .unwrap();
            sqlx::query("INSERT INTO catalog_barcodes (item_id, barcode) VALUES ($1, 'BUSY')")
                .bind(other)
                .execute(&db)
                .await
                .unwrap();
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let (s, html) = post(&app, &cookie, f.product_id, &[("barcodes", "OK1\nBUSY")]).await;
            assert_eq!(s, axum::http::StatusCode::OK);
            assert!(
                html.contains("Штрихкод BUSY, введенный для товара Товар, уже существует."),
                "{html}"
            );
            assert!(codes(&db, f.product_id).await.is_empty());
        }

        #[sqlx::test]
        async fn item_purchase_price_hidden_without_right(db: PgPool) {
            let f = order_fixture(&db).await;
            sqlx::query(
                "UPDATE catalog_products SET purchasing_price = 12.5, purchasing_currency = 'RUB'
                 WHERE item_id = $1",
            )
            .bind(f.product_id)
            .execute(&db)
            .await
            .unwrap();
            let mut tx = db.begin().await.unwrap();
            set_store_control(&mut tx, true).await.unwrap();
            tx.commit().await.unwrap();
            let app = crate::app(test_state(db.clone()));
            let url = format!("/admin/items/{}", f.product_id);
            let cookie = permitted_cookie(&db, "editor", &["collections.manage"]).await;
            let (_, html) = get(&app, &url, &cookie).await;
            assert!(
                html.contains("Остатки меняются складскими документами"),
                "{html}"
            );
            assert!(
                !html.contains("Закупочная цена") && !html.contains("12.50 RUB"),
                "{html}"
            );
            let (s, _) = post(&app, &cookie, f.product_id, &[]).await;
            assert!(s.is_redirection(), "{s}");
            let price: f64 = sqlx::query_scalar(
                "SELECT purchasing_price::float8 FROM catalog_products WHERE item_id = $1",
            )
            .bind(f.product_id)
            .fetch_one(&db)
            .await
            .unwrap();
            assert_eq!(price, 12.5);
            // с правом поле видно
            let cookie = admin_cookie(&db).await;
            let (_, html) = get(&app, &url, &cookie).await;
            assert!(
                html.contains("Закупочная цена") && html.contains("12.50 RUB"),
                "{html}"
            );
        }
    }

    mod store_control_tab {
        use sqlx::PgPool;

        use crate::{
            smoke_tests::{get, post_multipart},
            stock::set_store_control,
            test_support::{admin_cookie, amounts, order_fixture, test_state},
        };

        async fn setup(db: &PgPool, control: bool) -> (i64, String, axum::Router) {
            let f = order_fixture(db).await;
            sqlx::query(
                "UPDATE catalog_products SET purchasing_price = 12.5, purchasing_currency = 'RUB'
                 WHERE item_id = $1",
            )
            .bind(f.product_id)
            .execute(db)
            .await
            .unwrap();
            if control {
                let mut tx = db.begin().await.unwrap();
                set_store_control(&mut tx, true).await.unwrap();
                tx.commit().await.unwrap();
            }
            let app = crate::app(test_state(db.clone()));
            (f.product_id, admin_cookie(db).await, app)
        }

        async fn save(app: &axum::Router, cookie: &str, id: i64, amount: &str) {
            let (s, _) = post_multipart(
                app,
                &format!("/admin/items/{id}"),
                cookie,
                &[
                    ("name", "Товар"),
                    ("code", "tovar"),
                    ("sort", "500"),
                    ("available", "on"),
                    ("quantity_trace", "default"),
                    ("can_buy_zero", "default"),
                    ("amount_1", amount),
                ],
            )
            .await;
            assert!(s.is_redirection(), "{s}");
        }

        #[sqlx::test]
        async fn item_tab_readonly_with_store_control(db: PgPool) {
            let (id, cookie, app) = setup(&db, true).await;
            let (_, html) = get(&app, &format!("/admin/items/{id}"), &cookie).await;
            assert!(
                html.contains("Остатки меняются складскими документами"),
                "{html}"
            );
            assert!(html.contains("12.5"), "закупочная цена: {html}");
            assert!(
                html.contains("name=\"amount_1\"") && html.contains("disabled"),
                "{html}"
            );
            save(&app, &cookie, id, "99").await;
            let (quantity, amount) = amounts(&db, id).await;
            assert_eq!((quantity, amount), (5.0, 5.0));
        }

        #[sqlx::test]
        async fn item_tab_editable_without_store_control(db: PgPool) {
            let (id, cookie, app) = setup(&db, false).await;
            let (_, html) = get(&app, &format!("/admin/items/{id}"), &cookie).await;
            assert!(
                !html.contains("Остатки меняются складскими документами"),
                "{html}"
            );
            save(&app, &cookie, id, "99").await;
            let (_, amount) = amounts(&db, id).await;
            assert_eq!(amount, 99.0);
        }

        #[sqlx::test]
        async fn negative_amount_display(db: PgPool) {
            let (id, cookie, app) = setup(&db, true).await;
            sqlx::query("UPDATE catalog_store_amounts SET amount = -2 WHERE item_id = $1")
                .bind(id)
                .execute(&db)
                .await
                .unwrap();
            let (_, html) = get(&app, &format!("/admin/items/{id}"), &cookie).await;
            assert!(!html.contains("value=\"-2\""), "{html}");
            sqlx::query(
                "INSERT INTO options (module, name, value) VALUES ('catalog', 'allow_negative_amount', 'Y')
                 ON CONFLICT (module, name) DO UPDATE SET value = 'Y'",
            )
            .execute(&db)
            .await
            .unwrap();
            let (_, html) = get(&app, &format!("/admin/items/{id}"), &cookie).await;
            assert!(html.contains("value=\"-2\""), "{html}");
        }
    }

    mod card_tabs {
        use axum::http::StatusCode;
        use sqlx::PgPool;

        use crate::{
            smoke_tests::{get, post_multipart},
            test_support::{admin_cookie, test_state},
        };

        /// Коллекция (опционально с полем и каталогом) и одна запись в ней.
        async fn setup(db: &PgPool, with_field: bool, catalog: bool) -> (i64, i64) {
            let c: i64 = sqlx::query_scalar(
                "INSERT INTO collections (code, name, is_catalog) VALUES ('goods', 'Товары', $1) RETURNING id",
            )
            .bind(catalog)
            .fetch_one(db)
            .await
            .unwrap();
            if with_field {
                sqlx::query(
                    "INSERT INTO collection_fields (collection_id, code, name, kind) VALUES ($1, 'note', 'Заметка', 'string')",
                )
                .bind(c)
                .execute(db)
                .await
                .unwrap();
            }
            if catalog {
                sqlx::query("INSERT INTO catalog_price_types (id, code, name, is_base, sort) VALUES (1, 'BASE', 'Розница', TRUE, 100)")
                    .execute(db)
                    .await
                    .unwrap();
            }
            let item: i64 = sqlx::query_scalar(
                "INSERT INTO collection_items (collection_id, code, name) VALUES ($1, 'tovar', 'Товар') RETURNING id",
            )
            .bind(c)
            .fetch_one(db)
            .await
            .unwrap();
            (c, item)
        }

        fn has_tab(html: &str, key: &str) -> bool {
            html.contains(&format!("data-tab-btn=\"{key}\""))
        }

        #[sqlx::test]
        async fn item_card_tabs_visibility(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let (_, item) = setup(&db, false, false).await;
            let (s, html) = get(&app, &format!("/admin/items/{item}"), &cookie).await;
            assert_eq!(s, StatusCode::OK);
            assert!(has_tab(&html, "main") && has_tab(&html, "text"), "{html}");
            for k in ["fields", "catalog", "offers"] {
                assert!(!has_tab(&html, k), "{k}: {html}");
            }
            // каталог и поля открывают свои вкладки
            let (c, item) = setup_second(&db).await;
            let (_, html) = get(&app, &format!("/admin/items/{item}"), &cookie).await;
            assert!(
                has_tab(&html, "fields") && has_tab(&html, "catalog"),
                "{html}"
            );
            assert!(!has_tab(&html, "offers"), "{html}");
            // коллекция предложений делает видимой вкладку «Предложения»
            sqlx::query(
                "INSERT INTO collections (code, name, product_collection_id) VALUES ('goods_offers', 'Предложения', $1)",
            )
            .bind(c)
            .execute(&db)
            .await
            .unwrap();
            let (_, html) = get(&app, &format!("/admin/items/{item}"), &cookie).await;
            assert!(has_tab(&html, "offers"), "{html}");
        }

        /// Вторая коллекция с полем и каталогом (цены уже заведены).
        async fn setup_second(db: &PgPool) -> (i64, i64) {
            let c: i64 = sqlx::query_scalar(
                "INSERT INTO collections (code, name, is_catalog) VALUES ('goods2', 'Товары 2', TRUE) RETURNING id",
            )
            .fetch_one(db)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO collection_fields (collection_id, code, name, kind) VALUES ($1, 'note', 'Заметка', 'string')",
            )
            .bind(c)
            .execute(db)
            .await
            .unwrap();
            sqlx::query("INSERT INTO catalog_price_types (id, code, name, is_base, sort) VALUES (1, 'BASE', 'Розница', TRUE, 100)")
                .execute(db)
                .await
                .unwrap();
            let item: i64 = sqlx::query_scalar(
                "INSERT INTO collection_items (collection_id, code, name) VALUES ($1, 'tovar2', 'Товар 2') RETURNING id",
            )
            .bind(c)
            .fetch_one(db)
            .await
            .unwrap();
            (c, item)
        }

        #[sqlx::test]
        async fn item_form_actions_marked(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let (_, item) = setup(&db, false, true).await;
            let (_, html) = get(&app, &format!("/admin/items/{item}"), &cookie).await;
            assert!(
                html.contains("<div class=\"form-actions\" data-form-actions>"),
                "{html}"
            );
        }

        #[sqlx::test]
        async fn item_tab_query_opens_tab(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let (_, item) = setup(&db, false, true).await;
            let (_, html) = get(&app, &format!("/admin/items/{item}?tab=catalog"), &cookie).await;
            assert!(html.contains("<section data-tab=\"catalog\">"), "{html}");
            assert!(
                html.contains("<section data-tab=\"main\" hidden>"),
                "{html}"
            );
            // неизвестная вкладка — первая
            let (_, html) = get(&app, &format!("/admin/items/{item}?tab=nope"), &cookie).await;
            assert!(html.contains("<section data-tab=\"main\">"), "{html}");
        }

        #[sqlx::test]
        async fn item_error_opens_error_tab(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let (_, item) = setup(&db, false, true).await;
            let (s, html) = post_multipart(
                &app,
                &format!("/admin/items/{item}"),
                &cookie,
                &[
                    ("name", ""),
                    ("sort", "500"),
                    ("tab", "catalog"),
                    ("price_1", "abc"),
                ],
            )
            .await;
            assert_eq!(s, StatusCode::OK);
            // активна «Основное» (первая с ошибкой), точки на «Основное» и «Каталог»
            assert!(html.contains("<section data-tab=\"main\">"), "{html}");
            assert!(
                html.contains("<section data-tab=\"catalog\" hidden>"),
                "{html}"
            );
            assert_eq!(html.matches("tab-dot err").count(), 2, "{html}");
            let main = html.find("<section data-tab=\"main\">").unwrap();
            let cat = html.find("<section data-tab=\"catalog\"").unwrap();
            let name_err = html.find("Укажите название").unwrap();
            let price_err = html.find("Цена: ожидается число").unwrap();
            assert!(main < name_err && name_err < cat, "{html}");
            assert!(cat < price_err, "{html}");
        }

        #[sqlx::test]
        async fn item_save_returns_to_tab(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let (_, item) = setup(&db, false, true).await;
            let res = post_multipart_location(
                &app,
                &format!("/admin/items/{item}"),
                &cookie,
                &[("name", "Товар"), ("sort", "500"), ("tab", "catalog")],
            )
            .await;
            assert_eq!(res.0, StatusCode::SEE_OTHER);
            assert_eq!(res.1, format!("/admin/items/{item}?tab=catalog"));
            assert!(res.2.contains("admin_flash"), "{}", res.2);
        }

        /// Заголовки Location и Set-Cookie ответа на multipart-форму.
        async fn post_multipart_location(
            app: &axum::Router,
            path: &str,
            cookie: &str,
            fields: &[(&str, &str)],
        ) -> (StatusCode, String, String) {
            use tower::ServiceExt;
            let boundary = "tabboundary";
            let mut body = String::new();
            for (name, value) in fields {
                body.push_str(&format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                ));
            }
            body.push_str(&format!("--{boundary}--\r\n"));
            let req = axum::http::Request::post(path)
                .header(axum::http::header::COOKIE, cookie)
                .header(axum::http::header::HOST, "127.0.0.1:3000")
                .header(axum::http::header::ORIGIN, "http://127.0.0.1:3000")
                .header(
                    axum::http::header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(axum::body::Body::from(body))
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            let h = |name| {
                res.headers()
                    .get(name)
                    .map(|v| v.to_str().unwrap().to_string())
                    .unwrap_or_default()
            };
            (
                res.status(),
                h(axum::http::header::LOCATION),
                h(axum::http::header::SET_COOKIE),
            )
        }

        #[sqlx::test]
        async fn save_from_any_tab_sends_all_fields(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let (_, item) = setup(&db, false, true).await;
            let (_, html) = get(&app, &format!("/admin/items/{item}"), &cookie).await;
            // одна форма: поля «Основного» и «Каталога» — внутри неё, вкладка в скрытом поле
            let form_start = html.find("<form class=\"card form\"").unwrap();
            let form_end = html[form_start..].find("</form>").unwrap() + form_start;
            let form = &html[form_start..form_end];
            for name in ["name=\"name\"", "name=\"price_1\"", "name=\"tab\""] {
                assert!(form.contains(name), "{name}: {html}");
            }
            assert_eq!(html.matches("name=\"tab\"").count(), 1, "{html}");
            let (s, _) = post_multipart(
                &app,
                &format!("/admin/items/{item}"),
                &cookie,
                &[
                    ("name", "Новое имя"),
                    ("sort", "500"),
                    ("price_1", "42"),
                    ("tab", "main"),
                ],
            )
            .await;
            assert!(s.is_redirection(), "{s}");
            let name: String =
                sqlx::query_scalar("SELECT name FROM collection_items WHERE id = $1")
                    .bind(item)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            let price: f64 = sqlx::query_scalar(
                "SELECT price::float8 FROM catalog_prices WHERE item_id = $1 AND price_type_id = 1",
            )
            .bind(item)
            .fetch_one(&db)
            .await
            .unwrap();
            assert_eq!((name.as_str(), price), ("Новое имя", 42.0));
        }

        #[sqlx::test]
        async fn new_item_only_form_tabs(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let (c, _) = setup(&db, true, true).await;
            sqlx::query(
                "INSERT INTO collections (code, name, product_collection_id) VALUES ('goods_offers', 'Предложения', $1)",
            )
            .bind(c)
            .execute(&db)
            .await
            .unwrap();
            let (s, html) = get(&app, &format!("/admin/collections/{c}/items/new"), &cookie).await;
            assert_eq!(s, StatusCode::OK);
            for k in ["main", "text", "fields", "catalog"] {
                assert!(has_tab(&html, k), "{k}: {html}");
            }
            assert!(!has_tab(&html, "offers"), "{html}");
            assert!(!html.contains("/delete"), "{html}");
        }

        #[sqlx::test]
        async fn item_delete_button_in_header(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let (_, item) = setup(&db, false, false).await;
            let (_, html) = get(&app, &format!("/admin/items/{item}"), &cookie).await;
            let action = format!("action=\"/admin/items/{item}/delete\"");
            let at = html.find(&action).expect(&html);
            assert!(at < html.find("data-card>").unwrap(), "{html}");
            assert!(
                html[at..].contains("hx-confirm=\"Удалить «Товар»?\""),
                "{html}"
            );
            assert_eq!(html.matches(&action).count(), 1, "{html}");
            assert!(!html.contains("<h2>Удаление</h2>"), "{html}");
        }
    }
    mod peek {
        use axum::http::StatusCode;
        use sqlx::PgPool;

        use crate::test_support::{admin_cookie, htmx_get, test_state, user_cookie};

        struct Fx {
            collection: i64,
            item: i64,
            section: i64,
        }

        /// Каталог с ценой 123.5 RUB, двумя складами и записью с именем `name`.
        async fn fixture(db: &PgPool, name: &str, catalog: bool) -> Fx {
            let collection: i64 = sqlx::query_scalar(
                "INSERT INTO collections (code, name, is_catalog) VALUES ('goods', 'Товары', $1) RETURNING id",
            )
            .bind(catalog)
            .fetch_one(db)
            .await
            .unwrap();
            sqlx::query("INSERT INTO catalog_price_types (id, code, name, is_base, sort) VALUES (1, 'BASE', 'Розница', TRUE, 100)")
                .execute(db)
                .await
                .unwrap();
            sqlx::query("INSERT INTO collection_sections (collection_id, code, name) VALUES ($1, 'razdel', 'Раздел Икс')")
                .bind(collection)
                .execute(db)
                .await
                .unwrap();
            let item: i64 = sqlx::query_scalar(
                "INSERT INTO collection_items (collection_id, section_id, code, name, active)
                 VALUES ($1, (SELECT id FROM collection_sections WHERE collection_id = $1), 'tovar-1', $2, TRUE)
                 RETURNING id",
            )
            .bind(collection)
            .bind(name)
            .fetch_one(db)
            .await
            .unwrap();
            sqlx::query("INSERT INTO catalog_prices (item_id, price_type_id, price, currency) VALUES ($1, 1, 123.5, 'RUB')")
                .bind(item)
                .execute(db)
                .await
                .unwrap();
            sqlx::query("INSERT INTO catalog_stores (id, name) VALUES (11, 'Склад Север'), (12, 'Склад Юг')")
                .execute(db)
                .await
                .unwrap();
            sqlx::query("INSERT INTO catalog_products (item_id, quantity) VALUES ($1, 7) ON CONFLICT (item_id) DO UPDATE SET quantity = 7")
                .bind(item)
                .execute(db)
                .await
                .unwrap();
            sqlx::query("INSERT INTO catalog_store_amounts (item_id, store_id, amount) VALUES ($1, 11, 3), ($1, 12, 4)")
                .bind(item)
                .execute(db)
                .await
                .unwrap();
            let section =
                sqlx::query_scalar("SELECT section_id FROM collection_items WHERE id = $1")
                    .bind(item)
                    .fetch_one(db)
                    .await
                    .unwrap();
            Fx {
                collection,
                item,
                section,
            }
        }

        /// Пользователь без прав администратора с доступом `level` к коллекции.
        async fn cookie_with(db: &PgPool, collection: i64, level: Option<&str>) -> String {
            let (uid, cookie) = user_cookie(db, "editor", false).await;
            let group: i64 = sqlx::query_scalar(
                "INSERT INTO groups (code, name) VALUES ('ed', 'ed') RETURNING id",
            )
            .fetch_one(db)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO group_permissions (group_id, permission) VALUES ($1, 'admin.access')",
            )
            .bind(group)
            .execute(db)
            .await
            .unwrap();
            sqlx::query("INSERT INTO user_groups (user_id, group_id) VALUES ($1, $2)")
                .bind(uid)
                .bind(group)
                .execute(db)
                .await
                .unwrap();
            if let Some(level) = level {
                sqlx::query("INSERT INTO collection_access (collection_id, group_id, level) VALUES ($1, $2, $3)")
                    .bind(collection)
                    .bind(group)
                    .bind(level)
                    .execute(db)
                    .await
                    .unwrap();
            }
            cookie
        }

        async fn post_active(
            app: &axum::Router,
            item: i64,
            cookie: &str,
            body: &str,
            target: Option<&str>,
        ) -> (StatusCode, axum::http::HeaderMap, String) {
            use axum::http::header;
            use tower::ServiceExt;
            let mut req = axum::http::Request::post(format!("/admin/items/{item}/active"))
                .header(header::COOKIE, cookie)
                .header(header::HOST, "127.0.0.1:3000")
                .header(header::ORIGIN, "http://127.0.0.1:3000")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            if let Some(t) = target {
                req = req.header("HX-Request", "true").header("HX-Target", t);
            }
            let res = app
                .clone()
                .oneshot(req.body(axum::body::Body::from(body.to_string())).unwrap())
                .await
                .unwrap();
            let (status, headers) = (res.status(), res.headers().clone());
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                status,
                headers,
                String::from_utf8_lossy(&bytes).into_owned(),
            )
        }

        #[sqlx::test]
        async fn item_peek_fragment(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let fx = fixture(&db, "Дрель", true).await;
            let (s, html) = htmx_get(
                &app,
                &format!("/admin/items/{}/peek", fx.item),
                &cookie,
                "peek",
            )
            .await;
            assert_eq!(s, StatusCode::OK);
            assert!(html.contains("Дрель") && html.contains("tovar-1"), "{html}");
            assert!(html.contains("Раздел Икс"), "раздел: {html}");
            assert!(
                html.contains("123.5") && html.contains("RUB"),
                "цена: {html}"
            );
            assert!(
                html.contains("Склад Север") && html.contains("Склад Юг"),
                "склады: {html}"
            );
            assert!(html.contains("Доступное количество"), "{html}");
            assert!(
                html.contains("Открыть карточку")
                    && html.contains(&format!("items&#x2f;{}\"", fx.item)),
                "{html}"
            );
            assert!(!html.contains("<html"), "фрагмент");
        }

        #[sqlx::test]
        async fn item_peek_forbidden_without_access(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let fx = fixture(&db, "Дрель", true).await;
            let cookie = cookie_with(&db, fx.collection, None).await;
            let (s, _) = htmx_get(
                &app,
                &format!("/admin/items/{}/peek", fx.item),
                &cookie,
                "peek",
            )
            .await;
            assert_eq!(s, StatusCode::FORBIDDEN);
            let read = cookie_with_second(&db, fx.collection).await;
            let (s, _) = htmx_get(
                &app,
                &format!("/admin/items/{}/peek", fx.item),
                &read,
                "peek",
            )
            .await;
            assert_eq!(s, StatusCode::OK, "чтение открывает панель");
        }

        async fn cookie_with_second(db: &PgPool, collection: i64) -> String {
            let (uid, cookie) = user_cookie(db, "reader", false).await;
            let group: i64 = sqlx::query_scalar(
                "INSERT INTO groups (code, name) VALUES ('rd', 'rd') RETURNING id",
            )
            .fetch_one(db)
            .await
            .unwrap();
            for q in [
                "INSERT INTO group_permissions (group_id, permission) VALUES ($1, 'admin.access')",
                "INSERT INTO user_groups (group_id, user_id) VALUES ($1, $2)",
            ] {
                let mut query = sqlx::query(q).bind(group);
                if q.contains("user_groups") {
                    query = query.bind(uid);
                }
                query.execute(db).await.unwrap();
            }
            sqlx::query("INSERT INTO collection_access (collection_id, group_id, level) VALUES ($1, $2, 'read')")
                .bind(collection)
                .bind(group)
                .execute(db)
                .await
                .unwrap();
            cookie
        }

        #[sqlx::test]
        async fn item_active_toggle(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let fx = fixture(&db, "Дрель", true).await;
            let writer = cookie_with(&db, fx.collection, Some("write")).await;
            let before: chrono::DateTime<chrono::Utc> =
                sqlx::query_scalar("SELECT updated_at FROM collection_items WHERE id = $1")
                    .bind(fx.item)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            let (s, headers, html) =
                post_active(&app, fx.item, &writer, "active=off", Some("peek")).await;
            assert_eq!(s, StatusCode::OK, "{html}");
            let active: bool =
                sqlx::query_scalar("SELECT active FROM collection_items WHERE id = $1")
                    .bind(fx.item)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert!(!active);
            let after: chrono::DateTime<chrono::Utc> =
                sqlx::query_scalar("SELECT updated_at FROM collection_items WHERE id = $1")
                    .bind(fx.item)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert!(after > before, "updated_at обновлён");
            assert!(headers.contains_key("hx-trigger"), "тост");
            assert!(
                html.contains("Дрель") && html.contains("Открыть карточку"),
                "панель: {html}"
            );
            assert!(
                html.contains("data-list-changed"),
                "метка обновления списка: {html}"
            );
            assert!(!html.contains("<template>"), "{html}");
            // обратно
            let (s, _, _) = post_active(&app, fx.item, &writer, "active=on", Some("peek")).await;
            assert_eq!(s, StatusCode::OK);
            let active: bool =
                sqlx::query_scalar("SELECT active FROM collection_items WHERE id = $1")
                    .bind(fx.item)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert!(active);
            // только чтение — 403, значение не меняется
            let reader = cookie_with_second(&db, fx.collection).await;
            let (s, _, _) = post_active(&app, fx.item, &reader, "active=off", Some("peek")).await;
            assert_eq!(s, StatusCode::FORBIDDEN);
            let active: bool =
                sqlx::query_scalar("SELECT active FROM collection_items WHERE id = $1")
                    .bind(fx.item)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert!(active);
        }

        #[sqlx::test]
        async fn item_active_without_peek_redirects(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let fx = fixture(&db, "Дрель", true).await;
            let (s, headers, _) = post_active(&app, fx.item, &cookie, "active=off", None).await;
            assert_eq!(s, StatusCode::SEE_OTHER);
            assert_eq!(
                headers.get("location").unwrap(),
                &format!("/admin/items/{}", fx.item)
            );
        }

        #[sqlx::test]
        async fn no_peek_for_non_catalog_collection(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let plain = fixture(&db, "Новость", false).await;
            let (_, list) = crate::smoke_tests::get(
                &app,
                &format!(
                    "/admin/collections/{}/items?section={}",
                    plain.collection, plain.section
                ),
                &cookie,
            )
            .await;
            assert!(
                list.contains("Новость") && !list.contains("data-peek"),
                "{list}"
            );
            let (s, _) = htmx_get(
                &app,
                &format!("/admin/items/{}/peek", plain.item),
                &cookie,
                "peek",
            )
            .await;
            assert_eq!(s, StatusCode::NOT_FOUND);
        }

        #[sqlx::test]
        async fn catalog_list_rows_have_peek_attr(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let fx = fixture(&db, "Дрель", true).await;
            let (_, list) = crate::smoke_tests::get(
                &app,
                &format!(
                    "/admin/collections/{}/items?section={}",
                    fx.collection, fx.section
                ),
                &cookie,
            )
            .await;
            assert!(
                list.contains(&format!(
                    "data-open=\"&#x2f;admin&#x2f;items&#x2f;{0}\" data-peek=\"&#x2f;admin&#x2f;items&#x2f;{0}&#x2f;peek\"",
                    fx.item
                )),
                "{list}"
            );
            assert!(list.contains(&format!("href=\"&#x2f;admin&#x2f;items&#x2f;{}\"", fx.item)));
        }

        #[sqlx::test]
        async fn item_peek_escapes_name(db: PgPool) {
            let app = crate::app(test_state(db.clone()));
            let cookie = admin_cookie(&db).await;
            let fx = fixture(&db, "<script>alert(1)</script>", true).await;
            let (_, html) = htmx_get(
                &app,
                &format!("/admin/items/{}/peek", fx.item),
                &cookie,
                "peek",
            )
            .await;
            assert!(!html.contains("<script>"), "{html}");
            assert!(html.contains("&lt;script&gt;"), "{html}");
            let (_, _, html) =
                post_active(&app, fx.item, &cookie, "active=off", Some("peek")).await;
            assert!(!html.contains("<script>"), "{html}");
        }
    }
}

#[cfg(test)]
mod list_tests {
    use axum::http::StatusCode;
    use sqlx::PgPool;

    use crate::collection::repo;
    use crate::test_support::{
        admin_cookie, content_fixture, get_raw, htmx_get, permitted_cookie, test_state, user_cookie,
    };

    async fn section(db: &PgPool, col: i64, parent: Option<i64>, name: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO collection_sections (collection_id, parent_id, code, name)
             VALUES ($1, $2, lower($3), $3) RETURNING id",
        )
        .bind(col)
        .bind(parent)
        .bind(name)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn item(db: &PgPool, col: i64, sec: Option<i64>, name: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, section_id, code, name)
             VALUES ($1, $2, lower($3), $3) RETURNING id",
        )
        .bind(col)
        .bind(sec)
        .bind(name)
        .fetch_one(db)
        .await
        .unwrap()
    }

    // ---------- массовые действия ----------

    use crate::admin::list::bulk::BulkCtx;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, header},
    };
    use tower::ServiceExt;

    async fn bulk_post(app: &axum::Router, cookie: &str, body: &str) -> (StatusCode, String) {
        let req = Request::post("/admin/lists/bulk")
            .header(header::COOKIE, cookie)
            .header(header::HOST, "127.0.0.1:3000")
            .header(header::ORIGIN, "http://127.0.0.1:3000")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("hx-request", "true")
            .header("hx-target", "list-body")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn bulk_form(col: i64, action: &str, param: &str, scope: &str, ids: &[i64]) -> String {
        let ids: Vec<String> = ids.iter().map(i64::to_string).collect();
        format!(
            "list_key=collection%3A{col}&path=%2Fadmin%2Fcollections%2F{col}%2Fitems&query=&action={action}&param={param}&scope={scope}&expect=3&ids={}",
            ids.join(",")
        )
    }

    async fn state_of(db: &PgPool, id: i64) -> (bool, Option<i64>) {
        sqlx::query_as("SELECT active, section_id FROM collection_items WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn bulk_activate_deactivate_move(db: PgPool) {
        let fx = content_fixture(&db).await;
        let c = fx.collection_id;
        let sec = section(&db, c, None, "Второй").await;
        let a = item(&db, c, None, "A").await;
        let b = item(&db, c, None, "B").await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        // по ids, дважды (идемпотентно)
        for _ in 0..2 {
            let (st, body) = bulk_post(
                &app,
                &cookie,
                &bulk_form(c, "deactivate", "", "ids", &[a, b]),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{body}");
            assert!(!body.contains("bulk-summary"), "{body}");
            assert!(!state_of(&db, a).await.0 && !state_of(&db, b).await.0);
        }
        assert!(state_of(&db, fx.item_id).await.0);
        for _ in 0..2 {
            let (st, body) =
                bulk_post(&app, &cookie, &bulk_form(c, "activate", "", "ids", &[a])).await;
            assert_eq!(st, StatusCode::OK, "{body}");
            assert!(state_of(&db, a).await.0 && !state_of(&db, b).await.0);
        }
        let stamp = |id: i64| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>(
                    "SELECT updated_at FROM collection_items WHERE id = $1",
                )
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };
        let before = stamp(a).await;
        let (st, body) = bulk_post(
            &app,
            &cookie,
            &bulk_form(c, "move", &sec.to_string(), "ids", &[a, b]),
        )
        .await;
        assert!(stamp(a).await > before);
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(state_of(&db, a).await.1, Some(sec));
        assert_eq!(state_of(&db, b).await.1, Some(sec));
        let (st, _) = bulk_post(&app, &cookie, &bulk_form(c, "move", "none", "ids", &[a])).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(state_of(&db, a).await.1, None);
        let (st, body) = bulk_post(
            &app,
            &cookie,
            &bulk_form(c, "move", &sec.to_string(), "filter", &[]),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        for id in [a, b, fx.item_id] {
            assert_eq!(state_of(&db, id).await.1, Some(sec));
        }
        // по фильтру: все записи коллекции
        let (st, body) = bulk_post(
            &app,
            &cookie,
            &bulk_form(c, "deactivate", "", "filter", &[]),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        for id in [a, b, fx.item_id] {
            assert!(!state_of(&db, id).await.0);
        }
        let (st, _) = bulk_post(&app, &cookie, &bulk_form(c, "activate", "", "filter", &[])).await;
        assert_eq!(st, StatusCode::OK);
        assert!(state_of(&db, fx.item_id).await.0);
    }

    #[sqlx::test]
    async fn bulk_move_foreign_section_422(db: PgPool) {
        let fx = content_fixture(&db).await;
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name) VALUES ('o', 'O') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let foreign = section(&db, other, None, "Чужой").await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let c = fx.collection_id;
        for param in [foreign.to_string(), "999999".into(), "abc".into()] {
            let (st, _) = bulk_post(
                &app,
                &cookie,
                &bulk_form(c, "move", &param, "ids", &[fx.item_id]),
            )
            .await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{param}");
        }
        assert_eq!(state_of(&db, fx.item_id).await.1, Some(fx.section_id));
        // повторная проверка в самом действии
        let state = test_state(db.clone());
        let (_, user) = user_access(&db, &state).await;
        let ctx = BulkCtx {
            state: &state,
            user: &user,
            notify: false,
            collection_id: Some(c),
            collection: Default::default(),
            mails: Default::default(),
        };
        let r = super::bulk_run(&ctx, "move", &foreign.to_string(), fx.item_id).await;
        assert_eq!(r, Err("раздел не найден".to_string()));
    }

    async fn user_access(
        db: &PgPool,
        _state: &crate::state::AppState,
    ) -> (i64, crate::access::Access) {
        let user = crate::auth::create_user(db, "bulk-admin", "password123".into(), true)
            .await
            .unwrap();
        (
            user.id,
            crate::access::Access::load(db, user).await.unwrap(),
        )
    }

    #[sqlx::test]
    async fn bulk_offers_need_parent_write(db: PgPool) {
        let fx = content_fixture(&db).await;
        let offers: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name, product_collection_id) VALUES ('offs', 'Предложения', $1) RETURNING id",
        )
        .bind(fx.collection_id)
        .fetch_one(&db)
        .await
        .unwrap();
        let o = item(&db, offers, None, "O1").await;
        let cookie = permitted_cookie(&db, "offerer", &[]).await;
        let gid: i64 = sqlx::query_scalar("SELECT id FROM groups WHERE code = 'g_offerer'")
            .fetch_one(&db)
            .await
            .unwrap();
        for (coll, level) in [(offers, "write"), (fx.collection_id, "read")] {
            sqlx::query("INSERT INTO collection_access (collection_id, group_id, level) VALUES ($1, $2, $3)")
                .bind(coll)
                .bind(gid)
                .bind(level)
                .execute(&db)
                .await
                .unwrap();
        }
        let app = crate::app(test_state(db.clone()));
        let coll = repo::get_collection(&db, offers).await.unwrap().unwrap();
        let user = {
            let uid: i64 = sqlx::query_scalar("SELECT id FROM users WHERE login = 'offerer'")
                .fetch_one(&db)
                .await
                .unwrap();
            let u: crate::auth::User = sqlx::query_as(
                "SELECT id, login, email, is_admin, active FROM users WHERE id = $1",
            )
            .bind(uid)
            .fetch_one(&db)
            .await
            .unwrap();
            crate::access::Access::load(&db, u).await.unwrap()
        };
        let spec = crate::admin::list::collection::collection_spec(&db, &user, &coll)
            .await
            .unwrap();
        assert!(spec.bulk_actions.is_empty());
        for (action, param) in [("deactivate", ""), ("move", "none"), ("delete", "")] {
            let (st, _) = bulk_post(
                &app,
                &cookie,
                &bulk_form(offers, action, param, "ids", &[o]),
            )
            .await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{action}");
        }
        assert_eq!(state_of(&db, o).await, (true, None));
        // и в самом действии
        let state = test_state(db.clone());
        let ctx = BulkCtx {
            state: &state,
            user: &user,
            notify: false,
            collection_id: Some(offers),
            collection: Default::default(),
            mails: Default::default(),
        };
        for (action, param) in [("deactivate", ""), ("move", "none"), ("delete", "")] {
            assert_eq!(
                super::bulk_run(&ctx, action, param, o).await,
                Err("нет права".to_string()),
                "{action}"
            );
        }
        assert_eq!(state_of(&db, o).await, (true, None));
    }

    #[sqlx::test]
    async fn bulk_delete_like_single(db: PgPool) {
        let fx = content_fixture(&db).await;
        let c = fx.collection_id;
        let offers: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name, product_collection_id) VALUES ('offs', 'Предложения', $1) RETURNING id",
        )
        .bind(c)
        .fetch_one(&db)
        .await
        .unwrap();
        let mut products = Vec::new();
        for name in ["P1", "P2"] {
            let p = item(&db, c, None, name).await;
            for n in 0..2 {
                let o = item(&db, offers, None, &format!("{name}-o{n}")).await;
                sqlx::query("UPDATE collection_items SET product_id = $2 WHERE id = $1")
                    .bind(o)
                    .bind(p)
                    .execute(&db)
                    .await
                    .unwrap();
            }
            products.push(p);
        }
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (st, _, _) = crate::test_support::post_form_raw(
            &app,
            &format!("/admin/items/{}/delete", products[0]),
            &cookie,
            "",
        )
        .await;
        assert!(st.is_redirection(), "{st}");
        let (st, body) = bulk_post(
            &app,
            &cookie,
            &bulk_form(c, "delete", "", "ids", &[products[1]]),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let left = |name: &'static str| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM collection_items WHERE name LIKE $1",
                )
                .bind(format!("{name}%"))
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };
        assert_eq!((left("P1").await, left("P2").await), (0, 0));
        assert!(state_of(&db, fx.item_id).await.0);
    }

    #[sqlx::test]
    async fn bulk_delete_missing_record(db: PgPool) {
        let fx = content_fixture(&db).await;
        let c = fx.collection_id;
        let a = item(&db, c, None, "A").await;
        let b = item(&db, c, None, "B").await;
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name) VALUES ('o', 'O') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let foreign = item(&db, other, None, "F").await;
        let state = test_state(db.clone());
        let (_, user) = user_access(&db, &state).await;
        let ctx = BulkCtx {
            state: &state,
            user: &user,
            notify: false,
            collection_id: Some(c),
            collection: Default::default(),
            mails: Default::default(),
        };
        // запись удалена между выбором и выполнением
        sqlx::query("DELETE FROM collection_items WHERE id = $1")
            .bind(a)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            super::bulk_run(&ctx, "delete", "", a).await,
            Err("запись не найдена".to_string())
        );
        // перемещена в другую коллекцию
        assert_eq!(
            super::bulk_run(&ctx, "delete", "", foreign).await,
            Err("запись не найдена".to_string())
        );
        assert_eq!(super::bulk_run(&ctx, "delete", "", b).await, Ok(()));
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM collection_items WHERE id = ANY($1)")
                .bind([a, b, foreign])
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(left, 1);
        assert_eq!(
            super::bulk_run(&ctx, "nope", "", fx.item_id).await,
            Err("неизвестное действие".to_string())
        );
    }

    #[sqlx::test]
    async fn bulk_readonly_user(db: PgPool) {
        let fx = content_fixture(&db).await;
        let c = fx.collection_id;
        let cookie = permitted_cookie(&db, "reader", &[]).await;
        let uid: i64 = sqlx::query_scalar("SELECT id FROM users WHERE login = 'reader'")
            .fetch_one(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO user_groups (user_id, group_id) VALUES ($1, $2)")
            .bind(uid)
            .bind(fx.group_id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE collection_access SET level = 'read' WHERE group_id = $1")
            .bind(fx.group_id)
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let (s, _, b) = get_raw(&app, &format!("/admin/collections/{c}/items"), &cookie).await;
        assert_eq!(s, StatusCode::OK);
        assert!(!String::from_utf8_lossy(&b).contains("data-bulk"));
        for action in ["delete", "deactivate"] {
            let (st, _) = bulk_post(
                &app,
                &cookie,
                &bulk_form(c, action, "", "ids", &[fx.item_id]),
            )
            .await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{action}");
        }
        assert!(state_of(&db, fx.item_id).await.0);
    }

    #[sqlx::test]
    async fn bulk_checkbox_column_only_with_actions(db: PgPool) {
        let fx = content_fixture(&db).await;
        let c = fx.collection_id;
        item(&db, c, None, "Запись").await;
        let app = crate::app(test_state(db.clone()));
        let admin = admin_cookie(&db).await;
        let (s, _, b) = get_raw(&app, &format!("/admin/collections/{c}/items"), &admin).await;
        assert_eq!(s, StatusCode::OK);
        let html = String::from_utf8_lossy(&b);
        assert!(html.contains("data-bulk-all"), "шапка с чекбоксом");
        assert!(
            html.contains("<input type=\"checkbox\" data-bulk "),
            "чекбокс строки"
        );
        assert!(
            html.contains("\"bulk_actions\":[{"),
            "действия в описании списка"
        );
        assert!(html.contains("data-query=\""), "состояние в теле списка");
        // пользователь с правом только на чтение: ни колонки, ни действий
        let cookie = permitted_cookie(&db, "reader2", &[]).await;
        let uid: i64 = sqlx::query_scalar("SELECT id FROM users WHERE login = 'reader2'")
            .fetch_one(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO user_groups (user_id, group_id) VALUES ($1, $2)")
            .bind(uid)
            .bind(fx.group_id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE collection_access SET level = 'read' WHERE group_id = $1")
            .bind(fx.group_id)
            .execute(&db)
            .await
            .unwrap();
        let (s, _, b) = get_raw(&app, &format!("/admin/collections/{c}/items"), &cookie).await;
        assert_eq!(s, StatusCode::OK);
        let html = String::from_utf8_lossy(&b);
        assert!(!html.contains("data-bulk"));
        assert!(html.contains("\"bulk_actions\":[]"));
    }

    #[sqlx::test]
    async fn items_list_shows_subsection_items(db: PgPool) {
        let fx = content_fixture(&db).await;
        let sub = section(&db, fx.collection_id, Some(fx.section_id), "Подраздел Б").await;
        item(&db, fx.collection_id, Some(sub), "ВтораяНовость").await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let base = format!("/admin/collections/{}/items", fx.collection_id);

        let (s, _, b) = get_raw(&app, &format!("{base}?section={}", fx.section_id), &cookie).await;
        let html = String::from_utf8_lossy(&b);
        assert_eq!(s, StatusCode::OK);
        assert!(
            html.contains("Первая новость") && html.contains("ВтораяНовость"),
            "{html}"
        );
        assert!(html.contains("включая подразделы"), "{html}");
        assert!(
            html.contains("Подраздел Б"),
            "ветка до раздела в дереве: {html}"
        );
        assert!(
            html.contains("Раздел: ") || html.contains("class=\"chip section\""),
            "{html}"
        );

        let (_, frag) = htmx_get(
            &app,
            &format!("{base}?section={}", fx.section_id),
            &cookie,
            "list-body",
        )
        .await;
        assert!(
            frag.contains("hx-swap-oob")
                && frag.contains("Вся коллекция")
                && !frag.contains("<html"),
            "{frag}"
        );
        assert!(!html.contains("<h2>Раздел"), "подразделы таблицей убраны");

        let (_, _, b) = get_raw(
            &app,
            &format!("{base}?section={}&only=1", fx.section_id),
            &cookie,
        )
        .await;
        let html = String::from_utf8_lossy(&b);
        assert!(
            html.contains("Первая новость") && !html.contains("ВтораяНовость"),
            "{html}"
        );
    }

    #[sqlx::test]
    async fn items_without_section(db: PgPool) {
        let fx = content_fixture(&db).await;
        item(&db, fx.collection_id, None, "СиротаЗапись").await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (_, _, b) = get_raw(
            &app,
            &format!("/admin/collections/{}/items?section=none", fx.collection_id),
            &cookie,
        )
        .await;
        let html = String::from_utf8_lossy(&b);
        assert!(
            html.contains("СиротаЗапись") && !html.contains("Первая новость"),
            "{html}"
        );
        assert!(
            html.contains("Раздел:") && html.contains("без раздела"),
            "чип: {html}"
        );
    }

    #[sqlx::test]
    async fn items_foreign_section_dropped(db: PgPool) {
        let fx = content_fixture(&db).await;
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name) VALUES ('other', 'Другая') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let foreign = section(&db, other, None, "Чужой").await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s, headers, _) = get_raw(
            &app,
            &format!(
                "/admin/collections/{}/items?section={foreign}",
                fx.collection_id
            ),
            &cookie,
        )
        .await;
        assert_eq!(s, StatusCode::FOUND);
        let loc = headers.get("location").unwrap().to_str().unwrap();
        assert!(!loc.contains("section"), "{loc}");
    }

    #[sqlx::test]
    async fn tree_children_fragment(db: PgPool) {
        let fx = content_fixture(&db).await;
        section(&db, fx.collection_id, Some(fx.section_id), "Подраздел Б").await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s, html) = htmx_get(
            &app,
            &format!(
                "/admin/collections/{}/tree?parent={}",
                fx.collection_id, fx.section_id
            ),
            &cookie,
            "list-tree",
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(html.contains("Подраздел Б"), "{html}");
        assert!(
            !html.contains("Вся коллекция")
                && !html.contains("Добавить раздел")
                && !html.contains("<html"),
            "{html}"
        );
    }

    /// Число из `<span class="cnt">` узла дерева с названием `name`.
    fn node_count(html: &str, name: &str) -> String {
        let at = html
            .find(&format!(">{name}</a>"))
            .unwrap_or_else(|| panic!("узел {name} не найден"));
        let rest = &html[at..];
        let from = rest.find("<span class=\"cnt\">").expect("счётчик") + 18;
        let rest = &rest[from..];
        rest[..rest.find("</span>").unwrap()].to_string()
    }

    #[sqlx::test]
    async fn tree_counts_include_descendants(db: PgPool) {
        let fx = content_fixture(&db).await;
        let sub = section(&db, fx.collection_id, Some(fx.section_id), "Подраздел Б").await;
        item(&db, fx.collection_id, Some(sub), "Х1").await;
        item(&db, fx.collection_id, Some(sub), "Х2").await;
        item(&db, fx.collection_id, None, "Х3").await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        // Дерево в сайдбаре, а фрагмент подраздела по-прежнему считает поддерево
        let (s, _, b) = get_raw(
            &app,
            &format!(
                "/admin/collections/{}/tree?parent={}",
                fx.collection_id, fx.section_id
            ),
            &cookie,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let html = String::from_utf8_lossy(&b);
        assert_eq!(node_count(&html, "Подраздел Б"), "2", "{html}");
        assert!(!html.contains("Раздел А"), "{html}");
    }

    #[sqlx::test]
    async fn items_readonly_user(db: PgPool) {
        let fx = content_fixture(&db).await;
        let (uid, cookie) = user_cookie(&db, "reader", false).await;
        let group: i64 =
            sqlx::query_scalar("INSERT INTO groups (code, name) VALUES ('rd', 'rd') RETURNING id")
                .fetch_one(&db)
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO group_permissions (group_id, permission) VALUES ($1, 'admin.access')",
        )
        .bind(group)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO user_groups (user_id, group_id) VALUES ($1, $2)")
            .bind(uid)
            .bind(group)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO collection_access (collection_id, group_id, level) VALUES ($1, $2, 'read')")
            .bind(fx.collection_id)
            .bind(group)
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let (s, _, b) = get_raw(
            &app,
            &format!("/admin/collections/{}/items", fx.collection_id),
            &cookie,
        )
        .await;
        let html = String::from_utf8_lossy(&b);
        assert_eq!(s, StatusCode::OK);
        assert!(
            html.contains("Первая новость") && html.contains("Раздел А"),
            "{html}"
        );
        assert!(
            !html.contains("Добавить запись")
                && !html.contains("Добавить раздел")
                && !html.contains("Изменить раздел")
                && !html.contains("Добавить подраздел"),
            "{html}"
        );
    }
}
