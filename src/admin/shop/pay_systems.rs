//! Платёжные системы: тип для API и группы, которым доступна оплата.

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
    admin::{
        card::{TabErrors, tab_context},
        parse_sort, render,
    },
    error::{AppError, AppResult},
    state::AppState,
};

/// Типы платёжки для фронта (`payments[].type`).
pub const API_TYPES: &[(&str, &str)] = &[
    ("cashless", "Безналичная (cashless)"),
    ("cash", "Наличные (cash)"),
    ("document", "Счёт-документ (document)"),
    ("redirect", "Переход на оплату (redirect)"),
    ("qr", "QR-код (qr)"),
    ("other", "Другое (other)"),
];

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct PaySystemForm {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub active: Option<String>,
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub api_type: String,
    #[serde(default)]
    pub group_ids: Vec<i64>,
    #[serde(default)]
    pub handler: String,
    #[serde(default)]
    pub shop_id: String,
    #[serde(default)]
    pub secret_key: String,
    #[serde(default)]
    pub webhook_secret: String,
}

#[derive(Debug)]
pub struct PaySystemInput {
    pub code: String,
    pub name: String,
    pub description: String,
    pub active: bool,
    pub sort: i32,
    pub api_type: String,
    pub group_ids: Vec<i64>,
    pub handler: String,
    pub settings: serde_json::Value,
}

impl PaySystemForm {
    pub fn validate(&self) -> Result<PaySystemInput, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Укажите название".into());
        }
        let api_type = self.api_type.trim();
        if !API_TYPES.iter().any(|(t, _)| *t == api_type) {
            return Err("Неизвестный тип для API".into());
        }
        Ok(PaySystemInput {
            code: self.code.trim().to_string(),
            name: name.to_string(),
            description: self.description.trim().to_string(),
            active: self.active.is_some(),
            sort: parse_sort(&self.sort),
            api_type: api_type.to_string(),
            group_ids: self.group_ids.clone(),
            handler: self.handler.trim().to_string(),
            settings: serde_json::json!({
                "shop_id": self.shop_id.trim(),
                "secret_key": self.secret_key.trim(),
                "webhook_secret": self.webhook_secret.trim(),
            }),
        })
    }
}

#[derive(FromRow, Serialize)]
struct PaySystemRow {
    id: i64,
    name: String,
    active: bool,
    sort: i32,
    api_type: String,
    groups: String,
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<UsedQuery>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let search = q.search();
    let items: Vec<PaySystemRow> = sqlx::query_as(
        "SELECT p.id, p.name, p.active, p.sort, p.api_type,
                COALESCE((SELECT string_agg(g.name, ', ' ORDER BY g.sort, g.id)
                          FROM groups g WHERE g.id = ANY(p.group_ids)), '') AS groups
         FROM pay_systems p
         WHERE ($1 = '' OR p.name ILIKE $2 OR p.api_type ILIKE $2)
         ORDER BY p.sort, p.id",
    )
    .bind(&search)
    .bind(format!("%{search}%"))
    .fetch_all(&state.db)
    .await?;
    render(
        &state,
        "shop/pay_systems.html",
        context! { user, items, q => search, error => q.message() },
    )
}

async fn form_page(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: PaySystemForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let groups: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM groups ORDER BY sort, id")
            .fetch_all(&state.db)
            .await?;
    let api_types = API_TYPES;
    let card_tabs = vec![
        ("main", "Основное", None),
        ("access", "Доступ", Some(form.group_ids.len())),
        ("handler", "Обработчик", None),
    ];
    let handlers = [("", "Без обработчика"), ("yookassa", "ЮKassa")];
    let visible: Vec<&'static str> = card_tabs.iter().map(|tab| tab.0).collect();
    let tabctx = tab_context(&TabErrors::new(), &visible, None);
    render(
        state,
        "shop/pay_system_form.html",
        context! { user, id, form, error, groups, api_types, card_tabs, tabctx, handlers },
    )
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let form = PaySystemForm {
        sort: "100".into(),
        api_type: "other".into(),
        active: Some("on".into()),
        ..Default::default()
    };
    form_page(&state, user, None, form, None).await
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let (code, name, description, active, sort, api_type, group_ids, handler, settings): (
        String,
        String,
        String,
        bool,
        i32,
        String,
        Vec<i64>,
        String,
        serde_json::Value,
    ) = sqlx::query_as(
        "SELECT code, name, description, active, sort, api_type, group_ids, handler, settings
         FROM pay_systems WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let form = PaySystemForm {
        code,
        name,
        description,
        active: active.then(|| "on".into()),
        sort: sort.to_string(),
        api_type,
        group_ids,
        handler,
        shop_id: settings
            .get("shop_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        secret_key: settings
            .get("secret_key")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        webhook_secret: settings
            .get("webhook_secret")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    };
    form_page(&state, user, Some(id), form, None).await
}

async fn save(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: PaySystemForm,
) -> AppResult<Response> {
    let input = match form.validate() {
        Ok(i) => i,
        Err(e) => {
            return Ok(form_page(state, user, id, form, Some(e))
                .await?
                .into_response());
        }
    };
    let saved: Option<i64> = match id {
        Some(id) => sqlx::query_scalar(
            "UPDATE pay_systems SET code = $2, name = $3, description = $4, active = $5, sort = $6,
                 api_type = $7, group_ids = $8, handler = $9, settings = $10
             WHERE id = $1 RETURNING id",
        )
        .bind(id),
        None => sqlx::query_scalar(
            "INSERT INTO pay_systems (code, name, description, active, sort, api_type, group_ids, handler, settings)
             SELECT $2, $3, $4, $5, $6, $7, $8, $9, $10 WHERE $1::bigint IS NULL RETURNING id",
        )
        .bind(None::<i64>),
    }
    .bind(&input.code)
    .bind(&input.name)
    .bind(&input.description)
    .bind(input.active)
    .bind(input.sort)
    .bind(&input.api_type)
    .bind(&input.group_ids)
    .bind(&input.handler)
    .bind(&input.settings)
    .fetch_optional(&state.db)
    .await?;
    saved.ok_or(AppError::NotFound)?;
    Ok(Redirect::to("/admin/shop/pay-systems").into_response())
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<PaySystemForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, None, form).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<PaySystemForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, Some(id), form).await
}

/// Удаляет платёжку, если по ней нет оплат.
pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let used: i64 = sqlx::query_scalar("SELECT count(*) FROM payments WHERE pay_system_id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    if used > 0 {
        return Ok(Redirect::to(&format!("/admin/shop/pay-systems?used={used}")).into_response());
    }
    sqlx::query("DELETE FROM pay_systems WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to("/admin/shop/pay-systems").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn saves_handler_and_secrets(db: sqlx::PgPool) {
        use crate::smoke_tests::post_form;
        use crate::test_support::{admin_cookie, test_state};
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (status, _) = post_form(
            &app,
            "/admin/shop/pay-systems",
            &cookie,
            "name=QR+test&api_type=qr&handler=yookassa&shop_id=123&secret_key=sec&webhook_secret=hook",
        )
        .await;
        assert!(status.is_redirection(), "{status}");
        let (handler, shop, secret, hook): (String, String, String, String) = sqlx::query_as(
            "SELECT handler, settings->>'shop_id', settings->>'secret_key', settings->>'webhook_secret'
             FROM pay_systems WHERE name = 'QR test'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!((handler.as_str(), shop.as_str()), ("yookassa", "123"));
        assert_eq!((secret.as_str(), hook.as_str()), ("sec", "hook"));
    }

    #[test]
    fn pay_system_handler_settings() {
        let form = PaySystemForm {
            name: "QR".into(),
            api_type: "qr".into(),
            handler: "yookassa".into(),
            shop_id: " 123 ".into(),
            secret_key: " secret ".into(),
            webhook_secret: " hook ".into(),
            ..Default::default()
        };
        let input = form.validate().unwrap();
        assert_eq!(input.handler, "yookassa");
        assert_eq!(input.settings["shop_id"], "123");
        assert_eq!(input.settings["secret_key"], "secret");
        assert_eq!(input.settings["webhook_secret"], "hook");
    }

    #[test]
    fn pay_system_input_type() {
        let form = |api_type: &str| PaySystemForm {
            name: "Счёт".into(),
            api_type: api_type.into(),
            group_ids: vec![21],
            ..Default::default()
        };
        let input = form("document").validate().unwrap();
        assert_eq!(
            (input.api_type.as_str(), input.group_ids.clone()),
            ("document", vec![21])
        );
        assert_eq!(
            form("card").validate().unwrap_err(),
            "Неизвестный тип для API"
        );
    }
}
