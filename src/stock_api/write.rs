//! Методы записи API склада: черновики документов, поставщики (`stock.write`),
//! проведение и отмена (`stock.conduct`).

use axum::{Json, body::Bytes, extract::State, http::StatusCode};
use chrono::NaiveDate;
use serde_json::{Map, Value};
use sqlx::PgPool;

use super::{
    ApiAccess, Id,
    error::{ApiError, ApiResult},
    read::{self, load_document},
};
use crate::{
    access::SHOP_MANAGE,
    admin::shop::contractors::ContractorForm,
    state::AppState,
    stock::{
        StockError,
        documents::{self, DocInput, DocType, Line, NOT_DELETABLE, NOT_EDITABLE},
    },
};

fn bad(message: impl Into<String>) -> ApiError {
    ApiError::BadRequest(message.into())
}

fn json_object(body: &Bytes) -> ApiResult<Map<String, Value>> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(bad("Нужен JSON-объект")),
    }
}

fn opt_string(map: &Map<String, Value>, key: &str) -> ApiResult<String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.trim().to_string()),
        Some(_) => Err(bad(format!("Поле {key} должно быть строкой"))),
    }
}

/// Строковое поле поставщика: число принимается как строка, `null` и отсутствие — пусто.
fn lenient_string(map: &Map<String, Value>, key: &str) -> ApiResult<String> {
    match map.get(key) {
        Some(Value::Number(n)) => Ok(n.to_string()),
        _ => opt_string(map, key),
    }
}

fn opt_date(map: &Map<String, Value>, key: &str) -> ApiResult<Option<NaiveDate>> {
    let raw = opt_string(map, key)?;
    if raw.is_empty() {
        return Ok(None);
    }
    NaiveDate::parse_from_str(&raw, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| bad(format!("Некорректная дата {key}, нужен YYYY-MM-DD")))
}

fn opt_id(map: &Map<String, Value>, key: &str) -> ApiResult<Option<i64>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .map(Some)
            .ok_or_else(|| bad(format!("Поле {key} должно быть целым числом"))),
    }
}

/// Тело документа → `DocInput`. Ошибки формата — 400; тип числа в строке и неизвестный
/// внешний код товара — 422 списком (до создания).
async fn parse_document(
    db: &PgPool,
    body: &Bytes,
    user_id: i64,
    require_type: bool,
    keep_xml_id_of: Option<i64>,
) -> ApiResult<DocInput> {
    let map = json_object(body)?;
    let doc_type = opt_string(&map, "type")?;
    if (require_type || !doc_type.is_empty()) && DocType::from_code(&doc_type).is_none() {
        return Err(bad("Неизвестный тип документа"));
    }
    let contractor_id = opt_id(&map, "contractor_id")?;
    let raw_lines = match map.get("lines") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(_) => return Err(bad("Поле lines должно быть массивом")),
    };
    let mut messages = Vec::new();
    let mut lines = Vec::new();
    for (n, raw) in raw_lines.iter().enumerate() {
        let Value::Object(line) = raw else {
            return Err(bad(format!("Строка {} должна быть объектом", n + 1)));
        };
        let item_id = opt_id(line, "item_id")?;
        let xml = opt_string(line, "item_xml_id")?;
        if item_id.is_some() != xml.is_empty() {
            return Err(bad(format!(
                "Строка {}: нужен ровно один из item_id и item_xml_id",
                n + 1
            )));
        }
        let item_id = match item_id {
            Some(id) => {
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM collection_items i
                     JOIN catalog_products p ON p.item_id = i.id WHERE i.id = $1)",
                )
                .bind(id)
                .fetch_one(db)
                .await?;
                if !exists {
                    messages.push(format!("Не найден товар #{id}"));
                }
                Some(id)
            }
            None => {
                let found: Option<i64> = sqlx::query_scalar(
                    "SELECT min(i.id) FROM collection_items i
                     JOIN catalog_products p ON p.item_id = i.id
                     WHERE i.xml_id = $1 AND i.xml_id <> ''",
                )
                .bind(&xml)
                .fetch_one(db)
                .await?;
                if found.is_none() {
                    messages.push(format!("Не найден товар с внешним кодом {xml}"));
                }
                found
            }
        };
        let amount = match line.get("amount").and_then(Value::as_f64) {
            Some(a) => a,
            None => {
                messages.push(format!("Строка {}: количество должно быть числом", n + 1));
                0.0
            }
        };
        let purchasing_price = match line.get("purchasing_price") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_f64() {
                Some(p) => Some(p),
                None => {
                    messages.push(format!(
                        "Строка {}: закупочная цена должна быть числом",
                        n + 1
                    ));
                    None
                }
            },
        };
        let barcodes = match line.get("barcodes") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .map(|c| c.as_str().map(|s| s.trim().to_string()))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| bad(format!("Строка {}: barcodes — массив строк", n + 1)))?
                .into_iter()
                .filter(|c| !c.is_empty())
                .collect(),
            Some(_) => return Err(bad(format!("Строка {}: barcodes — массив строк", n + 1))),
        };
        lines.push(Line {
            row_id: 0,
            item_id,
            store_from: opt_id(line, "store_from")?,
            store_to: opt_id(line, "store_to")?,
            amount,
            purchasing_price,
            base_price: None,
            barcodes,
        });
    }
    if let Some(c) = contractor_id {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM contractors WHERE id = $1)")
                .bind(c)
                .fetch_one(db)
                .await?;
        if !exists {
            messages.push(format!("Не найден поставщик #{c}"));
        }
    }
    let currency = opt_string(&map, "currency")?;
    if !currency.is_empty() {
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM currencies WHERE code = $1)
                 OR $1 = (SELECT COALESCE(
                        (SELECT value FROM options
                         WHERE module = 'sale' AND name = 'default_currency' AND value <> ''),
                        (SELECT code FROM currencies WHERE base), 'RUB'))",
        )
        .bind(&currency)
        .fetch_one(db)
        .await?;
        if !known {
            messages.push(format!("Неизвестная валюта {currency}"));
        }
    }
    if !messages.is_empty() {
        return Err(ApiError::Rejected(messages));
    }
    // При замене отсутствующий `xml_id` сохраняет прежний; `""`/`null` очищают.
    let xml_id = match (map.contains_key("xml_id"), keep_xml_id_of) {
        (false, Some(id)) => sqlx::query_scalar::<_, Option<String>>(
            "SELECT xml_id FROM store_documents WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(db)
        .await?
        .flatten()
        .unwrap_or_default(),
        _ => opt_string(&map, "xml_id")?,
    };
    if xml_id.chars().count() > documents::XML_ID_MAX {
        return Err(bad(format!(
            "xml_id не длиннее {} символов",
            documents::XML_ID_MAX
        )));
    }
    Ok(DocInput {
        doc_type,
        doc_number: opt_string(&map, "number")?,
        date_document: opt_date(&map, "date")?,
        contractor_id,
        currency,
        items_order_date: opt_date(&map, "items_order_date")?,
        items_received_date: opt_date(&map, "items_received_date")?,
        responsible_id: Some(user_id),
        commentary: opt_string(&map, "comment")?,
        xml_id,
        lines,
    })
}

/// Отказ склада → ответ API: повтор внешнего кода и проведённый документ — 409.
async fn map_stock_error(db: &PgPool, e: StockError, xml_id: &str) -> ApiError {
    if let StockError::Rejected(messages) = &e
        && let [message] = messages.as_slice()
    {
        if !xml_id.is_empty() && *message == documents::duplicate_xml_id_message(xml_id) {
            let id = sqlx::query_scalar("SELECT id FROM store_documents WHERE xml_id = $1")
                .bind(xml_id)
                .fetch_optional(db)
                .await
                .ok()
                .flatten();
            return ApiError::Conflict {
                message: message.clone(),
                id,
            };
        }
        if message.starts_with(documents::NOT_FOUND_PREFIX) {
            return ApiError::NotFound;
        }
        if message == NOT_EDITABLE || message == NOT_DELETABLE {
            return ApiError::Conflict {
                message: message.clone(),
                id: None,
            };
        }
    }
    e.into()
}

/// `POST /documents`
pub async fn create_document(
    State(state): State<AppState>,
    api: ApiAccess,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<read::DocumentView>)> {
    api.require("stock.write", SHOP_MANAGE)?;
    read::require_store_control(&state.db).await?;
    let user_id = api.access.user.id;
    let input = parse_document(&state.db, &body, user_id, true, None).await?;
    let mut tx = state.db.begin().await?;
    match documents::create(&mut tx, &input, Some(user_id)).await {
        Ok(id) => {
            tx.commit().await?;
            Ok((
                StatusCode::CREATED,
                Json(load_document(&state.db, id).await?),
            ))
        }
        Err(e) => {
            tx.rollback().await?;
            Err(map_stock_error(&state.db, e, &input.xml_id).await)
        }
    }
}

async fn require_exists(db: &PgPool, id: i64) -> ApiResult<()> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM store_documents WHERE id = $1)")
            .bind(id)
            .fetch_one(db)
            .await?;
    exists.then_some(()).ok_or(ApiError::NotFound)
}

/// `PUT /documents/{id}`
pub async fn replace_document(
    State(state): State<AppState>,
    api: ApiAccess,
    Id(id): Id,
    body: Bytes,
) -> ApiResult<Json<read::DocumentView>> {
    api.require("stock.write", SHOP_MANAGE)?;
    read::require_store_control(&state.db).await?;
    require_exists(&state.db, id).await?;
    let user_id = api.access.user.id;
    let input = parse_document(&state.db, &body, user_id, false, Some(id)).await?;
    let mut tx = state.db.begin().await?;
    match documents::update(&mut tx, id, &input, Some(user_id)).await {
        Ok(()) => {
            tx.commit().await?;
            Ok(Json(load_document(&state.db, id).await?))
        }
        Err(e) => {
            tx.rollback().await?;
            Err(map_stock_error(&state.db, e, &input.xml_id).await)
        }
    }
}

/// `DELETE /documents/{id}`
pub async fn delete_document(
    State(state): State<AppState>,
    api: ApiAccess,
    Id(id): Id,
) -> ApiResult<StatusCode> {
    api.require("stock.write", SHOP_MANAGE)?;
    read::require_store_control(&state.db).await?;
    require_exists(&state.db, id).await?;
    let mut tx = state.db.begin().await?;
    match documents::delete(&mut tx, id).await {
        Ok(()) => {
            tx.commit().await?;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => {
            tx.rollback().await?;
            Err(map_stock_error(&state.db, e, "").await)
        }
    }
}

/// `POST /documents/{id}/conduct` и `/cancel`: проведение и отмена проведения.
async fn run_conduct(
    state: &AppState,
    api: &ApiAccess,
    id: i64,
    cancel: bool,
) -> ApiResult<Json<read::DocumentView>> {
    api.require("stock.conduct", SHOP_MANAGE)?;
    read::require_store_control(&state.db).await?;
    require_exists(&state.db, id).await?;
    let user_id = Some(api.access.user.id);
    let mut tx = state.db.begin().await?;
    let result = if cancel {
        documents::cancel(&mut tx, id, user_id).await
    } else {
        documents::conduct(&mut tx, id, user_id).await
    };
    match result {
        Ok(()) => {
            tx.commit().await?;
            Ok(Json(load_document(&state.db, id).await?))
        }
        Err(e) => {
            tx.rollback().await?;
            Err(map_stock_error(&state.db, e, "").await)
        }
    }
}

pub async fn conduct_document(
    State(state): State<AppState>,
    api: ApiAccess,
    Id(id): Id,
) -> ApiResult<Json<read::DocumentView>> {
    run_conduct(&state, &api, id, false).await
}

pub async fn cancel_document(
    State(state): State<AppState>,
    api: ApiAccess,
    Id(id): Id,
) -> ApiResult<Json<read::DocumentView>> {
    run_conduct(&state, &api, id, true).await
}

/// `POST /contractors`
pub async fn create_contractor(
    State(state): State<AppState>,
    api: ApiAccess,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    api.require("stock.write", SHOP_MANAGE)?;
    let map = json_object(&body).map_err(|_| bad("Неверный JSON"))?;
    let form = ContractorForm {
        person_type: lenient_string(&map, "person_type")?,
        person_name: lenient_string(&map, "person_name")?,
        company: lenient_string(&map, "company")?,
        inn: lenient_string(&map, "inn")?,
        kpp: lenient_string(&map, "kpp")?,
        address: lenient_string(&map, "address")?,
        phone: lenient_string(&map, "phone")?,
        email: lenient_string(&map, "email")?,
        post_index: lenient_string(&map, "post_index")?,
    };
    form.validate().map_err(ApiError::BadRequest)?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO contractors (person_type, person_name, company, inn, kpp, address, phone,
                                  email, post_index)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
    )
    .bind(&form.person_type)
    .bind(form.person_name.trim())
    .bind(form.company.trim())
    .bind(form.inn.trim())
    .bind(form.kpp.trim())
    .bind(form.address.trim())
    .bind(form.phone.trim())
    .bind(form.email.trim())
    .bind(form.post_index.trim())
    .fetch_one(&state.db)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(read::contractor_json(&state.db, id).await?),
    ))
}
