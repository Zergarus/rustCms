//! Настройки торгового каталога: значения флагов товара по умолчанию.

use axum::{
    Extension, Form,
    extract::State,
    response::{Html, Redirect},
};
use minijinja::context;
use serde::Deserialize;

use super::require_shop;
use crate::{access::Access, admin::render, error::AppResult, state::AppState};

const QUANTITY_TRACE: &str = "default_quantity_trace";
const CAN_BUY_ZERO: &str = "default_can_buy_zero";

async fn flag(state: &AppState, name: &str, fallback: bool) -> sqlx::Result<bool> {
    let v: Option<String> =
        sqlx::query_scalar("SELECT value FROM options WHERE module = 'catalog' AND name = $1")
            .bind(name)
            .fetch_optional(&state.db)
            .await?;
    Ok(v.map_or(fallback, |v| v == "Y"))
}

pub async fn form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let quantity_trace = flag(&state, QUANTITY_TRACE, true).await?;
    let can_buy_zero = flag(&state, CAN_BUY_ZERO, false).await?;
    render(
        &state,
        "shop/settings.html",
        context! { user, quantity_trace, can_buy_zero },
    )
}

#[derive(Deserialize)]
pub struct SettingsForm {
    quantity_trace: Option<String>,
    can_buy_zero: Option<String>,
}

pub async fn save(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<SettingsForm>,
) -> AppResult<Redirect> {
    require_shop(&user)?;
    for (name, on) in [
        (QUANTITY_TRACE, form.quantity_trace.is_some()),
        (CAN_BUY_ZERO, form.can_buy_zero.is_some()),
    ] {
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ('catalog', $1, $2)
             ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(name)
        .bind(if on { "Y" } else { "N" })
        .execute(&state.db)
        .await?;
    }
    Ok(Redirect::to("/admin/shop/settings"))
}
