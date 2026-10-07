//! Склады (как «Склады» торгового каталога Битрикса).

use axum::{
    Extension, Form,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use super::{location_labels, require_shop};
use crate::{
    access::Access,
    admin::{parse_sort, render},
    error::{AppError, AppResult},
    state::AppState,
};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct StoreForm {
    #[serde(default)]
    pub name: String,
    pub active: Option<String>,
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub schedule: String,
    /// Id местоположения города склада (`UF_CITY_ID`).
    #[serde(default)]
    pub city_id: String,
}

#[derive(Debug)]
pub struct StoreInput {
    pub name: String,
    pub active: bool,
    pub sort: i32,
    pub address: String,
    pub phone: String,
    pub email: String,
    pub schedule: String,
    pub city_id: Option<i64>,
}

impl StoreForm {
    pub fn validate(&self) -> Result<StoreInput, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Укажите название".into());
        }
        let city_id = match self.city_id.trim() {
            "" => None,
            raw => Some(raw.parse().map_err(|_| "Неверный город".to_string())?),
        };
        Ok(StoreInput {
            name: name.to_string(),
            active: self.active.is_some(),
            sort: parse_sort(&self.sort),
            address: self.address.trim().to_string(),
            phone: self.phone.trim().to_string(),
            email: self.email.trim().to_string(),
            schedule: self.schedule.trim().to_string(),
            city_id,
        })
    }
}

#[derive(FromRow, Serialize)]
struct StoreRow {
    id: i64,
    name: String,
    active: bool,
    sort: i32,
    address: String,
    phone: String,
    email: String,
    schedule: String,
    city_id: Option<i64>,
}

const COLS: &str = "id, name, active, sort, address, phone, email, schedule, \
     (extra ->> 'uf_city_id')::bigint AS city_id";

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let items: Vec<StoreRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM catalog_stores ORDER BY sort, id"
    )))
    .fetch_all(&state.db)
    .await?;
    let ids: Vec<i64> = items.iter().filter_map(|s| s.city_id).collect();
    let cities: std::collections::HashMap<String, String> = location_labels(&state, &ids)
        .await?
        .into_iter()
        .map(|(id, l)| (id.to_string(), l))
        .collect();
    render(&state, "shop/stores.html", context! { user, items, cities })
}

async fn render_form(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: StoreForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let city_label = match form.city_id.trim().parse::<i64>() {
        Ok(city) => location_labels(state, &[city]).await?.pop().map(|(_, l)| l),
        Err(_) => None,
    };
    render(
        state,
        "shop/store_form.html",
        context! { user, id, form, error, city_label },
    )
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let form = StoreForm {
        active: Some("on".into()),
        sort: "100".into(),
        ..Default::default()
    };
    render_form(&state, user, None, form, None).await
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let row: StoreRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM catalog_stores WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let form = StoreForm {
        name: row.name,
        active: row.active.then(|| "on".into()),
        sort: row.sort.to_string(),
        address: row.address,
        phone: row.phone,
        email: row.email,
        schedule: row.schedule,
        city_id: row.city_id.map(|c| c.to_string()).unwrap_or_default(),
    };
    render_form(&state, user, Some(id), form, None).await
}

async fn save(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: StoreForm,
) -> AppResult<Response> {
    let input = match form.validate() {
        Ok(input) => input,
        Err(e) => {
            return Ok(render_form(state, user, id, form, Some(e))
                .await?
                .into_response());
        }
    };
    // Город — UF-поле склада: пустое значение убирает ключ
    let city = input.city_id.map(serde_json::Value::from);
    let sql = match id {
        Some(_) => {
            "UPDATE catalog_stores SET name = $2, active = $3, sort = $4, address = $5, phone = $6,
                    email = $7, schedule = $8,
                    extra = CASE WHEN $9::jsonb IS NULL THEN extra - 'uf_city_id'
                                 ELSE extra || jsonb_build_object('uf_city_id', $9::jsonb) END
             WHERE id = $1 RETURNING id"
        }
        None => {
            "INSERT INTO catalog_stores (name, active, sort, address, phone, email, schedule, extra)
             SELECT $2, $3, $4, $5, $6, $7, $8,
                    CASE WHEN $9::jsonb IS NULL THEN '{}'::jsonb
                         ELSE jsonb_build_object('uf_city_id', $9::jsonb) END
             WHERE $1::bigint IS NULL RETURNING id"
        }
    };
    let saved: Option<i64> = sqlx::query_scalar(sql)
        .bind(id)
        .bind(&input.name)
        .bind(input.active)
        .bind(input.sort)
        .bind(&input.address)
        .bind(&input.phone)
        .bind(&input.email)
        .bind(&input.schedule)
        .bind(city.map(sqlx::types::Json))
        .fetch_optional(&state.db)
        .await?;
    saved.ok_or(AppError::NotFound)?;
    Ok(Redirect::to("/admin/shop/stores").into_response())
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<StoreForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, None, form).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<StoreForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, Some(id), form).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(name: &str, sort: &str, city: &str) -> StoreForm {
        StoreForm {
            name: name.into(),
            sort: sort.into(),
            city_id: city.into(),
            ..Default::default()
        }
    }

    #[test]
    fn store_form_validation() {
        assert_eq!(
            form(" ", "10", "").validate().unwrap_err(),
            "Укажите название"
        );
        assert_eq!(form("Склад", "abc", "").validate().unwrap().sort, 500);
        assert_eq!(
            form("Склад", "10", "12").validate().unwrap().city_id,
            Some(12)
        );
        assert_eq!(form("Склад", "10", "").validate().unwrap().city_id, None);
    }
}
