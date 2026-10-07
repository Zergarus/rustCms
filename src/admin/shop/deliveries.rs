//! Службы доставки и склады самовывоза.

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
pub struct DeliveryForm {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub active: Option<String>,
    pub public: Option<String>,
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub price: String,
    #[serde(default)]
    pub currency: String,
    #[serde(default)]
    pub store_ids: Vec<i64>,
}

#[derive(Debug)]
pub struct DeliveryInput {
    pub code: String,
    pub name: String,
    pub description: String,
    pub active: bool,
    pub public: bool,
    pub sort: i32,
    pub price: f64,
    pub currency: String,
    pub store_ids: Vec<i64>,
}

impl DeliveryForm {
    pub fn validate(&self) -> Result<DeliveryInput, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Укажите название".into());
        }
        let raw = self.price.trim().replace(',', ".").replace(' ', "");
        let price = if raw.is_empty() {
            0.0
        } else {
            raw.parse::<f64>()
                .ok()
                .filter(|p| p.is_finite() && *p >= 0.0)
                .ok_or("Цена — число не меньше нуля")?
        };
        let currency = self.currency.trim().to_uppercase();
        Ok(DeliveryInput {
            code: self.code.trim().to_string(),
            name: name.to_string(),
            description: self.description.trim().to_string(),
            active: self.active.is_some(),
            public: self.public.is_some(),
            sort: parse_sort(&self.sort),
            price,
            currency: if currency.is_empty() {
                "RUB".into()
            } else {
                currency
            },
            store_ids: self.store_ids.clone(),
        })
    }
}

#[derive(FromRow, Serialize)]
struct DeliveryRow {
    id: i64,
    name: String,
    active: bool,
    public: bool,
    sort: i32,
    price: f64,
    currency: String,
    stores: String,
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<UsedQuery>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let items: Vec<DeliveryRow> = sqlx::query_as(
        "SELECT d.id, d.name, d.active, d.public, d.sort, d.price::float8 AS price, d.currency,
                COALESCE(string_agg(s.name, ', ' ORDER BY s.sort, s.id), '') AS stores
         FROM deliveries d
         LEFT JOIN delivery_stores ds ON ds.delivery_id = d.id
         LEFT JOIN catalog_stores s ON s.id = ds.store_id
         GROUP BY d.id ORDER BY d.sort, d.id",
    )
    .fetch_all(&state.db)
    .await?;
    render(
        &state,
        "shop/deliveries.html",
        context! { user, items, error => q.message() },
    )
}

async fn form_page(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: DeliveryForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let stores: Vec<(i64, String, bool)> =
        sqlx::query_as("SELECT id, name, active FROM catalog_stores ORDER BY sort, id")
            .fetch_all(&state.db)
            .await?;
    render(
        state,
        "shop/delivery_form.html",
        context! { user, id, form, error, stores },
    )
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let form = DeliveryForm {
        sort: "100".into(),
        price: "0".into(),
        currency: "RUB".into(),
        active: Some("on".into()),
        public: Some("on".into()),
        ..Default::default()
    };
    form_page(&state, user, None, form, None).await
}

#[derive(FromRow)]
struct DeliveryEdit {
    code: String,
    name: String,
    description: String,
    active: bool,
    public: bool,
    sort: i32,
    price: f64,
    currency: String,
    store_ids: Vec<i64>,
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let d: DeliveryEdit = sqlx::query_as(
        "SELECT code, name, description, active, public, sort, price::float8 AS price, currency,
                ARRAY(SELECT store_id FROM delivery_stores WHERE delivery_id = $1) AS store_ids
         FROM deliveries WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let form = DeliveryForm {
        code: d.code,
        name: d.name,
        description: d.description,
        active: d.active.then(|| "on".into()),
        public: d.public.then(|| "on".into()),
        sort: d.sort.to_string(),
        price: d.price.to_string(),
        currency: d.currency,
        store_ids: d.store_ids,
    };
    form_page(&state, user, Some(id), form, None).await
}

async fn save(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: DeliveryForm,
) -> AppResult<Response> {
    let input = match form.validate() {
        Ok(i) => i,
        Err(e) => {
            return Ok(form_page(state, user, id, form, Some(e))
                .await?
                .into_response());
        }
    };
    let mut tx = state.db.begin().await?;
    let saved: Option<i64> = match id {
        Some(id) => sqlx::query_scalar(
            "UPDATE deliveries SET code = $2, name = $3, description = $4, active = $5, public = $6,
                 sort = $7, price = $8, currency = $9 WHERE id = $1 RETURNING id",
        )
        .bind(id),
        None => sqlx::query_scalar(
            "INSERT INTO deliveries (code, name, description, active, public, sort, price, currency)
             SELECT $2, $3, $4, $5, $6, $7, $8, $9 WHERE $1::bigint IS NULL RETURNING id",
        )
        .bind(None::<i64>),
    }
    .bind(&input.code)
    .bind(&input.name)
    .bind(&input.description)
    .bind(input.active)
    .bind(input.public)
    .bind(input.sort)
    .bind(input.price)
    .bind(&input.currency)
    .fetch_optional(&mut *tx)
    .await?;
    let saved = saved.ok_or(AppError::NotFound)?;
    sqlx::query("DELETE FROM delivery_stores WHERE delivery_id = $1")
        .bind(saved)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO delivery_stores (delivery_id, store_id)
         SELECT $1, id FROM catalog_stores WHERE id = ANY($2)",
    )
    .bind(saved)
    .bind(&input.store_ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Redirect::to("/admin/shop/deliveries").into_response())
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<DeliveryForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, None, form).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<DeliveryForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, Some(id), form).await
}

/// Удаляет службу, если по ней нет отгрузок.
pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let used: i64 = sqlx::query_scalar("SELECT count(*) FROM shipments WHERE delivery_id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    if used > 0 {
        return Ok(Redirect::to(&format!("/admin/shop/deliveries?used={used}")).into_response());
    }
    sqlx::query("DELETE FROM deliveries WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to("/admin/shop/deliveries").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_input_price() {
        let form = |price: &str| DeliveryForm {
            name: "До ТК".into(),
            price: price.into(),
            store_ids: vec![1, 3],
            ..Default::default()
        };
        let input = form("300,50").validate().unwrap();
        assert_eq!(input.price, 300.5);
        assert_eq!(input.store_ids, vec![1, 3]);
        assert_eq!(input.currency, "RUB");
        assert_eq!(form("").validate().unwrap().price, 0.0);
        assert_eq!(
            form("-1").validate().unwrap_err(),
            "Цена — число не меньше нуля"
        );
    }
}
