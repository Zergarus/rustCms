//! Типы цен (как «Типы цен» торгового каталога Битрикса).

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
    admin::{parse_sort, render},
    error::{AppError, AppResult, is_unique_violation},
    state::AppState,
};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct PriceTypeForm {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub name: String,
    pub is_base: Option<String>,
    #[serde(default)]
    pub sort: String,
}

#[derive(Debug)]
pub struct PriceTypeInput {
    pub code: String,
    pub name: String,
    pub is_base: bool,
    pub sort: i32,
}

impl PriceTypeForm {
    pub fn validate(&self) -> Result<PriceTypeInput, String> {
        let code = self.code.trim();
        if code.is_empty() {
            return Err("Укажите код".into());
        }
        let name = self.name.trim();
        Ok(PriceTypeInput {
            code: code.to_string(),
            name: if name.is_empty() {
                code.to_string()
            } else {
                name.to_string()
            },
            is_base: self.is_base.is_some(),
            sort: parse_sort(&self.sort),
        })
    }
}

#[derive(FromRow, Serialize)]
struct PriceTypeRow {
    id: i64,
    code: String,
    name: String,
    is_base: bool,
    sort: i32,
    prices: i64,
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let items: Vec<PriceTypeRow> = sqlx::query_as(
        "SELECT t.id, t.code, t.name, t.is_base, t.sort,
                (SELECT count(*) FROM catalog_prices p WHERE p.price_type_id = t.id) AS prices
         FROM catalog_price_types t ORDER BY t.sort, t.id",
    )
    .fetch_all(&state.db)
    .await?;
    render(
        &state,
        "shop/price_types.html",
        context! { user, items, error => None::<String> },
    )
}

fn form_page(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: PriceTypeForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    render(
        state,
        "shop/price_type_form.html",
        context! { user, id, form, error },
    )
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    form_page(
        &state,
        user,
        None,
        PriceTypeForm {
            sort: "100".into(),
            ..Default::default()
        },
        None,
    )
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let (code, name, is_base, sort): (String, String, bool, i32) =
        sqlx::query_as("SELECT code, name, is_base, sort FROM catalog_price_types WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?;
    let form = PriceTypeForm {
        code,
        name,
        is_base: is_base.then(|| "on".into()),
        sort: sort.to_string(),
    };
    form_page(&state, user, Some(id), form, None)
}

/// Сохраняет тип цены; базовый тип — только один, у остальных флаг снимается.
async fn save(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: PriceTypeForm,
) -> AppResult<Response> {
    let input = match form.validate() {
        Ok(i) => i,
        Err(e) => return Ok(form_page(state, user, id, form, Some(e))?.into_response()),
    };
    let mut tx = state.db.begin().await?;
    let saved: Result<i64, sqlx::Error> = match id {
        Some(id) => {
            sqlx::query_scalar(
                "UPDATE catalog_price_types SET code = $2, name = $3, is_base = $4, sort = $5
                 WHERE id = $1 RETURNING id",
            )
            .bind(id)
            .bind(&input.code)
            .bind(&input.name)
            .bind(input.is_base)
            .bind(input.sort)
            .fetch_one(&mut *tx)
            .await
        }
        None => {
            sqlx::query_scalar(
                "INSERT INTO catalog_price_types (code, name, is_base, sort) VALUES ($1, $2, $3, $4) RETURNING id",
            )
            .bind(&input.code)
            .bind(&input.name)
            .bind(input.is_base)
            .bind(input.sort)
            .fetch_one(&mut *tx)
            .await
        }
    };
    let saved_id = match saved {
        Ok(id) => id,
        Err(e) if is_unique_violation(&e) => {
            return Ok(form_page(
                state,
                user,
                id,
                form,
                Some("Тип с таким кодом уже есть".into()),
            )?
            .into_response());
        }
        Err(sqlx::Error::RowNotFound) => return Err(AppError::NotFound),
        Err(e) => return Err(e.into()),
    };
    if input.is_base {
        sqlx::query("UPDATE catalog_price_types SET is_base = FALSE WHERE id <> $1 AND is_base")
            .bind(saved_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Redirect::to("/admin/shop/price-types").into_response())
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<PriceTypeForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, None, form).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<PriceTypeForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, Some(id), form).await
}

/// Удаляет тип цены, если у него нет цен.
pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let used: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM catalog_prices WHERE price_type_id = $1)")
            .bind(id)
            .fetch_one(&state.db)
            .await?;
    if used {
        let items: Vec<PriceTypeRow> = sqlx::query_as(
            "SELECT t.id, t.code, t.name, t.is_base, t.sort,
                    (SELECT count(*) FROM catalog_prices p WHERE p.price_type_id = t.id) AS prices
             FROM catalog_price_types t ORDER BY t.sort, t.id",
        )
        .fetch_all(&state.db)
        .await?;
        let error = Some("У типа есть цены".to_string());
        return Ok(render(
            &state,
            "shop/price_types.html",
            context! { user, items, error },
        )?
        .into_response());
    }
    sqlx::query("DELETE FROM catalog_price_types WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to("/admin/shop/price-types").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_type_code_required() {
        let form = PriceTypeForm {
            code: " ".into(),
            name: "Опт".into(),
            ..Default::default()
        };
        assert_eq!(form.validate().unwrap_err(), "Укажите код");
    }
}
