use axum::{
    Extension,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::{CookieJar, Form};
use minijinja::context;
use serde::{Deserialize, Serialize};

use super::render;
use crate::{
    access::{Access, USERS_MANAGE},
    auth,
    error::{AppError, AppResult, is_unique_violation},
    groups,
    state::AppState,
    users::{self, UserInput, UserRow, is_valid_email, is_valid_login},
};

const MIN_PASSWORD_LEN: usize = 8;

#[derive(Default, Deserialize, Serialize)]
pub struct UserForm {
    #[serde(default)]
    login: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: String,
    is_admin: Option<String>,
    active: Option<String>,
    // пароли никогда не возвращаются обратно в форму
    #[serde(default, skip_serializing)]
    password: String,
    #[serde(default, skip_serializing)]
    password_confirm: String,
    /// Отмеченные группы (повторяющееся поле `groups`).
    #[serde(default)]
    groups: Vec<i64>,
}

impl UserForm {
    /// `password_required` — при создании пароль обязателен, при редактировании
    /// пустое поле означает «не менять».
    fn validate(&self, password_required: bool) -> Result<(UserInput, Option<String>), String> {
        let mut errors = Vec::new();
        let login = self.login.trim();
        if !is_valid_login(login) {
            errors.push("Логин: 3–50 символов, латиница, цифры и «_ . - @»");
        }
        let email = self.email.trim();
        if !email.is_empty() && !is_valid_email(email) {
            errors.push("Неверный e-mail");
        }
        let password = match (self.password.is_empty(), password_required) {
            (true, true) => {
                errors.push("Укажите пароль");
                None
            }
            (true, false) => None,
            (false, _) => {
                if self.password.chars().count() < MIN_PASSWORD_LEN {
                    errors.push("Пароль должен быть не короче 8 символов");
                } else if self.password != self.password_confirm {
                    errors.push("Пароли не совпадают");
                }
                Some(self.password.clone())
            }
        };
        if !errors.is_empty() {
            return Err(errors.join("; "));
        }
        let input = UserInput {
            login: login.to_string(),
            name: self.name.trim().to_string(),
            email: (!email.is_empty()).then(|| email.to_string()),
            is_admin: self.is_admin.is_some(),
            active: self.active.is_some(),
        };
        Ok((input, password))
    }
}

fn unique_error(e: &sqlx::Error) -> Option<String> {
    is_unique_violation(e).then(|| "Пользователь с таким логином уже есть".to_string())
}

/// Данные для формы: все группы и флаги, что может менять текущий пользователь.
async fn form_context(state: &AppState, user: &Access) -> AppResult<minijinja::Value> {
    let all_groups = groups::list(&state.db).await?;
    Ok(context! {
        all_groups,
        can_edit_groups => user.is_super(),
        can_set_admin => user.is_super(),
    })
}

fn render_form(
    state: &AppState,
    user: Access,
    extra: minijinja::Value,
    ctx: minijinja::Value,
) -> AppResult<Html<String>> {
    let ctx = minijinja::value::merge_maps([extra, ctx]);
    render(state, "user_form.html", context! { user, ..ctx })
}

/// Суперадминистраторов может менять только суперадминистратор.
fn check_target(user: &Access, target: &UserRow) -> AppResult<()> {
    if target.is_admin && !user.is_super() {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require(USERS_MANAGE)?;
    let items = users::list(&state.db).await?;
    render(&state, "users.html", context! { user, items })
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require(USERS_MANAGE)?;
    let form = UserForm {
        active: Some("on".into()),
        ..Default::default()
    };
    let extra = form_context(&state, &user).await?;
    render_form(&state, user, extra, context! { form })
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<UserForm>,
) -> AppResult<Response> {
    user.require(USERS_MANAGE)?;
    let error = match form.validate(true) {
        Ok((mut input, Some(password))) => {
            if !user.is_super() {
                input.is_admin = false;
            }
            let hash = auth::hash_password(password).await?;
            match users::create(&state.db, &input, &hash).await {
                Ok(id) => {
                    if user.is_super() {
                        groups::set_user_groups(&state.db, id, &form.groups).await?;
                    }
                    return Ok(Redirect::to("/admin/users").into_response());
                }
                Err(e) => unique_error(&e).ok_or(AppError::from(e))?,
            }
        }
        Ok((_, None)) => unreachable!("пароль обязателен при создании"),
        Err(msg) => msg,
    };
    let extra = form_context(&state, &user).await?;
    Ok(render_form(&state, user, extra, context! { form, error })?.into_response())
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    user.require(USERS_MANAGE)?;
    let target = users::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    check_target(&user, &target)?;
    let form = UserForm {
        login: target.login.clone(),
        name: target.name.clone(),
        email: target.email.clone().unwrap_or_default(),
        is_admin: target.is_admin.then(|| "on".into()),
        active: target.active.then(|| "on".into()),
        groups: groups::user_group_ids(&state.db, id).await?,
        ..Default::default()
    };
    let is_self = target.id == user.user.id;
    let extra = form_context(&state, &user).await?;
    render_form(&state, user, extra, context! { target, form, is_self })
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    jar: CookieJar,
    Path(id): Path<i64>,
    Form(form): Form<UserForm>,
) -> AppResult<Response> {
    user.require(USERS_MANAGE)?;
    let target = users::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    check_target(&user, &target)?;
    let is_self = target.id == user.user.id;
    let error = match form.validate(false) {
        Ok((mut input, password)) => {
            if is_self {
                // себя нельзя заблокировать или лишить прав — иначе можно потерять доступ
                input.active = true;
                input.is_admin = target.is_admin;
            }
            if !user.is_super() {
                // флаг суперадминистратора меняет только суперадминистратор
                input.is_admin = target.is_admin;
            }
            let hash = match &password {
                Some(p) => Some(auth::hash_password(p.clone()).await?),
                None => None,
            };
            match users::update(&state.db, id, &input, hash.as_deref()).await {
                Ok(()) => {
                    // свои группы суперадмину менять можно: доступ ему даёт флаг, а не группы
                    if user.is_super() {
                        groups::set_user_groups(&state.db, id, &form.groups).await?;
                    }
                    let lost_access = !input.active || (target.is_admin && !input.is_admin);
                    if password.is_some() || lost_access {
                        // своя текущая сессия остаётся, остальные закрываются
                        let keep = is_self.then_some(&jar);
                        auth::delete_user_sessions(&state.db, id, keep).await?;
                    }
                    return Ok(Redirect::to("/admin/users").into_response());
                }
                Err(e) => unique_error(&e).ok_or(AppError::from(e))?,
            }
        }
        Err(msg) => msg,
    };
    let extra = form_context(&state, &user).await?;
    let page = render_form(
        &state,
        user,
        extra,
        context! { target, form, is_self, error },
    )?;
    Ok(page.into_response())
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    user.require(USERS_MANAGE)?;
    if id == user.user.id {
        return Err(AppError::BadRequest("Нельзя удалить самого себя".into()));
    }
    let target = users::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    check_target(&user, &target)?;
    users::delete(&state.db, id).await?;
    Ok(Redirect::to("/admin/users"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(password: &str, confirm: &str) -> UserForm {
        UserForm {
            login: "ivan".into(),
            email: "ivan@example.com".into(),
            password: password.into(),
            password_confirm: confirm.into(),
            ..Default::default()
        }
    }

    #[test]
    fn password_rules() {
        assert!(form("", "").validate(true).is_err());
        assert_eq!(form("", "").validate(false).unwrap().1, None);
        assert!(form("short", "short").validate(true).is_err());
        assert!(form("longenough1", "different1").validate(true).is_err());
        assert!(form("longenough1", "longenough1").validate(true).is_ok());
    }
}
