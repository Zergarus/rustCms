//! Заказы: список с фильтрами, карточка, смена статуса, оплата, отмена, свойства.

use std::collections::HashMap;

use axum::{
    Extension,
    extract::{Multipart, Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::Form;
use chrono::{Days, NaiveDate, TimeZone, Utc};
use minijinja::context;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::{FromRow, Postgres, QueryBuilder};

use crate::{
    access::{Access, ORDERS_MANAGE},
    admin::{read_upload_form, render},
    error::{AppError, AppResult},
    files,
    sale::{self, OrderProperty, mail, repo, validate::validate_properties},
    state::AppState,
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
        qb.push(
            " AND concat_ws(' ', u.login, u.email, u.name, u.last_name,
                 (SELECT string_agg(v.value, ' ') FROM order_property_values v WHERE v.order_id = o.id)) ILIKE ",
        )
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

pub(crate) async fn card(
    state: &AppState,
    user: Access,
    id: i64,
    error: Option<String>,
    notice: Option<String>,
) -> AppResult<Html<String>> {
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
    let person_type: String = sqlx::query_scalar("SELECT name FROM person_types WHERE id = $1")
        .bind(order.person_type_id)
        .fetch_optional(&state.db)
        .await?
        .unwrap_or_default();
    render(
        state,
        "shop/order.html",
        context! { user, order, fields, file_fields, extra, stores, statuses, person_type, error, notice,
        store_list, deliveries, pay_systems, version },
    )
}

pub async fn view(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    require_orders(&user)?;
    card(&state, user, id, None, None).await
}

fn send(state: &AppState, event: String, order_id: i64) {
    let state = state.clone();
    tokio::spawn(async move { mail::notify(&state, &event, order_id).await });
}

fn back(id: i64) -> Response {
    Redirect::to(&format!("/admin/shop/orders/{id}")).into_response()
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
        return Ok(
            card(&state, user, id, Some("Нет такого статуса".into()), None)
                .await?
                .into_response(),
        );
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
    Ok(back(id))
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
    Ok(back(id))
}

pub async fn set_canceled(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<FlagForm>,
) -> AppResult<Response> {
    require_orders(&user)?;
    let cancel = form.value == "Y";
    if repo::set_canceled(&state.db, id, cancel, form.reason.trim()).await? && cancel {
        send(&state, "SALE_ORDER_CANCEL".into(), id);
    }
    Ok(back(id))
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
        Err(e) => {
            return Ok(card(&state, user, id, Some(e.message), None)
                .await?
                .into_response());
        }
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
    Ok(back(id))
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
        return Ok(card(&state, user, id, Some(form.rejected.join("; ")), None)
            .await?
            .into_response());
    }
    Ok(back(id))
}

#[derive(Deserialize)]
pub struct ShipmentForm {
    #[serde(default)]
    tracking_number: String,
    allow_delivery: Option<String>,
}

/// Трек-номер и разрешение доставки.
pub async fn save_shipment(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<ShipmentForm>,
) -> AppResult<Response> {
    require_orders(&user)?;
    if !repo::set_shipment(
        &state.db,
        id,
        &form.tracking_number,
        form.allow_delivery.is_some(),
    )
    .await?
    {
        return Err(AppError::NotFound);
    }
    Ok(back(id))
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
}
