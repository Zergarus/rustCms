//! Методы чтения API склада (`stock.read`).

use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool};

use super::{
    ApiAccess, Id, Page,
    error::{ApiError, ApiResult},
};
use crate::{
    access::{PURCHASE_INFO, SHOP_MANAGE},
    catalog::{TYPE_OFFER, TYPE_SIMPLE},
    state::AppState,
    stock::{self, barcodes},
};

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}

/// Список чисел через запятую; пустой или неверный элемент — 400.
fn parse_ids(raw: &str, name: &str) -> ApiResult<Vec<i64>> {
    raw.split(',')
        .map(|p| p.trim().parse::<i64>())
        .collect::<Result<_, _>>()
        .map_err(|_| ApiError::BadRequest(format!("Некорректный {name}")))
}

fn parse_date(raw: &str, name: &str) -> ApiResult<NaiveDate> {
    NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map_err(|_| ApiError::BadRequest(format!("Некорректная дата {name}, нужен YYYY-MM-DD")))
}

#[derive(Serialize, FromRow)]
struct StoreRow {
    id: i64,
    xml_id: String,
    name: String,
    address: String,
    active: bool,
    is_default: bool,
}

/// `GET /stores`
pub async fn stores(
    State(state): State<AppState>,
    api: ApiAccess,
    page: Page,
) -> ApiResult<Json<Value>> {
    api.require("stock.read", SHOP_MANAGE)?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_stores")
        .fetch_one(&state.db)
        .await?;
    let items: Vec<StoreRow> = sqlx::query_as(
        "SELECT id, xml_id, name, address, active, is_default FROM catalog_stores
         ORDER BY id LIMIT $1 OFFSET $2",
    )
    .bind(page.limit)
    .bind(page.offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({"items": items, "total": total})))
}

#[derive(Deserialize)]
pub struct RestsQuery {
    item_id: Option<String>,
    store_id: Option<String>,
}

#[derive(Serialize, FromRow)]
struct RestRow {
    item_id: i64,
    store_id: i64,
    amount: f64,
    quantity_reserved: f64,
}

/// `GET /rests?item_id=1,2&store_id=3`
pub async fn rests(
    State(state): State<AppState>,
    api: ApiAccess,
    page: Page,
    Query(q): Query<RestsQuery>,
) -> ApiResult<Json<Value>> {
    api.require("stock.read", SHOP_MANAGE)?;
    let items = q
        .item_id
        .as_deref()
        .map(|v| parse_ids(v, "item_id"))
        .transpose()?;
    let stores = q
        .store_id
        .as_deref()
        .map(|v| parse_ids(v, "store_id"))
        .transpose()?;
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM catalog_store_amounts
         WHERE ($1::bigint[] IS NULL OR item_id = ANY($1))
           AND ($2::bigint[] IS NULL OR store_id = ANY($2))",
    )
    .bind(&items)
    .bind(&stores)
    .fetch_one(&state.db)
    .await?;
    let rows: Vec<RestRow> = sqlx::query_as(
        "SELECT item_id, store_id, amount::float8 AS amount,
                quantity_reserved::float8 AS quantity_reserved
         FROM catalog_store_amounts
         WHERE ($1::bigint[] IS NULL OR item_id = ANY($1))
           AND ($2::bigint[] IS NULL OR store_id = ANY($2))
         ORDER BY item_id, store_id LIMIT $3 OFFSET $4",
    )
    .bind(&items)
    .bind(&stores)
    .bind(page.limit)
    .bind(page.offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({"items": rows, "total": total})))
}

#[derive(Deserialize)]
pub struct LookupQuery {
    barcode: Option<String>,
    xml_id: Option<String>,
    id: Option<String>,
}

#[derive(FromRow)]
struct ProductRow {
    id: i64,
    xml_id: String,
    name: String,
    product_type: i16,
    barcode_multi: bool,
    purchasing_price: Option<f64>,
    purchasing_currency: Option<String>,
}

#[derive(Serialize, FromRow)]
struct StoreAmount {
    store_id: i64,
    amount: f64,
    quantity_reserved: f64,
}

/// `GET /products/lookup?barcode=…|xml_id=…|id=…`
pub async fn lookup(
    State(state): State<AppState>,
    api: ApiAccess,
    Query(q): Query<LookupQuery>,
) -> ApiResult<Json<Value>> {
    api.require("stock.read", SHOP_MANAGE)?;
    let given = [q.barcode.is_some(), q.xml_id.is_some(), q.id.is_some()];
    if given.iter().filter(|g| **g).count() != 1 {
        return Err(bad("Нужен ровно один параметр: barcode, xml_id или id"));
    }
    let item_id: Option<i64> = if let Some(code) = &q.barcode {
        let mut conn = state.db.acquire().await.map_err(ApiError::from)?;
        barcodes::find_item(&mut conn, code).await?
    } else if let Some(xml) = &q.xml_id {
        sqlx::query_scalar(
            "SELECT min(i.id) FROM collection_items i
             JOIN catalog_products p ON p.item_id = i.id
             WHERE i.xml_id = $1 AND i.xml_id <> ''",
        )
        .bind(xml)
        .fetch_one(&state.db)
        .await?
    } else {
        let id: i64 =
            q.id.as_deref()
                .unwrap_or_default()
                .trim()
                .parse()
                .map_err(|_| bad("Некорректный id"))?;
        Some(id)
    };
    let item_id = item_id.ok_or(ApiError::NotFound)?;
    let product: ProductRow = sqlx::query_as(
        "SELECT i.id, COALESCE(i.xml_id, '') AS xml_id, i.name, p.type AS product_type,
                p.barcode_multi, p.purchasing_price::float8 AS purchasing_price,
                p.purchasing_currency
         FROM collection_items i JOIN catalog_products p ON p.item_id = i.id
         WHERE i.id = $1",
    )
    .bind(item_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::NotFound)?;
    let barcodes: Vec<String> = sqlx::query_scalar(
        "SELECT barcode FROM catalog_barcodes
         WHERE item_id = $1 AND store_id IS NULL AND order_id IS NULL ORDER BY id",
    )
    .bind(item_id)
    .fetch_all(&state.db)
    .await?;
    let stores: Vec<StoreAmount> = sqlx::query_as(
        "SELECT store_id, amount::float8 AS amount, quantity_reserved::float8 AS quantity_reserved
         FROM catalog_store_amounts WHERE item_id = $1 ORDER BY store_id",
    )
    .bind(item_id)
    .fetch_all(&state.db)
    .await?;
    let kind = match product.product_type {
        TYPE_SIMPLE => "simple",
        TYPE_OFFER => "offer",
        _ => "other",
    };
    let mut body = json!({
        "id": product.id,
        "xml_id": product.xml_id,
        "name": product.name,
        "type": kind,
        "barcode_multi": product.barcode_multi,
        "barcodes": barcodes,
        "stores": stores,
    });
    if api.access.can(PURCHASE_INFO) {
        body["purchasing_price"] = json!(product.purchasing_price);
        body["purchasing_currency"] = json!(product.purchasing_currency);
    }
    Ok(Json(body))
}

/// Складской учёт выключен — 409 для всех методов документов (как раздел в админке).
pub(crate) async fn require_store_control(db: &PgPool) -> ApiResult<()> {
    let mut conn = db.acquire().await?;
    if stock::settings(&mut conn).await?.store_control {
        Ok(())
    } else {
        Err(ApiError::Conflict {
            message: "Складской учёт выключен".into(),
            id: None,
        })
    }
}

/// Документ без строк (общие поля списка и карточки).
#[derive(Serialize, FromRow)]
pub struct DocumentHead {
    id: i64,
    xml_id: String,
    #[serde(rename = "type")]
    #[sqlx(rename = "doc_type")]
    kind: String,
    title: String,
    number: String,
    date: String,
    #[sqlx(rename = "status")]
    status: String,
    contractor_id: Option<i64>,
    currency: String,
    total: f64,
    comment: String,
    created_by: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Serialize, FromRow)]
pub struct LineView {
    id: i64,
    item_id: Option<i64>,
    store_from: Option<i64>,
    store_to: Option<i64>,
    amount: f64,
    purchasing_price: Option<f64>,
    #[sqlx(skip)]
    barcodes: Vec<String>,
}

#[derive(Serialize)]
pub struct DocumentView {
    #[serde(flatten)]
    head: DocumentHead,
    lines: Vec<LineView>,
}

const DOC_COLUMNS: &str = "id, COALESCE(xml_id, '') AS xml_id, doc_type::text AS doc_type, title,
    doc_number AS number, to_char(date_document, 'YYYY-MM-DD') AS date,
    CASE status WHEN 'Y' THEN 'conducted' ELSE 'draft' END AS status,
    contractor_id, currency, total::float8 AS total, commentary AS comment,
    created_by, created_at, updated_at";

/// Документ со строками и штрихкодами строк; нет документа — 404.
pub(crate) async fn load_document(db: &PgPool, id: i64) -> ApiResult<DocumentView> {
    let head: DocumentHead = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {DOC_COLUMNS} FROM store_documents WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or(ApiError::NotFound)?;
    let mut lines: Vec<LineView> = sqlx::query_as(
        "SELECT id, item_id, store_from, store_to, amount::float8 AS amount,
                purchasing_price::float8 AS purchasing_price
         FROM store_document_items WHERE doc_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    let codes: Vec<(i64, String)> = sqlx::query_as(
        "SELECT b.doc_item_id, b.barcode FROM store_document_barcodes b
         JOIN store_document_items i ON i.id = b.doc_item_id
         WHERE i.doc_id = $1 ORDER BY b.barcode",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    for (line_id, code) in codes {
        if let Some(line) = lines.iter_mut().find(|l| l.id == line_id) {
            line.barcodes.push(code);
        }
    }
    Ok(DocumentView { head, lines })
}

#[derive(Deserialize)]
pub struct DocumentsQuery {
    #[serde(rename = "type")]
    kind: Option<String>,
    status: Option<String>,
    from: Option<String>,
    to: Option<String>,
    contractor_id: Option<String>,
    xml_id: Option<String>,
}

/// `GET /documents`
pub async fn documents(
    State(state): State<AppState>,
    api: ApiAccess,
    page: Page,
    Query(q): Query<DocumentsQuery>,
) -> ApiResult<Json<Value>> {
    api.require("stock.read", SHOP_MANAGE)?;
    require_store_control(&state.db).await?;
    let kind = match q.kind.as_deref() {
        None => None,
        Some(t) if matches!(t, "A" | "S" | "M" | "R" | "D") => Some(t),
        Some(_) => return Err(bad("Неизвестный тип документа")),
    };
    let status = match q.status.as_deref() {
        None => None,
        Some("draft") => Some("N"),
        Some("conducted") => Some("Y"),
        Some(_) => return Err(bad("status: draft или conducted")),
    };
    let from = q
        .from
        .as_deref()
        .map(|v| parse_date(v, "from"))
        .transpose()?;
    let to = q.to.as_deref().map(|v| parse_date(v, "to")).transpose()?;
    let contractor = q
        .contractor_id
        .as_deref()
        .map(|v| {
            v.trim()
                .parse::<i64>()
                .map_err(|_| bad("Некорректный contractor_id"))
        })
        .transpose()?;
    const FILTER: &str = "($1::text IS NULL OR doc_type = $1)
        AND ($2::text IS NULL OR status = $2)
        AND ($3::date IS NULL OR date_document::date >= $3)
        AND ($4::date IS NULL OR date_document::date <= $4)
        AND ($5::bigint IS NULL OR contractor_id = $5)
        AND ($6::text IS NULL OR xml_id = $6)";
    let total: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM store_documents WHERE {FILTER}"
    )))
    .bind(kind)
    .bind(status)
    .bind(from)
    .bind(to)
    .bind(contractor)
    .bind(q.xml_id.as_deref())
    .fetch_one(&state.db)
    .await?;
    let items: Vec<DocumentHead> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {DOC_COLUMNS} FROM store_documents WHERE {FILTER}
         ORDER BY id DESC LIMIT $7 OFFSET $8"
    )))
    .bind(kind)
    .bind(status)
    .bind(from)
    .bind(to)
    .bind(contractor)
    .bind(q.xml_id.as_deref())
    .bind(page.limit)
    .bind(page.offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({"items": items, "total": total})))
}

/// `GET /documents/{id}`
pub async fn document(
    State(state): State<AppState>,
    api: ApiAccess,
    Id(id): Id,
) -> ApiResult<Json<DocumentView>> {
    api.require("stock.read", SHOP_MANAGE)?;
    require_store_control(&state.db).await?;
    Ok(Json(load_document(&state.db, id).await?))
}

#[derive(Serialize, FromRow)]
struct ContractorRow {
    id: i64,
    person_type: String,
    person_name: String,
    company: String,
    inn: String,
    kpp: String,
    address: String,
    phone: String,
    email: String,
    post_index: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

const CONTRACTOR_COLUMNS: &str = "id, person_type::text AS person_type, person_name, company, inn,
    kpp, address, phone, email, post_index, created_at, updated_at";

/// `GET /contractors`
pub async fn contractors(
    State(state): State<AppState>,
    api: ApiAccess,
    page: Page,
) -> ApiResult<Json<Value>> {
    api.require("stock.read", SHOP_MANAGE)?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM contractors")
        .fetch_one(&state.db)
        .await?;
    let items: Vec<ContractorRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CONTRACTOR_COLUMNS} FROM contractors ORDER BY id LIMIT $1 OFFSET $2"
    )))
    .bind(page.limit)
    .bind(page.offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({"items": items, "total": total})))
}

/// `GET /contractors/{id}`
pub async fn contractor(
    State(state): State<AppState>,
    api: ApiAccess,
    Id(id): Id,
) -> ApiResult<Json<Value>> {
    api.require("stock.read", SHOP_MANAGE)?;
    Ok(Json(contractor_json(&state.db, id).await?))
}

/// Поставщик как JSON; нет — 404.
pub(crate) async fn contractor_json(db: &PgPool, id: i64) -> ApiResult<Value> {
    let row: ContractorRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CONTRACTOR_COLUMNS} FROM contractors WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(json!(row))
}
