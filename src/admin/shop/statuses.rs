//! Статусы заказа (как «Статусы» магазина Битрикса).

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
    error::{AppError, AppResult, is_unique_violation},
    state::AppState,
};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct StatusForm {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub description: String,
    pub notify: Option<String>,
}

#[derive(Debug)]
pub struct StatusInput {
    pub code: String,
    pub name: String,
    pub sort: i32,
    pub description: String,
    pub notify: bool,
}

impl StatusForm {
    pub fn validate(&self) -> Result<StatusInput, String> {
        let code = self.code.trim();
        if code.is_empty() || code.len() > 2 || !code.chars().all(|c| c.is_ascii_uppercase()) {
            return Err("Код статуса — 1–2 латинские заглавные буквы".into());
        }
        let name = self.name.trim();
        Ok(StatusInput {
            code: code.to_string(),
            name: if name.is_empty() { code } else { name }.to_string(),
            sort: parse_sort(&self.sort),
            description: self.description.trim().to_string(),
            notify: self.notify.is_some(),
        })
    }
}

#[derive(FromRow, Serialize)]
struct StatusRow {
    code: String,
    name: String,
    sort: i32,
    notify: bool,
    orders: i64,
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<UsedQuery>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let items: Vec<StatusRow> = sqlx::query_as(
        "SELECT s.code, s.name, s.sort, s.notify,
                (SELECT count(*) FROM orders o WHERE o.status = s.code) AS orders
         FROM order_statuses s ORDER BY s.sort, s.code",
    )
    .fetch_all(&state.db)
    .await?;
    render(
        &state,
        "shop/statuses.html",
        context! { user, items, error => q.message() },
    )
}

fn form_page(
    state: &AppState,
    user: Access,
    code: Option<String>,
    form: StatusForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    render(
        state,
        "shop/status_form.html",
        context! { user, code, form, error },
    )
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let form = StatusForm {
        sort: "100".into(),
        ..Default::default()
    };
    form_page(&state, user, None, form, None)
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(code): Path<String>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let (name, sort, description, notify): (String, i32, String, bool) = sqlx::query_as(
        "SELECT name, sort, description, notify FROM order_statuses WHERE code = $1",
    )
    .bind(&code)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let form = StatusForm {
        code: code.clone(),
        name,
        sort: sort.to_string(),
        description,
        notify: notify.then(|| "on".into()),
    };
    form_page(&state, user, Some(code), form, None)
}

async fn save(
    state: &AppState,
    user: Access,
    old: Option<String>,
    form: StatusForm,
) -> AppResult<Response> {
    let input = match form.validate() {
        Ok(i) => i,
        Err(e) => return Ok(form_page(state, user, old, form, Some(e))?.into_response()),
    };
    let query = match &old {
        // Код можно сменить: заказы обновятся каскадом
        Some(_) => sqlx::query(
            "UPDATE order_statuses SET code = $2, name = $3, sort = $4, description = $5, notify = $6
             WHERE code = $1",
        ),
        None => sqlx::query(
            "INSERT INTO order_statuses (code, name, sort, description, notify)
             SELECT $2, $3, $4, $5, $6 WHERE $1::text IS NULL",
        ),
    };
    let result = query
        .bind(&old)
        .bind(&input.code)
        .bind(&input.name)
        .bind(input.sort)
        .bind(&input.description)
        .bind(input.notify)
        .execute(&state.db)
        .await;
    match result {
        Ok(r) if r.rows_affected() == 0 => Err(AppError::NotFound),
        Ok(_) => Ok(Redirect::to("/admin/shop/statuses").into_response()),
        Err(e) if is_unique_violation(&e) => Ok(form_page(
            state,
            user,
            old,
            form,
            Some("Статус с таким кодом уже есть".into()),
        )?
        .into_response()),
        Err(e) => Err(e.into()),
    }
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<StatusForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, None, form).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(code): Path<String>,
    Form(form): Form<StatusForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, Some(code), form).await
}

/// Удаляет статус, если он не используется в заказах.
pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(code): Path<String>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let used: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM orders WHERE status = $1)
              + (SELECT count(*) FROM order_status_history WHERE status = $1)",
    )
    .bind(&code)
    .fetch_one(&state.db)
    .await?;
    if used > 0 {
        return Ok(Redirect::to(&format!("/admin/shop/statuses?used={used}")).into_response());
    }
    sqlx::query("DELETE FROM order_statuses WHERE code = $1")
        .bind(&code)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to("/admin/shop/statuses").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_input_code() {
        let form = |code: &str| StatusForm {
            code: code.into(),
            name: String::new(),
            ..Default::default()
        };
        let input = form("N").validate().unwrap();
        assert_eq!((input.code.as_str(), input.name.as_str()), ("N", "N"));
        assert_eq!(form("DF").validate().unwrap().code, "DF");
        for bad in ["", "n", "ABC", "Я"] {
            assert_eq!(
                form(bad).validate().unwrap_err(),
                "Код статуса — 1–2 латинские заглавные буквы"
            );
        }
    }
}
