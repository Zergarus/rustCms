//! Документы складского учёта: приход, оприходование, перемещение, возврат, списание.

use std::collections::HashMap;

use chrono::NaiveDate;
use sqlx::PgConnection;

use super::{StockError, barcodes, batches, recalc_quantity, settings};
use crate::catalog;

/// Тип документа (`b_catalog_store_docs.DOC_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocType {
    /// `A` — приходная накладная
    Arrival,
    /// `S` — оприходование
    Adjustment,
    /// `M` — перемещение
    Moving,
    /// `R` — возврат
    Return,
    /// `D` — списание
    Deduct,
}

impl DocType {
    pub fn from_code(code: &str) -> Option<DocType> {
        match code {
            "A" => Some(DocType::Arrival),
            "S" => Some(DocType::Adjustment),
            "M" => Some(DocType::Moving),
            "R" => Some(DocType::Return),
            "D" => Some(DocType::Deduct),
            _ => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            DocType::Arrival => "A",
            DocType::Adjustment => "S",
            DocType::Moving => "M",
            DocType::Return => "R",
            DocType::Deduct => "D",
        }
    }

    /// Название документа с номером, как в Битриксе («Приходная накладная #2175»).
    pub fn title(self, n: i64) -> String {
        let prefix = match self {
            DocType::Arrival => "Приходная накладная",
            DocType::Adjustment => "Оприходование",
            DocType::Moving => "Перемещение",
            DocType::Return => "Возврат товара",
            DocType::Deduct => "Списание",
        };
        format!("{prefix} #{n}")
    }

    /// Название типа.
    pub fn name(self) -> &'static str {
        match self {
            DocType::Arrival => "Приход товара на склад",
            DocType::Adjustment => "Оприходование товара",
            DocType::Moving => "Перемещение товара между складами",
            DocType::Return => "Возврат товара",
            DocType::Deduct => "Списание товара",
        }
    }

    /// Склад-источник у строки (перемещение, списание).
    pub(super) fn uses_source(self) -> bool {
        matches!(self, DocType::Moving | DocType::Deduct)
    }

    /// Склад-получатель у строки (всё, кроме списания).
    pub(super) fn uses_destination(self) -> bool {
        !matches!(self, DocType::Deduct)
    }
}

/// Строка документа (`b_catalog_docs_element`).
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub row_id: i64,
    pub item_id: Option<i64>,
    pub store_from: Option<i64>,
    pub store_to: Option<i64>,
    pub amount: f64,
    pub purchasing_price: Option<f64>,
    pub base_price: Option<f64>,
    /// Штрихкоды строки (`store_document_barcodes`).
    pub barcodes: Vec<String>,
}

/// Изменение остатка товара на складе.
#[derive(Debug, Clone, PartialEq)]
pub struct Move {
    pub item_id: i64,
    pub store_id: i64,
    pub delta: f64,
}

/// Движения остатков по строкам: проведение — по таблице типов, отмена — те же с обратным
/// знаком. Строки без товара или нужного склада пропускаются (их отсекают проверки).
pub fn moves(doc_type: DocType, lines: &[Line], cancel: bool) -> Vec<Move> {
    let sign = if cancel { -1.0 } else { 1.0 };
    let mut result = Vec::new();
    for line in lines {
        let Some(item_id) = line.item_id else {
            continue;
        };
        if doc_type.uses_source()
            && let Some(store_id) = line.store_from
        {
            result.push(Move {
                item_id,
                store_id,
                delta: -sign * line.amount,
            });
        }
        if doc_type.uses_destination()
            && let Some(store_id) = line.store_to
        {
            result.push(Move {
                item_id,
                store_id,
                delta: sign * line.amount,
            });
        }
    }
    result
}

/// Поля документа из формы; строки заменяются целиком.
#[derive(Debug, Clone, Default)]
pub struct DocInput {
    pub doc_type: String,
    pub doc_number: String,
    pub date_document: Option<NaiveDate>,
    pub contractor_id: Option<i64>,
    /// Пусто — валюта сайта.
    pub currency: String,
    pub items_order_date: Option<NaiveDate>,
    pub items_received_date: Option<NaiveDate>,
    pub responsible_id: Option<i64>,
    pub commentary: String,
    /// Внешний код документа (WMS и т.п.); пусто — нет кода.
    pub xml_id: String,
    pub lines: Vec<Line>,
}

pub const NOT_EDITABLE: &str = "Проведённый документ нельзя изменить — сначала отмените проведение";
pub const NOT_DELETABLE: &str = "Проведённый документ нельзя удалить — сначала отмените проведение";

fn reject(message: impl Into<String>) -> StockError {
    StockError::Rejected(vec![message.into()])
}

const XML_ID_INDEX: &str = "store_documents_xml_id_uq";

/// Текст отказа при повторе внешнего кода (API по нему находит конфликт).
pub fn duplicate_xml_id_message(xml_id: &str) -> String {
    format!("Документ с внешним кодом {xml_id} уже есть")
}

/// Нарушение уникальности внешнего кода — отказ правил, остальное — ошибка БД.
fn xml_id_error(e: sqlx::Error, xml_id: &str) -> StockError {
    match e.as_database_error().and_then(|d| d.constraint()) {
        Some(XML_ID_INDEX) => reject(duplicate_xml_id_message(xml_id)),
        _ => StockError::Db(e),
    }
}

pub const NOT_FOUND_PREFIX: &str = "Не найден документ складского учета";

/// Максимальная длина внешнего кода документа.
pub const XML_ID_MAX: usize = 100;

fn not_found(id: i64) -> StockError {
    reject(format!("{NOT_FOUND_PREFIX} с идентификатором #{id}"))
}

/// Сумма документа: Σ количество × закупочная цена.
fn total(lines: &[Line]) -> f64 {
    lines
        .iter()
        .map(|l| l.amount * l.purchasing_price.unwrap_or(0.0))
        .sum()
}

/// Следующий номер документа типа (`catalog.store_document_numbers_<тип>`), атомарно.
async fn next_number(tx: &mut PgConnection, doc_type: DocType) -> sqlx::Result<i64> {
    let value: String = sqlx::query_scalar(
        "INSERT INTO options (module, name, value) VALUES ('catalog', $1, '1')
         ON CONFLICT (module, name) DO UPDATE SET value = (
             CASE WHEN options.value ~ '^[0-9]+$' THEN options.value::bigint ELSE 0 END + 1
         )::text
         RETURNING value",
    )
    .bind(format!("store_document_numbers_{}", doc_type.code()))
    .fetch_one(tx)
    .await?;
    Ok(value.parse().unwrap_or(0))
}

pub(super) async fn currency_or_site(
    tx: &mut PgConnection,
    currency: &str,
) -> sqlx::Result<String> {
    if currency.is_empty() {
        crate::currency::site_currency(tx).await
    } else {
        Ok(currency.to_string())
    }
}

async fn insert_lines(tx: &mut PgConnection, doc_id: i64, lines: &[Line]) -> sqlx::Result<()> {
    for line in lines {
        let row_id: i64 = sqlx::query_scalar(
            "INSERT INTO store_document_items
                 (doc_id, store_from, store_to, item_id, amount, purchasing_price, base_price)
             VALUES ($1, $2, $3, $4, $5::float8::numeric, $6::float8::numeric, $7::float8::numeric)
             RETURNING id",
        )
        .bind(doc_id)
        .bind(line.store_from)
        .bind(line.store_to)
        .bind(line.item_id)
        .bind(line.amount)
        .bind(line.purchasing_price)
        .bind(line.base_price)
        .fetch_one(&mut *tx)
        .await?;
        for code in &line.barcodes {
            sqlx::query(
                "INSERT INTO store_document_barcodes (doc_item_id, barcode) VALUES ($1, $2)",
            )
            .bind(row_id)
            .bind(code)
            .execute(&mut *tx)
            .await?;
        }
    }
    Ok(())
}

/// Создаёт черновик; название — «<Тип> #<N>» по счётчику типа.
pub async fn create(
    tx: &mut PgConnection,
    input: &DocInput,
    user: Option<i64>,
) -> Result<i64, StockError> {
    let doc_type = DocType::from_code(&input.doc_type)
        .ok_or_else(|| reject(format!("Неизвестный тип документа «{}»", input.doc_type)))?;
    let title = doc_type.title(next_number(&mut *tx, doc_type).await?);
    let currency = currency_or_site(&mut *tx, &input.currency).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO store_documents
             (doc_type, title, doc_number, date_document, contractor_id, currency, total,
              items_order_date, items_received_date, responsible_id, commentary,
              created_by, modified_by, xml_id)
         VALUES ($1, $2, $3, COALESCE($4::date::timestamptz, now()), $5, $6, $7::float8::numeric,
                 $8::date::timestamptz, $9::date::timestamptz, $10, $11, $12, $12, $13)
         RETURNING id",
    )
    .bind(doc_type.code())
    .bind(title)
    .bind(&input.doc_number)
    .bind(input.date_document)
    .bind(input.contractor_id)
    .bind(currency)
    .bind(total(&input.lines))
    .bind(input.items_order_date)
    .bind(input.items_received_date)
    .bind(input.responsible_id)
    .bind(&input.commentary)
    .bind(user)
    .bind(&input.xml_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| xml_id_error(e, &input.xml_id))?;
    insert_lines(tx, id, &input.lines).await?;
    Ok(id)
}

/// Статус документа под блокировкой строки; `None` — документа нет.
async fn lock_status(tx: &mut PgConnection, id: i64) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT status::text FROM store_documents WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(tx)
        .await
}

/// Правит черновик (тип и название не меняются), строки заменяются.
pub async fn update(
    tx: &mut PgConnection,
    id: i64,
    input: &DocInput,
    user: Option<i64>,
) -> Result<(), StockError> {
    match lock_status(&mut *tx, id).await?.as_deref() {
        None => return Err(not_found(id)),
        Some("Y") => return Err(reject(NOT_EDITABLE)),
        Some(_) => {}
    }
    let currency = currency_or_site(&mut *tx, &input.currency).await?;
    sqlx::query(
        "UPDATE store_documents SET
             doc_number = $2, date_document = COALESCE($3::date::timestamptz, date_document),
             contractor_id = $4, currency = $5, total = $6::float8::numeric,
             items_order_date = $7::date::timestamptz, items_received_date = $8::date::timestamptz,
             responsible_id = $9, commentary = $10, modified_by = $11, xml_id = $12,
             updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(&input.doc_number)
    .bind(input.date_document)
    .bind(input.contractor_id)
    .bind(currency)
    .bind(total(&input.lines))
    .bind(input.items_order_date)
    .bind(input.items_received_date)
    .bind(input.responsible_id)
    .bind(&input.commentary)
    .bind(user)
    .bind(&input.xml_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| xml_id_error(e, &input.xml_id))?;
    sqlx::query("DELETE FROM store_document_items WHERE doc_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    insert_lines(tx, id, &input.lines).await?;
    Ok(())
}

/// Удаляет черновик вместе со строками.
pub async fn delete(tx: &mut PgConnection, id: i64) -> Result<(), StockError> {
    match lock_status(&mut *tx, id).await?.as_deref() {
        None => return Err(not_found(id)),
        Some("Y") => return Err(reject(NOT_DELETABLE)),
        Some(_) => {}
    }
    sqlx::query("DELETE FROM store_documents WHERE id = $1")
        .bind(id)
        .execute(tx)
        .await?;
    Ok(())
}

/// Проводит документ (`conductDocument`): проверки, движения остатков, доступное, цены.
pub async fn conduct(tx: &mut PgConnection, id: i64, user: Option<i64>) -> Result<(), StockError> {
    run(tx, id, user, false).await
}

/// Отменяет проведение (`cancellationDocument`): обратные движения с теми же проверками;
/// цены не откатываются.
pub async fn cancel(tx: &mut PgConnection, id: i64, user: Option<i64>) -> Result<(), StockError> {
    run(tx, id, user, true).await
}

#[derive(sqlx::FromRow)]
struct DocRow {
    doc_type: String,
    status: String,
    contractor_id: Option<i64>,
    currency: String,
}

#[derive(sqlx::FromRow)]
struct LineRow {
    id: i64,
    item_id: Option<i64>,
    store_from: Option<i64>,
    store_to: Option<i64>,
    amount: f64,
    purchasing_price: Option<f64>,
    base_price: Option<f64>,
}

/// Строки документа, сгруппированные по товару в порядке появления (как `ELEMENTS` Битрикса).
struct Product<'a> {
    /// 0 — строка без товара.
    id: i64,
    name: String,
    lines: Vec<&'a Line>,
}

async fn run(
    tx: &mut PgConnection,
    id: i64,
    user: Option<i64>,
    cancel: bool,
) -> Result<(), StockError> {
    let s = settings(&mut *tx).await?;
    if !s.store_control {
        return Err(reject("Складской учёт выключен"));
    }
    let doc: DocRow = sqlx::query_as(
        "SELECT doc_type::text, status::text, contractor_id, currency
         FROM store_documents WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| not_found(id))?;
    let doc_type = DocType::from_code(&doc.doc_type).ok_or_else(|| not_found(id))?;
    match (cancel, doc.status.as_str()) {
        (false, "Y") => return Err(reject("Документ уже проведён")),
        (true, "N") => return Err(reject("Документ не проведён")),
        _ => {}
    }
    // Поставщик проверяется до загрузки строк (`checkParamsForConduction`)
    if !cancel && doc_type == DocType::Arrival && doc.contractor_id.is_none() {
        return Err(reject("Не указан поставщик."));
    }

    let lines: Vec<Line> = sqlx::query_as::<_, LineRow>(
        "SELECT id, item_id, store_from, store_to, amount::float8,
                purchasing_price::float8, base_price::float8
         FROM store_document_items WHERE doc_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|r| Line {
        row_id: r.id,
        item_id: r.item_id,
        store_from: r.store_from,
        store_to: r.store_to,
        amount: r.amount,
        purchasing_price: r.purchasing_price,
        base_price: r.base_price,
        barcodes: Vec::new(),
    })
    .collect();
    let mut lines = lines;
    let codes: Vec<(i64, String)> = sqlx::query_as(
        "SELECT b.doc_item_id, b.barcode FROM store_document_barcodes b
         JOIN store_document_items i ON i.id = b.doc_item_id
         WHERE i.doc_id = $1 ORDER BY b.id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    for (row, code) in codes {
        if let Some(l) = lines.iter_mut().find(|l| l.row_id == row) {
            l.barcodes.push(code);
        }
    }
    if lines.is_empty() {
        return Err(reject(format!(
            "Не найдены позиции документа складского учета с идентификатором #{id}"
        )));
    }

    let mut item_ids: Vec<i64> = lines.iter().filter_map(|l| l.item_id).collect();
    item_ids.sort_unstable();
    item_ids.dedup();
    let info = catalog::load_locked(&mut *tx, &item_ids).await?;
    let names: HashMap<i64, String> =
        sqlx::query_as("SELECT id, name FROM collection_items WHERE id = ANY($1)")
            .bind(&item_ids)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect();

    let mut products: Vec<Product> = Vec::new();
    for line in &lines {
        let pid = line.item_id.unwrap_or(0);
        match products.iter_mut().find(|p| p.id == pid) {
            Some(p) => p.lines.push(line),
            None => products.push(Product {
                id: pid,
                name: names.get(&pid).cloned().unwrap_or_default(),
                lines: vec![line],
            }),
        }
    }

    check_products(&products, &info)?;
    let stores: HashMap<i64, (String, bool)> =
        sqlx::query_as::<_, (i64, String, bool)>("SELECT id, name, active FROM catalog_stores")
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(|(id, name, active)| (id, (name, active)))
            .collect();
    if doc_type.uses_source() {
        check_stores(&products, &stores, doc_type, true)?;
    }
    if doc_type.uses_destination() {
        check_stores(&products, &stores, doc_type, false)?;
    }
    if doc_type == DocType::Moving && lines.iter().any(|l| l.store_from == l.store_to) {
        return Err(reject(
            "Склад-получатель не должен совпадать со складом-отправителем",
        ));
    }

    // Движения, просуммированные по (товар, склад); минус — по итогу
    let mut totals: Vec<Move> = Vec::new();
    for m in moves(doc_type, &lines, cancel) {
        match totals
            .iter_mut()
            .find(|t| t.item_id == m.item_id && t.store_id == m.store_id)
        {
            Some(t) => t.delta += m.delta,
            None => totals.push(m),
        }
    }
    let current: HashMap<(i64, i64), f64> = sqlx::query_as::<_, (i64, i64, f64)>(
        "SELECT item_id, store_id, amount::float8 FROM catalog_store_amounts
         WHERE item_id = ANY($1)",
    )
    .bind(&item_ids)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|(item, store, amount)| ((item, store), amount))
    .collect();
    let mut errors = Vec::new();
    for m in &totals {
        let after = current
            .get(&(m.item_id, m.store_id))
            .copied()
            .unwrap_or(0.0)
            + m.delta;
        let can_buy_zero = info.get(&m.item_id).is_some_and(|i| i.can_buy_zero);
        if m.delta < 0.0 && after < -1e-9 && !can_buy_zero {
            errors.push(format!(
                "Указанное количество товара {} больше, чем есть на складе {}",
                names
                    .get(&m.item_id)
                    .map(String::as_str)
                    .unwrap_or_default(),
                stores
                    .get(&m.store_id)
                    .map(|s| s.0.as_str())
                    .unwrap_or_default(),
            ));
        }
    }
    if !errors.is_empty() {
        return Err(StockError::Rejected(errors));
    }
    barcodes::apply_document(&mut *tx, doc_type, &lines, &names, &stores, cancel, user).await?;

    for m in totals.iter().filter(|m| m.delta != 0.0) {
        sqlx::query(
            "INSERT INTO catalog_store_amounts (item_id, store_id, amount)
             VALUES ($1, $2, $3::float8::numeric)
             ON CONFLICT (item_id, store_id)
             DO UPDATE SET amount = catalog_store_amounts.amount + EXCLUDED.amount",
        )
        .bind(m.item_id)
        .bind(m.store_id)
        .bind(m.delta)
        .execute(&mut *tx)
        .await?;
    }
    recalc_quantity(&mut *tx, &item_ids).await?;
    if let Some(method) = s.method {
        let currency = currency_or_site(&mut *tx, &doc.currency).await?;
        batches::apply_document(&mut *tx, method, doc_type, &lines, &currency, cancel).await?;
    }

    if !cancel && matches!(doc_type, DocType::Arrival | DocType::Adjustment) {
        let currency = currency_or_site(&mut *tx, &doc.currency).await?;
        update_prices(&mut *tx, &lines, &currency).await?;
    }

    sqlx::query(
        "UPDATE store_documents SET status = $2, was_cancelled = was_cancelled OR $3,
             date_status = now(), status_by = $4, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(if cancel { "N" } else { "Y" })
    .bind(cancel)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Товары и количества (`checkDocumentProducts`).
fn check_products(
    products: &[Product],
    info: &HashMap<i64, catalog::PurchaseInfo>,
) -> Result<(), StockError> {
    let mut errors = Vec::new();
    for p in products {
        let Some(product) = info.get(&p.id) else {
            errors.push(format!(
                "Не найден элемент инфоблока с идентификатором #{}, присутствующий в документе складского учета",
                p.id
            ));
            continue;
        };
        let described = format!("#{} ({})", p.id, p.name);
        if !product.is_catalog {
            errors.push(format!(
                "Не найден товар с идентификатором {described}, присутствующий в документе складского учета"
            ));
            continue;
        }
        if !matches!(
            product.product_type,
            catalog::TYPE_SIMPLE | catalog::TYPE_OFFER
        ) {
            errors.push(format!(
                "Неверный тип товара {described} в документе складского учета"
            ));
            continue;
        }
        for line in &p.lines {
            if line.amount <= 0.0 {
                errors.push(format!(
                    "Некорректное количество товара {described} в документе складского учета"
                ));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(StockError::Rejected(errors))
    }
}

/// Склады строк (`checkDocumentStores`): у каждого товара — первая ошибка его строк;
/// тексты по видам ошибки, для нескольких товаров — во множественном числе.
fn check_stores(
    products: &[Product],
    stores: &HashMap<i64, (String, bool)>,
    doc_type: DocType,
    source: bool,
) -> Result<(), StockError> {
    // не указан, несуществующий, деактивированный
    let mut found: [Vec<String>; 3] = Default::default();
    for p in products {
        for line in &p.lines {
            let store = if source {
                line.store_from
            } else {
                line.store_to
            };
            let kind = match store {
                None => 0,
                Some(s) => match stores.get(&s) {
                    None => 1,
                    Some((_, false)) => 2,
                    Some((_, true)) => continue,
                },
            };
            found[kind].push(format!("{} {}", p.id, p.name));
            break;
        }
    }
    let noun = match (doc_type, source) {
        (DocType::Moving, true) => "склад-отправитель",
        (DocType::Deduct, _) => "склад списания",
        (DocType::Arrival, _) => "склад прихода",
        (DocType::Adjustment, _) => "склад оприходования",
        (DocType::Moving, false) => "склад назначения",
        (DocType::Return, _) => "склад возврата",
    };
    let errors: Vec<String> = found
        .iter()
        .zip([
            "не указан",
            "указан несуществующий",
            "указан деактивированный",
        ])
        .filter(|(list, _)| !list.is_empty())
        .map(|(list, what)| {
            let whom = if list.len() > 1 {
                "товаров"
            } else {
                "товара"
            };
            format!("Для {whom} {} {what} {noun}", list.join(", "))
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(StockError::Rejected(errors))
    }
}

/// Цены прихода и оприходования (`UpdateProductPricesAction`): закупочная — в валюте
/// документа (0 очищает); базовая — первая цена базового типа или новая, кроме товаров с
/// диапазонами цен базового типа.
async fn update_prices(tx: &mut PgConnection, lines: &[Line], currency: &str) -> sqlx::Result<()> {
    let base_type: Option<i64> =
        sqlx::query_scalar("SELECT id FROM catalog_price_types WHERE is_base ORDER BY id LIMIT 1")
            .fetch_optional(&mut *tx)
            .await?;
    for line in lines {
        let Some(item) = line.item_id else {
            continue;
        };
        if let Some(price) = line.purchasing_price {
            sqlx::query(
                "UPDATE catalog_products
                 SET purchasing_price = NULLIF($2::float8::numeric, 0), purchasing_currency = $3
                 WHERE item_id = $1",
            )
            .bind(item)
            .bind(price)
            .bind(currency)
            .execute(&mut *tx)
            .await?;
        }
        let (Some(price), Some(type_id)) = (line.base_price, base_type) else {
            continue;
        };
        let rows: Vec<(i64, bool)> = sqlx::query_as(
            "SELECT id, quantity_from IS NOT NULL OR quantity_to IS NOT NULL FROM catalog_prices
             WHERE item_id = $1 AND price_type_id = $2 ORDER BY id",
        )
        .bind(item)
        .bind(type_id)
        .fetch_all(&mut *tx)
        .await?;
        if rows.iter().any(|(_, ranged)| *ranged) {
            continue;
        }
        match rows.first() {
            Some((price_id, _)) => {
                sqlx::query(
                    "UPDATE catalog_prices SET price = $2::float8::numeric, currency = $3
                     WHERE id = $1",
                )
                .bind(price_id)
                .bind(price)
                .bind(currency)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO catalog_prices (item_id, price_type_id, price, currency)
                     VALUES ($1, $2, $3::float8::numeric, $4)",
                )
                .bind(item)
                .bind(type_id)
                .bind(price)
                .bind(currency)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stock::{StockError, set_store_control};
    use crate::test_support::order_fixture;
    use sqlx::PgPool;

    fn line() -> Line {
        Line {
            row_id: 1,
            item_id: Some(7),
            store_from: Some(1),
            store_to: Some(2),
            amount: 3.0,
            purchasing_price: None,
            base_price: None,
            barcodes: Vec::new(),
        }
    }

    fn mv(item_id: i64, store_id: i64, delta: f64) -> Move {
        Move {
            item_id,
            store_id,
            delta,
        }
    }

    #[test]
    fn moves_by_type() {
        let to_only = Line {
            store_from: None,
            ..line()
        };
        let from_only = Line {
            store_to: None,
            ..line()
        };
        assert_eq!(
            moves(DocType::Arrival, std::slice::from_ref(&to_only), false),
            [mv(7, 2, 3.0)]
        );
        assert_eq!(
            moves(DocType::Adjustment, &[line()], false),
            [mv(7, 2, 3.0)]
        );
        assert_eq!(
            moves(DocType::Moving, &[line()], false),
            [mv(7, 1, -3.0), mv(7, 2, 3.0)]
        );
        assert_eq!(moves(DocType::Return, &[line()], false), [mv(7, 2, 3.0)]);
        assert_eq!(
            moves(DocType::Deduct, &[from_only], false),
            [mv(7, 1, -3.0)]
        );
        assert_eq!(
            moves(DocType::Moving, &[line()], true),
            [mv(7, 1, 3.0), mv(7, 2, -3.0)]
        );
        assert_eq!(moves(DocType::Arrival, &[to_only], true), [mv(7, 2, -3.0)]);
    }

    /// Учёт включён, склады 1 и 2 активны, 3 — неактивен, поставщик 1, базовый тип цены.
    /// Товар `order_fixture`: склад 1 — 5, доступно 5.
    async fn setup(db: &PgPool) -> i64 {
        let f = order_fixture(db).await;
        for sql in [
            "INSERT INTO catalog_stores (id, name) VALUES (2, 'Склад 2')",
            "INSERT INTO catalog_stores (id, name, active) VALUES (3, 'Склад 3', FALSE)",
            "INSERT INTO contractors (id, company) VALUES (1, 'Поставщик')",
            "INSERT INTO catalog_price_types (id, code, name, is_base) VALUES (1, 'BASE', 'Розница', TRUE)",
        ] {
            sqlx::query(sql).execute(db).await.unwrap();
        }
        let mut tx = db.begin().await.unwrap();
        set_store_control(&mut tx, true).await.unwrap();
        tx.commit().await.unwrap();
        f.product_id
    }

    fn ln(item: i64, from: Option<i64>, to: Option<i64>, amount: f64) -> Line {
        Line {
            row_id: 0,
            item_id: Some(item),
            store_from: from,
            store_to: to,
            amount,
            purchasing_price: None,
            base_price: None,
            barcodes: Vec::new(),
        }
    }

    fn input(doc_type: &str, lines: Vec<Line>) -> DocInput {
        DocInput {
            doc_type: doc_type.into(),
            contractor_id: Some(1),
            lines,
            ..DocInput::default()
        }
    }

    async fn create_doc(db: &PgPool, input: &DocInput) -> i64 {
        let mut tx = db.begin().await.unwrap();
        let id = create(&mut tx, input, None).await.unwrap();
        tx.commit().await.unwrap();
        id
    }

    /// Проводит (или отменяет) и коммитит в любом случае: отказ не должен ничего менять.
    async fn run(
        db: &PgPool,
        id: i64,
        cancel_it: bool,
        user: Option<i64>,
    ) -> Result<(), StockError> {
        let mut tx = db.begin().await.unwrap();
        let r = if cancel_it {
            cancel(&mut tx, id, user).await
        } else {
            conduct(&mut tx, id, user).await
        };
        tx.commit().await.unwrap();
        r
    }

    fn rejected(r: Result<(), StockError>) -> Vec<String> {
        match r {
            Err(StockError::Rejected(m)) => m,
            other => panic!("ожидался отказ, получено {other:?}"),
        }
    }

    async fn amount(db: &PgPool, item: i64, store: i64) -> f64 {
        sqlx::query_scalar(
            "SELECT COALESCE((SELECT amount::float8 FROM catalog_store_amounts
                              WHERE item_id = $1 AND store_id = $2), 0)",
        )
        .bind(item)
        .bind(store)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn quantity(db: &PgPool, item: i64) -> f64 {
        sqlx::query_scalar("SELECT quantity::float8 FROM catalog_products WHERE item_id = $1")
            .bind(item)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn status(db: &PgPool, id: i64) -> (String, bool, Option<i64>) {
        sqlx::query_as(
            "SELECT status::text, was_cancelled, status_by FROM store_documents WHERE id = $1",
        )
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn base_prices(db: &PgPool, item: i64) -> Vec<f64> {
        sqlx::query_scalar(
            "SELECT price::float8 FROM catalog_prices WHERE item_id = $1 AND price_type_id = 1
             ORDER BY id",
        )
        .bind(item)
        .fetch_all(db)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn arrival_conduct_and_cancel(db: PgPool) {
        let item = setup(&db).await;
        let (user, _) = crate::test_support::user_cookie(&db, "store", true).await;
        let line = Line {
            purchasing_price: Some(100.0),
            base_price: Some(150.0),
            ..ln(item, None, Some(2), 4.0)
        };
        let id = create_doc(&db, &input("A", vec![line])).await;
        let total: f64 =
            sqlx::query_scalar("SELECT total::float8 FROM store_documents WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(total, 400.0);

        run(&db, id, false, Some(user)).await.unwrap();
        assert_eq!(amount(&db, item, 2).await, 4.0);
        assert_eq!(quantity(&db, item).await, 9.0);
        let purchasing: (Option<f64>, Option<String>) = sqlx::query_as(
            "SELECT purchasing_price::float8, purchasing_currency FROM catalog_products
             WHERE item_id = $1",
        )
        .bind(item)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(purchasing, (Some(100.0), Some("RUB".into())));
        assert_eq!(base_prices(&db, item).await, [150.0]);
        assert_eq!(status(&db, id).await, ("Y".into(), false, Some(user)));

        run(&db, id, true, Some(user)).await.unwrap();
        assert_eq!(amount(&db, item, 2).await, 0.0);
        assert_eq!(quantity(&db, item).await, 5.0);
        let (st, cancelled, _) = status(&db, id).await;
        assert_eq!((st.as_str(), cancelled), ("N", true));
        assert_eq!(base_prices(&db, item).await, [150.0]);
        let price: Option<f64> = sqlx::query_scalar(
            "SELECT purchasing_price::float8 FROM catalog_products WHERE item_id = $1",
        )
        .bind(item)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(price, Some(100.0));
    }

    #[sqlx::test]
    async fn arrival_requires_contractor(db: PgPool) {
        let item = setup(&db).await;
        let mut doc = input("A", vec![ln(item, None, Some(2), 1.0)]);
        doc.contractor_id = None;
        let id = create_doc(&db, &doc).await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            ["Не указан поставщик."]
        );
        assert_eq!(amount(&db, item, 2).await, 0.0);
    }

    #[sqlx::test]
    async fn adjustment_without_contractor(db: PgPool) {
        let item = setup(&db).await;
        let mut doc = input("S", vec![ln(item, None, Some(2), 1.0)]);
        doc.contractor_id = None;
        let id = create_doc(&db, &doc).await;
        run(&db, id, false, None).await.unwrap();
        assert_eq!(amount(&db, item, 2).await, 1.0);
        assert_eq!(quantity(&db, item).await, 6.0);
    }

    #[sqlx::test]
    async fn moving_between_stores(db: PgPool) {
        let item = setup(&db).await;
        let id = create_doc(&db, &input("M", vec![ln(item, Some(1), Some(2), 2.0)])).await;
        run(&db, id, false, None).await.unwrap();
        assert_eq!(amount(&db, item, 1).await, 3.0);
        assert_eq!(amount(&db, item, 2).await, 2.0);
        assert_eq!(quantity(&db, item).await, 5.0);

        let id = create_doc(&db, &input("M", vec![ln(item, Some(1), Some(1), 1.0)])).await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            ["Склад-получатель не должен совпадать со складом-отправителем"]
        );
    }

    #[sqlx::test]
    async fn deduct_negative_rejected(db: PgPool) {
        let item = setup(&db).await;
        let id = create_doc(&db, &input("D", vec![ln(item, Some(1), None, 6.0)])).await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            ["Указанное количество товара Товар больше, чем есть на складе Склад"]
        );
        assert_eq!(amount(&db, item, 1).await, 5.0);
        assert_eq!(quantity(&db, item).await, 5.0);
        assert_eq!(status(&db, id).await.0, "N");

        sqlx::query("UPDATE catalog_products SET can_buy_zero = TRUE WHERE item_id = $1")
            .bind(item)
            .execute(&db)
            .await
            .unwrap();
        run(&db, id, false, None).await.unwrap();
        assert_eq!(amount(&db, item, 1).await, -1.0);
        assert_eq!(quantity(&db, item).await, -1.0);
    }

    #[sqlx::test]
    async fn cancel_arrival_after_sale_rejected(db: PgPool) {
        let item = setup(&db).await;
        let arrival = create_doc(
            &db,
            &input(
                "A",
                vec![ln(item, None, Some(1), 1.0), ln(item, None, Some(2), 4.0)],
            ),
        )
        .await;
        run(&db, arrival, false, None).await.unwrap();
        let deduct = create_doc(&db, &input("D", vec![ln(item, Some(2), None, 3.0)])).await;
        run(&db, deduct, false, None).await.unwrap();

        assert_eq!(
            rejected(run(&db, arrival, true, None).await),
            ["Указанное количество товара Товар больше, чем есть на складе Склад 2"]
        );
        assert_eq!(amount(&db, item, 1).await, 6.0);
        assert_eq!(amount(&db, item, 2).await, 1.0);
        assert_eq!(status(&db, arrival).await.0, "Y");
    }

    #[sqlx::test]
    async fn store_errors(db: PgPool) {
        let item = setup(&db).await;
        for (doc_type, from, to, text) in [
            ("A", None, None, "не указан склад прихода"),
            ("A", None, Some(3), "указан деактивированный склад прихода"),
            ("A", None, Some(99), "указан несуществующий склад прихода"),
            ("S", None, None, "не указан склад оприходования"),
            ("R", None, Some(99), "указан несуществующий склад возврата"),
            ("M", None, Some(2), "не указан склад-отправитель"),
            (
                "M",
                Some(1),
                Some(3),
                "указан деактивированный склад назначения",
            ),
            ("D", None, None, "не указан склад списания"),
        ] {
            let id = create_doc(&db, &input(doc_type, vec![ln(item, from, to, 1.0)])).await;
            assert_eq!(
                rejected(run(&db, id, false, None).await),
                [format!("Для товара {item} Товар {text}")],
                "{doc_type} {from:?} {to:?}"
            );
        }

        // Несколько товаров — текст для списка
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name)
             SELECT collection_id, 'drugoy', 'Другой' FROM collection_items WHERE id = $1
             RETURNING id",
        )
        .bind(item)
        .fetch_one(&db)
        .await
        .unwrap();
        let id = create_doc(
            &db,
            &input(
                "A",
                vec![ln(item, None, None, 1.0), ln(other, None, None, 1.0)],
            ),
        )
        .await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            [format!(
                "Для товаров {item} Товар, {other} Другой не указан склад прихода"
            )]
        );
    }

    #[sqlx::test]
    async fn empty_document_rejected(db: PgPool) {
        setup(&db).await;
        let id = create_doc(&db, &input("A", vec![])).await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            [format!(
                "Не найдены позиции документа складского учета с идентификатором #{id}"
            )]
        );
    }

    #[sqlx::test]
    async fn bad_product_rejected(db: PgPool) {
        let item = setup(&db).await;
        sqlx::query("UPDATE catalog_products SET type = $2 WHERE item_id = $1")
            .bind(item)
            .bind(crate::catalog::TYPE_SKU)
            .execute(&db)
            .await
            .unwrap();
        let id = create_doc(&db, &input("S", vec![ln(item, None, Some(2), 1.0)])).await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            [format!(
                "Неверный тип товара #{item} (Товар) в документе складского учета"
            )]
        );
        sqlx::query("UPDATE catalog_products SET type = 1 WHERE item_id = $1")
            .bind(item)
            .execute(&db)
            .await
            .unwrap();

        let line = Line {
            item_id: None,
            ..ln(item, None, Some(2), 1.0)
        };
        let id = create_doc(&db, &input("S", vec![line])).await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            [
                "Не найден элемент инфоблока с идентификатором #0, присутствующий в документе складского учета"
            ]
        );

        let id = create_doc(&db, &input("S", vec![ln(item, None, Some(2), 0.0)])).await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            [format!(
                "Некорректное количество товара #{item} (Товар) в документе складского учета"
            )]
        );
        assert_eq!(amount(&db, item, 2).await, 0.0);
    }

    #[sqlx::test]
    async fn conducted_is_readonly(db: PgPool) {
        let item = setup(&db).await;
        let doc = input("S", vec![ln(item, None, Some(2), 1.0)]);
        let id = create_doc(&db, &doc).await;
        assert_eq!(
            rejected(run(&db, id, true, None).await),
            ["Документ не проведён"]
        );

        // Черновик правится
        let mut changed = doc.clone();
        changed.commentary = "правка".into();
        changed.lines = vec![ln(item, None, Some(2), 2.0)];
        let mut tx = db.begin().await.unwrap();
        update(&mut tx, id, &changed, None).await.unwrap();
        tx.commit().await.unwrap();

        run(&db, id, false, None).await.unwrap();
        assert_eq!(amount(&db, item, 2).await, 2.0);

        let mut tx = db.begin().await.unwrap();
        assert_eq!(
            rejected(update(&mut tx, id, &doc, None).await),
            ["Проведённый документ нельзя изменить — сначала отмените проведение"]
        );
        assert_eq!(
            rejected(delete(&mut tx, id).await),
            ["Проведённый документ нельзя удалить — сначала отмените проведение"]
        );
        tx.commit().await.unwrap();
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            ["Документ уже проведён"]
        );
        assert_eq!(amount(&db, item, 2).await, 2.0);

        // После отмены — снова черновик, удаляется
        run(&db, id, true, None).await.unwrap();
        let mut tx = db.begin().await.unwrap();
        delete(&mut tx, id).await.unwrap();
        tx.commit().await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM store_document_items")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    #[sqlx::test]
    async fn numbering_per_type(db: PgPool) {
        let item = setup(&db).await;
        let lines = vec![ln(item, Some(1), Some(2), 1.0)];
        let a1 = create_doc(&db, &input("A", lines.clone())).await;
        let a2 = create_doc(&db, &input("A", lines.clone())).await;
        let d1 = create_doc(&db, &input("D", lines)).await;
        let mut titles = Vec::new();
        for id in [a1, a2, d1] {
            let title: String =
                sqlx::query_scalar("SELECT title FROM store_documents WHERE id = $1")
                    .bind(id)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            titles.push(title);
        }
        assert_eq!(
            titles,
            [
                "Приходная накладная #1",
                "Приходная накладная #2",
                "Списание #1"
            ]
        );
        let options: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, value FROM options
             WHERE module = 'catalog' AND name LIKE 'store_document_numbers_%' ORDER BY name",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            options,
            [
                ("store_document_numbers_A".to_string(), "2".to_string()),
                ("store_document_numbers_D".to_string(), "1".to_string()),
            ]
        );

        let mut tx = db.begin().await.unwrap();
        let bad = create(&mut tx, &input("X", vec![]), None).await;
        assert!(matches!(bad, Err(StockError::Rejected(_))));
    }

    #[sqlx::test]
    async fn base_price_skips_tiered(db: PgPool) {
        let item = setup(&db).await;
        sqlx::query(
            "INSERT INTO catalog_prices (item_id, price_type_id, price, currency, quantity_from, quantity_to)
             VALUES ($1, 1, 100, 'RUB', 1, 9), ($1, 1, 90, 'RUB', 10, NULL)",
        )
        .bind(item)
        .execute(&db)
        .await
        .unwrap();
        let line = Line {
            base_price: Some(150.0),
            ..ln(item, None, Some(2), 1.0)
        };
        let id = create_doc(&db, &input("A", vec![line])).await;
        run(&db, id, false, None).await.unwrap();
        assert_eq!(base_prices(&db, item).await, [100.0, 90.0]);
    }

    #[sqlx::test]
    async fn documents_need_store_control(db: PgPool) {
        let item = setup(&db).await;
        let id = create_doc(&db, &input("S", vec![ln(item, None, Some(2), 1.0)])).await;
        let mut tx = db.begin().await.unwrap();
        set_store_control(&mut tx, false).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            ["Складской учёт выключен"]
        );
        assert_eq!(
            rejected(run(&db, id, true, None).await),
            ["Складской учёт выключен"]
        );
        assert_eq!(amount(&db, item, 2).await, 0.0);
    }

    #[sqlx::test]
    async fn conduct_waits_for_lock(db: PgPool) {
        let item = setup(&db).await;
        let first = create_doc(&db, &input("D", vec![ln(item, Some(1), None, 4.0)])).await;
        let second = create_doc(&db, &input("D", vec![ln(item, Some(1), None, 3.0)])).await;

        let mut a = db.begin().await.unwrap();
        conduct(&mut a, first, None).await.unwrap();

        let pool = db.clone();
        let b = tokio::spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            let r = conduct(&mut tx, second, None).await;
            tx.commit().await.unwrap();
            r
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!b.is_finished(), "B должна ждать блокировку A");
        a.commit().await.unwrap();

        assert_eq!(
            rejected(b.await.unwrap()),
            ["Указанное количество товара Товар больше, чем есть на складе Склад"]
        );
        assert_eq!(amount(&db, item, 1).await, 1.0);
        assert_eq!(status(&db, second).await.0, "N");
    }

    fn lnb(item: i64, from: Option<i64>, to: Option<i64>, codes: &[&str]) -> Line {
        Line {
            barcodes: codes.iter().map(|c| c.to_string()).collect(),
            ..ln(item, from, to, codes.len() as f64)
        }
    }

    async fn make_multi(db: &PgPool, item: i64) {
        sqlx::query("UPDATE catalog_products SET barcode_multi = TRUE WHERE item_id = $1")
            .bind(item)
            .execute(db)
            .await
            .unwrap();
    }

    /// Штрихкоды в базе: (код, склад).
    async fn codes_of(db: &PgPool, item: i64) -> Vec<(String, Option<i64>)> {
        sqlx::query_as(
            "SELECT barcode, store_id FROM catalog_barcodes WHERE item_id = $1 ORDER BY barcode",
        )
        .bind(item)
        .fetch_all(db)
        .await
        .unwrap()
    }

    fn cs(code: &str, store: Option<i64>) -> (String, Option<i64>) {
        (code.to_string(), store)
    }

    #[sqlx::test]
    async fn ordinary_line_barcodes_become_common(db: PgPool) {
        let item = setup(&db).await;
        let line = Line {
            barcodes: vec!["A1".into(), " A2 ".into(), "A1".into(), "".into()],
            ..ln(item, None, Some(1), 2.0)
        };
        let id = create_doc(&db, &input("A", vec![line])).await;
        run(&db, id, false, None).await.unwrap();
        assert_eq!(codes_of(&db, item).await, [cs("A1", None), cs("A2", None)]);
        // Отмена коды не трогает
        run(&db, id, true, None).await.unwrap();
        assert_eq!(codes_of(&db, item).await.len(), 2);
        // Повторное проведение: коды уже у этого товара
        run(&db, id, false, None).await.unwrap();
        assert_eq!(codes_of(&db, item).await.len(), 2);
    }

    #[sqlx::test]
    async fn ordinary_foreign_barcode_rejected(db: PgPool) {
        let item = setup(&db).await;
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name)
             SELECT collection_id, 'drugoy', 'Другой' FROM collection_items WHERE id = $1
             RETURNING id",
        )
        .bind(item)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO catalog_barcodes (item_id, barcode) VALUES ($1, 'F1')")
            .bind(other)
            .execute(&db)
            .await
            .unwrap();
        let id = create_doc(&db, &input("A", vec![lnb(item, None, Some(1), &["F1"])])).await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            [format!(
                "Штрихкод F1 товара #{item} (Товар) привязан к товару {other}"
            )]
        );
        assert_eq!(status(&db, id).await.0, "N");
        assert_eq!(amount(&db, item, 1).await, 5.0);
    }

    #[sqlx::test]
    async fn multi_count_mismatch_rejected(db: PgPool) {
        let item = setup(&db).await;
        make_multi(&db, item).await;
        let two = Line {
            barcodes: vec!["M1".into()],
            ..ln(item, None, Some(1), 2.0)
        };
        let id = create_doc(&db, &input("A", vec![two])).await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            [format!(
                "В документе складского учета число штрихкодов товара #{item} (Товар) не совпадает с количеством самого товара"
            )]
        );
        // Сумма сходится, по строкам — нет
        let a = Line {
            barcodes: vec!["M1".into(), "M2".into()],
            ..ln(item, None, Some(1), 1.0)
        };
        let b = Line {
            barcodes: vec![],
            ..ln(item, None, Some(1), 1.0)
        };
        let id = create_doc(&db, &input("A", vec![a, b])).await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            [format!(
                "Не совпадает количество товара #{item} (Товар) и число штрихкодов в строке документа складского учета"
            )]
        );
        assert!(codes_of(&db, item).await.is_empty());
    }

    #[sqlx::test]
    async fn multi_arrival_binds_and_cancel_removes(db: PgPool) {
        let item = setup(&db).await;
        make_multi(&db, item).await;
        let id = create_doc(
            &db,
            &input("A", vec![lnb(item, None, Some(2), &["M1", "M2"])]),
        )
        .await;
        run(&db, id, false, None).await.unwrap();
        assert_eq!(
            codes_of(&db, item).await,
            [cs("M1", Some(2)), cs("M2", Some(2))]
        );
        assert_eq!(amount(&db, item, 2).await, 2.0);
        run(&db, id, true, None).await.unwrap();
        assert!(codes_of(&db, item).await.is_empty());
        // Код уже на складе у этого товара
        run(&db, id, false, None).await.unwrap();
        let again = create_doc(&db, &input("A", vec![lnb(item, None, Some(1), &["M1"])])).await;
        assert_eq!(
            rejected(run(&db, again, false, None).await),
            ["Штрихкод M1, введенный для товара Товар, уже существует."]
        );
    }

    #[sqlx::test]
    async fn multi_moving_moves_and_back(db: PgPool) {
        let item = setup(&db).await;
        make_multi(&db, item).await;
        let arr = create_doc(&db, &input("A", vec![lnb(item, None, Some(1), &["M1"])])).await;
        run(&db, arr, false, None).await.unwrap();
        let mv = create_doc(&db, &input("M", vec![lnb(item, Some(1), Some(2), &["M1"])])).await;
        run(&db, mv, false, None).await.unwrap();
        assert_eq!(codes_of(&db, item).await, [cs("M1", Some(2))]);
        run(&db, mv, true, None).await.unwrap();
        assert_eq!(codes_of(&db, item).await, [cs("M1", Some(1))]);
    }

    #[sqlx::test]
    async fn multi_moving_wrong_store_rejected(db: PgPool) {
        let item = setup(&db).await;
        make_multi(&db, item).await;
        let arr = create_doc(&db, &input("A", vec![lnb(item, None, Some(2), &["M1"])])).await;
        run(&db, arr, false, None).await.unwrap();
        // Остаток на складе 1 есть, но код лежит на складе 2
        let mv = create_doc(&db, &input("M", vec![lnb(item, Some(1), Some(2), &["M1"])])).await;
        assert_eq!(
            rejected(run(&db, mv, false, None).await),
            ["На складе Склад нет товара Товар со штрихкодом M1"]
        );
        assert_eq!(codes_of(&db, item).await, [cs("M1", Some(2))]);
        assert_eq!(amount(&db, item, 1).await, 5.0);
    }

    #[sqlx::test]
    async fn multi_deduct_removes_and_cancel_restores(db: PgPool) {
        let item = setup(&db).await;
        make_multi(&db, item).await;
        let arr = create_doc(
            &db,
            &input("A", vec![lnb(item, None, Some(1), &["M1", "M2"])]),
        )
        .await;
        run(&db, arr, false, None).await.unwrap();
        let d = create_doc(&db, &input("D", vec![lnb(item, Some(1), None, &["M1"])])).await;
        run(&db, d, false, None).await.unwrap();
        assert_eq!(codes_of(&db, item).await, [cs("M2", Some(1))]);
        run(&db, d, true, None).await.unwrap();
        assert_eq!(
            codes_of(&db, item).await,
            [cs("M1", Some(1)), cs("M2", Some(1))]
        );
    }

    #[sqlx::test]
    async fn same_code_ordinary_and_multi_rejected(db: PgPool) {
        let item = setup(&db).await;
        make_multi(&db, item).await;
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name)
             SELECT collection_id, 'drugoy', 'Другой' FROM collection_items WHERE id = $1
             RETURNING id",
        )
        .bind(item)
        .fetch_one(&db)
        .await
        .unwrap();
        // Сначала обычный товар, потом поштучный
        let id = create_doc(
            &db,
            &input(
                "A",
                vec![
                    lnb(other, None, Some(1), &["X"]),
                    lnb(item, None, Some(1), &["X"]),
                ],
            ),
        )
        .await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            [format!(
                "Штрихкод X товара #{item} (Товар) привязан к товару {other}"
            )]
        );
        // Сначала поштучный, потом обычный
        let id = create_doc(
            &db,
            &input(
                "A",
                vec![
                    lnb(item, None, Some(1), &["X"]),
                    lnb(other, None, Some(1), &["X"]),
                ],
            ),
        )
        .await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            [format!(
                "Штрихкод X товара #{other} (Другой) привязан к товару {item}"
            )]
        );
        assert!(codes_of(&db, item).await.is_empty());
        assert!(codes_of(&db, other).await.is_empty());
    }

    #[sqlx::test]
    async fn duplicate_barcode_in_document_rejected(db: PgPool) {
        let item = setup(&db).await;
        make_multi(&db, item).await;
        let id = create_doc(
            &db,
            &input(
                "A",
                vec![
                    lnb(item, None, Some(1), &["M1"]),
                    lnb(item, None, Some(2), &["M1"]),
                ],
            ),
        )
        .await;
        assert_eq!(
            rejected(run(&db, id, false, None).await),
            ["Штрихкод M1, введенный для товара Товар, уже существует."]
        );
        assert!(codes_of(&db, item).await.is_empty());
    }

    #[sqlx::test]
    async fn multi_enabled_after_stock_requires_barcodes(db: PgPool) {
        let item = setup(&db).await;
        let arr = create_doc(&db, &input("A", vec![ln(item, None, Some(1), 2.0)])).await;
        run(&db, arr, false, None).await.unwrap();
        make_multi(&db, item).await;
        let d = create_doc(&db, &input("D", vec![lnb(item, Some(1), None, &["X1"])])).await;
        assert_eq!(
            rejected(run(&db, d, false, None).await),
            ["На складе Склад нет товара Товар со штрихкодом X1"]
        );
        assert_eq!(amount(&db, item, 1).await, 7.0);
    }

    #[sqlx::test]
    async fn barcodes_ignored_without_multi_flag(db: PgPool) {
        let item = setup(&db).await;
        let id = create_doc(
            &db,
            &input("A", vec![lnb(item, None, Some(2), &["Z1", "Z2"])]),
        )
        .await;
        run(&db, id, false, None).await.unwrap();
        assert_eq!(amount(&db, item, 2).await, 2.0);
        assert_eq!(quantity(&db, item).await, 7.0);
        run(&db, id, true, None).await.unwrap();
        assert_eq!(amount(&db, item, 2).await, 0.0);
        assert_eq!(quantity(&db, item).await, 5.0);
    }
}
