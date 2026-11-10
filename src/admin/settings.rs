//! «Настройки сайта»: название, домен, отправитель и SMTP. Только суперадмин.

use axum::{
    Extension, Form,
    extract::State,
    response::{Html, IntoResponse, Response},
};
use minijinja::context;

use super::{flash, render};
use crate::{
    access::Access,
    error::AppResult,
    settings::{self, SettingsForm},
    state::AppState,
};

pub async fn form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require_super()?;
    let settings = settings::load(&state.db).await?;
    render(&state, "settings.html", context! { user, settings })
}

pub async fn save(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(input): Form<SettingsForm>,
) -> AppResult<Response> {
    user.require_super()?;
    if let Err(error) = input.validate() {
        let settings = settings::load(&state.db).await?;
        return Ok(render(
            &state,
            "settings.html",
            context! { user, settings, error, input },
        )?
        .into_response());
    }
    settings::save(&state.db, &input).await?;
    Ok(flash::redirect(
        "/admin/settings",
        "Настройки сохранены",
        flash::FlashKind::Ok,
    ))
}
