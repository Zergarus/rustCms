//! Заказы: список с фильтрами, карточка, смена статуса, оплата, отмена, свойства.

use std::collections::HashMap;

use axum::{
    Extension,
    extract::{MatchedPath, Multipart, Path, Query, State},
    http::{HeaderMap, header},
    response::{Html, IntoResponse, Response},
};
use axum_extra::extract::Form;
use chrono::{Days, NaiveDate, TimeZone, Utc};
use minijinja::context;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::{FromRow, Postgres, QueryBuilder};

use crate::{
    access::{Access, ORDERS_MANAGE},
    admin::{
        flash::{self, FlashKind},
        nav::ORDER_TABS,
        read_upload_form, render, shell, tabs,
    },
    error::{AppError, AppResult},
    files,
    sale::{self, OrderProperty, mail, repo, validate::validate_properties},
    state::AppState,
    stock::{self, StockError},
};

const PER_PAGE: i64 = 50;

pub(crate) fn require_orders(user: &Access) -> AppResult<()> {
    user.require(ORDERS_MANAGE)
}

/// Фильтры списка заказов (пустое поле — без условия).
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct OrdersQuery {
    #[serde(default)]
    pub number: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub buyer: String,
    /// `Y` | `N` | пусто
    #[serde(default)]
    pub paid: String,
    #[serde(default)]
    pub canceled: String,
    pub page: Option<i64>,
}

fn date(raw: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").ok()
}

fn yes_no(raw: &str) -> Option<bool> {
    match raw {
        "Y" => Some(true),
        "N" => Some(false),
        _ => None,
    }
}

/// Текст покупателя для поиска (логин, почта, имя, свойства заказа); запрос с `orders o` и `users u`.
pub(crate) const BUYER_TEXT_SQL: &str = "concat_ws(' ', u.login, u.email, u.name, u.last_name,
                 (SELECT string_agg(v.value, ' ') FROM order_property_values v WHERE v.order_id = o.id))";

/// Условия фильтра к запросу с `orders o` и `users u` (… WHERE TRUE).
pub(crate) fn push_filters(qb: &mut QueryBuilder<Postgres>, q: &OrdersQuery) {
    let number = q.number.trim();
    if !number.is_empty() {
        qb.push(" AND o.account_number = ")
            .push_bind(number.to_string());
    }
    if !q.status.trim().is_empty() {
        qb.push(" AND o.status = ")
            .push_bind(q.status.trim().to_string());
    }
    if let Some(from) = date(&q.from) {
        qb.push(" AND o.created_at >= ")
            .push_bind(Utc.from_utc_datetime(&from.and_hms_opt(0, 0, 0).unwrap_or_default()));
    }
    if let Some(to) = date(&q.to).and_then(|d| d.checked_add_days(Days::new(1))) {
        qb.push(" AND o.created_at < ")
            .push_bind(Utc.from_utc_datetime(&to.and_hms_opt(0, 0, 0).unwrap_or_default()));
    }
    let buyer = q.buyer.trim();
    if !buyer.is_empty() {
        qb.push(" AND ")
            .push(BUYER_TEXT_SQL)
            .push(" ILIKE ")
            .push_bind(format!("%{}%", buyer.replace('%', "")));
    }
    if let Some(paid) = yes_no(&q.paid) {
        qb.push(" AND o.paid = ").push_bind(paid);
    }
    if let Some(canceled) = yes_no(&q.canceled) {
        qb.push(" AND o.canceled = ").push_bind(canceled);
    }
}

#[derive(FromRow, Serialize)]
struct OrderRow {
    id: i64,
    account_number: String,
    created_at: chrono::DateTime<Utc>,
    buyer: String,
    status_name: String,
    price: f64,
    currency: String,
    paid: bool,
    canceled: bool,
    delivery: String,
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<OrdersQuery>,
) -> AppResult<Html<String>> {
    require_orders(&user)?;
    let page = q.page.unwrap_or(1).max(1);
    let mut count = QueryBuilder::<Postgres>::new(
        "SELECT count(*) FROM orders o LEFT JOIN users u ON u.id = o.user_id WHERE TRUE",
    );
    push_filters(&mut count, &q);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT o.id, COALESCE(o.account_number, o.id::text) AS account_number, o.created_at,
                COALESCE(NULLIF((SELECT v.value FROM order_property_values v
                                 JOIN order_properties p ON p.id = v.property_id
                                 WHERE v.order_id = o.id AND p.is_payer LIMIT 1), ''), u.login, '—') AS buyer,
                COALESCE(s.name, o.status) AS status_name, o.price::float8 AS price, o.currency,
                o.paid, o.canceled,
                COALESCE((SELECT sh.delivery_name FROM shipments sh WHERE sh.order_id = o.id LIMIT 1), '') AS delivery
         FROM orders o
         LEFT JOIN users u ON u.id = o.user_id
         LEFT JOIN order_statuses s ON s.code = o.status
         WHERE TRUE",
    );
    push_filters(&mut qb, &q);
    qb.push(" ORDER BY o.created_at DESC, o.id DESC LIMIT ")
        .push_bind(PER_PAGE)
        .push(" OFFSET ")
        .push_bind((page - 1) * PER_PAGE);
    let rows: Vec<OrderRow> = qb.build_query_as().fetch_all(&state.db).await?;
    let statuses: Vec<(String, String)> =
        sqlx::query_as("SELECT code, name FROM order_statuses ORDER BY sort, code")
            .fetch_all(&state.db)
            .await?;
    let pages = (total + PER_PAGE - 1) / PER_PAGE;
    let query = serde_urlencoded_query(&q);
    render(
        &state,
        "shop/orders.html",
        context! { user, rows, total, page, pages, q, statuses, query },
    )
}

/// Фильтры строкой для ссылок пагинации (без `page`).
fn serde_urlencoded_query(q: &OrdersQuery) -> String {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    [
        ("number", &q.number),
        ("status", &q.status),
        ("from", &q.from),
        ("to", &q.to),
        ("buyer", &q.buyer),
        ("paid", &q.paid),
        ("canceled", &q.canceled),
    ]
    .iter()
    .filter(|(_, v)| !v.is_empty())
    .map(|(k, v)| format!("&{k}={}", enc(v)))
    .collect()
}

/// Свойства типа плательщика, относящиеся к оплате и доставке заказа (служебные — тоже).
fn order_properties<'a>(
    settings: &'a sale::SaleSettings,
    order: &repo::OrderView,
) -> Vec<&'a OrderProperty> {
    let payment = order
        .payment
        .as_ref()
        .and_then(|p| p.pay_system_id)
        .unwrap_or(0);
    let delivery = order
        .shipment
        .as_ref()
        .and_then(|s| s.delivery_id)
        .unwrap_or(0);
    settings
        .properties
        .iter()
        .filter(|p| p.person_type_id == order.person_type_id && p.active)
        .filter(|p| settings.property_applies(p.id, payment, delivery))
        .collect()
}

/// Файл свойства в карточке.
#[derive(Serialize)]
struct PropFile {
    id: i64,
    url: String,
    name: String,
}

/// Файловое свойство заказа (накладная, счёт, УПД).
#[derive(Serialize)]
struct FileField {
    id: i64,
    name: String,
    multiple: bool,
    files: Vec<PropFile>,
}

#[derive(Serialize)]
struct PropField {
    code: String,
    name: String,
    kind: String,
    required: bool,
    util: bool,
    value: String,
    options: Vec<(String, String)>,
}

/// Данные обмена с 1С заказа.
#[derive(FromRow, Serialize)]
struct OneCOrder {
    id_1c: String,
    version_1c: String,
    updated_1c: bool,
    external: bool,
}

#[derive(FromRow, Serialize)]
struct OneCPayment {
    name: String,
    id_1c: String,
    version_1c: String,
    pay_voucher_num: String,
    pay_voucher_date: Option<String>,
    is_return: bool,
    return_comment: String,
}

#[derive(FromRow, Serialize)]
struct OneCShipment {
    delivery_name: String,
    id_1c: String,
    version_1c: String,
    deducted: bool,
    deducted_at: Option<String>,
    delivery_doc_num: String,
    delivery_doc_date: Option<String>,
    tracking_number: String,
}

/// Адрес вкладки карточки заказа (`""` — «Обзор»).
fn tab_url(id: i64, tab: &str) -> String {
    match tab {
        "" => format!("/admin/shop/orders/{id}"),
        t => format!("/admin/shop/orders/{id}/{t}"),
    }
}

/// Запрос вкладки из HTMX-навигации карточки: отдаётся только вкладка и колонка справа.
fn is_tab_fragment(headers: &HeaderMap) -> bool {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    header("hx-request") == Some("true")
        && header("hx-target") == Some("order-tab")
        && !headers.contains_key("hx-boosted")
}

/// Карточка заказа на вкладке `tab` (код из [`ORDER_TABS`], неизвестный — 404): страница
/// целиком или, при `fragment`, вкладка с колонкой справа (out-of-band).
pub(crate) async fn card(
    state: &AppState,
    user: Access,
    id: i64,
    tab: &str,
    error: Option<String>,
    notice: Option<String>,
    fragment: bool,
) -> AppResult<Response> {
    let active_tab = ORDER_TABS
        .iter()
        .map(|(code, _)| *code)
        .find(|code| *code == tab)
        .ok_or(AppError::NotFound)?;
    let order = repo::load(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let settings = sale::load_settings(&state.db).await?;
    let values: HashMap<&str, &str> = order
        .properties
        .iter()
        .map(|v| (v.code.as_str(), v.value.as_str()))
        .collect();
    let (file_props, text_props): (Vec<_>, Vec<_>) = order_properties(&settings, &order)
        .into_iter()
        .partition(|p| p.kind == "file");
    let stored: HashMap<i64, &str> = order
        .properties
        .iter()
        .map(|v| (v.property_id, v.value.as_str()))
        .collect();
    let ids: Vec<i64> = file_props
        .iter()
        .flat_map(|p| repo::file_ids(stored.get(&p.id).copied().unwrap_or("")))
        .collect();
    let records: HashMap<i64, files::FileRecord> = files::get_many(&state.db, &ids)
        .await?
        .into_iter()
        .map(|f| (f.id, f))
        .collect();
    let file_fields: Vec<FileField> = file_props
        .iter()
        .map(|p| FileField {
            id: p.id,
            name: p.name.clone(),
            multiple: p.multiple,
            files: repo::file_ids(stored.get(&p.id).copied().unwrap_or(""))
                .into_iter()
                .filter_map(|id| records.get(&id))
                .map(|f| PropFile {
                    id: f.id,
                    url: f.url(),
                    name: f.original_name.clone(),
                })
                .collect(),
        })
        .collect();
    let fields: Vec<PropField> = text_props
        .into_iter()
        .map(|p| PropField {
            code: p.code.clone(),
            name: p.name.clone(),
            kind: p.kind.clone(),
            required: p.required,
            util: p.util,
            value: values.get(p.code.as_str()).unwrap_or(&"").to_string(),
            options: settings
                .variants
                .get(&p.id)
                .map(|vs| {
                    vs.iter()
                        .map(|v| (v.value.clone(), v.name.clone()))
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();
    // Значения свойств, которых больше нет в справочнике или они к заказу не относятся
    let extra: Vec<&repo::OrderPropertyValue> = order
        .properties
        .iter()
        .filter(|v| !fields.iter().any(|f| f.code == v.code))
        .filter(|v| !file_fields.iter().any(|f| f.id == v.property_id))
        .collect();
    let stores: HashMap<String, String> =
        sqlx::query_as::<_, (i64, String)>("SELECT id, name FROM catalog_stores")
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|(id, name)| (id.to_string(), name))
            .collect();
    let statuses: Vec<(String, String)> =
        sqlx::query_as("SELECT code, name FROM order_statuses ORDER BY sort, code")
            .fetch_all(&state.db)
            .await?;
    let store_list: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM catalog_stores ORDER BY sort, id")
            .fetch_all(&state.db)
            .await?;
    let deliveries: Vec<(i64, String, f64)> = sqlx::query_as(
        "SELECT id, name, price::float8 FROM deliveries ORDER BY active DESC, sort, id",
    )
    .fetch_all(&state.db)
    .await?;
    let pay_systems: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM pay_systems ORDER BY active DESC, sort, id")
            .fetch_all(&state.db)
            .await?;
    let version = super::order_edit::version(&order.updated_at);
    let onec: OneCOrder =
        sqlx::query_as("SELECT id_1c, version_1c, updated_1c, external FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(&state.db)
            .await?;
    let onec_payments: Vec<OneCPayment> = sqlx::query_as(
        "SELECT name, id_1c, version_1c, pay_voucher_num,
                to_char(pay_voucher_date, 'YYYY-MM-DD') AS pay_voucher_date, is_return, return_comment
         FROM payments WHERE order_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let onec_shipments: Vec<OneCShipment> = sqlx::query_as(
        "SELECT delivery_name, id_1c, version_1c, deducted,
                to_char(deducted_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI') AS deducted_at,
                delivery_doc_num, to_char(delivery_doc_date, 'YYYY-MM-DD') AS delivery_doc_date,
                tracking_number
         FROM shipments WHERE order_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let person_type: String = sqlx::query_scalar("SELECT name FROM person_types WHERE id = $1")
        .bind(order.person_type_id)
        .fetch_optional(&state.db)
        .await?
        .unwrap_or_default();
    let discounts = repo::order_discounts(&state.db, id).await?;
    let shipment_base: Option<f64> = sqlx::query_scalar(
        "SELECT base_price::float8 FROM shipments WHERE order_id = $1 ORDER BY id LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    // Основная отгрузка: «Отгружено» и склады списания позиций
    let shipped = onec_shipments.first().is_some_and(|s| s.deducted);
    let ship_stores: HashMap<String, i64> = sqlx::query_as::<_, (i64, i64)>(
        "SELECT cart_item_id, min(store_id) FROM shipment_item_stores
         WHERE shipment_id = (SELECT min(id) FROM shipments WHERE order_id = $1)
         GROUP BY cart_item_id",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|(line, store)| (line.to_string(), store))
    .collect();
    let reserves: HashMap<String, f64> = sqlx::query_as::<_, (i64, f64)>(
        "SELECT r.cart_item_id, sum(r.quantity)::float8 FROM basket_reservations r
         JOIN cart_items c ON c.id = r.cart_item_id WHERE c.order_id = $1 GROUP BY r.cart_item_id",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|(line, qty)| (line.to_string(), qty))
    .collect();
    let active_stores: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM catalog_stores WHERE active ORDER BY sort, id")
            .fetch_all(&state.db)
            .await?;
    let discount_total = (discounts.iter().map(|d| d.sum).sum::<f64>() * 100.0).round() / 100.0;
    let tab_items: Vec<(&str, &str, String)> = ORDER_TABS
        .iter()
        .map(|(code, label)| (*code, *label, tab_url(id, code)))
        .collect();
    let title = format!("Заказ № {}", order.account_number);
    if fragment {
        // Полная страница не загружается — полоса вкладок узнаёт адрес вкладки здесь
        let user_id = user.user.id;
        match shell::load_prefs(&state.db, user_id).await {
            Ok(prefs) if prefs.tabs => {
                let url = tab_url(id, active_tab);
                if let Err(e) = tabs::open(&state.db, user_id, &url, &title).await {
                    tracing::warn!(order_id = id, error = %e, "вкладка заказа не запомнена");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(order_id = id, error = %e, "вкладка заказа не запомнена"),
        }
    }
    let template = if fragment {
        "shop/order_fragment.html"
    } else {
        "shop/order.html"
    };
    Ok(render(
        state,
        template,
        context! { user, order, reserves, active_stores, fields, file_fields, extra, stores, statuses, person_type, error, notice,
        store_list, deliveries, pay_systems, version, onec, onec_payments, onec_shipments, discounts,
        shipment_base, shipped, ship_stores, active_tab, tab_items, title, fragment, discount_total },
    )?
    .into_response())
}

/// «Обзор»: `/admin/shop/orders/{id}`.
pub async fn view(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> AppResult<Response> {
    require_orders(&user)?;
    card(&state, user, id, "", None, None, is_tab_fragment(&headers)).await
}

/// Остальные вкладки: `/admin/shop/orders/{id}/<вкладка>`; код вкладки — последний
/// сегмент шаблона маршрута.
pub async fn view_tab(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    matched: MatchedPath,
    headers: HeaderMap,
) -> AppResult<Response> {
    require_orders(&user)?;
    let tab = matched.as_str().rsplit('/').next().unwrap_or_default();
    card(&state, user, id, tab, None, None, is_tab_fragment(&headers)).await
}

/// XML заказа в формате обмена с 1С (схема 2.08) для проверки. Ничего не отмечает: Ид
/// контрагента не сохраняется, метка выгрузки не двигается.
pub async fn xml1c(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    use crate::exchange::sale::{
        document::{self, SchemaVersion},
        select,
        settings::{SaleSettings, load_map},
    };
    require_orders(&user)?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM orders WHERE id = $1)")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    if !exists {
        return Err(AppError::NotFound);
    }
    let settings = SaleSettings::load(&state.db).await?;
    let map = load_map(&state.db).await?;
    let version = SchemaVersion::parse(Some("2.08"));
    let hook = state.project.order_export_hook.as_deref();
    let mut conn = state.db.acquire().await?;
    let order = select::build(&mut conn, id, &settings, &map, version, hook, false).await?;
    let xml = document::render(&[order], version, Utc::now());
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "application/xml; charset=windows-1251".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"order_{id}_1c.xml\""),
            ),
        ],
        xml,
    )
        .into_response())
}

fn send(state: &AppState, event: String, order_id: i64) {
    let state = state.clone();
    tokio::spawn(async move { mail::notify(&state, &event, order_id).await });
}

/// 303 на вкладку `tab` карточки с уведомлением `text`.
pub(crate) fn back(id: i64, tab: &str, text: &str) -> Response {
    flash::redirect(&tab_url(id, tab), text, FlashKind::Ok)
}

#[derive(Deserialize)]
pub struct StatusForm {
    status: String,
    #[serde(default)]
    comment: String,
}

pub async fn set_status(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<StatusForm>,
) -> AppResult<Response> {
    require_orders(&user)?;
    let notify: Option<bool> =
        sqlx::query_scalar("SELECT notify FROM order_statuses WHERE code = $1")
            .bind(&form.status)
            .fetch_optional(&state.db)
            .await?;
    let Some(notify) = notify else {
        return card(
            &state,
            user,
            id,
            "",
            Some("Нет такого статуса".into()),
            None,
            false,
        )
        .await;
    };
    if repo::set_status(
        &state.db,
        id,
        &form.status,
        Some(user.user.id),
        form.comment.trim(),
    )
    .await?
        && notify
    {
        send(&state, format!("SALE_STATUS_CHANGED_{}", form.status), id);
    }
    Ok(back(id, "", "Статус изменён"))
}

#[derive(Deserialize)]
pub struct FlagForm {
    /// `Y` — установить, иначе снять.
    value: String,
    #[serde(default)]
    reason: String,
}

pub async fn set_paid(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<FlagForm>,
) -> AppResult<Response> {
    require_orders(&user)?;
    let paid = form.value == "Y";
    if repo::set_paid(&state.db, id, paid).await? && paid {
        send(&state, "SALE_ORDER_PAID".into(), id);
    }
    Ok(back(id, "", "Оплата сохранена"))
}

pub async fn set_canceled(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<FlagForm>,
) -> AppResult<Response> {
    require_orders(&user)?;
    let cancel = form.value == "Y";
    match repo::set_canceled(&state.db, id, cancel, form.reason.trim()).await {
        Ok(changed) => {
            if changed && cancel {
                send(&state, "SALE_ORDER_CANCEL".into(), id);
            }
            Ok(back(id, "", "Отмена заказа сохранена"))
        }
        // Отгруженный заказ не отменяется — ошибка в карточке
        Err(StockError::Rejected(messages)) => {
            card(&state, user, id, "", Some(messages.join("; ")), None, false).await
        }
        Err(StockError::Db(e)) => Err(e.into()),
    }
}

/// «Пересчитать скидки»: текущие правила к позициям заказа (правка состава скидки не
/// пересчитывает). Оплаченный заказ — ошибка в карточке.
pub async fn recalculate_discounts(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    require_orders(&user)?;
    let hook = state.project.discount_condition_hook.as_deref();
    match repo::recalculate_discounts_with(&state.db, id, hook).await {
        Ok(()) => Ok(back(id, "items", "Скидки пересчитаны")),
        Err(e) if e.downcast_ref::<sqlx::Error>().is_some() => Err(e.into()),
        Err(e) => card(&state, user, id, "items", Some(e.to_string()), None, false).await,
    }
}

/// Свойства (`prop_<код>`) и комментарий менеджера.
pub async fn save_properties(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    require_orders(&user)?;
    let order = repo::load(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let settings = sale::load_settings(&state.db).await?;
    // Файлы правятся отдельной формой — здесь их значения не трогаются
    let props: Vec<_> = order_properties(&settings, &order)
        .into_iter()
        .filter(|p| p.kind != "file")
        .collect();
    let keep: Vec<i64> = settings
        .properties
        .iter()
        .filter(|p| p.kind == "file")
        .map(|p| p.id)
        .collect();
    let input: Map<String, Value> = form
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("prop_")?.to_string(), Value::from(v.clone()))))
        .collect();
    let known = repo::known_locations(&state.db, &input).await?;
    let values = match validate_properties(&props, &settings.variants, &input, &known) {
        Ok(v) => v,
        Err(e) => return card(&state, user, id, "props", Some(e.message), None, false).await,
    };
    let rows: Vec<(i64, String, String, String)> = values
        .into_iter()
        .filter_map(|(pid, value)| {
            let p = props.iter().find(|p| p.id == pid)?;
            Some((p.id, p.code.clone(), p.name.clone(), value))
        })
        .collect();
    let comment = form
        .get("manager_comment")
        .map(|s| s.trim())
        .unwrap_or_default();
    repo::update_properties(&state.db, id, &rows, &keep, comment).await?;
    Ok(back(id, "props", "Свойства сохранены"))
}

/// Файлы свойств: загрузка (`file_<id свойства>`) и удаление (`remove_<свойство>_<файл>`).
pub async fn save_files(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    multipart: Multipart,
) -> AppResult<Response> {
    require_orders(&user)?;
    let order = repo::load(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let settings = sale::load_settings(&state.db).await?;
    let props: Vec<&OrderProperty> = order_properties(&settings, &order)
        .into_iter()
        .filter(|p| p.kind == "file")
        .collect();
    let form = read_upload_form(&state, multipart, "sale").await?;
    for (name, _) in &form.fields {
        let Some((prop, file)) = name
            .strip_prefix("remove_")
            .and_then(|r| r.split_once('_'))
            .and_then(|(p, f)| Some((p.parse::<i64>().ok()?, f.parse::<i64>().ok()?)))
        else {
            continue;
        };
        if props.iter().any(|p| p.id == prop) {
            repo::remove_property_file(&state.db, id, prop, file).await?;
        }
    }
    for p in &props {
        let key = format!("file_{}", p.id);
        let ids: Vec<i64> = form
            .uploads
            .iter()
            .filter(|(name, _)| *name == key)
            .map(|(_, f)| f.id)
            .collect();
        if !ids.is_empty() {
            repo::add_property_files(&state.db, id, p, &ids).await?;
        }
    }
    if !form.rejected.is_empty() {
        let error = Some(form.rejected.join("; "));
        return card(&state, user, id, "files", error, None, false).await;
    }
    Ok(back(id, "files", "Документы сохранены"))
}

/// Ручной резерв («Зарезервировать») и снятие резерва («Снять резерв»): весь заказ или одна
/// позиция (поле `line`).
async fn change_reserve(
    state: AppState,
    user: Access,
    id: i64,
    form: HashMap<String, String>,
    reserve: bool,
) -> AppResult<Response> {
    require_orders(&user)?;
    let line: Option<i64> = form.get("line").and_then(|v| v.trim().parse().ok());
    let mut tx = state.db.begin().await?;
    // Заказ блокируется, как в ship.rs: ручной резерв не гонится с отгрузкой
    let canceled: Option<bool> =
        sqlx::query_scalar("SELECT canceled FROM orders WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(canceled) = canceled else {
        return Err(AppError::NotFound);
    };
    let shipped: bool = sqlx::query_scalar(
        "SELECT coalesce((SELECT deducted FROM shipments WHERE order_id = $1 ORDER BY id LIMIT 1), false)",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let refusal = if shipped {
        Some("Заказ отгружен — резервы не меняются")
    } else if canceled && reserve {
        Some("Заказ отменён — резервы не ставятся")
    } else {
        None
    };
    if let Some(message) = refusal {
        drop(tx);
        return card(
            &state,
            user,
            id,
            "shipment",
            Some(message.into()),
            None,
            false,
        )
        .await;
    }
    let warnings = match (reserve, line) {
        (true, Some(l)) => stock::reserve::reserve_line(&mut tx, id, l, Some(user.user.id)).await?,
        (true, None) => stock::reserve::reserve_lines(&mut tx, id, Some(user.user.id)).await?,
        (false, Some(l)) => {
            stock::reserve::release_line(&mut tx, id, l).await?;
            Vec::new()
        }
        (false, None) => {
            stock::reserve::release_lines(&mut tx, id).await?;
            Vec::new()
        }
    };
    tx.commit().await?;
    if warnings.is_empty() {
        let text = if reserve {
            "Резерв поставлен"
        } else {
            "Резерв снят"
        };
        Ok(back(id, "shipment", text))
    } else {
        let notice = Some(warnings.join("; "));
        card(&state, user, id, "shipment", None, notice, false).await
    }
}

pub async fn reserve(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    change_reserve(state, user, id, form, true).await
}

pub async fn unreserve(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    change_reserve(state, user, id, form, false).await
}

/// Склады позиций из формы отгрузки: `store_<позиция>` = склад (пусто — выбор снят).
fn ship_stores(form: &HashMap<String, String>) -> Vec<(i64, Option<i64>)> {
    let mut out: Vec<(i64, Option<i64>)> = form
        .iter()
        .filter_map(|(k, v)| {
            let line = k.strip_prefix("store_")?.parse().ok()?;
            Some((line, v.trim().parse().ok()))
        })
        .collect();
    out.sort_unstable();
    out
}

/// Трек-номер, разрешение доставки и «Отгружено» с выбором складов позиций.
pub async fn save_shipment(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    require_orders(&user)?;
    let field = |name: &str| form.get(name).map(String::as_str);
    let mut tx = state.db.begin().await?;
    let shipment: Option<i64> =
        sqlx::query_scalar("SELECT min(id) FROM shipments WHERE order_id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let Some(shipment) = shipment else {
        return Err(AppError::NotFound);
    };
    // Сначала флажок «Отгружено», затем доставка: при её разрешении отгрузка может
    // поставиться сама (`sale.allow_deduction_on_delivery`)
    let result = async {
        // Выбор складов хранится независимо от отгрузки (шаг 1 выбора склада)
        stock::ship::choose_stores(&mut tx, shipment, &ship_stores(&form)).await?;
        stock::ship::set_deducted(
            &mut tx,
            shipment,
            field("deducted").is_some(),
            &[],
            Some(user.user.id),
        )
        .await?;
        repo::set_shipment_in(
            &mut tx,
            id,
            field("tracking_number").unwrap_or_default(),
            field("allow_delivery").is_some(),
        )
        .await
    }
    .await;
    match result {
        Ok(_) => {
            tx.commit().await?;
            Ok(back(id, "shipment", "Отгрузка сохранена"))
        }
        Err(StockError::Rejected(m)) => {
            drop(tx);
            card(
                &state,
                user,
                id,
                "shipment",
                Some(m.join("; ")),
                None,
                false,
            )
            .await
        }
        Err(StockError::Db(e)) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use sqlx::{Postgres, QueryBuilder};

    use super::*;

    #[test]
    fn orders_query_filters() {
        let base = "SELECT o.id FROM orders o WHERE TRUE";
        let mut empty = QueryBuilder::<Postgres>::new(base);
        push_filters(&mut empty, &OrdersQuery::default());
        assert_eq!(empty.sql().as_str(), base);

        let q = OrdersQuery {
            number: "42".into(),
            status: "N".into(),
            from: "2026-10-01".into(),
            to: "2026-10-08".into(),
            buyer: "ivan".into(),
            paid: "Y".into(),
            canceled: "N".into(),
            page: None,
        };
        let mut qb = QueryBuilder::<Postgres>::new(base);
        push_filters(&mut qb, &q);
        let sql = qb.sql().as_str().to_string();
        for part in [
            "o.account_number = $1",
            "o.status = $2",
            "o.created_at >= $3",
            "o.created_at < $4",
            "ILIKE $5",
            "o.paid = $6",
            "o.canceled = $7",
        ] {
            assert!(sql.contains(part), "{part} в {sql}");
        }
        let mut bad = QueryBuilder::<Postgres>::new(base);
        push_filters(
            &mut bad,
            &OrdersQuery {
                from: "вчера".into(),
                paid: "?".into(),
                ..Default::default()
            },
        );
        assert_eq!(bad.sql().as_str(), base);
    }

    use axum::http::{StatusCode, header};

    use crate::smoke_tests::get;
    use crate::test_support::{
        admin_cookie, get_raw, order_fixture, permitted_cookie, place_order, test_state,
    };

    #[sqlx::test]
    async fn card_shows_1c_block_and_xml_export(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let user: i64 = sqlx::query_scalar(
            "INSERT INTO users (login, email, password_hash) VALUES ('buyer', 'b@e.x', 'x') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let id = place_order(&db, &f, Some(user)).await;
        sqlx::query(
            "INSERT INTO payments (order_id, name, id_1c, version_1c, pay_voucher_num, pay_voucher_date, is_return, return_comment)
             VALUES ($1, 'Оплата 1С', 'PAY-1', '2', 'ПП-15', '2026-05-12', TRUE, 'возврат клиенту')",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE shipments SET id_1c = 'SHIP-1', version_1c = '3', deducted = TRUE,
                    deducted_at = '2026-05-13 10:20:00+00', delivery_doc_num = 'РН-9',
                    delivery_doc_date = '2026-05-13', tracking_number = 'TRACK123'
             WHERE order_id = $1",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        // Метка «из 1С» ставится только внутри обмена
        let mut tx = db.begin().await.unwrap();
        sqlx::query("SELECT set_config('cms.exchange_1c', 'on', true)")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE orders SET id_1c = 'N-77', version_1c = '5', updated_1c = TRUE, external = TRUE WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let state = test_state(db.clone());
        let app = crate::app(state);
        let cookie = admin_cookie(&db).await;

        let (status, html) = get(&app, &format!("/admin/shop/orders/{id}/files"), &cookie).await;
        assert_eq!(status, StatusCode::OK);
        for n in [
            "N-77",
            "изменён из 1С",
            "создан в 1С",
            "PAY-1",
            "ПП-15",
            "2026-05-12",
            "возврат клиенту",
            "SHIP-1",
            "РН-9",
            "TRACK123",
            "2026-05-13 10:20",
            &format!("/admin/shop/orders/{id}/xml1c"),
        ] {
            assert!(html.contains(n), "нет «{n}»");
        }

        let marks = || async {
            sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT (SELECT xml_id FROM users WHERE id = $1),
                        (SELECT value FROM options WHERE module = 'exchange' AND name = 'sale_last_export_committed')",
            )
            .bind(user)
            .fetch_one(&db)
            .await
            .unwrap()
        };
        let before = marks().await;
        let (status, headers, body) =
            get_raw(&app, &format!("/admin/shop/orders/{id}/xml1c"), &cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[header::CONTENT_TYPE],
            "application/xml; charset=windows-1251"
        );
        assert_eq!(
            headers[header::CONTENT_DISPOSITION],
            format!("attachment; filename=\"order_{id}_1c.xml\"")
        );
        let (text, _, bad) = encoding_rs::WINDOWS_1251.decode(&body);
        assert!(!bad);
        assert!(
            text.starts_with(r#"<?xml version="1.0" encoding="windows-1251"?>"#),
            "{text}"
        );
        assert!(text.contains("<Документ>"), "{text}");
        assert!(
            text.contains("<ХозОперация>Заказ товара</ХозОперация>"),
            "{text}"
        );
        assert!(text.contains("<Номер1С>N-77</Номер1С>"), "{text}");
        assert_eq!(
            marks().await,
            before,
            "выгрузка из карточки ничего не отмечает"
        );
        assert_eq!(before.0, "", "Ид контрагента не сохранён");

        let (status, _, _) = get_raw(&app, "/admin/shop/orders/999999/xml1c", &cookie).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let none = permitted_cookie(&db, "plain", &["exchange.1c.sale"]).await;
        let (status, _, _) = get_raw(&app, &format!("/admin/shop/orders/{id}/xml1c"), &none).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let manager = permitted_cookie(&db, "manager", &["orders.manage"]).await;
        let (status, _, _) =
            get_raw(&app, &format!("/admin/shop/orders/{id}/xml1c"), &manager).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[sqlx::test]
    async fn admin_shipment_form(db: sqlx::PgPool) {
        use crate::smoke_tests::post_form;
        let f = order_fixture(&db).await;
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ('catalog', 'default_use_store_control', 'Y')",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO catalog_stores (id, name) VALUES (2, 'Склад 2')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO catalog_store_amounts (item_id, store_id, amount) VALUES ($1, 2, 5)",
        )
        .bind(f.product_id)
        .execute(&db)
        .await
        .unwrap();
        let id = place_order(&db, &f, None).await;
        let line: i64 = sqlx::query_scalar("SELECT id FROM cart_items WHERE order_id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        let state = test_state(db.clone());
        let app = crate::app(state);
        let cookie = admin_cookie(&db).await;
        let path = format!("/admin/shop/orders/{id}/shipment");
        let amount = |store: i64| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, f64>(
                    "SELECT amount::float8 FROM catalog_store_amounts WHERE item_id = $1 AND store_id = $2",
                )
                .bind(f.product_id)
                .bind(store)
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };
        let shipped = || async {
            sqlx::query_as::<_, (bool, String)>(
                "SELECT deducted, tracking_number FROM shipments WHERE order_id = $1",
            )
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap()
        };

        // Склад не выбран, товар на двух складах — ошибка в карточке, ничего не сохранено
        let (status, html) =
            post_form(&app, &path, &cookie, "tracking_number=T-1&deducted=Y").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains(&format!(
                "Не указаны склады для отгрузки товара Товар (#{})",
                f.product_id
            )),
            "{html}"
        );
        assert_eq!(shipped().await, (false, String::new()));

        // Нехватка на выбранном складе
        sqlx::query(
            "UPDATE catalog_store_amounts SET amount = 0 WHERE item_id = $1 AND store_id = 2",
        )
        .bind(f.product_id)
        .execute(&db)
        .await
        .unwrap();
        let body = format!("deducted=on&store_{line}=2");
        let (status, html) = post_form(&app, &path, &cookie, &body).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("на складе"), "{html}");
        assert!(html.contains("для отгрузки."), "{html}");
        assert!(!shipped().await.0);

        sqlx::query(
            "UPDATE catalog_store_amounts SET amount = 5 WHERE item_id = $1 AND store_id = 2",
        )
        .bind(f.product_id)
        .execute(&db)
        .await
        .unwrap();
        let body = format!("tracking_number=T-2&deducted=on&store_{line}=2");
        let (status, html) = post_form(&app, &path, &cookie, &body).await;
        assert!(status.is_redirection(), "{status} {html}");
        assert_eq!(shipped().await, (true, "T-2".to_string()));
        assert_eq!((amount(1).await, amount(2).await), (5.0, 4.0));

        // Карточка: флажок стоит, склад позиции выбран
        let (_, html) = get(&app, &path, &cookie).await;
        assert!(
            html.contains(r#"name="deducted" value="Y" checked"#),
            "{html}"
        );
        assert!(html.contains(&format!(r#"name="store_{line}""#)), "{html}");

        // Снятие флажка возвращает товар на склад
        let (status, _) = post_form(&app, &path, &cookie, "tracking_number=T-2").await;
        assert!(status.is_redirection());
        assert!(!shipped().await.0);
        assert_eq!(amount(2).await, 5.0);
    }

    #[sqlx::test]
    async fn admin_shipment_store_choice_persists(db: sqlx::PgPool) {
        use crate::smoke_tests::post_form;
        let f = order_fixture(&db).await;
        for sql in [
            "INSERT INTO options (module, name, value) VALUES ('catalog', 'default_use_store_control', 'Y')",
            "INSERT INTO catalog_stores (id, name) VALUES (2, 'Склад 2')",
        ] {
            sqlx::query(sql).execute(&db).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO catalog_store_amounts (item_id, store_id, amount) VALUES ($1, 2, 5)",
        )
        .bind(f.product_id)
        .execute(&db)
        .await
        .unwrap();
        let state = test_state(db.clone());
        let app = crate::app(state);
        let cookie = admin_cookie(&db).await;
        let amount = |store: i64| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, f64>(
                    "SELECT amount::float8 FROM catalog_store_amounts WHERE item_id = $1 AND store_id = $2",
                )
                .bind(f.product_id)
                .bind(store)
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };
        let parts = |order: i64| {
            let db = db.clone();
            async move {
                sqlx::query_as::<_, (i64, i64)>(
                    "SELECT (SELECT min(id) FROM shipments WHERE order_id = $1),
                            (SELECT min(id) FROM cart_items WHERE order_id = $1)",
                )
                .bind(order)
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };

        // Склад выбран без «Отгружено» — выбор сохранён, отгрузка без формы берёт его
        let id = place_order(&db, &f, None).await;
        let (sh, line) = parts(id).await;
        let path = format!("/admin/shop/orders/{id}/shipment");
        let (status, html) = post_form(&app, &path, &cookie, &format!("store_{line}=2")).await;
        assert!(status.is_redirection(), "{status} {html}");
        let rows: Vec<(i64, f64)> = sqlx::query_as(
            "SELECT store_id, quantity::float8 FROM shipment_item_stores WHERE shipment_id = $1",
        )
        .bind(sh)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(rows, [(2, 1.0)]);
        let (_, html) = get(&app, &path, &cookie).await;
        assert!(html.contains(r#"<option value="2" selected>"#), "{html}");
        let mut tx = db.begin().await.unwrap();
        assert!(
            crate::stock::ship::set_deducted(&mut tx, sh, true, &[], None)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
        assert_eq!((amount(1).await, amount(2).await), (5.0, 4.0));

        // Отгрузка при разрешении доставки: склад из формы, отмечена только доставка
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ('sale', 'allow_deduction_on_delivery', 'Y')",
        )
        .execute(&db)
        .await
        .unwrap();
        let other = place_order(&db, &f, None).await;
        let (osh, oline) = parts(other).await;
        let path = format!("/admin/shop/orders/{other}/shipment");
        let body = format!("allow_delivery=Y&store_{oline}=2");
        let (status, html) = post_form(&app, &path, &cookie, &body).await;
        assert!(status.is_redirection(), "{status} {html}");
        let deducted: bool = sqlx::query_scalar("SELECT deducted FROM shipments WHERE id = $1")
            .bind(osh)
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(deducted);
        assert_eq!((amount(1).await, amount(2).await), (5.0, 3.0));
    }

    async fn reserved(db: &sqlx::PgPool, order: i64) -> Vec<(i64, f64)> {
        sqlx::query_as(
            "SELECT r.cart_item_id, r.quantity::float8 FROM basket_reservations r
             JOIN cart_items c ON c.id = r.cart_item_id WHERE c.order_id = $1 ORDER BY 1",
        )
        .bind(order)
        .fetch_all(db)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn card_shows_reserve_and_stores(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        sqlx::query(
            "INSERT INTO catalog_stores (id, name, active) VALUES (2, 'Склад закрытый', FALSE)",
        )
        .execute(&db)
        .await
        .unwrap();
        let id = place_order(&db, &f, None).await;
        let line: i64 = sqlx::query_scalar("SELECT id FROM cart_items WHERE order_id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(reserved(&db, id).await, vec![(line, 1.0)]);
        sqlx::query("UPDATE cart_items SET store_id = 1 WHERE id = $1")
            .bind(line)
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (_, html) =
            crate::smoke_tests::get(&app, &format!("/admin/shop/orders/{id}/shipment"), &cookie)
                .await;
        assert!(html.contains("<th>Резерв</th>"), "{html}");
        assert!(
            html.contains("<td>1.0</td>\n        <td>1.0</td>"),
            "{html}"
        );
        // Выбран склад позиции; неактивный склад в выборе отгрузки не предлагается
        let select = html
            .split(&format!(r#"name="store_{line}""#))
            .nth(1)
            .unwrap();
        let select = select.split("</select>").next().unwrap();
        assert!(select.contains(r#"value="1" selected"#), "{select}");
        assert!(!select.contains("Склад закрытый"), "{select}");
        assert!(html.contains(r#"name="deducted" value="Y""#), "{html}");
    }

    #[sqlx::test]
    async fn manual_unreserve_and_reserve(db: sqlx::PgPool) {
        use crate::smoke_tests::post_form;
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let line: i64 = sqlx::query_scalar("SELECT id FROM cart_items WHERE order_id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let base = format!("/admin/shop/orders/{id}");
        let (status, _) = post_form(&app, &format!("{base}/unreserve"), &cookie, "").await;
        assert!(status.is_redirection());
        assert!(reserved(&db, id).await.is_empty());
        let (status, _) = post_form(&app, &format!("{base}/reserve"), &cookie, "").await;
        assert!(status.is_redirection());
        assert_eq!(reserved(&db, id).await, vec![(line, 1.0)]);
        // Одна позиция
        let (status, _) = post_form(
            &app,
            &format!("{base}/unreserve"),
            &cookie,
            &format!("line={line}"),
        )
        .await;
        assert!(status.is_redirection());
        assert!(reserved(&db, id).await.is_empty());
        let (status, _) = post_form(
            &app,
            &format!("{base}/reserve"),
            &cookie,
            &format!("line={line}"),
        )
        .await;
        assert!(status.is_redirection());
        assert_eq!(reserved(&db, id).await, vec![(line, 1.0)]);
        // Чужая позиция заказ не затрагивает
        let (status, _) =
            post_form(&app, &format!("{base}/unreserve"), &cookie, "line=999999").await;
        assert!(status.is_redirection());
        assert_eq!(reserved(&db, id).await, vec![(line, 1.0)]);
    }

    #[sqlx::test]
    async fn manual_reserve_refused_on_canceled(db: sqlx::PgPool) {
        use crate::smoke_tests::post_form;
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let base = format!("/admin/shop/orders/{id}");
        sqlx::query("UPDATE orders SET canceled = TRUE WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM basket_reservations")
            .execute(&db)
            .await
            .unwrap();
        let (status, html) = post_form(&app, &format!("{base}/reserve"), &cookie, "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Заказ отменён — резервы не ставятся"),
            "{html}"
        );
        assert!(reserved(&db, id).await.is_empty());
        let (status, _) = post_form(&app, &format!("{base}/unreserve"), &cookie, "").await;
        assert!(status.is_redirection());
    }

    #[sqlx::test]
    async fn card_shipped_locks_stores(db: sqlx::PgPool) {
        use crate::smoke_tests::post_form;
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let line: i64 = sqlx::query_scalar("SELECT id FROM cart_items WHERE order_id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let path = format!("/admin/shop/orders/{id}");
        let page = || async {
            let (_, html) =
                crate::smoke_tests::get(&app, &format!("{path}/shipment"), &cookie).await;
            html.split(&format!(r#"name="store_{line}""#))
                .nth(1)
                .unwrap()
                .split('>')
                .next()
                .unwrap()
                .to_string()
        };
        assert!(!page().await.contains("disabled"));
        let body = format!("deducted=on&store_{line}=1");
        let (status, html) = post_form(&app, &format!("{path}/shipment"), &cookie, &body).await;
        assert!(status.is_redirection(), "{html}");
        assert!(page().await.contains("disabled"));
        // Отгруженный заказ: резервы вручную не меняются
        let (status, html) = post_form(&app, &format!("{path}/reserve"), &cookie, "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Заказ отгружен"), "{html}");
    }

    #[sqlx::test]
    async fn card_shipped_locks_composition(db: sqlx::PgPool) {
        use crate::smoke_tests::post_form;
        let f = order_fixture(&db).await;
        for sql in [
            "INSERT INTO deliveries (id, name) VALUES (1, 'Самовывоз')",
            "INSERT INTO pay_systems (id, name) VALUES (1, 'QR')",
        ] {
            sqlx::query(sql).execute(&db).await.unwrap();
        }
        let id = place_order(&db, &f, None).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let path = format!("/admin/shop/orders/{id}");
        let items_form = || async {
            let (_, html) = crate::smoke_tests::get(&app, &format!("{path}/items"), &cookie).await;
            assert!(!html.contains("Остатки: "), "{html}");
            html.split(&format!(r#"action="/admin/shop/orders/{id}/items""#))
                .nth(1)
                .unwrap()
                .split("</form>")
                .next()
                .unwrap()
                .to_string()
        };
        assert!(!items_form().await.contains("disabled"));
        sqlx::query("UPDATE shipments SET deducted = TRUE WHERE order_id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let form = items_form().await;
        assert!(form.contains("<fieldset disabled"), "{form}");
        assert!(
            form.contains("Заказ отгружен — состав не правится"),
            "{form}"
        );
        // Запрос в обход формы отклоняется
        let (_, html) = crate::smoke_tests::get(&app, &format!("{path}/items"), &cookie).await;
        let version = html
            .split(r#"name="version" value=""#)
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_string();
        let line: i64 = sqlx::query_scalar("SELECT id FROM cart_items WHERE order_id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        let body = format!(
            "version={}&item_{line}_quantity=5&item_{line}_price=100&delivery_id=1&delivery_price=0&pay_system_id=1",
            version.replace(':', "%3A").replace('+', "%2B")
        );
        let (status, html) = post_form(&app, &format!("{path}/items"), &cookie, &body).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Заказ отгружен — состав не правится"),
            "{html}"
        );
        let quantity: f64 =
            sqlx::query_scalar("SELECT quantity::float8 FROM cart_items WHERE id = $1")
                .bind(line)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(quantity, 1.0);
    }

    #[sqlx::test]
    async fn recalculate_discounts_admin(db: sqlx::PgPool) {
        use crate::smoke_tests::post_form;
        let f = order_fixture(&db).await;
        sqlx::query("INSERT INTO catalog_price_types (id, code, name, is_base, sort) VALUES (1, 'BASE', 'Розница', TRUE, 100)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO catalog_prices (item_id, price_type_id, price, currency) VALUES ($1, 1, 120, 'RUB')")
            .bind(f.product_id)
            .execute(&db)
            .await
            .unwrap();
        let id = place_order(&db, &f, None).await;
        let rule: i64 = sqlx::query_scalar(
            "INSERT INTO discounts (name, actions) VALUES ('Минус 10%', $1) RETURNING id",
        )
        .bind(serde_json::json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": [{
            "CLASS_ID": "ActSaleBsktGrp",
            "DATA": {"Type": "Discount", "Value": 10, "Unit": "Perc", "Max": 0, "All": "AND", "True": "True"},
            "CHILDREN": []}]}))
        .fetch_one(&db)
        .await
        .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let path = format!("/admin/shop/orders/{id}/discounts");
        let (status, html) = post_form(&app, &path, &cookie, "").await;
        assert!(
            status.is_success() || status.is_redirection(),
            "{status} {html}"
        );

        // Базовая цена позиции — из заказа (100), не текущая в каталоге (120)
        let line: (f64, f64, f64) = sqlx::query_as(
            "SELECT base_price::float8, discount_price::float8, price::float8 FROM cart_items WHERE order_id = $1",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(line, (100.0, 10.0, 90.0));
        let totals: (f64, f64, f64) = sqlx::query_as(
            "SELECT o.goods_price::float8, o.price::float8, p.sum::float8
             FROM orders o JOIN payments p ON p.order_id = o.id WHERE o.id = $1",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(totals, (90.0, 90.0, 90.0));
        let applied: Vec<(Option<i64>, f64)> = sqlx::query_as(
            "SELECT discount_id, sum::float8 FROM order_discounts WHERE order_id = $1",
        )
        .bind(id)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(applied, [(Some(rule), 10.0)]);
        let (_, card) = get(&app, &format!("/admin/shop/orders/{id}/items"), &cookie).await;
        assert!(card.contains("Скидки"), "блок скидок");
        assert!(card.contains("Минус 10%"));
        assert!(card.contains(&path), "кнопка «Пересчитать скидки»");

        // Оплаченный заказ — ошибка, ничего не меняется
        sqlx::query("UPDATE discounts SET actions = jsonb_set(actions, '{CHILDREN,0,DATA,Value}', '50') WHERE id = $1")
            .bind(rule)
            .execute(&db)
            .await
            .unwrap();
        crate::sale::repo::set_paid(&db, id, true).await.unwrap();
        let (_, html) = post_form(&app, &path, &cookie, "").await;
        assert!(html.contains("заказ оплачен"), "{html}");
        assert_eq!(
            crate::sale::repo::recalculate_discounts(&db, id)
                .await
                .unwrap_err()
                .to_string(),
            "заказ оплачен"
        );
        let price: f64 =
            sqlx::query_scalar("SELECT price::float8 FROM cart_items WHERE order_id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(price, 90.0);
    }

    /// Адреса всех вкладок карточки заказа.
    fn tab_urls(id: i64) -> Vec<String> {
        crate::admin::nav::ORDER_TABS
            .iter()
            .map(|(code, _)| match *code {
                "" => format!("/admin/shop/orders/{id}"),
                c => format!("/admin/shop/orders/{id}/{c}"),
            })
            .collect()
    }

    #[sqlx::test]
    async fn order_tabs_full_and_fragment(db: sqlx::PgPool) {
        use crate::test_support::htmx_get;
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let number: String = sqlx::query_scalar("SELECT account_number FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        let title = format!("<title>Заказ № {number} — CMS</title>");
        for (url, (_, label)) in tab_urls(id).iter().zip(crate::admin::nav::ORDER_TABS) {
            let (status, html) = get(&app, url, &cookie).await;
            assert_eq!(status, StatusCode::OK, "{url}");
            assert!(html.contains(r#"id="order-tab""#), "{url}: {html}");
            assert!(html.contains(r#"id="order-aside""#), "{url}: {html}");
            for (_, l) in crate::admin::nav::ORDER_TABS {
                assert!(html.contains(l), "{url}: нет вкладки «{l}»");
            }
            assert!(html.contains(&title), "{url}: заголовок «{title}»");

            let (status, frag) = htmx_get(&app, url, &cookie, "order-tab").await;
            assert_eq!(status, StatusCode::OK, "{url}");
            assert!(!frag.contains("<html"), "{url}: {frag}");
            assert!(frag.contains(r#"hx-swap-oob="true""#), "{url}: {frag}");
            assert!(frag.contains(r#"id="order-aside""#), "{url}: {frag}");
            // В навигации фрагмента активна запрошенная вкладка
            let active = frag
                .split(r#"aria-current="page""#)
                .nth(1)
                .and_then(|rest| rest.split("</a>").next())
                .unwrap_or_default();
            assert!(active.ends_with(label), "{url}: {active}");
        }

        // Фрагмент запоминает последнюю вкладку карточки в полосе открытых вкладок
        let (status, _) = htmx_get(
            &app,
            &format!("/admin/shop/orders/{id}/shipment"),
            &cookie,
            "order-tab",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let url: String = sqlx::query_scalar("SELECT url FROM admin_tabs WHERE key = $1")
            .bind(format!("/admin/shop/orders/{id}"))
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(url.ends_with("/shipment"), "{url}");
    }

    #[sqlx::test]
    async fn order_unknown_tab_404(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (status, _) = get(&app, &format!("/admin/shop/orders/{id}/bogus"), &cookie).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn order_tabs_require_orders_manage(db: sqlx::PgPool) {
        use crate::test_support::htmx_get;
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let app = crate::app(test_state(db.clone()));
        let shop = permitted_cookie(&db, "shopman", &["shop.manage"]).await;
        for url in tab_urls(id) {
            let (status, _) = get(&app, &url, &shop).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{url}");
            let (status, _) = htmx_get(&app, &url, &shop, "order-tab").await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{url}");
        }
        let admin = admin_cookie(&db).await;
        for url in tab_urls(999_999) {
            let (status, _) = get(&app, &url, &admin).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{url}");
        }
    }

    #[sqlx::test]
    async fn order_post_redirects_to_tab(db: sqlx::PgPool) {
        use crate::test_support::post_form_raw;
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let base = format!("/admin/shop/orders/{id}");

        let (status, headers, body) = post_form_raw(
            &app,
            &format!("{base}/shipment"),
            &cookie,
            "tracking_number=T-9",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        assert_eq!(headers[header::LOCATION], format!("{base}/shipment"));
        let flash = headers
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|v| v.to_str().unwrap_or_default().starts_with("admin_flash="));
        assert!(flash, "{headers:?}");

        let (status, headers, body) =
            post_form_raw(&app, &format!("{base}/status"), &cookie, "status=N").await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        assert_eq!(headers[header::LOCATION], base);

        // Скидки — до оплаты (оплаченный заказ не пересчитывается), отмена — после резервов
        for (action, form, tab) in [
            ("discounts", "", "/items"),
            ("props", "manager_comment=x", "/props"),
            ("unreserve", "", "/shipment"),
            ("reserve", "", "/shipment"),
            ("paid", "value=Y", ""),
            ("cancel", "value=Y", ""),
            ("cancel", "value=N", ""),
        ] {
            let (status, headers, body) =
                post_form_raw(&app, &format!("{base}/{action}"), &cookie, form).await;
            assert_eq!(status, StatusCode::SEE_OTHER, "{action}: {body}");
            assert_eq!(
                headers[header::LOCATION],
                format!("{base}{tab}"),
                "{action}"
            );
        }

        // Уведомление показывается на следующей полной странице
        let (_, headers, _) =
            post_form_raw(&app, &format!("{base}/status"), &cookie, "status=N").await;
        let flash = headers[header::SET_COOKIE].to_str().unwrap();
        let flash = flash.split(';').next().unwrap();
        let (status, html) = get(&app, &base, &format!("{cookie}; {flash}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Статус изменён"), "{html}");
    }

    #[sqlx::test]
    async fn order_form_error_renders_tab(db: sqlx::PgPool) {
        use crate::smoke_tests::post_form;
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (status, html) = post_form(
            &app,
            &format!("/admin/shop/orders/{id}/status"),
            &cookie,
            "status=NOPE",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Нет такого статуса"), "{html}");
        // активных ссылок несколько (меню, вкладки): нужна среди них «Обзор»
        let active: Vec<&str> = html
            .split(r#"aria-current="page""#)
            .skip(1)
            .filter_map(|rest| rest.split("</a>").next())
            .collect();
        assert!(active.iter().any(|a| a.ends_with("Обзор")), "{active:?}");
    }

    #[sqlx::test]
    async fn order_aside_totals(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        sqlx::query("UPDATE orders SET price = 1234.5 WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (_, html) = get(&app, &format!("/admin/shop/orders/{id}/history"), &cookie).await;
        let aside = html
            .split(r#"id="order-aside""#)
            .nth(1)
            .and_then(|rest| rest.split("</aside>").next())
            .unwrap_or_default();
        assert!(aside.contains("1234.5 RUB"), "{aside}");
        assert!(aside.contains("Итого"), "{aside}");
    }

    #[sqlx::test]
    async fn post_error_page_has_shell(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let cookie = admin_cookie(&db).await;
        let app = crate::app(test_state(db.clone()));
        let (status, _, body) = crate::test_support::post_form_raw(
            &app,
            &format!("/admin/shop/orders/{id}/status"),
            &cookie,
            "status=NOPE",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("Нет такого статуса"), "{body}");
        assert!(body.contains("Все разделы"), "{body}");
        assert!(body.contains("data-palette-open"), "{body}");
        assert!(!body.contains("hx-post=\"/admin/tabs\""), "{body}");
    }
}
