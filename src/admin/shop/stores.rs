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
    /// Склад по умолчанию (один на систему).
    pub is_default: Option<String>,
    /// Пункт выдачи.
    pub issuing_center: Option<String>,
    /// Отгрузочный склад.
    pub shipping_center: Option<String>,
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
    pub is_default: bool,
    pub issuing_center: bool,
    pub shipping_center: bool,
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
            is_default: self.is_default.is_some(),
            issuing_center: self.issuing_center.is_some(),
            shipping_center: self.shipping_center.is_some(),
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
    is_default: bool,
    issuing_center: bool,
    shipping_center: bool,
}

const COLS: &str = "id, name, active, sort, address, phone, email, schedule, \
     is_default, issuing_center, shipping_center, (extra ->> 'uf_city_id')::bigint AS city_id";

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
        is_default: row.is_default.then(|| "on".into()),
        issuing_center: row.issuing_center.then(|| "on".into()),
        shipping_center: row.shipping_center.then(|| "on".into()),
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
    let mut tx = state.db.begin().await?;
    // Склады со складскими остатками: сначала блокировка товаров (порядок по id, как при оформлении),
    // потом изменение активности — иначе пересчёт может разойтись с параллельным заказом
    let item_ids: Vec<i64> = match id {
        Some(id) => {
            sqlx::query_scalar(
                "SELECT DISTINCT item_id FROM catalog_store_amounts WHERE store_id = $1 ORDER BY item_id",
            )
            .bind(id)
            .fetch_all(&mut *tx)
            .await?
        }
        None => Vec::new(),
    };
    crate::catalog::load_locked(&mut tx, &item_ids).await?;
    // Склад по умолчанию один: частичный уникальный индекс, поэтому прежний снимается первым
    if input.is_default {
        sqlx::query("UPDATE catalog_stores SET is_default = FALSE WHERE is_default AND id IS DISTINCT FROM $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    let sql = match id {
        Some(_) => {
            "UPDATE catalog_stores SET name = $2, active = $3, sort = $4, address = $5, phone = $6,
                    email = $7, schedule = $8,
                    extra = CASE WHEN $9::jsonb IS NULL THEN extra - 'uf_city_id'
                                 ELSE extra || jsonb_build_object('uf_city_id', $9::jsonb) END,
                    is_default = $10, issuing_center = $11, shipping_center = $12
             WHERE id = $1 RETURNING id"
        }
        None => {
            "INSERT INTO catalog_stores (name, active, sort, address, phone, email, schedule, extra,
                                        is_default, issuing_center, shipping_center)
             SELECT $2, $3, $4, $5, $6, $7, $8,
                    CASE WHEN $9::jsonb IS NULL THEN '{}'::jsonb
                         ELSE jsonb_build_object('uf_city_id', $9::jsonb) END,
                    $10, $11, $12
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
        .bind(input.is_default)
        .bind(input.issuing_center)
        .bind(input.shipping_center)
        .fetch_optional(&mut *tx)
        .await?;
    saved.ok_or(AppError::NotFound)?;
    // Активность склада входит в доступное количество товаров
    crate::stock::recalc_quantity(&mut tx, &item_ids).await?;
    tx.commit().await?;
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

    use crate::{
        smoke_tests::post_form,
        stock::set_store_control,
        test_support::{admin_cookie, order_fixture, test_state},
    };
    use sqlx::PgPool;

    #[sqlx::test]
    async fn store_default_unique(db: PgPool) {
        order_fixture(&db).await;
        sqlx::query("INSERT INTO catalog_stores (id, name) VALUES (2, 'Второй')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE catalog_stores SET is_default = TRUE WHERE id = 1")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s, _) = post_form(
            &app,
            "/admin/shop/stores/2",
            &cookie,
            "name=%D0%92%D1%82%D0%BE%D1%80%D0%BE%D0%B9&sort=100&active=on&is_default=on&issuing_center=on",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let rows: Vec<(i64, bool, bool, bool)> = sqlx::query_as(
            "SELECT id, is_default, issuing_center, shipping_center FROM catalog_stores ORDER BY id",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(rows, vec![(1, false, false, false), (2, true, true, false)]);
    }

    #[sqlx::test]
    async fn store_deactivate_recalcs(db: PgPool) {
        let f = order_fixture(&db).await;
        let mut tx = db.begin().await.unwrap();
        set_store_control(&mut tx, true).await.unwrap();
        tx.commit().await.unwrap();
        let (quantity, _) = crate::test_support::amounts(&db, f.product_id).await;
        assert_eq!(quantity, 5.0);
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        // без active — склад снимается с активности
        let (s, _) = post_form(
            &app,
            "/admin/shop/stores/1",
            &cookie,
            "name=%D0%A1%D0%BA%D0%BB%D0%B0%D0%B4&sort=100",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let (quantity, _) = crate::test_support::amounts(&db, f.product_id).await;
        assert_eq!(quantity, 0.0);
    }
}
