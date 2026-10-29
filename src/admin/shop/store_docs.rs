//! Документы складского учёта: приход, оприходование, перемещение, возврат, списание.
//! Логика проведения — в `crate::stock::documents`, здесь только формы и список.

use axum::{
    Extension,
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::Form;
use chrono::NaiveDate;
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use super::{escape, require_shop};
use crate::{
    access::Access,
    admin::render,
    error::{AppError, AppResult},
    state::AppState,
    stock::{
        self, StockError,
        documents::{self, DocInput, DocType, Line},
    },
};

const TYPES: [DocType; 5] = [
    DocType::Arrival,
    DocType::Adjustment,
    DocType::Moving,
    DocType::Return,
    DocType::Deduct,
];

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default, rename = "type")]
    doc_type: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    store: String,
    #[serde(default)]
    contractor: String,
    #[serde(default)]
    date_from: String,
    #[serde(default)]
    date_to: String,
}

#[derive(FromRow, Serialize)]
struct DocRow {
    id: i64,
    doc_type: String,
    title: String,
    doc_number: String,
    date: String,
    total: f64,
    currency: String,
    status: String,
}

/// Страница «Складской учёт выключен» вместо раздела.
fn disabled(state: &AppState, user: Access) -> AppResult<Response> {
    Ok(render(
        state,
        "shop/store_docs.html",
        context! { user, disabled => true },
    )?
    .into_response())
}

async fn control_on(state: &AppState) -> AppResult<bool> {
    let mut conn = state.db.acquire().await?;
    Ok(stock::settings(&mut conn).await?.store_control)
}

fn type_options() -> Vec<minijinja::Value> {
    TYPES
        .iter()
        .map(|t| context! { code => t.code(), name => t.name() })
        .collect()
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<ListQuery>,
) -> AppResult<Response> {
    require_shop(&user)?;
    if !control_on(&state).await? {
        return disabled(&state, user);
    }
    let doc_type = DocType::from_code(&q.doc_type).map(DocType::code);
    let status = matches!(q.status.as_str(), "N" | "Y").then_some(q.status.as_str());
    let items: Vec<DocRow> = sqlx::query_as(
        "SELECT id, doc_type::text, title, doc_number, to_char(date_document, 'DD.MM.YYYY') AS date,
                total::float8, currency, status::text
         FROM store_documents
         WHERE ($1::text IS NULL OR doc_type = $1) AND ($2::text IS NULL OR status = $2)
           AND ($3::bigint IS NULL OR EXISTS (
                SELECT 1 FROM store_document_items i
                WHERE i.doc_id = store_documents.id AND ($3 IN (i.store_from, i.store_to))))
           AND ($4::bigint IS NULL OR contractor_id = $4)
           AND ($5::date IS NULL OR COALESCE(date_document, created_at)::date >= $5)
           AND ($6::date IS NULL OR COALESCE(date_document, created_at)::date <= $6)
         ORDER BY date_document DESC, id DESC",
    )
    .bind(doc_type)
    .bind(status)
    .bind(parse_id(&q.store))
    .bind(parse_id(&q.contractor))
    .bind(parse_date(&q.date_from, "").ok().flatten())
    .bind(parse_date(&q.date_to, "").ok().flatten())
    .fetch_all(&state.db)
    .await?;
    let stores: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM catalog_stores ORDER BY sort, id")
            .fetch_all(&state.db)
            .await?;
    let stores: Vec<minijinja::Value> = stores
        .into_iter()
        .map(|(id, name)| context! { id, name })
        .collect();
    let contractors = contractor_options(&state).await?;
    let (filter_store, filter_contractor) = (q.store.trim(), q.contractor.trim());
    let (date_from, date_to) = (q.date_from.trim(), q.date_to.trim());
    let types = type_options();
    let (filter_type, filter_status) = (doc_type.unwrap_or(""), status.unwrap_or(""));
    Ok(render(
        &state,
        "shop/store_docs.html",
        context! {
            user, items, types, filter_type, filter_status, stores, contractors,
            filter_store, filter_contractor, date_from, date_to,
        },
    )?
    .into_response())
}

/// Поля формы документа; строки — параллельные массивы `line_*[]`.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct DocForm {
    #[serde(default)]
    pub doc_type: String,
    #[serde(default)]
    pub doc_number: String,
    #[serde(default)]
    pub date_document: String,
    #[serde(default)]
    pub contractor_id: String,
    #[serde(default)]
    pub currency: String,
    #[serde(default)]
    pub items_order_date: String,
    #[serde(default)]
    pub items_received_date: String,
    #[serde(default)]
    pub commentary: String,
    #[serde(default, rename = "line_item[]")]
    pub line_item: Vec<String>,
    #[serde(default, rename = "line_store_from[]")]
    pub line_store_from: Vec<String>,
    #[serde(default, rename = "line_store_to[]")]
    pub line_store_to: Vec<String>,
    #[serde(default, rename = "line_amount[]")]
    pub line_amount: Vec<String>,
    #[serde(default, rename = "line_purchasing_price[]")]
    pub line_purchasing_price: Vec<String>,
}

fn parse_id(raw: &str) -> Option<i64> {
    raw.trim().parse().ok()
}

fn parse_number(raw: &str, label: &str) -> Result<Option<f64>, String> {
    let raw = raw.trim().replace(',', ".");
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .map(Some)
        .ok_or_else(|| format!("Неверное число в поле «{label}»: {raw}"))
}

fn parse_date(raw: &str, label: &str) -> Result<Option<NaiveDate>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| format!("Неверная дата в поле «{label}»"))
}

impl DocForm {
    fn cell(values: &[String], i: usize) -> &str {
        values.get(i).map_or("", String::as_str)
    }

    /// Поля документа; полностью пустые строки отбрасываются.
    fn to_input(&self, responsible_id: Option<i64>) -> Result<DocInput, String> {
        let mut lines = Vec::new();
        for i in 0..self.line_item.len() {
            let item = Self::cell(&self.line_item, i);
            let amount = Self::cell(&self.line_amount, i);
            if item.trim().is_empty() && amount.trim().is_empty() {
                continue;
            }
            lines.push(Line {
                row_id: 0,
                item_id: parse_id(item),
                store_from: parse_id(Self::cell(&self.line_store_from, i)),
                store_to: parse_id(Self::cell(&self.line_store_to, i)),
                amount: parse_number(amount, "Количество")?.unwrap_or(0.0),
                purchasing_price: parse_number(
                    Self::cell(&self.line_purchasing_price, i),
                    "Закупочная цена",
                )?,
                base_price: None,
                barcodes: Vec::new(),
            });
        }
        Ok(DocInput {
            doc_type: self.doc_type.clone(),
            doc_number: self.doc_number.trim().to_string(),
            date_document: parse_date(&self.date_document, "Дата")?,
            contractor_id: parse_id(&self.contractor_id),
            currency: self.currency.trim().to_string(),
            items_order_date: parse_date(&self.items_order_date, "Дата заказа")?,
            items_received_date: parse_date(&self.items_received_date, "Дата получения")?,
            responsible_id,
            commentary: self.commentary.trim().to_string(),
            lines,
        })
    }
}

#[derive(FromRow)]
struct DocHead {
    doc_type: String,
    title: String,
    doc_number: String,
    date_document: String,
    contractor_id: Option<i64>,
    currency: String,
    items_order_date: String,
    items_received_date: String,
    commentary: String,
    status: String,
    total: f64,
}

#[derive(FromRow, Serialize)]
struct LineRow {
    item_id: Option<i64>,
    item_name: String,
    store_from: Option<i64>,
    store_to: Option<i64>,
    amount: f64,
    purchasing_price: Option<f64>,
}

const HEAD_SQL: &str = "SELECT doc_type::text, title, doc_number,
        to_char(date_document, 'YYYY-MM-DD') AS date_document, contractor_id, currency,
        COALESCE(to_char(items_order_date, 'YYYY-MM-DD'), '') AS items_order_date,
        COALESCE(to_char(items_received_date, 'YYYY-MM-DD'), '') AS items_received_date,
        commentary, status::text, total::float8
    FROM store_documents WHERE id = $1";

/// Строки документа для формы: названия товаров подтягиваются из каталога.
async fn load_lines(state: &AppState, id: i64) -> sqlx::Result<Vec<LineRow>> {
    sqlx::query_as(
        "SELECT i.item_id, COALESCE(ci.name, '') AS item_name, i.store_from, i.store_to,
                i.amount::float8 AS amount, i.purchasing_price::float8 AS purchasing_price
         FROM store_document_items i LEFT JOIN collection_items ci ON ci.id = i.item_id
         WHERE i.doc_id = $1 ORDER BY i.id",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
}

/// Строки из отправленной формы (при ошибке) с названиями товаров.
async fn form_lines(state: &AppState, form: &DocForm) -> sqlx::Result<Vec<LineRow>> {
    let mut rows = Vec::new();
    for i in 0..form.line_item.len() {
        let item = DocForm::cell(&form.line_item, i);
        let amount = DocForm::cell(&form.line_amount, i);
        if item.trim().is_empty() && amount.trim().is_empty() {
            continue;
        }
        let item_id = parse_id(item);
        let item_name: Option<String> = match item_id {
            Some(id) => {
                sqlx::query_scalar("SELECT name FROM collection_items WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&state.db)
                    .await?
            }
            None => None,
        };
        rows.push(LineRow {
            item_id,
            item_name: item_name.unwrap_or_default(),
            store_from: parse_id(DocForm::cell(&form.line_store_from, i)),
            store_to: parse_id(DocForm::cell(&form.line_store_to, i)),
            amount: parse_number(amount, "").ok().flatten().unwrap_or(0.0),
            purchasing_price: parse_number(DocForm::cell(&form.line_purchasing_price, i), "")
                .ok()
                .flatten(),
        });
    }
    Ok(rows)
}

/// Все поставщики для выпадающих списков.
async fn contractor_options(state: &AppState) -> sqlx::Result<Vec<minijinja::Value>> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, CASE WHEN person_type = 'C' THEN company ELSE person_name END
         FROM contractors ORDER BY 2, id",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name)| context! { id, name })
        .collect())
}

/// Форма документа: `id` — существующий (проведённый показывается только для чтения).
async fn render_form(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: DocForm,
    lines: Vec<LineRow>,
    meta: (String, String, f64),
    errors: Vec<String>,
) -> AppResult<Html<String>> {
    let (status, title, total) = meta;
    let doc_type = DocType::from_code(&form.doc_type).ok_or(AppError::NotFound)?;
    let stores: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM catalog_stores WHERE active ORDER BY sort, id")
            .fetch_all(&state.db)
            .await?;
    let stores: Vec<minijinja::Value> = stores
        .into_iter()
        .map(|(id, name)| context! { id, name })
        .collect();
    let contractors = contractor_options(state).await?;
    let type_code = doc_type.code();
    let type_name = doc_type.name();
    let uses_source = matches!(doc_type, DocType::Moving | DocType::Deduct);
    let uses_destination = doc_type != DocType::Deduct;
    let uses_contractor = doc_type == DocType::Arrival;
    render(
        state,
        "shop/store_doc_form.html",
        context! {
            user, id, form, lines, status, title, total, errors, stores, contractors,
            type_code, type_name, uses_source, uses_destination, uses_contractor,
        },
    )
}

#[derive(Deserialize)]
pub struct NewQuery {
    #[serde(default, rename = "type")]
    doc_type: String,
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<NewQuery>,
) -> AppResult<Response> {
    require_shop(&user)?;
    if !control_on(&state).await? {
        return disabled(&state, user);
    }
    let doc_type = DocType::from_code(&q.doc_type).ok_or(AppError::NotFound)?;
    let form = DocForm {
        doc_type: doc_type.code().into(),
        ..Default::default()
    };
    let meta = (
        "N".to_string(),
        format!("Новый документ: {}", doc_type.name()),
        0.0,
    );
    Ok(
        render_form(&state, user, None, form, Vec::new(), meta, Vec::new())
            .await?
            .into_response(),
    )
}

/// Страница существующего документа.
async fn show(state: &AppState, user: Access, id: i64, errors: Vec<String>) -> AppResult<Response> {
    let head: DocHead = sqlx::query_as(HEAD_SQL)
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let form = DocForm {
        doc_type: head.doc_type,
        doc_number: head.doc_number,
        date_document: head.date_document,
        contractor_id: head
            .contractor_id
            .map(|c| c.to_string())
            .unwrap_or_default(),
        currency: head.currency,
        items_order_date: head.items_order_date,
        items_received_date: head.items_received_date,
        commentary: head.commentary,
        ..Default::default()
    };
    let lines = load_lines(state, id).await?;
    let meta = (head.status, head.title, head.total);
    Ok(
        render_form(state, user, Some(id), form, lines, meta, errors)
            .await?
            .into_response(),
    )
}

pub async fn view(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    require_shop(&user)?;
    if !control_on(&state).await? {
        return disabled(&state, user);
    }
    show(&state, user, id, Vec::new()).await
}

fn rejected(e: StockError) -> Result<Vec<String>, AppError> {
    match e {
        StockError::Rejected(messages) => Ok(messages),
        StockError::Db(e) => Err(e.into()),
    }
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<DocForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    if !control_on(&state).await? {
        return disabled(&state, user);
    }
    let user_id = user.user.id;
    let result = match form.to_input(Some(user_id)) {
        Ok(input) => {
            let mut tx = state.db.begin().await?;
            match documents::create(&mut tx, &input, Some(user_id)).await {
                Ok(id) => {
                    tx.commit().await?;
                    return Ok(
                        Redirect::to(&format!("/admin/shop/store-docs/{id}")).into_response()
                    );
                }
                Err(e) => rejected(e)?,
            }
        }
        Err(message) => vec![message],
    };
    let lines = form_lines(&state, &form).await?;
    let title = DocType::from_code(&form.doc_type)
        .map(|t| format!("Новый документ: {}", t.name()))
        .unwrap_or_default();
    Ok(render_form(
        &state,
        user,
        None,
        form,
        lines,
        ("N".into(), title, 0.0),
        result,
    )
    .await?
    .into_response())
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<DocForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    if !control_on(&state).await? {
        return disabled(&state, user);
    }
    let user_id = user.user.id;
    let responsible: Option<i64> =
        sqlx::query_scalar("SELECT responsible_id FROM store_documents WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?;
    let errors = match form.to_input(responsible) {
        Ok(input) => {
            let mut tx = state.db.begin().await?;
            match documents::update(&mut tx, id, &input, Some(user_id)).await {
                Ok(()) => {
                    tx.commit().await?;
                    return Ok(
                        Redirect::to(&format!("/admin/shop/store-docs/{id}")).into_response()
                    );
                }
                Err(e) => rejected(e)?,
            }
        }
        Err(message) => vec![message],
    };
    // Форма с введёнными значениями и сообщениями; тип, статус и название — из документа
    let head: DocHead = sqlx::query_as(HEAD_SQL)
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    let lines = form_lines(&state, &form).await?;
    let form = DocForm {
        doc_type: head.doc_type,
        ..form
    };
    Ok(render_form(
        &state,
        user,
        Some(id),
        form,
        lines,
        (head.status, head.title, head.total),
        errors,
    )
    .await?
    .into_response())
}

#[derive(Clone, Copy)]
enum Action {
    Conduct,
    Cancel,
    Delete,
}

async fn act(state: AppState, user: Access, id: i64, action: Action) -> AppResult<Response> {
    require_shop(&user)?;
    if !control_on(&state).await? {
        return disabled(&state, user);
    }
    let user_id = Some(user.user.id);
    let mut tx = state.db.begin().await?;
    let result = match action {
        Action::Conduct => documents::conduct(&mut tx, id, user_id).await,
        Action::Cancel => documents::cancel(&mut tx, id, user_id).await,
        Action::Delete => documents::delete(&mut tx, id).await,
    };
    match result {
        Ok(()) => {
            tx.commit().await?;
            let to = match action {
                Action::Delete => "/admin/shop/store-docs".to_string(),
                _ => format!("/admin/shop/store-docs/{id}"),
            };
            Ok(Redirect::to(&to).into_response())
        }
        Err(e) => {
            let errors = rejected(e)?;
            tx.rollback().await?;
            show(&state, user, id, errors).await
        }
    }
}

pub async fn conduct(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    act(state, user, id, Action::Conduct).await
}

pub async fn cancel(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    act(state, user, id, Action::Cancel).await
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    act(state, user, id, Action::Delete).await
}

#[derive(Deserialize)]
pub struct ProductQuery {
    #[serde(default)]
    q: String,
}

/// Варианты товаров для строки документа (HTMX): до 20 по названию, коду и артикулу.
pub async fn products(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<ProductQuery>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let q = q.q.trim();
    let mut html = String::from("<option value=\"\">— выберите товар —</option>");
    if q.chars().count() >= 2 {
        let pattern = format!(
            "%{}%",
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        // Артикул — системное поле CML2_ARTICLE (приходит из 1С)
        let rows: Vec<(i64, String, Option<String>)> = sqlx::query_as(
            "SELECT ci.id, ci.name, ci.field_values ->> 'cml2_article'
             FROM collection_items ci JOIN collections c ON c.id = ci.collection_id
             WHERE c.is_catalog
               AND (ci.name ILIKE $1 OR ci.code ILIKE $1
                    OR ci.field_values ->> 'cml2_article' ILIKE $1)
             ORDER BY lower(ci.name), ci.id LIMIT 20",
        )
        .bind(pattern)
        .fetch_all(&state.db)
        .await?;
        for (id, name, article) in rows {
            let article = article
                .filter(|a| !a.is_empty())
                .map(|a| format!(" [{a}]"))
                .unwrap_or_default();
            html.push_str(&format!(
                "<option value=\"{id}\">{}</option>",
                escape(&format!("{name}{article} (#{id})"))
            ));
        }
    }
    Ok(Html(html))
}

#[cfg(test)]
mod tests {
    use crate::{
        smoke_tests::{get, post_form},
        stock::set_store_control,
        test_support::{admin_cookie, order_fixture, test_state},
    };
    use sqlx::PgPool;

    async fn control(db: &PgPool, on: bool) {
        let mut tx = db.begin().await.unwrap();
        set_store_control(&mut tx, on).await.unwrap();
        tx.commit().await.unwrap();
    }

    async fn contractor(db: &PgPool) -> i64 {
        sqlx::query_scalar("INSERT INTO contractors (company) VALUES ('ООО Поставка') RETURNING id")
            .fetch_one(db)
            .await
            .unwrap()
    }

    fn arrival_body(item: i64, contractor: Option<i64>) -> String {
        format!(
            "doc_type=A&doc_number=15&contractor_id={}&line_item%5B%5D={item}&line_store_from%5B%5D=\
             &line_store_to%5B%5D=1&line_amount%5B%5D=3&line_purchasing_price%5B%5D=10",
            contractor.map(|c| c.to_string()).unwrap_or_default()
        )
    }

    async fn first_doc(db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT max(id) FROM store_documents")
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn docs_list_filters(db: PgPool) {
        let f = order_fixture(&db).await;
        control(&db, true).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let c = contractor(&db).await;
        for t in ["A", "S", "D"] {
            let body =
                arrival_body(f.product_id, Some(c)).replace("doc_type=A", &format!("doc_type={t}"));
            let (s, _) = post_form(&app, "/admin/shop/store-docs", &cookie, &body).await;
            assert!(s.is_redirection(), "{t}: {s}");
        }
        let (_, html) = get(&app, "/admin/shop/store-docs", &cookie).await;
        for want in [
            "Номер",
            "Тип",
            "Дата",
            "Сумма",
            "Статус",
            "Приходная накладная",
            "Оприходование",
            "Списание",
        ] {
            assert!(html.contains(want), "{want}: {html}");
        }
        let (_, html) = get(&app, "/admin/shop/store-docs?type=A", &cookie).await;
        assert!(html.contains("Приходная накладная #1"), "{html}");
        assert!(!html.contains("Оприходование #1"), "{html}");
        // Проводим приход; остальные — черновики
        let arrival: i64 =
            sqlx::query_scalar("SELECT id FROM store_documents WHERE doc_type = 'A'")
                .fetch_one(&db)
                .await
                .unwrap();
        let (s, _) = post_form(
            &app,
            &format!("/admin/shop/store-docs/{arrival}/conduct"),
            &cookie,
            "",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let (_, html) = get(&app, "/admin/shop/store-docs?status=Y", &cookie).await;
        assert!(html.contains("Приходная накладная #1"), "{html}");
        assert!(!html.contains("Списание #1"), "{html}");
    }

    #[sqlx::test]
    async fn docs_list_filters_store_contractor_date(db: PgPool) {
        let f = order_fixture(&db).await;
        control(&db, true).await;
        sqlx::query("INSERT INTO catalog_stores (id, name) VALUES (2, 'Второй')")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let c = contractor(&db).await;
        let other: i64 =
            sqlx::query_scalar("INSERT INTO contractors (company) VALUES ('Другой') RETURNING id")
                .fetch_one(&db)
                .await
                .unwrap();
        // Приход на склад 1 от c (10.01), оприходование на склад 2 от other (20.01)
        let a = arrival_body(f.product_id, Some(c)) + "&date_document=2026-01-10";
        post_form(&app, "/admin/shop/store-docs", &cookie, &a).await;
        let b = arrival_body(f.product_id, Some(other))
            .replace("doc_type=A", "doc_type=S")
            .replace("line_store_to%5B%5D=1", "line_store_to%5B%5D=2")
            + "&date_document=2026-01-20";
        post_form(&app, "/admin/shop/store-docs", &cookie, &b).await;
        let url = |q: &str| format!("/admin/shop/store-docs?{q}");
        let (_, h) = get(&app, &url("store=2"), &cookie).await;
        assert!(
            h.contains("Оприходование #1") && !h.contains("Приходная накладная #1"),
            "{h}"
        );
        let (_, h) = get(&app, &url(&format!("contractor={c}")), &cookie).await;
        assert!(
            h.contains("Приходная накладная #1") && !h.contains("Оприходование #1"),
            "{h}"
        );
        let (_, h) = get(&app, &url("date_from=2026-01-15"), &cookie).await;
        assert!(
            h.contains("Оприходование #1") && !h.contains("Приходная накладная #1"),
            "{h}"
        );
        let (_, h) = get(&app, &url("date_to=2026-01-10"), &cookie).await;
        assert!(
            h.contains("Приходная накладная #1") && !h.contains("Оприходование #1"),
            "{h}"
        );
        let (_, h) = get(
            &app,
            &url("date_from=2026-01-10&date_to=2026-01-20"),
            &cookie,
        )
        .await;
        assert!(
            h.contains("Приходная накладная #1") && h.contains("Оприходование #1"),
            "{h}"
        );
        // Значения фильтров сохраняются в форме
        let (_, h) = get(&app, &url("store=2&date_from=2026-01-15"), &cookie).await;
        assert!(
            h.contains("value=\"2026-01-15\"") && h.contains("value=\"2\" selected"),
            "{h}"
        );
    }

    #[sqlx::test]
    async fn doc_create_conduct_flow(db: PgPool) {
        let f = order_fixture(&db).await;
        control(&db, true).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let c = contractor(&db).await;
        let (s, _) = post_form(
            &app,
            "/admin/shop/store-docs",
            &cookie,
            &arrival_body(f.product_id, Some(c)),
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let id = first_doc(&db).await;
        let (_, html) = get(&app, &format!("/admin/shop/store-docs/{id}"), &cookie).await;
        assert!(
            html.contains("class=\"btn primary\" type=\"submit\">Сохранить</button>")
                && html.contains("Провести"),
            "{html}"
        );
        let before: f64 = sqlx::query_scalar(
            "SELECT amount::float8 FROM catalog_store_amounts WHERE item_id = $1 AND store_id = 1",
        )
        .bind(f.product_id)
        .fetch_one(&db)
        .await
        .unwrap();
        let (s, _) = post_form(
            &app,
            &format!("/admin/shop/store-docs/{id}/conduct"),
            &cookie,
            "",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let after: f64 = sqlx::query_scalar(
            "SELECT amount::float8 FROM catalog_store_amounts WHERE item_id = $1 AND store_id = 1",
        )
        .bind(f.product_id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(after, before + 3.0);
        let (_, html) = get(&app, &format!("/admin/shop/store-docs/{id}"), &cookie).await;
        // проведённый документ только для чтения: кнопки сохранения формы документа нет
        assert!(
            !html.contains("class=\"btn primary\" type=\"submit\">Сохранить</button>"),
            "{html}"
        );
        assert!(html.contains("Отменить проведение"), "{html}");
        // Отмена возвращает остаток
        let (s, _) = post_form(
            &app,
            &format!("/admin/shop/store-docs/{id}/cancel"),
            &cookie,
            "",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let back: f64 = sqlx::query_scalar(
            "SELECT amount::float8 FROM catalog_store_amounts WHERE item_id = $1 AND store_id = 1",
        )
        .bind(f.product_id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(back, before);
        // Черновик можно изменить и удалить
        let body = arrival_body(f.product_id, Some(c))
            .replace("line_amount%5B%5D=3", "line_amount%5B%5D=4,5");
        let (s, _) = post_form(
            &app,
            &format!("/admin/shop/store-docs/{id}"),
            &cookie,
            &body,
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let amount: f64 =
            sqlx::query_scalar("SELECT amount::float8 FROM store_document_items WHERE doc_id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(amount, 4.5);
        let (s, _) = post_form(
            &app,
            &format!("/admin/shop/store-docs/{id}/delete"),
            &cookie,
            "",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        assert_eq!(first_doc_count(&db).await, 0);
    }

    async fn first_doc_count(db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM store_documents")
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn doc_conduct_errors_shown(db: PgPool) {
        let f = order_fixture(&db).await;
        control(&db, true).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s, _) = post_form(
            &app,
            "/admin/shop/store-docs",
            &cookie,
            &arrival_body(f.product_id, None),
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let id = first_doc(&db).await;
        let (s, html) = post_form(
            &app,
            &format!("/admin/shop/store-docs/{id}/conduct"),
            &cookie,
            "",
        )
        .await;
        assert!(s.is_success(), "{s}");
        assert!(html.contains("Не указан поставщик."), "{html}");
        assert!(
            html.contains("class=\"btn primary\" type=\"submit\">Сохранить</button>"),
            "{html}"
        );
        let status: String =
            sqlx::query_scalar("SELECT status::text FROM store_documents WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(status, "N");
    }

    #[sqlx::test]
    async fn docs_hidden_without_store_control(db: PgPool) {
        order_fixture(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s, html) = get(&app, "/admin/shop/store-docs", &cookie).await;
        assert!(s.is_success(), "{s}");
        assert!(html.contains("Складской учёт выключен"), "{html}");
        assert!(html.contains("/admin/shop/settings"), "{html}");
        assert!(!html.contains("href=\"/admin/shop/store-docs\""), "{html}");
        let (_, html) = get(&app, "/admin/shop/stores", &cookie).await;
        assert!(!html.contains("/admin/shop/store-docs"), "{html}");
        control(&db, true).await;
        let (_, html) = get(&app, "/admin/shop/stores", &cookie).await;
        assert!(html.contains("href=\"/admin/shop/store-docs\""), "{html}");
    }

    #[sqlx::test]
    async fn contractor_in_use_not_deleted(db: PgPool) {
        let f = order_fixture(&db).await;
        control(&db, true).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let c = contractor(&db).await;
        post_form(
            &app,
            "/admin/shop/store-docs",
            &cookie,
            &arrival_body(f.product_id, Some(c)),
        )
        .await;
        let (s, html) = post_form(
            &app,
            &format!("/admin/shop/contractors/{c}/delete"),
            &cookie,
            "",
        )
        .await;
        assert!(s.is_success(), "{s}");
        assert!(html.contains("Поставщик указан в документах: 1"), "{html}");
        // Свободный поставщик удаляется
        let (s, _) = post_form(
            &app,
            "/admin/shop/contractors",
            &cookie,
            "person_type=C&company=Другой",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let other: i64 = sqlx::query_scalar("SELECT max(id) FROM contractors")
            .fetch_one(&db)
            .await
            .unwrap();
        let (s, _) = post_form(
            &app,
            &format!("/admin/shop/contractors/{other}/delete"),
            &cookie,
            "",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
    }

    #[sqlx::test]
    async fn products_search(db: PgPool) {
        order_fixture(&db).await;
        control(&db, true).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (_, html) = get(
            &app,
            "/admin/shop/store-docs/products?q=%D0%A2%D0%BE%D0%B2",
            &cookie,
        )
        .await;
        assert!(html.contains("<option") && html.contains("Товар"), "{html}");
        let (_, html) = get(
            &app,
            "/admin/shop/store-docs/products?q=%D0%AF%D0%AF%D0%AF",
            &cookie,
        )
        .await;
        assert!(!html.contains("Товар"), "{html}");
    }

    #[sqlx::test]
    async fn products_search_by_article_and_contractor_crud(db: PgPool) {
        let f = order_fixture(&db).await;
        control(&db, true).await;
        sqlx::query("UPDATE collection_items SET field_values = '{\"cml2_article\": \"ART-77\"}' WHERE id = $1")
            .bind(f.product_id)
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (_, html) = get(&app, "/admin/shop/store-docs/products?q=ART-77", &cookie).await;
        assert!(html.contains("[ART-77]"), "{html}");
        // Поставщик: проверка и правка
        let (s, html) = post_form(
            &app,
            "/admin/shop/contractors",
            &cookie,
            "person_type=C&company=",
        )
        .await;
        assert!(
            s.is_success() && html.contains("Укажите название компании"),
            "{html}"
        );
        let (s, _) = post_form(
            &app,
            "/admin/shop/contractors",
            &cookie,
            "person_type=C&company=Ромашка&inn=123",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let (_, html) = get(&app, "/admin/shop/contractors", &cookie).await;
        assert!(html.contains("Ромашка"), "{html}");
        let id: i64 = sqlx::query_scalar("SELECT max(id) FROM contractors")
            .fetch_one(&db)
            .await
            .unwrap();
        let (s, _) = post_form(
            &app,
            &format!("/admin/shop/contractors/{id}"),
            &cookie,
            "person_type=I&person_name=Иван",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let (_, html) = get(&app, &format!("/admin/shop/contractors/{id}"), &cookie).await;
        assert!(html.contains("value=\"Иван\""), "{html}");
    }
}
