use axum::{
    Extension, Form,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use minijinja::{Value, context};
use serde::{Deserialize, Serialize};

use super::{parse_sort, render};
use crate::{
    access::{Access, IBLOCKS_MANAGE, Level},
    error::{AppError, AppResult, is_unique_violation},
    iblock::{IblockInput, PropertyInput, is_valid_code, props, repo},
    state::AppState,
};

#[derive(Default, Deserialize, Serialize)]
pub struct IblockForm {
    #[serde(default)]
    code: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    api_enabled: Option<String>,
    #[serde(default)]
    sort: String,
}

impl IblockForm {
    fn validate(&self) -> Result<IblockInput, String> {
        let name = self.name.trim();
        let code = self.code.trim();
        if name.is_empty() {
            return Err("Укажите название".into());
        }
        if !is_valid_code(code) {
            return Err("Код: только латиница в нижнем регистре, цифры, «_» и «-»".into());
        }
        Ok(IblockInput {
            code: code.to_string(),
            name: name.to_string(),
            description: self.description.trim().to_string(),
            api_enabled: self.api_enabled.is_some(),
            sort: parse_sort(&self.sort),
        })
    }
}

#[derive(Default, Deserialize, Serialize)]
pub struct PropertyForm {
    #[serde(default)]
    code: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: String,
    is_required: Option<String>,
    #[serde(default)]
    sort: String,
}

impl PropertyForm {
    fn validate(&self) -> Result<PropertyInput, String> {
        let name = self.name.trim();
        let code = self.code.trim();
        if name.is_empty() {
            return Err("Укажите название свойства".into());
        }
        if !is_valid_code(code) {
            return Err("Код свойства: только латиница в нижнем регистре, цифры, «_» и «-»".into());
        }
        if !props::is_valid_kind(&self.kind) {
            return Err("Неизвестный тип свойства".into());
        }
        Ok(PropertyInput {
            code: code.to_string(),
            name: name.to_string(),
            kind: self.kind.clone(),
            is_required: self.is_required.is_some(),
            sort: parse_sort(&self.sort),
        })
    }
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    let items: Vec<_> = repo::list_iblocks(&state.db)
        .await?
        .into_iter()
        .filter(|s| user.iblock_level(s.iblock.id) >= Level::Read)
        .collect();
    let can_manage = user.can(IBLOCKS_MANAGE);
    render(&state, "iblocks.html", context! { user, items, can_manage })
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require(IBLOCKS_MANAGE)?;
    let form = IblockForm {
        api_enabled: Some("on".into()),
        sort: "500".into(),
        ..Default::default()
    };
    render(&state, "iblock_form.html", context! { user, form })
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<IblockForm>,
) -> AppResult<Response> {
    user.require(IBLOCKS_MANAGE)?;
    let error = match form.validate() {
        Ok(input) => match repo::create_iblock(&state.db, &input).await {
            Ok(iblock) => {
                return Ok(Redirect::to(&format!("/admin/iblocks/{}", iblock.id)).into_response());
            }
            Err(e) if is_unique_violation(&e) => "Инфоблок с таким кодом уже существует".into(),
            Err(e) => return Err(e.into()),
        },
        Err(msg) => msg,
    };
    Ok(render(&state, "iblock_form.html", context! { user, form, error })?.into_response())
}

/// Страница редактирования инфоблока вместе со списком свойств.
async fn render_edit(
    state: &AppState,
    user: Access,
    id: i64,
    form: Option<IblockForm>,
    error: Option<String>,
    prop_form: PropertyForm,
    prop_error: Option<String>,
) -> AppResult<Html<String>> {
    let iblock = repo::get_iblock(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let properties = repo::list_properties(&state.db, id).await?;
    let form = form.unwrap_or_else(|| IblockForm {
        code: iblock.code.clone(),
        name: iblock.name.clone(),
        description: iblock.description.clone(),
        api_enabled: iblock.api_enabled.then(|| "on".into()),
        sort: iblock.sort.to_string(),
    });
    render(
        state,
        "iblock_form.html",
        context! {
            user,
            iblock,
            form,
            error,
            properties,
            prop_form,
            prop_error,
            kinds => Value::from_serialize(props::KINDS),
        },
    )
}

fn empty_prop_form() -> PropertyForm {
    PropertyForm {
        kind: "string".into(),
        sort: "500".into(),
        ..Default::default()
    }
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    user.require(IBLOCKS_MANAGE)?;
    render_edit(&state, user, id, None, None, empty_prop_form(), None).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<IblockForm>,
) -> AppResult<Response> {
    user.require(IBLOCKS_MANAGE)?;
    let error = match form.validate() {
        Ok(input) => match repo::update_iblock(&state.db, id, &input).await {
            Ok(true) => {
                return Ok(Redirect::to(&format!("/admin/iblocks/{id}")).into_response());
            }
            Ok(false) => return Err(AppError::NotFound),
            Err(e) if is_unique_violation(&e) => "Инфоблок с таким кодом уже существует".into(),
            Err(e) => return Err(e.into()),
        },
        Err(msg) => msg,
    };
    let page = render_edit(
        &state,
        user,
        id,
        Some(form),
        Some(error),
        empty_prop_form(),
        None,
    );
    Ok(page.await?.into_response())
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    user.require(IBLOCKS_MANAGE)?;
    repo::delete_iblock(&state.db, id).await?;
    Ok(Redirect::to("/admin/iblocks"))
}

pub async fn add_property(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<PropertyForm>,
) -> AppResult<Response> {
    user.require(IBLOCKS_MANAGE)?;
    let error = match form.validate() {
        Ok(input) => match repo::create_property(&state.db, id, &input).await {
            Ok(()) => return Ok(Redirect::to(&format!("/admin/iblocks/{id}")).into_response()),
            Err(e) if is_unique_violation(&e) => "Свойство с таким кодом уже есть".into(),
            Err(e) => return Err(e.into()),
        },
        Err(msg) => msg,
    };
    let page = render_edit(&state, user, id, None, None, form, Some(error));
    Ok(page.await?.into_response())
}

pub async fn delete_property(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    user.require(IBLOCKS_MANAGE)?;
    let iblock_id = repo::delete_property(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Redirect::to(&format!("/admin/iblocks/{iblock_id}")))
}
