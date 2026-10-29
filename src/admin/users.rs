use axum::{
    Extension,
    extract::{Path, State},
    http::{HeaderValue, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::{CookieJar, Form};
use minijinja::context;
use serde::{Deserialize, Serialize};

use super::render;
use crate::{
    access::{Access, USERS_MANAGE},
    apikeys::{self, KeyRow, NewKey, SCOPES},
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
    edit_page(&state, user, target, context! {}).await
}

/// Карточка пользователя; `keys_extra` — состояние блока ключей (новый ключ, ошибка,
/// подтверждение отзыва).
async fn edit_page(
    state: &AppState,
    user: Access,
    target: UserRow,
    keys_extra: minijinja::Value,
) -> AppResult<Html<String>> {
    let form = UserForm {
        login: target.login.clone(),
        name: target.name.clone(),
        email: target.email.clone().unwrap_or_default(),
        is_admin: target.is_admin.then(|| "on".into()),
        active: target.active.then(|| "on".into()),
        groups: groups::user_group_ids(&state.db, target.id).await?,
        ..Default::default()
    };
    let is_self = target.id == user.user.id;
    let keys = apikeys::list(&state.db, target.id).await?;
    let scopes: Vec<minijinja::Value> = SCOPES
        .iter()
        .map(|(code, name)| context! { code, name })
        .collect();
    let extra = form_context(state, &user).await?;
    let keys_ctx = minijinja::value::merge_maps([context! { keys, scopes }, keys_extra]);
    let extra = minijinja::value::merge_maps([extra, keys_ctx]);
    render_form(state, user, extra, context! { target, form, is_self })
}

#[derive(Default, Deserialize)]
pub struct KeyForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    scopes: Vec<String>,
    /// Дата окончания `ГГГГ-ММ-ДД`; пусто — бессрочно.
    #[serde(default)]
    expires_at: String,
}

impl KeyForm {
    fn validate(&self, user_id: i64) -> Result<NewKey, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Укажите название ключа".into());
        }
        let mut scopes: Vec<String> = Vec::new();
        for s in &self.scopes {
            if !SCOPES.iter().any(|(code, _)| code == s) {
                return Err(format!("Неизвестный scope {s}"));
            }
            if !scopes.contains(s) {
                scopes.push(s.clone());
            }
        }
        if name.chars().count() > 100 {
            return Err("Название ключа: не длиннее 100 символов".into());
        }
        if scopes.is_empty() {
            return Err("Выберите хотя бы один scope".into());
        }
        let date = self.expires_at.trim();
        let expires_at = if date.is_empty() {
            None
        } else {
            let day = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
                .map_err(|_| "Срок: дата в формате ГГГГ-ММ-ДД".to_string())?;
            if day < chrono::Utc::now().date_naive() {
                return Err("Срок действия не может быть в прошлом".into());
            }
            // ключ действует до конца указанного дня (UTC)
            day.and_hms_opt(23, 59, 59).map(|t| t.and_utc())
        };
        Ok(NewKey {
            user_id,
            name: name.to_string(),
            scopes,
            expires_at,
        })
    }
}

/// Загружает пользователя и проверяет доступ к его ключам.
async fn key_target(state: &AppState, user: &Access, id: i64) -> AppResult<UserRow> {
    user.require(USERS_MANAGE)?;
    let target = users::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    check_target(user, &target)?;
    Ok(target)
}

pub async fn issue_key(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<KeyForm>,
) -> AppResult<Response> {
    let target = key_target(&state, &user, id).await?;
    let by = user.user.id;
    let keys_extra = match form.validate(id) {
        Ok(new) => {
            let (_, full) = apikeys::issue(&state.db, &new, by).await?;
            // полный ключ — только в этом ответе, не кэшируется и нигде не хранится
            let page = edit_page(&state, user, target, context! { new_key => full }).await?;
            let mut res = page.into_response();
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return Ok(res);
        }
        Err(key_error) => context! {
            key_error,
            key_name => form.name.clone(),
            key_scopes => form.scopes.clone(),
            key_expires => form.expires_at.clone(),
        },
    };
    Ok(edit_page(&state, user, target, keys_extra)
        .await?
        .into_response())
}

/// GET на адреса POST-форм (reload, «Назад») — обратно в карточку.
pub async fn keys_redirect(Path(id): Path<i64>) -> Redirect {
    Redirect::to(&format!("/admin/users/{id}"))
}

pub async fn revoke_redirect(Path((id, _)): Path<(i64, i64)>) -> Redirect {
    Redirect::to(&format!("/admin/users/{id}"))
}

#[derive(Default, Deserialize)]
pub struct RevokeForm {
    /// Второй шаг отзыва.
    #[serde(default)]
    confirm: String,
}

pub async fn revoke_key(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path((id, key_id)): Path<(i64, i64)>,
    Form(form): Form<RevokeForm>,
) -> AppResult<Response> {
    let target = key_target(&state, &user, id).await?;
    if form.confirm == "1" {
        if !apikeys::revoke(&state.db, id, key_id).await? {
            return Err(AppError::NotFound);
        }
        return Ok(Redirect::to(&format!("/admin/users/{id}")).into_response());
    }
    let key: KeyRow = apikeys::list(&state.db, id)
        .await?
        .into_iter()
        .find(|k| k.id == key_id && k.revoked_at.is_none())
        .ok_or(AppError::NotFound)?;
    Ok(
        edit_page(&state, user, target, context! { confirm_key => key })
            .await?
            .into_response(),
    )
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

    use crate::{
        smoke_tests::get,
        test_support::{admin_cookie, permitted_cookie, post_form_raw, test_state, user_cookie},
    };
    use sqlx::PgPool;

    /// Полный ключ из страницы выпуска.
    fn full_key(html: &str) -> String {
        let start = html.find("cms_").expect("ключ в ответе");
        let rest = &html[start..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap();
        rest[..end].to_string()
    }

    #[sqlx::test]
    async fn issue_key_shows_once(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (uid, _) = user_cookie(&db, "worker", false).await;
        let path = format!("/admin/users/{uid}/api-keys");
        let (s, _, html) = post_form_raw(
            &app,
            &path,
            &cookie,
            "name=%D0%A1%D0%BA%D0%BB%D0%B0%D0%B4&scopes=stock.read&scopes=stock.write&expires_at=",
        )
        .await;
        assert!(s.is_success(), "{s}");
        let key = full_key(&html);
        assert_eq!(key.len(), 4 + 8 + 1 + 32, "{key}");
        assert!(html.contains("Сохраните ключ — потом его не показать"));
        assert!(html.contains("Копировать"));
        // повторное открытие: только префикс
        let (_, page) = get(&app, &format!("/admin/users/{uid}"), &cookie).await;
        let prefix = &key[..12];
        assert!(!page.contains(&key), "полный ключ не должен повторяться");
        assert!(page.contains(&format!("{prefix}_…")), "{page}");
        assert!(!page.contains("Сохраните ключ"));
    }

    #[sqlx::test]
    async fn issue_validation_errors(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (uid, _) = user_cookie(&db, "worker", false).await;
        let path = format!("/admin/users/{uid}/api-keys");
        let past = (chrono::Utc::now().date_naive() - chrono::Days::new(1)).to_string();
        let bad = [
            "name=&scopes=stock.read".to_string(),
            "name=K".to_string(),
            "name=K&scopes=stock.nope".to_string(),
            "name=K&scopes=stock.read&expires_at=junk".to_string(),
            format!("name=K&scopes=stock.read&expires_at={past}"),
        ];
        for body in bad {
            let (s, _, html) = post_form_raw(&app, &path, &cookie, &body).await;
            assert!(s.is_success(), "{body}: {s}");
            assert!(html.contains("class=\"alert\""), "{body}");
            assert!(!html.contains("Сохраните ключ"), "{body}");
        }
        assert!(crate::apikeys::list(&db, uid).await.unwrap().is_empty());
        // дубли scope схлопываются
        let (s, _, _) = post_form_raw(
            &app,
            &path,
            &cookie,
            "name=K&scopes=stock.read&scopes=stock.read&expires_at=2999-01-01",
        )
        .await;
        assert!(s.is_success());
        let keys = crate::apikeys::list(&db, uid).await.unwrap();
        assert_eq!(keys[0].scopes, vec!["stock.read".to_string()]);
        assert!(keys[0].expires_at.is_some());
    }

    #[sqlx::test]
    async fn revoke_two_steps(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (uid, _) = user_cookie(&db, "worker", false).await;
        let (_, _, _) = post_form_raw(
            &app,
            &format!("/admin/users/{uid}/api-keys"),
            &cookie,
            "name=K&scopes=stock.read",
        )
        .await;
        let kid = crate::apikeys::list(&db, uid).await.unwrap()[0].id;
        let path = format!("/admin/users/{uid}/api-keys/{kid}/revoke");
        let (s, _, html) = post_form_raw(&app, &path, &cookie, "").await;
        assert!(s.is_success(), "{s}");
        assert!(html.contains("name=\"confirm\""), "{html}");
        assert!(
            crate::apikeys::list(&db, uid).await.unwrap()[0]
                .revoked_at
                .is_none()
        );
        let (s, _, _) = post_form_raw(&app, &path, &cookie, "confirm=1").await;
        assert!(s.is_redirection(), "{s}");
        assert!(
            crate::apikeys::list(&db, uid).await.unwrap()[0]
                .revoked_at
                .is_some()
        );
        let (_, page) = get(&app, &format!("/admin/users/{uid}"), &cookie).await;
        assert!(page.contains("отозван"));
    }

    #[sqlx::test]
    async fn super_keys_hidden_from_admin(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let (sid, _) = user_cookie(&db, "boss", true).await;
        let cookie = permitted_cookie(&db, "manager", &[USERS_MANAGE]).await;
        let (key_id, _) = crate::apikeys::issue(
            &db,
            &crate::apikeys::NewKey {
                user_id: sid,
                name: "K".into(),
                scopes: vec!["stock.read".into()],
                expires_at: None,
            },
            sid,
        )
        .await
        .unwrap();
        let (s, _) = get(&app, &format!("/admin/users/{sid}"), &cookie).await;
        assert_eq!(s, 403);
        for path in [
            format!("/admin/users/{sid}/api-keys"),
            format!("/admin/users/{sid}/api-keys/{key_id}/revoke"),
        ] {
            let (s, _, _) =
                post_form_raw(&app, &path, &cookie, "name=K&scopes=stock.read&confirm=1").await;
            assert_eq!(s, 403, "{path}");
        }
        assert_eq!(crate::apikeys::list(&db, sid).await.unwrap().len(), 1);
        assert!(
            crate::apikeys::list(&db, sid).await.unwrap()[0]
                .revoked_at
                .is_none()
        );
    }

    #[sqlx::test]
    async fn block_hidden_without_users_manage(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let (uid, _) = user_cookie(&db, "worker", false).await;
        let cookie = permitted_cookie(&db, "clerk", &[]).await;
        let (s, page) = get(&app, &format!("/admin/users/{uid}"), &cookie).await;
        assert_eq!(s, 403);
        assert!(!page.contains("API-ключи"));
        let (s, _, _) = post_form_raw(
            &app,
            &format!("/admin/users/{uid}/api-keys"),
            &cookie,
            "name=K&scopes=stock.read",
        )
        .await;
        assert_eq!(s, 403);
        assert!(crate::apikeys::list(&db, uid).await.unwrap().is_empty());
        // а с правом блок виден
        let admin = admin_cookie(&db).await;
        let (_, page) = get(&app, &format!("/admin/users/{uid}"), &admin).await;
        assert!(page.contains("API-ключи"));
    }

    #[sqlx::test]
    async fn issued_key_not_cached_in_history(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (uid, _) = user_cookie(&db, "worker", false).await;
        let (_, _, html) = post_form_raw(
            &app,
            &format!("/admin/users/{uid}/api-keys"),
            &cookie,
            "name=K&scopes=stock.read",
        )
        .await;
        let at = html.find("id=\"new-key\"").expect("ключ на странице");
        let block_start = html[..at].rfind("<div").unwrap();
        assert!(
            html[block_start..at].contains("hx-history=\"false\""),
            "блок с ключом без hx-history"
        );
        assert!(html.contains(&format!(
            "action=\"/admin/users/{uid}/api-keys\" hx-push-url=\"false\""
        )));
        assert!(html.contains("/revoke\" hx-push-url=\"false\""));
        // GET на служебные адреса — редирект на карточку
        let (s, h, _) =
            crate::test_support::get_raw(&app, &format!("/admin/users/{uid}/api-keys"), &cookie)
                .await;
        assert_eq!(s, 303);
        assert_eq!(h["location"], format!("/admin/users/{uid}").as_str());
        let (s, _, _) = crate::test_support::get_raw(
            &app,
            &format!("/admin/users/{uid}/api-keys/1/revoke"),
            &cookie,
        )
        .await;
        assert_eq!(s, 303);
    }

    #[sqlx::test]
    async fn revoke_foreign_key_noop(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (a, _) = user_cookie(&db, "usera", false).await;
        let (b, _) = user_cookie(&db, "userb", false).await;
        post_form_raw(
            &app,
            &format!("/admin/users/{b}/api-keys"),
            &cookie,
            "name=K&scopes=stock.read",
        )
        .await;
        let kid = crate::apikeys::list(&db, b).await.unwrap()[0].id;
        let (s, _, _) = post_form_raw(
            &app,
            &format!("/admin/users/{a}/api-keys/{kid}/revoke"),
            &cookie,
            "confirm=1",
        )
        .await;
        assert_eq!(s, 404);
        assert!(
            crate::apikeys::list(&db, b).await.unwrap()[0]
                .revoked_at
                .is_none()
        );
    }

    #[sqlx::test]
    async fn long_key_name_rejected(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (uid, _) = user_cookie(&db, "worker", false).await;
        let body = format!("name={}&scopes=stock.read", "a".repeat(101));
        let (_, _, html) = post_form_raw(
            &app,
            &format!("/admin/users/{uid}/api-keys"),
            &cookie,
            &body,
        )
        .await;
        assert!(html.contains("class=\"alert\""));
        assert!(crate::apikeys::list(&db, uid).await.unwrap().is_empty());
    }
}
