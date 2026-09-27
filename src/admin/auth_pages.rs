use axum::{
    Form,
    extract::State,
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::CookieJar;
use minijinja::context;
use serde::Deserialize;

use super::render;
use crate::{access::Access, auth, error::AppResult, state::AppState};

pub async fn login_form(State(state): State<AppState>, jar: CookieJar) -> AppResult<Response> {
    if let Some(user) = auth::current_user(&state.db, &jar).await?
        && Access::load(&state.db, user).await?.can_enter_admin()
    {
        return Ok(Redirect::to("/admin").into_response());
    }
    Ok(render(&state, "login.html", context! {})?.into_response())
}

#[derive(Deserialize)]
pub struct LoginForm {
    login: String,
    password: String,
}

pub async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> AppResult<Response> {
    let login = form.login.trim();
    let access = match auth::verify_login(&state.db, login, form.password).await? {
        Some(user) => Some(Access::load(&state.db, user).await?),
        None => None,
    };
    match access {
        Some(access) if access.can_enter_admin() => {
            let user = access.user;
            let token = auth::create_session(&state.db, user.id).await?;
            let jar = jar.add(auth::session_cookie(token, state.config.cookie_secure));
            tracing::info!(login = %user.login, "admin login");
            Ok((jar, Redirect::to("/admin")).into_response())
        }
        _ => {
            tracing::warn!(%login, "failed admin login");
            let page: Html<String> = render(
                &state,
                "login.html",
                context! { login, error => "Неверный логин или пароль" },
            )?;
            Ok(page.into_response())
        }
    }
}

pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> AppResult<Response> {
    auth::delete_session(&state.db, &jar).await?;
    Ok((
        jar.remove(auth::removal_cookie()),
        Redirect::to("/admin/login"),
    )
        .into_response())
}
