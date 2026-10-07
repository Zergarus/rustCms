//! Типы плательщика.

use axum::{
    Extension,
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::Form;
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use super::{UsedQuery, require_shop};
use crate::{
    access::Access,
    admin::{parse_sort, render},
    error::{AppError, AppResult},
    state::AppState,
};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct PersonTypeForm {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub sort: String,
    pub active: Option<String>,
}

#[derive(Debug)]
pub struct PersonTypeInput {
    pub code: String,
    pub name: String,
    pub sort: i32,
    pub active: bool,
}

impl PersonTypeForm {
    pub fn validate(&self) -> Result<PersonTypeInput, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Укажите название".into());
        }
        Ok(PersonTypeInput {
            code: self.code.trim().to_string(),
            name: name.to_string(),
            sort: parse_sort(&self.sort),
            active: self.active.is_some(),
        })
    }
}

#[derive(FromRow, Serialize)]
struct PersonTypeRow {
    id: i64,
    code: String,
    name: String,
    active: bool,
    sort: i32,
    properties: i64,
    orders: i64,
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<UsedQuery>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let items: Vec<PersonTypeRow> = sqlx::query_as(
        "SELECT t.id, t.code, t.name, t.active, t.sort,
                (SELECT count(*) FROM order_properties p WHERE p.person_type_id = t.id) AS properties,
                (SELECT count(*) FROM orders o WHERE o.person_type_id = t.id) AS orders
         FROM person_types t ORDER BY t.sort, t.id",
    )
    .fetch_all(&state.db)
    .await?;
    render(
        &state,
        "shop/person_types.html",
        context! { user, items, error => q.message() },
    )
}

fn form_page(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: PersonTypeForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    render(
        state,
        "shop/person_type_form.html",
        context! { user, id, form, error },
    )
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let form = PersonTypeForm {
        sort: "100".into(),
        active: Some("on".into()),
        ..Default::default()
    };
    form_page(&state, user, None, form, None)
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let (code, name, sort, active): (String, String, i32, bool) =
        sqlx::query_as("SELECT code, name, sort, active FROM person_types WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?;
    let form = PersonTypeForm {
        code,
        name,
        sort: sort.to_string(),
        active: active.then(|| "on".into()),
    };
    form_page(&state, user, Some(id), form, None)
}

async fn save(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: PersonTypeForm,
) -> AppResult<Response> {
    let input = match form.validate() {
        Ok(i) => i,
        Err(e) => return Ok(form_page(state, user, id, form, Some(e))?.into_response()),
    };
    let saved: Option<i64> = match id {
        Some(id) => sqlx::query_scalar(
            "UPDATE person_types SET code = $2, name = $3, sort = $4, active = $5 WHERE id = $1 RETURNING id",
        )
        .bind(id),
        None => sqlx::query_scalar(
            "INSERT INTO person_types (code, name, sort, active)
             SELECT $2, $3, $4, $5 WHERE $1::bigint IS NULL RETURNING id",
        )
        .bind(None::<i64>),
    }
    .bind(&input.code)
    .bind(&input.name)
    .bind(input.sort)
    .bind(input.active)
    .fetch_optional(&state.db)
    .await?;
    let saved = saved.ok_or(AppError::NotFound)?;
    Ok(Redirect::to(&format!("/admin/shop/person-types/{saved}/props")).into_response())
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<PersonTypeForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, None, form).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<PersonTypeForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, Some(id), form).await
}

/// Удаляет тип плательщика без заказов вместе с его группами и свойствами.
pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let used: i64 = sqlx::query_scalar("SELECT count(*) FROM orders WHERE person_type_id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    if used > 0 {
        return Ok(Redirect::to(&format!("/admin/shop/person-types?used={used}")).into_response());
    }
    sqlx::query("DELETE FROM person_types WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to("/admin/shop/person-types").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn person_type_name_required() {
        let form = PersonTypeForm {
            name: "  ".into(),
            ..Default::default()
        };
        assert_eq!(form.validate().unwrap_err(), "Укажите название");
    }
}
