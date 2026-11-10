//! «Письма»: почтовые шаблоны (только суперадмин).

use axum::{
    Extension, Form,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use minijinja::context;

use super::{flash, render};
use crate::{
    access::Access,
    error::AppResult,
    mail_templates::{self, Template, TemplateForm},
    state::AppState,
};

fn form_of(template: &Template) -> TemplateForm {
    TemplateForm {
        event_name: template.event_name.clone(),
        email_from: template.email_from.clone(),
        email_to: template.email_to.clone(),
        bcc: template.bcc.clone(),
        subject: template.subject.clone(),
        body: template.body.clone(),
        body_type: template.body_type.clone(),
        active: template.active.then(|| "on".to_string()),
    }
}

async fn page(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: TemplateForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    render(state, "mail_form.html", context! { user, id, form, error })
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require_super()?;
    let templates = mail_templates::list(&state.db).await?;
    render(&state, "mail_list.html", context! { user, templates })
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require_super()?;
    page(&state, user, None, TemplateForm::default(), None).await
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    user.require_super()?;
    let template = mail_templates::get(&state.db, id).await?;
    page(&state, user, Some(id), form_of(&template), None).await
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<TemplateForm>,
) -> AppResult<Response> {
    user.require_super()?;
    if let Err(error) = form.validate() {
        return Ok(page(&state, user, None, form, Some(error))
            .await?
            .into_response());
    }
    let id = mail_templates::create(&state.db, &form).await?;
    Ok(Redirect::to(&format!("/admin/mail/{id}")).into_response())
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<TemplateForm>,
) -> AppResult<Response> {
    user.require_super()?;
    if let Err(error) = form.validate() {
        return Ok(page(&state, user, Some(id), form, Some(error))
            .await?
            .into_response());
    }
    mail_templates::update(&state.db, id, &form).await?;
    Ok(flash::redirect(
        &format!("/admin/mail/{id}"),
        "Шаблон сохранён",
        flash::FlashKind::Ok,
    ))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    user.require_super()?;
    mail_templates::delete(&state.db, id).await?;
    Ok(flash::redirect(
        "/admin/mail",
        "Шаблон удалён",
        flash::FlashKind::Ok,
    ))
}
