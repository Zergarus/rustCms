//! Обмен с 1С: настройки и журнал последних обменов.

use std::collections::HashMap;

use axum::{
    Extension, Form,
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use minijinja::context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::render;
use crate::{
    access::{Access, EXCHANGE_1C, EXCHANGE_1C_SALE},
    error::{AppError, AppResult},
    exchange::{
        sale::{
            apply::SaleStats,
            http::summary as sale_summary,
            settings::{
                CONTRAGENT_CODES, Domain, MapEntry, MapSource, ORDER_FIELDS, SaleSettings,
                USER_FIELDS, load_map, save_map,
            },
        },
        settings::{Action, Settings},
    },
    state::AppState,
};

const LOG_ROWS: i64 = 50;
const MB: u64 = 1024 * 1024;
/// Наибольший лимит части файла в форме, МБ (4 ГБ).
const MAX_FILE_LIMIT_MB: u64 = 4096;

#[derive(Serialize)]
struct LogRow {
    created_at: String,
    kind: &'static str,
    mode: String,
    filename: String,
    result: String,
    summary: String,
    error: String,
}

#[derive(Serialize)]
struct CollectionOption {
    id: i64,
    name: String,
}

/// Счётчики обмена заказами: выгрузка (`{"orders": N}`) или итог приёма (`SaleStats`).
fn summarize_sale(stats: &Value) -> String {
    if let Some(n) = stats.get("orders").and_then(Value::as_u64) {
        return format!("Выгружено: заказы {n}");
    }
    let Ok(parsed) = serde_json::from_value::<SaleStats>(stats.clone()) else {
        return String::new();
    };
    if stats.as_object().is_none_or(|o| o.is_empty()) {
        return String::new();
    }
    let mut text = sale_summary(&parsed);
    if !parsed.errors.is_empty() {
        text.push_str(&format!(". Ошибок: {}", parsed.errors.len()));
    }
    text
}

/// Счётчики из `stats` одной строкой: «создано: items 3, sections 1; обновлено: …».
fn summarize(kind: &str, stats: &Value) -> String {
    if kind == "sale" {
        return summarize_sale(stats);
    }
    const KINDS: [(&str, &str); 5] = [
        ("created", "создано"),
        ("updated", "обновлено"),
        ("deactivated", "деактивировано"),
        ("deleted", "удалено"),
        ("skipped", "пропущено"),
    ];
    let mut parts = Vec::new();
    for (key, label) in KINDS {
        let Some(map) = stats.get(key).and_then(Value::as_object) else {
            continue;
        };
        let items: Vec<String> = map
            .iter()
            .filter(|(_, n)| n.as_u64().unwrap_or(0) > 0)
            .map(|(k, n)| format!("{k} {n}"))
            .collect();
        if !items.is_empty() {
            parts.push(format!("{label}: {}", items.join(", ")));
        }
    }
    parts.join("; ")
}

/// Последние обмены тех видов, на которые у пользователя есть право.
async fn journal(state: &AppState, user: &Access) -> AppResult<Vec<LogRow>> {
    let kinds: Vec<&str> = [(EXCHANGE_1C, "catalog"), (EXCHANGE_1C_SALE, "sale")]
        .into_iter()
        .filter(|(permission, _)| user.can(permission))
        .map(|(_, kind)| kind)
        .collect();
    let rows: Vec<(String, String, String, String, String, Value, String)> = sqlx::query_as(
        "SELECT to_char(created_at, 'YYYY-MM-DD HH24:MI:SS'), kind, mode, filename, result, stats, error
         FROM exchange_log WHERE kind = ANY($2) ORDER BY id DESC LIMIT $1",
    )
    .bind(LOG_ROWS)
    .bind(&kinds)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(created_at, kind, mode, filename, result, stats, error)| LogRow {
                created_at,
                kind: if kind == "sale" {
                    "Заказы"
                } else {
                    "Каталог"
                },
                mode,
                filename,
                result,
                summary: summarize(&kind, &stats),
                error,
            },
        )
        .collect())
}

async fn page(
    state: &AppState,
    user: &Access,
    settings: &Settings,
    file_limit_mb: String,
    interval_secs: String,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let collections: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, name FROM collections
             WHERE is_catalog AND product_collection_id IS NULL ORDER BY name, id",
    )
    .fetch_all(&state.db)
    .await?;
    let collections: Vec<CollectionOption> = collections
        .into_iter()
        .map(|(id, name)| CollectionOption { id, name })
        .collect();
    let log = journal(state, user).await?;
    render(
        state,
        "exchange.html",
        context! {
            user,
            tab => "catalog",
            error,
            collections,
            log,
            default_collection => settings.default_collection_id,
            section_action => settings.section_action.code(),
            element_action => settings.element_action.code(),
            translit_on_add => settings.translit_on_add,
            translit_on_update => settings.translit_on_update,
            use_offers => settings.use_offers,
            deactivate_without_price => settings.deactivate_without_price,
            use_zip => settings.use_zip,
            interval_secs,
            file_limit_mb,
        },
    )
}

#[derive(Deserialize)]
pub struct TabQuery {
    tab: Option<String>,
}

pub async fn form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<TabQuery>,
) -> AppResult<Html<String>> {
    // Без `tab` — первая доступная вкладка; без обоих прав — 403
    let sale = match q.tab.as_deref() {
        Some("sale") => true,
        Some(_) => false,
        None => !user.can(EXCHANGE_1C),
    };
    if sale {
        user.require(EXCHANGE_1C_SALE)?;
        let view = SaleView::from_settings(&SaleSettings::load(&state.db).await?);
        return sale_page(&state, &user, view, None).await;
    }
    user.require(EXCHANGE_1C)?;
    let s = Settings::load(&state.db).await?;
    let mb = (s.file_limit / MB).max(1).to_string();
    page(&state, &user, &s, mb, s.interval_secs.to_string(), None).await
}

#[derive(Deserialize)]
pub struct ExchangeForm {
    #[serde(default)]
    default_collection: String,
    #[serde(default)]
    section_action: String,
    #[serde(default)]
    element_action: String,
    translit_on_add: Option<String>,
    translit_on_update: Option<String>,
    use_offers: Option<String>,
    deactivate_without_price: Option<String>,
    use_zip: Option<String>,
    #[serde(default)]
    interval_secs: String,
    #[serde(default)]
    file_limit_mb: String,
}

pub async fn save(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<ExchangeForm>,
) -> AppResult<Response> {
    user.require(EXCHANGE_1C)?;
    let raw_collection = form.default_collection.trim();
    let collection_parsed = if raw_collection.is_empty() {
        Ok(None)
    } else {
        raw_collection.parse::<i64>().map(Some)
    };
    let default_collection_id = collection_parsed.clone().ok().flatten();
    let mut settings = Settings {
        default_collection_id,
        section_action: Action::from_code(&form.section_action),
        element_action: Action::from_code(&form.element_action),
        translit_on_add: form.translit_on_add.is_some(),
        translit_on_update: form.translit_on_update.is_some(),
        use_offers: form.use_offers.is_some(),
        deactivate_without_price: form.deactivate_without_price.is_some(),
        use_zip: form.use_zip.is_some(),
        ..Settings::default()
    };
    let interval = form
        .interval_secs
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|n| (0..=3600).contains(n));
    let limit_mb = form
        .file_limit_mb
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|n| (1..=MAX_FILE_LIMIT_MB).contains(n));
    let collection_ok = match default_collection_id {
        None => true,
        Some(id) => {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM collections
                         WHERE id = $1 AND is_catalog AND product_collection_id IS NULL)",
            )
            .bind(id)
            .fetch_one(&state.db)
            .await?
        }
    };
    let error = if collection_parsed.is_err() {
        Some("Коллекция по умолчанию: некорректный номер")
    } else if interval.is_none() {
        Some("Интервал — целое число секунд от 0 (без ограничения) до 3600")
    } else if limit_mb.is_none() {
        Some("Лимит файла — целое число мегабайт от 1 до 4096")
    } else if !collection_ok {
        Some("Коллекция по умолчанию должна быть коллекцией каталога (не предложений)")
    } else {
        None
    };
    if let Some(error) = error {
        settings.interval_secs = interval.unwrap_or(30);
        return Ok(page(
            &state,
            &user,
            &settings,
            form.file_limit_mb,
            form.interval_secs,
            Some(error.to_string()),
        )
        .await?
        .into_response());
    }
    settings.interval_secs = interval.unwrap_or(30);
    settings.file_limit = limit_mb.unwrap_or(1) * MB;
    settings.save(&state.db).await?;
    Ok(Redirect::to("/admin/exchange").into_response())
}

// ── Обмен заказами ──────────────────────────────────────────────────────────

/// Настройки обмена заказами в виде формы: списки — строки (пусто — не задано).
#[derive(Serialize, Clone, Default)]
struct SaleView {
    export_paid_only: bool,
    export_allow_delivery_only: bool,
    export_from_status: String,
    replace_currency: String,
    number_prefix: String,
    change_status_from_1c: bool,
    final_status_on_delivery: String,
    update_basket_quantity: bool,
    import_new_orders: bool,
    import_new_payments: bool,
    import_new_shipments: bool,
    new_order_new_shipment: bool,
    default_ps_cash: String,
    default_ps_cashless: String,
    default_ps_acquiring: String,
    default_ps_order_paid: String,
    default_delivery: String,
}

impl SaleView {
    fn from_settings(s: &SaleSettings) -> SaleView {
        let id = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_default();
        SaleView {
            export_paid_only: s.export_paid_only,
            export_allow_delivery_only: s.export_allow_delivery_only,
            export_from_status: s.export_from_status.clone().unwrap_or_default(),
            replace_currency: s.replace_currency.clone(),
            number_prefix: s.number_prefix.clone(),
            change_status_from_1c: s.change_status_from_1c,
            final_status_on_delivery: s.final_status_on_delivery.clone().unwrap_or_default(),
            update_basket_quantity: s.update_basket_quantity,
            import_new_orders: s.import_new_orders,
            import_new_payments: s.import_new_payments,
            import_new_shipments: s.import_new_shipments,
            new_order_new_shipment: s.new_order_new_shipment,
            default_ps_cash: id(s.default_ps_cash),
            default_ps_cashless: id(s.default_ps_cashless),
            default_ps_acquiring: id(s.default_ps_acquiring),
            default_ps_order_paid: id(s.default_ps_order_paid),
            default_delivery: id(s.default_delivery),
        }
    }
}

#[derive(Serialize)]
struct PersonTypeRow {
    id: i64,
    name: String,
    domain: &'static str,
    mapped: i64,
}

fn domain_label(domain: &str) -> &'static str {
    if domain == "E" {
        "Юрлицо"
    } else {
        "Физлицо"
    }
}

async fn sale_page(
    state: &AppState,
    user: &Access,
    sale: SaleView,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let statuses: Vec<(String, String)> =
        sqlx::query_as("SELECT code, name FROM order_statuses ORDER BY sort, code")
            .fetch_all(&state.db)
            .await?;
    let pay_systems: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM pay_systems ORDER BY active DESC, sort, id")
            .fetch_all(&state.db)
            .await?;
    let deliveries: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM deliveries ORDER BY active DESC, sort, id")
            .fetch_all(&state.db)
            .await?;
    let sale_types: Vec<(i64, String, String, i64)> = sqlx::query_as(
        "SELECT p.id, p.name, p.domain,
                (SELECT count(*) FROM exchange_order_map m WHERE m.person_type_id = p.id)
         FROM person_types p ORDER BY p.sort, p.id",
    )
    .fetch_all(&state.db)
    .await?;
    let mapped_all: i64 =
        sqlx::query_scalar("SELECT count(*) FROM exchange_order_map WHERE person_type_id = 0")
            .fetch_one(&state.db)
            .await?;
    let mut person_types = vec![(
        0,
        "Для всех типов плательщика".to_string(),
        String::new(),
        mapped_all,
    )];
    person_types.extend(sale_types);
    let person_types: Vec<PersonTypeRow> = person_types
        .into_iter()
        .map(|(id, name, domain, mapped)| PersonTypeRow {
            id,
            name,
            domain: if id == 0 {
                "—"
            } else {
                domain_label(&domain)
            },
            mapped,
        })
        .collect();
    let last_export: Option<String> = SaleSettings::load(&state.db)
        .await?
        .last_export_committed
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string());
    let log = journal(state, user).await?;
    render(
        state,
        "exchange.html",
        context! {
            user,
            tab => "sale",
            error,
            log,
            s => sale,
            statuses,
            pay_systems,
            deliveries,
            person_types,
            last_export,
        },
    )
}

#[derive(Deserialize)]
pub struct SaleForm {
    export_paid_only: Option<String>,
    export_allow_delivery_only: Option<String>,
    #[serde(default)]
    export_from_status: String,
    #[serde(default)]
    replace_currency: String,
    #[serde(default)]
    number_prefix: String,
    change_status_from_1c: Option<String>,
    #[serde(default)]
    final_status_on_delivery: String,
    update_basket_quantity: Option<String>,
    import_new_orders: Option<String>,
    import_new_payments: Option<String>,
    import_new_shipments: Option<String>,
    new_order_new_shipment: Option<String>,
    #[serde(default)]
    default_ps_cash: String,
    #[serde(default)]
    default_ps_cashless: String,
    #[serde(default)]
    default_ps_acquiring: String,
    #[serde(default)]
    default_ps_order_paid: String,
    #[serde(default)]
    default_delivery: String,
}

impl SaleForm {
    fn view(&self) -> SaleView {
        SaleView {
            export_paid_only: self.export_paid_only.is_some(),
            export_allow_delivery_only: self.export_allow_delivery_only.is_some(),
            export_from_status: self.export_from_status.trim().to_string(),
            replace_currency: self.replace_currency.trim().to_string(),
            number_prefix: self.number_prefix.trim().to_string(),
            change_status_from_1c: self.change_status_from_1c.is_some(),
            final_status_on_delivery: self.final_status_on_delivery.trim().to_string(),
            update_basket_quantity: self.update_basket_quantity.is_some(),
            import_new_orders: self.import_new_orders.is_some(),
            import_new_payments: self.import_new_payments.is_some(),
            import_new_shipments: self.import_new_shipments.is_some(),
            new_order_new_shipment: self.new_order_new_shipment.is_some(),
            default_ps_cash: self.default_ps_cash.trim().to_string(),
            default_ps_cashless: self.default_ps_cashless.trim().to_string(),
            default_ps_acquiring: self.default_ps_acquiring.trim().to_string(),
            default_ps_order_paid: self.default_ps_order_paid.trim().to_string(),
            default_delivery: self.default_delivery.trim().to_string(),
        }
    }
}

/// Статус должен существовать; пусто — не задан. Внешний `Err` — база, внутренний — текст ошибки.
async fn check_status(
    state: &AppState,
    code: &str,
    label: &str,
) -> AppResult<Result<Option<String>, String>> {
    if code.is_empty() {
        return Ok(Ok(None));
    }
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM order_statuses WHERE code = $1)")
            .bind(code)
            .fetch_one(&state.db)
            .await?;
    Ok(if exists {
        Ok(Some(code.to_string()))
    } else {
        Err(format!("{label}: нет такого статуса"))
    })
}

/// Платёжная система (`pay_system`) / служба доставки должна существовать; пусто — не задана.
async fn check_ref(
    state: &AppState,
    pay_system: bool,
    raw: &str,
    label: &str,
) -> AppResult<Result<Option<i64>, String>> {
    if raw.is_empty() {
        return Ok(Ok(None));
    }
    let Some(id) = raw.parse::<i64>().ok().filter(|id| *id > 0) else {
        return Ok(Err(format!("{label}: некорректный номер")));
    };
    let sql = if pay_system {
        "SELECT EXISTS (SELECT 1 FROM pay_systems WHERE id = $1)"
    } else {
        "SELECT EXISTS (SELECT 1 FROM deliveries WHERE id = $1)"
    };
    let exists: bool = sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok(if exists {
        Ok(Some(id))
    } else {
        Err(format!("{label}: нет такой записи"))
    })
}

pub async fn save_sale(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<SaleForm>,
) -> AppResult<Response> {
    user.require(EXCHANGE_1C_SALE)?;
    let view = form.view();
    let from = check_status(
        &state,
        &view.export_from_status,
        "Выгружать начиная со статуса",
    )
    .await?;
    let fin = check_status(
        &state,
        &view.final_status_on_delivery,
        "Статус при отгрузке",
    )
    .await?;
    let ps = |raw: &str, label: &str| {
        let (state, raw, label) = (state.clone(), raw.to_string(), label.to_string());
        async move { check_ref(&state, true, &raw, &label).await }
    };
    let cash = ps(&view.default_ps_cash, "Платёжная система: наличные").await?;
    let cashless = ps(&view.default_ps_cashless, "Платёжная система: безналичные").await?;
    let acquiring = ps(&view.default_ps_acquiring, "Платёжная система: эквайринг").await?;
    let paid = ps(
        &view.default_ps_order_paid,
        "Платёжная система: оплаченный заказ",
    )
    .await?;
    let delivery = check_ref(&state, false, &view.default_delivery, "Служба доставки").await?;

    let error = [
        from.clone().err(),
        fin.clone().err(),
        cash.clone().err(),
        cashless.clone().err(),
        acquiring.clone().err(),
        paid.clone().err(),
        delivery.clone().err(),
    ]
    .into_iter()
    .flatten()
    .next();
    if let Some(error) = error {
        return Ok(sale_page(&state, &user, view, Some(error))
            .await?
            .into_response());
    }
    // Метка последней выгрузки принадлежит обмену, форма её не меняет
    let current = SaleSettings::load(&state.db).await?;
    SaleSettings {
        export_paid_only: view.export_paid_only,
        export_allow_delivery_only: view.export_allow_delivery_only,
        export_from_status: from.unwrap_or_default(),
        replace_currency: view.replace_currency,
        number_prefix: view.number_prefix,
        change_status_from_1c: view.change_status_from_1c,
        final_status_on_delivery: fin.unwrap_or_default(),
        update_basket_quantity: view.update_basket_quantity,
        import_new_orders: view.import_new_orders,
        import_new_payments: view.import_new_payments,
        import_new_shipments: view.import_new_shipments,
        new_order_new_shipment: view.new_order_new_shipment,
        default_ps_cash: cash.unwrap_or_default(),
        default_ps_cashless: cashless.unwrap_or_default(),
        default_ps_acquiring: acquiring.unwrap_or_default(),
        default_ps_order_paid: paid.unwrap_or_default(),
        default_delivery: delivery.unwrap_or_default(),
        last_export_committed: current.last_export_committed,
    }
    .save(&state.db)
    .await?;
    Ok(Redirect::to("/admin/exchange?tab=sale").into_response())
}

// ── Соответствие реквизитов ─────────────────────────────────────────────────

/// Строка кода контрагента (`BUYER_*`).
#[derive(Serialize, Clone)]
struct CodeRow {
    code: &'static str,
    field: &'static str,
    source: String,
    value: String,
}

/// Строка произвольного реквизита `1C_REKV_<n>`.
#[derive(Serialize, Clone)]
struct RekvRow {
    n: u32,
    name: String,
    source: String,
    value: String,
}

/// Пустых строк для новых реквизитов под таблицей.
const BLANK_REKV: u32 = 3;

/// Коды контрагента домена; без домена (соответствие «для всех типов») — обоих.
fn codes_of(
    domain: Option<Domain>,
) -> impl Iterator<Item = &'static (&'static str, &'static str, Domain)> {
    CONTRAGENT_CODES
        .iter()
        .filter(move |c| domain.is_none_or(|d| c.2 == d))
}

fn rekv_number(code: &str) -> Option<u32> {
    code.strip_prefix("1C_REKV_")?
        .parse()
        .ok()
        .filter(|n| *n > 0)
}

/// Строки формы из сохранённого соответствия.
fn rows_from_map(domain: Option<Domain>, entries: &[MapEntry]) -> (Vec<CodeRow>, Vec<RekvRow>) {
    let by_code: HashMap<&str, &MapEntry> = entries.iter().map(|e| (e.code.as_str(), e)).collect();
    let codes = codes_of(domain)
        .map(|(code, field, _)| {
            let e = by_code.get(code);
            CodeRow {
                code,
                field,
                source: e.map(|e| e.source.code().to_string()).unwrap_or_default(),
                value: e.map(|e| e.value.clone()).unwrap_or_default(),
            }
        })
        .collect();
    let mut rekv: Vec<RekvRow> = entries
        .iter()
        .filter_map(|e| {
            Some(RekvRow {
                n: rekv_number(&e.code)?,
                name: e.name.clone(),
                source: e.source.code().to_string(),
                value: e.value.clone(),
            })
        })
        .collect();
    rekv.sort_by_key(|r| r.n);
    (codes, rekv)
}

/// Строки формы из присланных полей: введённое остаётся на месте при ошибке.
fn rows_from_form(
    domain: Option<Domain>,
    f: &HashMap<String, String>,
) -> (Vec<CodeRow>, Vec<RekvRow>) {
    let get = |k: String| f.get(&k).map(|v| v.trim().to_string()).unwrap_or_default();
    let codes = codes_of(domain)
        .map(|(code, field, _)| CodeRow {
            code,
            field,
            source: get(format!("src_{code}")),
            value: get(format!("val_{code}")),
        })
        .collect();
    let mut numbers: Vec<u32> = f
        .keys()
        .filter_map(|k| k.strip_prefix("rekv_")?.split('_').next()?.parse().ok())
        .filter(|n| *n > 0)
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    let rekv = numbers
        .into_iter()
        .map(|n| RekvRow {
            n,
            name: get(format!("rekv_{n}_name")),
            source: get(format!("rekv_{n}_source")),
            value: get(format!("rekv_{n}_value")),
        })
        .filter(|r| !(r.name.is_empty() && r.source.is_empty() && r.value.is_empty()))
        .collect();
    (codes, rekv)
}

#[derive(Serialize)]
struct PropertyOption {
    id: i64,
    name: String,
    kind: String,
}

async fn map_page(
    state: &AppState,
    user: &Access,
    person_type_id: i64,
    domain: Option<Domain>,
    codes: Vec<CodeRow>,
    mut rekv: Vec<RekvRow>,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let name: String = if person_type_id == 0 {
        "Для всех типов плательщика".into()
    } else {
        sqlx::query_scalar("SELECT name FROM person_types WHERE id = $1")
            .bind(person_type_id)
            .fetch_one(&state.db)
            .await?
    };
    let properties: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id, name, kind FROM order_properties
         WHERE person_type_id = $1 OR $1 = 0 ORDER BY person_type_id, sort, id",
    )
    .bind(person_type_id)
    .fetch_all(&state.db)
    .await?;
    let properties: Vec<PropertyOption> = properties
        .into_iter()
        .map(|(id, name, kind)| PropertyOption { id, name, kind })
        .collect();
    let next = rekv.iter().map(|r| r.n).max().unwrap_or(0);
    rekv.extend((1..=BLANK_REKV).map(|i| RekvRow {
        n: next + i,
        name: String::new(),
        source: String::new(),
        value: String::new(),
    }));
    render(
        state,
        "exchange_map.html",
        context! {
            user,
            error,
            person_type_id,
            person_type => name,
            domain => domain.map(Domain::code),
            codes,
            rekv,
            properties,
            user_fields => USER_FIELDS,
            order_fields => ORDER_FIELDS,
        },
    )
}

/// Домен типа плательщика; `None` — соответствие «для всех типов» (id 0).
async fn person_type_domain(state: &AppState, id: i64) -> AppResult<Option<Domain>> {
    if id == 0 {
        return Ok(None);
    }
    let domain: Option<String> =
        sqlx::query_scalar("SELECT domain FROM person_types WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    domain
        .map(|d| Some(Domain::from_code(&d)))
        .ok_or(AppError::NotFound)
}

pub async fn map_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(person_type_id): Path<i64>,
) -> AppResult<Html<String>> {
    user.require(EXCHANGE_1C_SALE)?;
    let domain = person_type_domain(&state, person_type_id).await?;
    let entries: Vec<MapEntry> = load_map(&state.db)
        .await?
        .into_iter()
        .filter(|e| e.person_type_id == person_type_id)
        .collect();
    let (codes, rekv) = rows_from_map(domain, &entries);
    map_page(&state, &user, person_type_id, domain, codes, rekv, None).await
}

/// Допустимо ли значение источника; `Err` — текст ошибки.
async fn check_value(
    state: &AppState,
    person_type_id: i64,
    label: &str,
    source: MapSource,
    value: &str,
) -> AppResult<Result<(), String>> {
    let bad = |what: &str| Err(format!("{label}: {what}"));
    Ok(match source {
        MapSource::Property => {
            let (id, part) = match value.split_once('_') {
                Some((id, part)) => (id, Some(part)),
                None => (value, None),
            };
            let part_ok = matches!(part, None | Some("CITY" | "COUNTRY" | "REGION"));
            let Some(id) = id.parse::<i64>().ok().filter(|id| *id > 0 && part_ok) else {
                return Ok(bad("свойство — номер или номер_CITY / _COUNTRY / _REGION"));
            };
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM order_properties
                 WHERE id = $1 AND (person_type_id = $2 OR $2 = 0))",
            )
            .bind(id)
            .bind(person_type_id)
            .fetch_one(&state.db)
            .await?;
            if exists {
                Ok(())
            } else {
                bad("нет такого свойства у этого типа плательщика")
            }
        }
        MapSource::User if USER_FIELDS.contains(&value) => Ok(()),
        MapSource::User => bad("неизвестное поле пользователя"),
        MapSource::Order if ORDER_FIELDS.contains(&value) => Ok(()),
        MapSource::Order => bad("неизвестное поле заказа"),
        MapSource::Value => Ok(()),
    })
}

pub async fn save_map_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(person_type_id): Path<i64>,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    user.require(EXCHANGE_1C_SALE)?;
    let stored_domain = person_type_domain(&state, person_type_id).await?;
    let f: HashMap<String, String> = fields.into_iter().collect();
    let domain = match f.get("domain").map(|d| d.trim()) {
        Some("I") => Some(Domain::Individual),
        Some("E") => Some(Domain::Entity),
        _ => None,
    };
    let for_all = stored_domain.is_none();
    let shown = if for_all {
        None
    } else {
        domain.or(stored_domain)
    };
    let (codes, rekv) = rows_from_form(shown, &f);

    let mut entries = Vec::new();
    let mut error =
        (!for_all && domain.is_none()).then(|| "Домен: выберите физлицо или юрлицо".to_string());
    for row in codes.iter().filter(|r| !r.source.is_empty()) {
        if error.is_some() {
            break;
        }
        let Some(source) = MapSource::from_code(&row.source) else {
            error = Some(format!("{}: неизвестный источник", row.code));
            break;
        };
        match check_value(&state, person_type_id, row.code, source, &row.value).await? {
            Ok(()) => entries.push(MapEntry {
                person_type_id,
                code: row.code.to_string(),
                name: String::new(),
                source,
                value: row.value.clone(),
            }),
            Err(e) => error = Some(e),
        }
    }
    for row in &rekv {
        if error.is_some() {
            break;
        }
        let label = format!("1C_REKV_{}", row.n);
        let Some(source) = MapSource::from_code(&row.source) else {
            error = Some(format!("{label}: выберите источник"));
            break;
        };
        if row.name.is_empty() {
            error = Some(format!("{label}: укажите название реквизита"));
            break;
        }
        match check_value(&state, person_type_id, &label, source, &row.value).await? {
            Ok(()) => entries.push(MapEntry {
                person_type_id,
                code: label,
                name: row.name.clone(),
                source,
                value: row.value.clone(),
            }),
            Err(e) => error = Some(e),
        }
    }
    if let Some(error) = error {
        return Ok(map_page(
            &state,
            &user,
            person_type_id,
            shown,
            codes,
            rekv,
            Some(error),
        )
        .await?
        .into_response());
    }
    if let Some(shown) = shown {
        sqlx::query("UPDATE person_types SET domain = $2 WHERE id = $1")
            .bind(person_type_id)
            .bind(shown.code())
            .execute(&state.db)
            .await?;
    }
    save_map(&state.db, person_type_id, &entries).await?;
    Ok(Redirect::to(&format!("/admin/exchange/map/{person_type_id}")).into_response())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use sqlx::PgPool;

    use crate::exchange::sale::settings::{MapEntry, MapSource, SaleSettings, load_map};
    use crate::smoke_tests::{get, post_form};
    use crate::test_support::{admin_cookie, permitted_cookie, test_state};

    fn app(db: &PgPool) -> axum::Router {
        crate::app(test_state(db.clone()))
    }

    async fn dictionaries(db: &PgPool) {
        sqlx::query(
            "INSERT INTO order_statuses (code, name, sort) VALUES
                ('N', 'Принят', 100), ('P', 'Оплачен', 200), ('F', 'Выполнен', 300)",
        )
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO pay_systems (id, name, sort) VALUES
                (1, 'Наличные', 1), (2, 'Безнал', 2), (3, 'Эквайринг', 3), (4, 'Оплачено', 4)",
        )
        .execute(db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO deliveries (id, name, sort) VALUES (5, 'Курьер', 1)")
            .execute(db)
            .await
            .unwrap();
    }

    const FULL: &str = "export_paid_only=on&export_allow_delivery_only=on&export_from_status=P\
        &replace_currency=%D1%80%D1%83%D0%B1.&number_prefix=TR-&change_status_from_1c=on\
        &final_status_on_delivery=F&update_basket_quantity=on&import_new_orders=on\
        &import_new_payments=on&import_new_shipments=on&new_order_new_shipment=on\
        &default_ps_cash=1&default_ps_cashless=2&default_ps_acquiring=3&default_ps_order_paid=4\
        &default_delivery=5";

    #[sqlx::test]
    async fn sale_settings_are_shown_and_saved(db: PgPool) {
        dictionaries(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        sqlx::query(
            "INSERT INTO exchange_log (kind, mode, filename, result, stats, error, duration_ms)
             VALUES ('sale', 'query', '', 'success', '{}', '', 1),
                    ('catalog', 'file', 'import.xml', 'success', '{}', '', 1)",
        )
        .execute(&db)
        .await
        .unwrap();

        let (status, html) = get(&app, "/admin/exchange?tab=sale", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        for n in [
            "Выгружать только оплаченные",
            "Принят",
            "Наличные",
            "Курьер",
            "<th>Тип</th>",
            "import.xml",
        ] {
            assert!(html.contains(n), "нет «{n}»");
        }
        // вкладка заказов не показывает форму каталога
        assert!(!html.contains("Принимать файлы в zip-архиве"));

        // метка выгрузки не затирается сохранением
        let at = chrono::DateTime::parse_from_rfc3339("2026-05-13T12:59:22Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        crate::exchange::sale::settings::commit_export(&db, at)
            .await
            .unwrap();
        let (status, html) = post_form(&app, "/admin/exchange/sale", &cookie, FULL).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
        let s = SaleSettings::load(&db).await.unwrap();
        assert_eq!(
            s,
            SaleSettings {
                export_paid_only: true,
                export_allow_delivery_only: true,
                export_from_status: Some("P".into()),
                replace_currency: "руб.".into(),
                number_prefix: "TR-".into(),
                change_status_from_1c: true,
                final_status_on_delivery: Some("F".into()),
                update_basket_quantity: true,
                import_new_orders: true,
                import_new_payments: true,
                import_new_shipments: true,
                new_order_new_shipment: true,
                default_ps_cash: Some(1),
                default_ps_cashless: Some(2),
                default_ps_acquiring: Some(3),
                default_ps_order_paid: Some(4),
                default_delivery: Some(5),
                last_export_committed: Some(at),
            }
        );

        let (_, html) = get(&app, "/admin/exchange?tab=sale", &cookie).await;
        for n in [
            r#"name="export_paid_only" checked"#,
            r#"name="export_allow_delivery_only" checked"#,
            r#"name="change_status_from_1c" checked"#,
            r#"name="update_basket_quantity" checked"#,
            r#"name="new_order_new_shipment" checked"#,
            r#"value="TR-""#,
            "руб.",
            r#"<option value="P" selected>"#,
            r#"<option value="F" selected>"#,
            r#"<option value="2" selected>"#,
            r#"<option value="5" selected>"#,
            "2026-05-13 12:59",
        ] {
            assert!(html.contains(n), "нет «{n}»: {html}");
        }

        // сброс: пустые списки и снятые флажки
        let (status, _) = post_form(&app, "/admin/exchange/sale", &cookie, "").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let s = SaleSettings::load(&db).await.unwrap();
        assert_eq!(
            s,
            SaleSettings {
                update_basket_quantity: false,
                import_new_orders: false,
                import_new_payments: false,
                import_new_shipments: false,
                new_order_new_shipment: false,
                last_export_committed: Some(at),
                ..SaleSettings::default()
            }
        );
    }

    #[sqlx::test]
    async fn sale_form_error_keeps_input(db: PgPool) {
        dictionaries(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let before = SaleSettings::load(&db).await.unwrap();
        for bad in [
            "number_prefix=KEEP-&export_from_status=ZZ",
            "number_prefix=KEEP-&final_status_on_delivery=ZZ",
            "number_prefix=KEEP-&default_ps_cash=99",
            "number_prefix=KEEP-&default_ps_cashless=abc",
            "number_prefix=KEEP-&default_delivery=99",
        ] {
            let (status, html) = post_form(&app, "/admin/exchange/sale", &cookie, bad).await;
            assert_eq!(status, StatusCode::OK, "{bad}");
            assert!(html.contains(r#"class="alert""#), "{bad}: {html}");
            assert!(html.contains(r#"value="KEEP-""#), "{bad}: {html}");
            assert_eq!(SaleSettings::load(&db).await.unwrap(), before, "{bad}");
        }
        // введённые флажки тоже остаются отмеченными
        let (_, html) = post_form(
            &app,
            "/admin/exchange/sale",
            &cookie,
            "export_paid_only=on&default_ps_cash=99",
        )
        .await;
        assert!(
            html.contains(r#"name="export_paid_only" checked"#),
            "{html}"
        );
    }

    #[sqlx::test]
    async fn tabs_follow_permissions(db: PgPool) {
        dictionaries(&db).await;
        let app = app(&db);
        let sale = permitted_cookie(&db, "saler", &["exchange.1c.sale"]).await;
        let catalog = permitted_cookie(&db, "cataloger", &["exchange.1c"]).await;
        let none = permitted_cookie(&db, "nobody", &[]).await;
        let both = permitted_cookie(&db, "both", &["exchange.1c", "exchange.1c.sale"]).await;

        // только заказы: вкладка по умолчанию — заказы, каталог закрыт
        let (status, html) = get(&app, "/admin/exchange", &sale).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Выгружать только оплаченные"));
        assert!(!html.contains("tab=catalog"));
        let (status, _) = get(&app, "/admin/exchange?tab=catalog", &sale).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = post_form(
            &app,
            "/admin/exchange",
            &sale,
            "interval_secs=30&file_limit_mb=1",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = post_form(&app, "/admin/exchange/sale", &sale, "").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let (_, home) = get(&app, "/admin", &sale).await;
        assert!(home.contains("href=\"/admin/exchange\""));

        // только каталог
        let (status, html) = get(&app, "/admin/exchange", &catalog).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Принимать файлы в zip-архиве"));
        assert!(!html.contains("tab=sale"));
        let (status, _) = get(&app, "/admin/exchange?tab=sale", &catalog).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = post_form(&app, "/admin/exchange/sale", &catalog, "").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = get(&app, "/admin/exchange/map/1", &catalog).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // ни одного права
        for path in ["/admin/exchange", "/admin/exchange?tab=sale"] {
            let (status, _) = get(&app, path, &none).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        }

        // оба: по умолчанию каталог, обе вкладки
        let (status, html) = get(&app, "/admin/exchange", &both).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Принимать файлы в zip-архиве"));
        assert!(html.contains("tab=sale") && html.contains("tab=catalog"));
        let (status, _) = get(&app, "/admin/exchange?tab=sale", &both).await;
        assert_eq!(status, StatusCode::OK);
    }

    async fn person_type(db: &PgPool) -> (i64, i64, i64) {
        let pt: i64 =
            sqlx::query_scalar("INSERT INTO person_types (name) VALUES ('Клиент') RETURNING id")
                .fetch_one(db)
                .await
                .unwrap();
        let other: i64 =
            sqlx::query_scalar("INSERT INTO person_types (name) VALUES ('Чужой') RETURNING id")
                .fetch_one(db)
                .await
                .unwrap();
        let prop: i64 = sqlx::query_scalar(
            "INSERT INTO order_properties (person_type_id, code, name) VALUES ($1, 'INN', 'ИНН') RETURNING id",
        )
        .bind(pt)
        .fetch_one(db)
        .await
        .unwrap();
        (pt, other, prop)
    }

    #[sqlx::test]
    async fn contragent_map_is_saved_and_shown(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (pt, other, prop) = person_type(&db).await;

        let (status, html) = get(&app, &format!("/admin/exchange/map/{pt}"), &cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("BUYER_PERSON_NAME_FIRST"));
        assert!(!html.contains("BUYER_COMPANY_INN"));
        let (status, _) = get(&app, "/admin/exchange/map/99999", &cookie).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let body = format!(
            "domain=I&src_BUYER_PERSON_NAME_FIRST=user&val_BUYER_PERSON_NAME_FIRST=name\
             &src_BUYER_PERSON_INN=property&val_BUYER_PERSON_INN={prop}\
             &src_BUYER_PERSON_CITY=property&val_BUYER_PERSON_CITY={prop}_CITY\
             &src_BUYER_PERSON_PHONE=&val_BUYER_PERSON_PHONE=ignored\
             &src_BUYER_PERSON_EMAIL=order&val_BUYER_PERSON_EMAIL=account_number\
             &rekv_1_name=%D0%A0%D0%B5%D0%B3%D0%B8%D0%BE%D0%BD&rekv_1_source=value&rekv_1_value=77\
             &rekv_2_name=&rekv_2_source=&rekv_2_value="
        );
        let (status, html) =
            post_form(&app, &format!("/admin/exchange/map/{pt}"), &cookie, &body).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
        let entry = |code: &str, name: &str, source, value: &str| MapEntry {
            person_type_id: pt,
            code: code.into(),
            name: name.into(),
            source,
            value: value.into(),
        };
        let mut got = load_map(&db).await.unwrap();
        got.sort_by(|a, b| a.code.cmp(&b.code));
        assert_eq!(
            got,
            vec![
                entry("1C_REKV_1", "Регион", MapSource::Value, "77"),
                entry(
                    "BUYER_PERSON_CITY",
                    "",
                    MapSource::Property,
                    &format!("{prop}_CITY")
                ),
                entry("BUYER_PERSON_EMAIL", "", MapSource::Order, "account_number"),
                entry(
                    "BUYER_PERSON_INN",
                    "",
                    MapSource::Property,
                    &prop.to_string()
                ),
                entry("BUYER_PERSON_NAME_FIRST", "", MapSource::User, "name"),
            ]
        );

        let (_, html) = get(&app, &format!("/admin/exchange/map/{pt}"), &cookie).await;
        for n in [
            r#"<option value="user" selected>"#,
            r#"value="name""#,
            &format!(r#"value="{prop}_CITY""#),
            "Регион",
            r#"value="77""#,
            "1C_REKV_2",
        ] {
            assert!(html.contains(n), "нет «{n}»: {html}");
        }

        // свойство чужого типа плательщика, неизвестное поле, пустое название реквизита
        let before = load_map(&db).await.unwrap();
        for bad in [
            "domain=I&src_BUYER_PERSON_INN=property&val_BUYER_PERSON_INN=999999".to_string(),
            "domain=I&src_BUYER_PERSON_INN=user&val_BUYER_PERSON_INN=nope".to_string(),
            "domain=I&src_BUYER_PERSON_INN=order&val_BUYER_PERSON_INN=nope".to_string(),
            "domain=I&src_BUYER_PERSON_INN=bogus&val_BUYER_PERSON_INN=1".to_string(),
            "domain=X".to_string(),
            "domain=I&rekv_1_name=&rekv_1_source=value&rekv_1_value=7".to_string(),
            "domain=I&rekv_1_name=%D0%90&rekv_1_source=&rekv_1_value=7".to_string(),
        ] {
            let url = format!("/admin/exchange/map/{pt}");
            let (status, html) = post_form(&app, &url, &cookie, &format!("{bad}&x=1")).await;
            assert_eq!(status, StatusCode::OK, "{bad}");
            assert!(html.contains(r#"class="alert""#), "{bad}: {html}");
            assert_eq!(load_map(&db).await.unwrap(), before, "{bad}");
        }
        // введённое остаётся в форме
        let (_, html) = post_form(
            &app,
            &format!("/admin/exchange/map/{pt}"),
            &cookie,
            "domain=I&src_BUYER_PERSON_INN=user&val_BUYER_PERSON_INN=nope",
        )
        .await;
        assert!(html.contains(r#"value="nope""#), "{html}");
        // чужой тип плательщика — отдельное соответствие
        let (status, _) = post_form(
            &app,
            &format!("/admin/exchange/map/{other}"),
            &cookie,
            &format!("domain=E&src_BUYER_COMPANY_INN=property&val_BUYER_COMPANY_INN={prop}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "свойство не этого типа плательщика");
        assert_eq!(load_map(&db).await.unwrap(), before);
    }

    #[sqlx::test]
    async fn domain_switch_shows_company_codes(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (pt, _, _) = person_type(&db).await;
        let url = format!("/admin/exchange/map/{pt}");
        let (status, _) = post_form(
            &app,
            &url,
            &cookie,
            "domain=E&src_BUYER_COMPANY_INN=value&val_BUYER_COMPANY_INN=1",
        )
        .await;
        // домен сохранён, коды юрлица из формы тоже
        assert_eq!(status, StatusCode::SEE_OTHER);
        let domain: String = sqlx::query_scalar("SELECT domain FROM person_types WHERE id = $1")
            .bind(pt)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(domain, "E");
        let (_, html) = get(&app, &url, &cookie).await;
        assert!(html.contains("BUYER_COMPANY_INN"));
        assert!(!html.contains("BUYER_PERSON_INN"));
        let (_, html) = get(&app, "/admin/exchange?tab=sale", &cookie).await;
        assert!(html.contains(&url), "{html}");
        assert!(html.contains("Юрлицо"));
    }

    #[sqlx::test]
    async fn journal_summarizes_sale_rows(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        sqlx::query(
            "INSERT INTO exchange_log (kind, mode, filename, result, stats, error, duration_ms) VALUES
             ('sale', 'file', 'o.xml', 'success',
              '{\"orders_created\":2,\"orders_updated\":1,\"payments_created\":0,\"payments_updated\":3,\"shipments_created\":0,\"shipments_updated\":0,\"skipped\":4,\"errors\":[\"плохой документ\"]}', '', 1),
             ('sale', 'query', '', 'success', '{\"orders\":5}', '', 1)",
        )
        .execute(&db)
        .await
        .unwrap();
        let (_, html) = get(&app, "/admin/exchange?tab=sale", &cookie).await;
        for n in [
            "Выгружено: заказы 5",
            "Создано: заказы 2. Обновлено: заказы 1, оплаты 3. Пропущено: 4. Ошибок: 1",
        ] {
            assert!(html.contains(n), "нет «{n}»: {html}");
        }
    }

    #[sqlx::test]
    async fn map_for_all_person_types(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (_, _, prop) = person_type(&db).await;
        sqlx::query(
            "INSERT INTO exchange_order_map (person_type_id, code, name, source, value)
             VALUES (0, 'BUYER_PERSON_INN', '', 'value', 'ОБЩИЙ'), (0, '1C_REKV_4', 'Общий', 'user', 'login')",
        )
        .execute(&db)
        .await
        .unwrap();
        let (status, html) = get(&app, "/admin/exchange/map/0", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        for n in [
            "BUYER_PERSON_INN",
            "BUYER_COMPANY_INN",
            "ОБЩИЙ",
            "1C_REKV_4",
            "Общий",
        ] {
            assert!(html.contains(n), "нет «{n}»");
        }
        assert!(!html.contains(r#"name="domain""#));
        let (_, list) = get(&app, "/admin/exchange?tab=sale", &cookie).await;
        assert!(list.contains("/admin/exchange/map/0"));

        // свойство любого типа плательщика; домен не требуется
        let body = format!(
            "src_BUYER_PERSON_INN=value&val_BUYER_PERSON_INN=NEW\
             &src_BUYER_COMPANY_INN=property&val_BUYER_COMPANY_INN={prop}\
             &rekv_4_name=%D0%9E&rekv_4_source=order&rekv_4_value=id"
        );
        let (status, html) = post_form(&app, "/admin/exchange/map/0", &cookie, &body).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
        let mut got: Vec<(i64, String, String, String)> = load_map(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|e| {
                (
                    e.person_type_id,
                    e.code,
                    e.source.code().to_string(),
                    e.value,
                )
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                (0, "1C_REKV_4".into(), "order".into(), "id".into()),
                (
                    0,
                    "BUYER_COMPANY_INN".into(),
                    "property".into(),
                    prop.to_string()
                ),
                (0, "BUYER_PERSON_INN".into(), "value".into(), "NEW".into()),
            ]
        );
        let (status, html) = post_form(
            &app,
            "/admin/exchange/map/0",
            &cookie,
            "src_BUYER_PERSON_INN=property&val_BUYER_PERSON_INN=999999",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(r#"class="alert""#));
    }
}
