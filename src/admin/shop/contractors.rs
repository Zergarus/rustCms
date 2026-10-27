//! Поставщики складского учёта (как «Поставщики» торгового каталога Битрикса).

use axum::{
    Extension, Form,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use super::require_shop;
use crate::{
    access::Access,
    admin::render,
    error::{AppError, AppResult},
    state::AppState,
};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ContractorForm {
    /// `I` — физлицо, `C` — юрлицо.
    #[serde(default)]
    pub person_type: String,
    #[serde(default)]
    pub person_name: String,
    #[serde(default)]
    pub company: String,
    #[serde(default)]
    pub inn: String,
    #[serde(default)]
    pub kpp: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub post_index: String,
}

impl ContractorForm {
    /// Проверка: у юрлица обязательна компания, у физлица — имя.
    fn validate(&self) -> Result<(), String> {
        match self.person_type.as_str() {
            "I" if self.person_name.trim().is_empty() => Err("Укажите имя поставщика".into()),
            "C" if self.company.trim().is_empty() => Err("Укажите название компании".into()),
            "I" | "C" => Ok(()),
            _ => Err("Неверный тип поставщика".into()),
        }
    }
}

#[derive(FromRow, Serialize)]
struct ContractorRow {
    id: i64,
    person_type: String,
    person_name: String,
    company: String,
    inn: String,
    phone: String,
    email: String,
    docs: i64,
}

async fn render_list(
    state: &AppState,
    user: Access,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let items: Vec<ContractorRow> = sqlx::query_as(
        "SELECT c.id, c.person_type::text, c.person_name, c.company, c.inn, c.phone, c.email,
                (SELECT count(*) FROM store_documents d WHERE d.contractor_id = c.id) AS docs
         FROM contractors c ORDER BY lower(CASE WHEN c.person_type = 'C' THEN c.company
                                                ELSE c.person_name END), c.id",
    )
    .fetch_all(&state.db)
    .await?;
    render(
        state,
        "shop/contractors.html",
        context! { user, items, error },
    )
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    render_list(&state, user, None).await
}

fn render_form(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: ContractorForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    render(
        state,
        "shop/contractor_form.html",
        context! { user, id, form, error },
    )
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let form = ContractorForm {
        person_type: "C".into(),
        ..Default::default()
    };
    render_form(&state, user, None, form, None)
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let (person_type, person_name, company, inn, kpp, address, phone, email, post_index): (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT person_type::text, person_name, company, inn, kpp, address, phone, email,
                post_index
         FROM contractors WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let form = ContractorForm {
        person_type,
        person_name,
        company,
        inn,
        kpp,
        address,
        phone,
        email,
        post_index,
    };
    render_form(&state, user, Some(id), form, None)
}

async fn save(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: ContractorForm,
) -> AppResult<Response> {
    if let Err(e) = form.validate() {
        return Ok(render_form(state, user, id, form, Some(e))?.into_response());
    }
    let sql = match id {
        Some(_) => {
            "UPDATE contractors SET person_type = $2, person_name = $3, company = $4, inn = $5,
                    kpp = $6, address = $7, phone = $8, email = $9, post_index = $10,
                    updated_at = now()
             WHERE id = $1 RETURNING id"
        }
        None => {
            "INSERT INTO contractors (person_type, person_name, company, inn, kpp, address, phone,
                                      email, post_index)
             SELECT $2, $3, $4, $5, $6, $7, $8, $9, $10
             WHERE $1::bigint IS NULL RETURNING id"
        }
    };
    let saved: Option<i64> = sqlx::query_scalar(sql)
        .bind(id)
        .bind(&form.person_type)
        .bind(form.person_name.trim())
        .bind(form.company.trim())
        .bind(form.inn.trim())
        .bind(form.kpp.trim())
        .bind(form.address.trim())
        .bind(form.phone.trim())
        .bind(form.email.trim())
        .bind(form.post_index.trim())
        .fetch_optional(&state.db)
        .await?;
    saved.ok_or(AppError::NotFound)?;
    Ok(Redirect::to("/admin/shop/contractors").into_response())
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<ContractorForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, None, form).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<ContractorForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, Some(id), form).await
}

/// Удаление; поставщик, указанный в документах, остаётся (внешний ключ RESTRICT).
pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let used: i64 =
        sqlx::query_scalar("SELECT count(*) FROM store_documents WHERE contractor_id = $1")
            .bind(id)
            .fetch_one(&state.db)
            .await?;
    if used > 0 {
        let message = format!("Поставщик указан в документах: {used}");
        return Ok(render_list(&state, user, Some(message))
            .await?
            .into_response());
    }
    sqlx::query("DELETE FROM contractors WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to("/admin/shop/contractors").into_response())
}
