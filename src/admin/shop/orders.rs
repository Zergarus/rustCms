//! Заказы: список с фильтрами, карточка, смена статуса, оплата, отмена, свойства.

use std::collections::HashMap;

use axum::{
    Extension,
    extract::{MatchedPath, Multipart, OriginalUri, Path, RawQuery, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use axum_extra::extract::Form;
use chrono::Utc;
use minijinja::context;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::{FromRow, Postgres, QueryBuilder};

use crate::{
    access::{Access, ORDERS_MANAGE},
    admin::{
        flash::{self, FlashKind},
        list::{
            bulk::mails_to_send,
            orders::{BUYER_SQL, orders_spec},
            render::{ListPage, run_list},
            sql::Scope,
            views,
        },
        nav::ORDER_TABS,
        read_upload_form, render, shell, tabs,
    },
    error::{AppError, AppResult},
    files,
    sale::{
        self, OrderProperty,
        actions::{self, ActionError, MailEvent},
        mail, repo,
        validate::validate_properties,
    },
    state::AppState,
    stock::{self, StockError},
};

pub(crate) fn require_orders(user: &Access) -> AppResult<()> {
    user.require(ORDERS_MANAGE)
}

/// Текст покупателя для поиска (логин, почта, имя, свойства заказа); запрос с `orders o` и `users u`.
pub(crate) const BUYER_TEXT_SQL: &str = "concat_ws(' ', u.login, u.email, u.name, u.last_name,
                 (SELECT string_agg(v.value, ' ') FROM order_property_values v WHERE v.order_id = o.id))";

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> AppResult<Response> {
    list_inner(state, user, raw, headers, uri, false, None).await
}

/// `after_action` — перерисовка после действия с видом: показывает запомненное и ничего не запоминает.
pub(crate) async fn list_inner(
    state: AppState,
    user: Access,
    raw: Option<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    after_action: bool,
    bulk: Option<crate::admin::list::bulk::BulkSummary>,
) -> AppResult<Response> {
    require_orders(&user)?;
    let spec = orders_spec(&state.db, &user).await?;
    let resolved = views::resolve(
        &state.db,
        user.user.id,
        &spec,
        raw.as_deref().unwrap_or(""),
        views::restores(&headers, after_action),
    )
    .await?;
    let page = ListPage {
        title: "Заказы".into(),
        crumbs: vec![("Магазин".into(), String::new())],
        bulk,
        ..ListPage::default()
    };
    run_list(
        &state,
        &user,
        &headers,
        uri.path(),
        &spec,
        resolved.parsed,
        Scope::All,
        page,
        resolved.notices,
        !after_action,
    )
    .await
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
    card_typed(state, user, id, tab, error, notice, fragment, None).await
}

/// [`card`] с введёнными, но не сохранёнными значениями формы отгрузки: они показываются
/// вместо данных из БД (после отказа в сохранении ничего не нужно вводить заново).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn card_typed(
    state: &AppState,
    user: Access,
    id: i64,
    tab: &str,
    error: Option<String>,
    notice: Option<String>,
    fragment: bool,
    typed: Option<&HashMap<String, String>>,
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
    // Себестоимость и маржа — только с правом на закупочные цены
    let show_cost = user.can(crate::access::PURCHASE_INFO);
    let (costs, order_margin) = if show_cost {
        line_costs(&state.db, &order).await?
    } else {
        (HashMap::new(), None)
    };
    // Поштучные позиции отгрузки: код товара → признак; коды и число штрихкодов позиции
    let multi: Vec<i64> = sqlx::query_scalar(
        "SELECT item_id FROM catalog_products WHERE item_id = ANY($1) AND barcode_multi",
    )
    .bind(order.items.iter().map(|i| i.product_id).collect::<Vec<_>>())
    .fetch_all(&state.db)
    .await?;
    let ship_barcodes: HashMap<String, String> = sqlx::query_as::<_, (i64, String)>(
        "SELECT cart_item_id, string_agg(barcode, E'\\n' ORDER BY id) FROM shipment_item_barcodes
         WHERE shipment_id = (SELECT min(id) FROM shipments WHERE order_id = $1)
         GROUP BY cart_item_id",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|(line, codes)| (line.to_string(), codes))
    .collect();
    let (mut ship_stores, mut ship_barcodes) = (ship_stores, ship_barcodes);
    if let Some(form) = typed {
        for (line, store) in self::ship_stores(form) {
            match store {
                Some(store) => ship_stores.insert(line.to_string(), store),
                None => ship_stores.remove(&line.to_string()),
            };
        }
        for (line, _, codes) in self::ship_barcodes(form) {
            ship_barcodes.insert(line.to_string(), codes.join("\n"));
        }
    }
    let typed_form = typed.map(|f| {
        context! {
            tracking_number => f.get("tracking_number").cloned().unwrap_or_default(),
            allow_delivery => f.contains_key("allow_delivery"),
            deducted => f.contains_key("deducted"),
        }
    });
    let ship_barcode_n: HashMap<String, usize> = ship_barcodes
        .iter()
        .map(|(line, codes)| (line.clone(), codes.lines().count()))
        .collect();
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
        shipment_base, shipped, ship_stores, active_tab, tab_items, title, fragment, discount_total,
        show_cost, costs, order_margin, multi, ship_barcodes, ship_barcode_n, typed_form },
    )?
    .into_response())
}

/// Себестоимость и маржа позиций: `(позиция → показ, «Маржа заказа»)`.
async fn line_costs(
    db: &sqlx::PgPool,
    order: &repo::OrderView,
) -> AppResult<(HashMap<String, minijinja::Value>, Option<String>)> {
    let list = stock::cost::order_costs(&mut *db.acquire().await?, order.id).await?;
    let mut costs = HashMap::new();
    let mut lines = Vec::new();
    for item in &order.items {
        let unit = list
            .iter()
            .find(|c| c.cart_item_id == item.id)
            .map(|c| (c.unit_cost, c.kind.clone()));
        lines.push((item.quantity, item.price, unit.as_ref().and_then(|u| u.0)));
        let Some((unit_cost, kind)) = unit else {
            continue;
        };
        let note = match kind {
            stock::cost::CostKind::Actual => "",
            stock::cost::CostKind::Estimate => "оценка",
            stock::cost::CostKind::ByPurchasePrice => "по закупочной цене",
        };
        let cost = unit_cost.map(|u| round2(u).to_string());
        let margin = unit_cost.map(|u| round2((item.price - u) * item.quantity).to_string());
        costs.insert(item.id.to_string(), context! { cost, margin, note });
    }
    let order_margin = stock::cost::margin(&lines).map(|m| match m.percent {
        Some(p) => format!("{} ({p}%)", m.sum),
        None => m.sum.to_string(),
    });
    Ok((costs, order_margin))
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
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

fn send(state: &AppState, event: &MailEvent, order_id: i64) {
    let state = state.clone();
    let event = event.event_name();
    tokio::spawn(async move { mail::notify(&state, &event, order_id).await });
}

/// 303 на вкладку `tab` карточки с уведомлением `text`.
pub(crate) fn back(id: i64, tab: &str, text: &str) -> Response {
    flash::redirect(&tab_url(id, tab), text, FlashKind::Ok)
}

/// Запрос из панели быстрого просмотра: HTMX с целью `#peek`.
fn is_peek(headers: &HeaderMap) -> bool {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    header("hx-request") == Some("true") && header("hx-target") == Some("peek")
}

/// Панель быстрого просмотра заказа (фрагмент); `toast` — уведомление в `HX-Trigger`.
/// `list_changed` просит страницу перезапросить список (метка `data-list-changed`).
async fn peek_panel(
    state: &AppState,
    id: i64,
    toast: Option<(&str, FlashKind)>,
    list_changed: bool,
) -> AppResult<Response> {
    let order = repo::load(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let statuses: Vec<(String, String)> =
        sqlx::query_as("SELECT code, name FROM order_statuses ORDER BY sort, code")
            .fetch_all(&state.db)
            .await?;
    let shipped: bool = sqlx::query_scalar(
        "SELECT COALESCE((SELECT deducted FROM shipments WHERE order_id = $1 ORDER BY id LIMIT 1), FALSE)",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    let prop = |pick: fn(&repo::OrderPropertyValue) -> bool| {
        order
            .properties
            .iter()
            .find(|p| pick(p) && !p.value.trim().is_empty())
            .map(|p| p.value.clone())
    };
    let phone = prop(|p| p.is_phone).unwrap_or_default();
    let email = prop(|p| p.is_email).unwrap_or_else(|| order.user_email.clone());
    // Покупатель — по тому же правилу, что в списке (свойство-плательщик, иначе логин)
    let mut qb = QueryBuilder::<Postgres>::new(format!(
        "SELECT {BUYER_SQL} FROM orders o LEFT JOIN users u ON u.id = o.user_id WHERE o.id = "
    ));
    qb.push_bind(id);
    let buyer: String = qb
        .build_query_scalar()
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let html = render(
        state,
        "shop/order_peek.html",
        context! { order, statuses, shipped, buyer, phone, email, list_changed },
    )?;
    Ok(match toast {
        Some((text, kind)) => (flash::toast(text, kind), html).into_response(),
        None => html.into_response(),
    })
}

/// Быстрый просмотр заказа: `GET /admin/shop/orders/{id}/peek`.
pub async fn peek(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    require_orders(&user)?;
    peek_panel(&state, id, None, false).await
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
    headers: HeaderMap,
    Form(form): Form<StatusForm>,
) -> AppResult<Response> {
    require_orders(&user)?;
    let (actor_id, actor_login) = (user.user.id, user.user.login.clone());
    let before: Option<String> = sqlx::query_scalar("SELECT status FROM orders WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    let from_peek = is_peek(&headers);
    match actions::change_status(
        &state.db,
        id,
        &form.status,
        Some(user.user.id),
        form.comment.trim(),
    )
    .await
    {
        Ok(Some(event)) => send(&state, &event, id),
        // Заказа нет — как раньше: без письма
        Ok(None) | Err(ActionError::NotFound) => {}
        Err(ActionError::Rejected(message)) => {
            if from_peek {
                let toast = Some((message.as_str(), FlashKind::Error));
                return peek_panel(&state, id, toast, false).await;
            }
            return card(&state, user, id, "", Some(message), None, false).await;
        }
        Err(ActionError::Db(e)) => return Err(e.into()),
    }
    let after: Option<String> = sqlx::query_scalar("SELECT status FROM orders WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    if after.as_deref() == Some(form.status.as_str()) && before.as_deref() != after.as_deref() {
        crate::audit::record(
            &state.db,
            actor_id,
            &actor_login,
            "status",
            "order",
            id,
            format!("статус → {}", form.status),
        )
        .await;
    }
    if from_peek {
        return peek_panel(&state, id, Some(("Статус изменён", FlashKind::Ok)), true).await;
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
    headers: HeaderMap,
    Form(form): Form<FlagForm>,
) -> AppResult<Response> {
    require_orders(&user)?;
    let (actor_id, actor_login) = (user.user.id, user.user.login.clone());
    let before: Option<bool> = sqlx::query_scalar("SELECT paid FROM orders WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    let paid = form.value == "Y";
    match actions::change_paid(&state.db, id, paid).await {
        Ok(Some(event)) => send(&state, &event, id),
        Ok(None) | Err(ActionError::NotFound) => {}
        Err(ActionError::Rejected(message)) => {
            return card(&state, user, id, "", Some(message), None, false).await;
        }
        Err(ActionError::Db(e)) => return Err(e.into()),
    }
    let after: Option<bool> = sqlx::query_scalar("SELECT paid FROM orders WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    if after == Some(paid) && before != after {
        crate::audit::record(
            &state.db,
            actor_id,
            &actor_login,
            "paid",
            "order",
            id,
            if paid {
                "оплачен"
            } else {
                "оплата снята"
            },
        )
        .await;
    }
    if is_peek(&headers) {
        return peek_panel(&state, id, Some(("Оплата сохранена", FlashKind::Ok)), true).await;
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
    let (actor_id, actor_login) = (user.user.id, user.user.login.clone());
    let before: Option<bool> = sqlx::query_scalar("SELECT canceled FROM orders WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    let cancel = form.value == "Y";
    match actions::change_canceled(&state.db, id, cancel, form.reason.trim()).await {
        Ok(event) => {
            if let Some(event) = event {
                send(&state, &event, id);
            }
            let after: Option<bool> =
                sqlx::query_scalar("SELECT canceled FROM orders WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&state.db)
                    .await?;
            if after == Some(cancel) && before != after {
                crate::audit::record(
                    &state.db,
                    actor_id,
                    &actor_login,
                    "cancel",
                    "order",
                    id,
                    if cancel {
                        format!("отмена: {}", form.reason.trim())
                    } else {
                        "отмена снята".to_string()
                    },
                )
                .await;
            }
            Ok(back(id, "", "Отмена заказа сохранена"))
        }
        Err(ActionError::NotFound) => Ok(back(id, "", "Отмена заказа сохранена")),
        // Отгруженный заказ не отменяется — ошибка в карточке
        Err(ActionError::Rejected(message)) => {
            card(&state, user, id, "", Some(message), None, false).await
        }
        Err(ActionError::Db(e)) => Err(e.into()),
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

/// Штрихкоды позиций из формы отгрузки: `barcodes_<позиция>_<склад>` = коды по строкам.
fn ship_barcodes(form: &HashMap<String, String>) -> Vec<(i64, i64, Vec<String>)> {
    let mut out: Vec<(i64, i64, Vec<String>)> = form
        .iter()
        .filter_map(|(k, v)| {
            let (line, store) = k.strip_prefix("barcodes_")?.split_once('_')?;
            let codes = v.lines().map(|c| c.trim().to_string()).collect();
            Some((line.parse().ok()?, store.parse().ok()?, codes))
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
        stock::barcodes::choose_barcodes(&mut tx, shipment, &ship_barcodes(&form)).await?;
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
            card_typed(
                &state,
                user,
                id,
                "shipment",
                Some(m.join("; ")),
                None,
                false,
                Some(&form),
            )
            .await
        }
        Err(StockError::Db(e)) => Err(e.into()),
    }
}

/// Массовое действие над заказом (реестр действий — `list::bulk`).
pub(crate) async fn bulk_run(
    ctx: &crate::admin::list::bulk::BulkCtx<'_>,
    action: &str,
    param: &str,
    id: i64,
) -> Result<(), String> {
    match apply(ctx, action, param, id).await {
        Ok(event) => {
            if let Some(event) = mails_to_send(event, ctx.notify) {
                ctx.mails.push(event.event_name(), id);
            }
            Ok(())
        }
        Err(ActionError::Rejected(text)) => Err(text),
        Err(ActionError::NotFound) => Err("заказ не найден".into()),
        Err(ActionError::Db(e)) => {
            tracing::error!("массовое действие над заказом {id}: {e}");
            Err("ошибка сервера".into())
        }
    }
}

/// Изменение заказа без отправки письма: какое письмо положено, решает вызывающий.
pub(crate) async fn apply(
    ctx: &crate::admin::list::bulk::BulkCtx<'_>,
    action: &str,
    param: &str,
    id: i64,
) -> Result<Option<MailEvent>, ActionError> {
    if require_orders(ctx.user).is_err() {
        return Err(ActionError::Rejected("нет права".into()));
    }
    let db = &ctx.state.db;
    match action {
        "status" => actions::change_status(db, id, param, Some(ctx.user.user.id), "").await,
        "paid" => actions::change_paid(db, id, true).await,
        "unpaid" => actions::change_paid(db, id, false).await,
        "cancel" => actions::change_canceled(db, id, true, param).await,
        "uncancel" => actions::change_canceled(db, id, false, "").await,
        _ => Err(ActionError::Rejected("неизвестное действие".into())),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{StatusCode, header};

    use crate::smoke_tests::get;
    use crate::test_support::{
        admin_cookie, get_raw, order_fixture, permitted_cookie, place_order, test_state,
    };

    use crate::admin::list::bulk::BulkCtx;
    use crate::sale::actions::{ActionError, MailEvent};
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    async fn bulk_post(app: &axum::Router, cookie: &str, body: &str) -> (StatusCode, String) {
        let (status, _, text) = bulk_post_h(app, cookie, body).await;
        (status, text)
    }

    async fn bulk_post_h(
        app: &axum::Router,
        cookie: &str,
        body: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
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
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    fn orders_form(action: &str, param: &str, ids: &[i64]) -> String {
        let ids: Vec<String> = ids.iter().map(i64::to_string).collect();
        format!(
            "list_key=orders&path=%2Fadmin%2Fshop%2Forders&query=&action={action}&param={param}&scope=ids&ids={}",
            ids.join(",")
        )
    }

    async fn add_statuses(db: &sqlx::PgPool) {
        sqlx::query(
            "INSERT INTO order_statuses (code, name, notify) VALUES ('P', 'Оплачен', TRUE)",
        )
        .execute(db)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn bulk_status_change(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        add_statuses(&db).await;
        let ids = [
            place_order(&db, &f, None).await,
            place_order(&db, &f, None).await,
            place_order(&db, &f, None).await,
        ];
        let cookie = admin_cookie(&db).await;
        let app = crate::app(test_state(db.clone()));
        let (st, h, body) = bulk_post_h(&app, &cookie, &orders_form("status", "P", &ids)).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(!body.contains("bulk-summary"), "{body}");
        let toast: serde_json::Value =
            serde_json::from_str(h["hx-trigger"].to_str().unwrap()).unwrap();
        assert_eq!(toast["toast"]["text"], "Готово: 3");
        let actor: i64 = sqlx::query_scalar("SELECT id FROM users WHERE login = 'smoke'")
            .fetch_one(&db)
            .await
            .unwrap();
        for id in ids {
            let who: Vec<Option<i64>> = sqlx::query_scalar(
                "SELECT user_id FROM order_status_history WHERE order_id = $1 AND status = 'P'",
            )
            .bind(id)
            .fetch_all(&db)
            .await
            .unwrap();
            assert_eq!(who, vec![Some(actor)], "заказ {id}");
        }
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM orders WHERE status = 'P'")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 3);
        // статус не из справочника — 422
        let (st, _) = bulk_post(&app, &cookie, &orders_form("status", "nope", &ids)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test]
    async fn bulk_cancel_skips_shipped(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let shipped = place_order(&db, &f, None).await;
        let a = place_order(&db, &f, None).await;
        let b = place_order(&db, &f, None).await;
        sqlx::query("UPDATE shipments SET deducted = TRUE WHERE order_id = $1")
            .bind(shipped)
            .execute(&db)
            .await
            .unwrap();
        let cookie = admin_cookie(&db).await;
        let app = crate::app(test_state(db.clone()));
        let (st, body) = bulk_post(
            &app,
            &cookie,
            &orders_form("cancel", "ошибка", &[shipped, a, b]),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(body.contains("bulk-summary"), "{body}");
        assert!(body.contains("Некоторые товары уже отгружены"), "{body}");
        let canceled: Vec<(i64, bool)> =
            sqlx::query_as("SELECT id, canceled FROM orders ORDER BY id")
                .fetch_all(&db)
                .await
                .unwrap();
        for (id, c) in canceled {
            assert_eq!(c, id != shipped, "заказ {id}");
        }
    }

    #[sqlx::test]
    async fn bulk_repeat_is_harmless(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let state = test_state(db.clone());
        let user = crate::access::Access::load(
            &db,
            crate::auth::create_user(&db, "boss", "password123".into(), true)
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        let ctx = BulkCtx {
            state: &state,
            user: &user,
            notify: true,
            collection_id: None,
            collection: Default::default(),
            mails: Default::default(),
        };
        assert!(matches!(
            super::apply(&ctx, "cancel", "", id).await,
            Ok(Some(MailEvent::Canceled))
        ));
        assert!(matches!(
            super::apply(&ctx, "cancel", "", id).await,
            Ok(None)
        ));
        assert!(matches!(
            super::apply(&ctx, "paid", "", id).await,
            Ok(Some(MailEvent::Paid))
        ));
        assert!(matches!(super::apply(&ctx, "paid", "", id).await, Ok(None)));
        assert_eq!(super::bulk_run(&ctx, "paid", "", id).await, Ok(()));
        assert_eq!(super::bulk_run(&ctx, "cancel", "", id).await, Ok(()));
        assert_eq!(super::bulk_run(&ctx, "uncancel", "", id).await, Ok(()));
        assert_eq!(super::bulk_run(&ctx, "unpaid", "", id).await, Ok(()));
        // без «Уведомить» письма нет даже при изменении
        let quiet = BulkCtx {
            state: &state,
            user: &user,
            notify: false,
            collection_id: None,
            collection: Default::default(),
            mails: Default::default(),
        };
        assert!(matches!(
            super::apply(&quiet, "paid", "", id).await,
            Ok(Some(MailEvent::Paid))
        ));
        assert!(crate::admin::list::bulk::mails_to_send(Some(MailEvent::Paid), false).is_none());
        assert!(matches!(
            super::apply(&quiet, "bogus", "", id).await,
            Err(ActionError::Rejected(_))
        ));
        assert_eq!(
            super::bulk_run(&quiet, "paid", "", id + 999).await,
            Err("заказ не найден".to_string())
        );
    }

    #[sqlx::test]
    async fn bulk_run_queues_mails_for_one_batch(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        add_statuses(&db).await;
        let ids = [
            place_order(&db, &f, None).await,
            place_order(&db, &f, None).await,
            place_order(&db, &f, None).await,
        ];
        let state = test_state(db.clone());
        let user = crate::access::Access::load(
            &db,
            crate::auth::create_user(&db, "boss", "password123".into(), true)
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        let mk = |notify| BulkCtx {
            state: &state,
            user: &user,
            notify,
            collection_id: None,
            collection: Default::default(),
            mails: Default::default(),
        };
        let ctx = mk(true);
        for id in ids {
            assert_eq!(super::bulk_run(&ctx, "status", "P", id).await, Ok(()));
        }
        // ничего не отправлено по ходу цикла: все письма ждут в очереди, по порядку
        let expected: Vec<(String, i64)> = ids
            .iter()
            .map(|id| ("SALE_STATUS_CHANGED_P".to_string(), *id))
            .collect();
        assert_eq!(ctx.mails.take(), expected);
        // без «Уведомить» очередь пуста
        let quiet = mk(false);
        assert_eq!(super::bulk_run(&quiet, "paid", "", ids[0]).await, Ok(()));
        assert!(quiet.mails.take().is_empty());
    }

    #[sqlx::test]
    async fn bulk_reason_too_long_422(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let cookie = admin_cookie(&db).await;
        let app = crate::app(test_state(db.clone()));
        let long = "а".repeat(501);
        let (st, _) = bulk_post(&app, &cookie, &orders_form("cancel", &long, &[id])).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let ok = "а".repeat(500);
        let (st, _) = bulk_post(&app, &cookie, &orders_form("cancel", &ok, &[id])).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[sqlx::test]
    async fn bulk_orders_without_permission_403(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let cookie = permitted_cookie(&db, "nobody", &[]).await;
        let app = crate::app(test_state(db.clone()));
        let (st, _) = bulk_post(&app, &cookie, &orders_form("paid", "", &[id])).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let paid: bool = sqlx::query_scalar("SELECT paid FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(!paid);
    }

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

    // ---- Штрихкоды отгрузки, себестоимость и маржа ----

    use crate::stock::batches::{
        Method,
        db_tests::{arrival, setup},
    };

    async fn multi_order(db: &sqlx::PgPool, codes: &[&str], qty: f64) -> (i64, i64, i64) {
        let f = setup(db, Some(Method::Fifo)).await;
        sqlx::query("UPDATE catalog_products SET barcode_multi = TRUE WHERE item_id = $1")
            .bind(f.product_id)
            .execute(db)
            .await
            .unwrap();
        for c in codes {
            sqlx::query(
                "INSERT INTO catalog_barcodes (item_id, barcode, store_id) VALUES ($1, $2, 1)",
            )
            .bind(f.product_id)
            .bind(c)
            .execute(db)
            .await
            .unwrap();
        }
        let order = crate::test_support::place_order_with(db, &f, None, Some(1), qty).await;
        let line: i64 =
            sqlx::query_scalar("SELECT id FROM cart_items WHERE order_id = $1 AND item_id = $2")
                .bind(order)
                .bind(f.product_id)
                .fetch_one(db)
                .await
                .unwrap();
        (f.product_id, order, line)
    }

    #[sqlx::test]
    async fn shipment_saves_barcodes_and_ships(db: sqlx::PgPool) {
        let (_, order, line) = multi_order(&db, &["A1", "A2"], 2.0).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s, _, html) = crate::test_support::get_raw(
            &app,
            &format!("/admin/shop/orders/{order}/shipment"),
            &cookie,
        )
        .await;
        let html = String::from_utf8_lossy(&html).into_owned();
        assert!(s.is_success());
        assert!(
            html.contains(&format!("name=\"barcodes_{line}_1\"")),
            "{html}"
        );
        assert!(html.contains("0/2"), "{html}");
        let (s, _, _) = crate::test_support::post_form_raw(
            &app,
            &format!("/admin/shop/orders/{order}/shipment"),
            &cookie,
            &format!("deducted=Y&store_{line}=1&barcodes_{line}_1=A1%0D%0AA2"),
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let deducted: bool =
            sqlx::query_scalar("SELECT deducted FROM shipments WHERE order_id = $1")
                .bind(order)
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(deducted);
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM shipment_item_barcodes")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 2);
    }

    #[sqlx::test]
    async fn shipment_barcode_error_shown(db: sqlx::PgPool) {
        let (_, order, line) = multi_order(&db, &["A1"], 2.0).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s, _, html) = crate::test_support::post_form_raw(
            &app,
            &format!("/admin/shop/orders/{order}/shipment"),
            &cookie,
            &format!("deducted=Y&store_{line}=1&barcodes_{line}_1=A1%0AZZ%3Cb%3E"),
        )
        .await;
        assert!(s.is_success(), "{s}");
        assert!(html.contains("не найден штрихкод"), "{html}");
        assert!(html.contains("ZZ&lt;b&gt;"), "{html}");
        assert!(!html.contains("ZZ<b>"), "{html}");
        let deducted: bool =
            sqlx::query_scalar("SELECT deducted FROM shipments WHERE order_id = $1")
                .bind(order)
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(!deducted);
    }

    #[sqlx::test]
    async fn shipment_error_keeps_typed_codes(db: sqlx::PgPool) {
        let (_, order, line) = multi_order(&db, &["A1"], 2.0).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s, _, html) = crate::test_support::post_form_raw(
            &app,
            &format!("/admin/shop/orders/{order}/shipment"),
            &cookie,
            &format!(
                "deducted=Y&allow_delivery=Y&tracking_number=TR-9&store_{line}=1&barcodes_{line}_1=A1%0D%0AZZ"
            ),
        )
        .await;
        assert!(s.is_success(), "{s}");
        assert!(html.contains("не найден штрихкод"), "{html}");
        assert!(
            html.contains(&format!("name=\"barcodes_{line}_1\"")),
            "{html}"
        );
        assert!(html.contains(">A1\nZZ</textarea>"), "{html}");
        assert!(html.contains("2/2"), "{html}");
        assert!(html.contains("<option value=\"1\" selected>"), "{html}");
        assert!(html.contains("value=\"TR-9\""), "{html}");
        assert!(
            html.contains("name=\"deducted\" value=\"Y\" checked"),
            "{html}"
        );
        assert!(
            html.contains("name=\"allow_delivery\" value=\"Y\" checked"),
            "{html}"
        );
    }

    async fn costed_order(db: &sqlx::PgPool) -> i64 {
        let f = setup(db, Some(Method::Fifo)).await;
        arrival(db, f.product_id, 1, 5.0, 60.0).await;
        crate::test_support::place_order_with(db, &f, None, Some(1), 2.0).await
    }

    #[sqlx::test]
    async fn order_items_show_cost_and_margin(db: sqlx::PgPool) {
        let order = costed_order(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (_, _, html) = crate::test_support::get_raw(
            &app,
            &format!("/admin/shop/orders/{order}/items"),
            &cookie,
        )
        .await;
        let html = String::from_utf8_lossy(&html).into_owned();
        for want in [
            "Себестоимость",
            "Маржа",
            "оценка",
            "60",
            "Маржа заказа: 80 (40%)",
        ] {
            assert!(html.contains(want), "{want}: {html}");
        }
    }

    #[sqlx::test]
    async fn order_cost_hidden_without_right(db: sqlx::PgPool) {
        let order = costed_order(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = permitted_cookie(&db, "mgr", &["orders.manage"]).await;
        let (s, _, html) = crate::test_support::get_raw(
            &app,
            &format!("/admin/shop/orders/{order}/items"),
            &cookie,
        )
        .await;
        let html = String::from_utf8_lossy(&html).into_owned();
        assert!(s.is_success());
        for no in ["Себестоимость", "Маржа", "оценка"] {
            assert!(!html.contains(no), "{no}: {html}");
        }
    }
    // ---------- Быстрый просмотр ----------

    use crate::test_support::{htmx_get, post_form_raw};

    fn app(db: &sqlx::PgPool) -> axum::Router {
        crate::app(test_state(db.clone()))
    }

    /// POST формы как HTMX-запрос из панели (`HX-Target: peek`): статус, заголовки, тело.
    async fn peek_post(
        app: &axum::Router,
        path: &str,
        cookie: &str,
        body: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        use axum::http::header;
        use tower::ServiceExt;
        let req = axum::http::Request::post(path)
            .header(header::COOKIE, cookie)
            .header(header::HOST, "127.0.0.1:3000")
            .header(header::ORIGIN, "http://127.0.0.1:3000")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("HX-Target", "peek")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
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
    async fn order_peek_fragment(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        // ещё шесть позиций: всего 7, в панели 5 и «ещё 2»
        sqlx::query(
            "INSERT INTO cart_items (buyer_id, item_id, store_id, quantity, name, price, order_id)
             SELECT buyer_id, item_id, NULL, 1, 'Позиция ' || g, 100, $1
             FROM cart_items, generate_series(2, 7) g WHERE order_id = $1",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        let cookie = admin_cookie(&db).await;
        let (status, html) = htmx_get(
            &app(&db),
            &format!("/admin/shop/orders/{id}/peek"),
            &cookie,
            "peek",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let number: String = sqlx::query_scalar(
            "SELECT COALESCE(account_number, id::text) FROM orders WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(html.contains(&format!("Заказ № {number}")), "номер: {html}");
        assert!(html.contains("100"), "сумма");
        assert!(html.contains("ещё 2"), "«ещё N»: {html}");
        assert!(
            html.contains("Позиция 5") && !html.contains("Позиция 6"),
            "до 5 позиций"
        );
        assert!(
            html.contains("Открыть карточку") && html.contains(&format!("orders&#x2f;{id}\"")),
            "ссылка на карточку: {html}"
        );
        assert!(!html.contains("<html"), "это фрагмент");
        assert!(html.contains("Принят"), "статус");
    }

    #[sqlx::test]
    async fn order_peek_forbidden_without_right(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let cookie = permitted_cookie(&db, "nobody", &[]).await;
        let (status, _) = get(&app(&db), &format!("/admin/shop/orders/{id}/peek"), &cookie).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = get(
            &app(&db),
            "/admin/shop/orders/999999/peek",
            &admin_cookie(&db).await,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn peek_status_returns_panel_and_marker(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        sqlx::query("INSERT INTO order_statuses (code, name) VALUES ('F', 'Выполнен')")
            .execute(&db)
            .await
            .unwrap();
        let id = place_order(&db, &f, None).await;
        let cookie = admin_cookie(&db).await;
        let (status, _, html) = peek_post(
            &app(&db),
            &format!("/admin/shop/orders/{id}/status"),
            &cookie,
            "status=F",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("peek-head"), "панель: {html}");
        assert!(
            html.contains("data-list-changed"),
            "метка обновления: {html}"
        );
        assert!(html.contains("Выполнен"));
        let st: String = sqlx::query_scalar("SELECT status FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(st, "F");
    }

    #[sqlx::test]
    async fn peek_paid_returns_panel_and_marker(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let cookie = admin_cookie(&db).await;
        let (status, _, html) = peek_post(
            &app(&db),
            &format!("/admin/shop/orders/{id}/paid"),
            &cookie,
            "value=Y",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("peek-head"), "панель: {html}");
        assert!(
            html.contains("data-list-changed"),
            "метка обновления: {html}"
        );
        assert!(html.contains("оплачен"));
        let paid: bool = sqlx::query_scalar("SELECT paid FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(paid);
    }

    #[sqlx::test]
    async fn peek_unknown_status_shows_error_toast(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let cookie = admin_cookie(&db).await;
        let (status, headers, html) = peek_post(
            &app(&db),
            &format!("/admin/shop/orders/{id}/status"),
            &cookie,
            "status=ZZ",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("peek-head"));
        let trigger = headers.get("hx-trigger").unwrap().to_str().unwrap();
        assert!(trigger.contains("\"kind\":\"error\""), "{trigger}");
    }

    #[sqlx::test]
    async fn order_peek_escapes_item_name(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        sqlx::query("UPDATE cart_items SET name = '<script>x</script>' WHERE order_id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let cookie = admin_cookie(&db).await;
        let (_, html) = htmx_get(
            &app(&db),
            &format!("/admin/shop/orders/{id}/peek"),
            &cookie,
            "peek",
        )
        .await;
        assert!(!html.contains("<script>x"), "{html}");
        assert!(html.contains("&lt;script&gt;x"), "{html}");
    }

    #[sqlx::test]
    async fn status_without_peek_unchanged(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        sqlx::query("INSERT INTO order_statuses (code, name) VALUES ('F', 'Выполнен')")
            .execute(&db)
            .await
            .unwrap();
        let id = place_order(&db, &f, None).await;
        let cookie = admin_cookie(&db).await;
        let (status, headers, _) = post_form_raw(
            &app(&db),
            &format!("/admin/shop/orders/{id}/status"),
            &cookie,
            "status=F",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(
            headers[axum::http::header::LOCATION],
            format!("/admin/shop/orders/{id}").as_str()
        );
        let (status, headers, _) = post_form_raw(
            &app(&db),
            &format!("/admin/shop/orders/{id}/paid"),
            &cookie,
            "value=Y",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(headers.get("hx-trigger").is_none());
    }

    #[sqlx::test]
    async fn row_has_peek_attr_and_links_stay(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let cookie = admin_cookie(&db).await;
        let (status, html) = get(&app(&db), "/admin/shop/orders", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(&format!("<tr data-id=\"{id}\"")), "{html}");
        assert!(
            html.contains(&format!(
                "data-peek=\"&#x2f;admin&#x2f;shop&#x2f;orders&#x2f;{id}&#x2f;peek\""
            )),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                "<a href=\"&#x2f;admin&#x2f;shop&#x2f;orders&#x2f;{id}\">"
            )),
            "{html}"
        );
        assert!(html.contains("id=\"peek\""), "контейнер панели в оболочке");
    }

    #[sqlx::test]
    async fn orders_full_page_and_fragment(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        place_order(&db, &f, None).await;
        let cookie = admin_cookie(&db).await;
        let (status, html) = get(&app(&db), "/admin/shop/orders", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("id=\"list-body\""), "{html}");
        assert!(html.contains("<aside class=\"sidebar\""), "меню оболочки");
        assert!(html.contains("id=\"list-spec\""));
        let (status, frag) = htmx_get(&app(&db), "/admin/shop/orders", &cookie, "list-body").await;
        assert_eq!(status, StatusCode::OK);
        assert!(frag.contains("<table"), "{frag}");
        assert!(frag.contains("100,00 RUB"), "{frag}");
        assert!(!frag.contains("<aside class=\"sidebar\""), "{frag}");
    }

    #[sqlx::test]
    async fn orders_fragment_updates_count_oob(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        place_order(&db, &f, None).await;
        let cookie = admin_cookie(&db).await;
        let (_, full) = get(&app(&db), "/admin/shop/orders", &cookie).await;
        assert!(full.contains("id=\"list-count\""), "{full}");
        assert!(
            !full.contains("hx-swap-oob=\"true\" id=\"list-count\""),
            "в полной странице счётчик не oob"
        );
        let (_, all) = htmx_get(&app(&db), "/admin/shop/orders", &cookie, "list-body").await;
        assert!(
            all.contains("id=\"list-count\" hx-swap-oob=\"true\">1<"),
            "{all}"
        );
        let (_, none) = htmx_get(
            &app(&db),
            "/admin/shop/orders?f.paid=eq:Y",
            &cookie,
            "list-body",
        )
        .await;
        assert!(
            none.contains("id=\"list-count\" hx-swap-oob=\"true\">0<"),
            "{none}"
        );
    }

    #[sqlx::test]
    async fn orders_normalizes_url(db: sqlx::PgPool) {
        let cookie = admin_cookie(&db).await;
        let (status, headers, _) =
            get_raw(&app(&db), "/admin/shop/orders?f.zzz=eq:1", &cookie).await;
        assert_eq!(status, StatusCode::FOUND);
        assert_eq!(headers[header::LOCATION], "/admin/shop/orders");
        use tower::ServiceExt;
        let req = axum::http::Request::get("/admin/shop/orders?f.zzz=eq:1")
            .header(header::COOKIE, &cookie)
            .header("HX-Request", "true")
            .header("HX-Target", "list-body")
            .body(axum::body::Body::empty())
            .unwrap();
        let res = app(&db).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["hx-replace-url"], "/admin/shop/orders");
    }

    #[sqlx::test]
    async fn orders_page_past_end(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        place_order(&db, &f, None).await;
        let cookie = admin_cookie(&db).await;
        let (status, html) = htmx_get(
            &app(&db),
            "/admin/shop/orders?page=999",
            &cookie,
            "list-body",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("<table"),
            "последняя страница с данными: {html}"
        );
    }

    #[sqlx::test]
    async fn orders_readonly_403(db: sqlx::PgPool) {
        let cookie = permitted_cookie(&db, "nobody", &[]).await;
        let (status, _) = get(&app(&db), "/admin/shop/orders", &cookie).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn orders_filter_chip_and_sort(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        place_order(&db, &f, None).await;
        let cookie = admin_cookie(&db).await;
        let (_, html) = htmx_get(
            &app(&db),
            "/admin/shop/orders?f.status=in:N&sort=number",
            &cookie,
            "list-body",
        )
        .await;
        assert!(html.contains("class=\"chip\""), "{html}");
        assert!(html.contains("Принят"), "подпись варианта: {html}");
        assert!(html.contains("Сбросить всё"));
        let (_, none) = htmx_get(
            &app(&db),
            "/admin/shop/orders?f.number=eq:zzz",
            &cookie,
            "list-body",
        )
        .await;
        assert!(none.contains("Ничего не найдено"), "{none}");
    }

    #[sqlx::test]
    async fn peek_change_triggers_refresh(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        sqlx::query("INSERT INTO order_statuses (code, name) VALUES ('F', 'Выполнен')")
            .execute(&db)
            .await
            .unwrap();
        let id = place_order(&db, &f, None).await;
        let cookie = admin_cookie(&db).await;
        let (status, _, html) = peek_post(
            &app(&db),
            &format!("/admin/shop/orders/{id}/status"),
            &cookie,
            "status=F",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("data-list-changed"), "{html}");
        // GET панели без изменения метки не несёт
        let (_, html) = htmx_get(
            &app(&db),
            &format!("/admin/shop/orders/{id}/peek"),
            &cookie,
            "peek",
        )
        .await;
        assert!(!html.contains("data-list-changed"));
    }

    #[sqlx::test]
    async fn orders_page_past_end_redirects_to_clamped(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        place_order(&db, &f, None).await;
        let cookie = admin_cookie(&db).await;
        let (status, headers, _) = get_raw(&app(&db), "/admin/shop/orders?page=999", &cookie).await;
        assert_eq!(status, StatusCode::FOUND);
        assert_eq!(headers[header::LOCATION], "/admin/shop/orders");
        use tower::ServiceExt;
        let req = axum::http::Request::get("/admin/shop/orders?page=999")
            .header(header::COOKIE, &cookie)
            .header("HX-Request", "true")
            .header("HX-Target", "list-body")
            .body(axum::body::Body::empty())
            .unwrap();
        let res = app(&db).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["hx-replace-url"], "/admin/shop/orders");
    }

    #[sqlx::test]
    async fn orders_cyrillic_filter_location_roundtrips(db: sqlx::PgPool) {
        let cookie = admin_cookie(&db).await;
        let (status, headers, _) = get_raw(
            &app(&db),
            "/admin/shop/orders?f.buyer=has:%D0%98%D0%B2%D0%B0%D0%BD&zzz=1&sort=-created_at",
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::FOUND);
        let loc = headers[header::LOCATION].to_str().unwrap().to_string();
        assert!(loc.is_ascii(), "{loc}");
        assert_eq!(
            loc,
            "/admin/shop/orders?f.buyer=has%3A%D0%98%D0%B2%D0%B0%D0%BD"
        );
        let (status, _, html) = get_raw(&app(&db), &loc, &cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&html).contains("Иван"));
    }

    #[sqlx::test]
    async fn orders_canceled_row_is_muted(db: sqlx::PgPool) {
        let f = order_fixture(&db).await;
        let ok = place_order(&db, &f, None).await;
        let canceled = place_order(&db, &f, None).await;
        sqlx::query("UPDATE orders SET canceled = TRUE WHERE id = $1")
            .bind(canceled)
            .execute(&db)
            .await
            .unwrap();
        let cookie = admin_cookie(&db).await;
        let (_, html) = htmx_get(&app(&db), "/admin/shop/orders", &cookie, "list-body").await;
        assert!(
            html.contains(&format!("<tr data-id=\"{canceled}\" class=\"inactive\"")),
            "{html}"
        );
        assert!(
            html.contains(&format!("<tr data-id=\"{ok}\" data-open")),
            "{html}"
        );
    }
}
