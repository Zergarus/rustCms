use std::collections::HashMap;

use axum::{
    Extension, Form,
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::NaiveDateTime;
use minijinja::context;
use serde_json::Map;

use super::{PageQuery, parse_sort, render};
use crate::{
    access::{Access, Level},
    error::{AppError, AppResult, is_unique_violation},
    iblock::{Element, ElementInput, Iblock, Property, is_valid_code, props, repo, slugify},
    state::AppState,
};

const PER_PAGE: i64 = 50;
const DATETIME_FORMAT: &str = "%Y-%m-%dT%H:%M";

/// Поля формы элемента: name, code, active, sort, preview_text, detail_text,
/// published_at и `prop_<код>` для каждого свойства.
type FormValues = HashMap<String, String>;

fn build_input(form: &FormValues, properties: &[Property]) -> Result<ElementInput, String> {
    let get = |key: &str| form.get(key).map(|s| s.trim()).unwrap_or("");
    let mut errors = Vec::new();

    let name = get("name");
    if name.is_empty() {
        errors.push("Укажите название".to_string());
    }
    let code = match get("code") {
        "" => slugify(name),
        code => code.to_string(),
    };
    if !name.is_empty() && !is_valid_code(&code) {
        errors.push("Код: только латиница в нижнем регистре, цифры, «_» и «-»".into());
    }
    let published_at = match get("published_at") {
        "" => None,
        raw => match NaiveDateTime::parse_from_str(raw, DATETIME_FORMAT) {
            Ok(dt) => Some(dt.and_utc()),
            Err(_) => {
                errors.push("Неверная дата публикации".into());
                None
            }
        },
    };

    let mut values = Map::new();
    for prop in properties {
        let raw = form.get(&format!("prop_{}", prop.code)).map(String::as_str);
        match props::parse_value(prop, raw) {
            Ok(value) => {
                values.insert(prop.code.clone(), value);
            }
            Err(e) => errors.push(e),
        }
    }

    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(ElementInput {
        code,
        name: name.to_string(),
        active: form.contains_key("active"),
        sort: parse_sort(get("sort")),
        preview_text: get("preview_text").to_string(),
        detail_text: get("detail_text").to_string(),
        published_at,
        properties: values,
    })
}

fn element_to_form(element: &Element) -> FormValues {
    let mut form = FormValues::from([
        ("name".into(), element.name.clone()),
        ("code".into(), element.code.clone()),
        ("sort".into(), element.sort.to_string()),
        ("preview_text".into(), element.preview_text.clone()),
        ("detail_text".into(), element.detail_text.clone()),
    ]);
    if element.active {
        form.insert("active".into(), "on".into());
    }
    if let Some(dt) = element.published_at {
        form.insert(
            "published_at".into(),
            dt.format(DATETIME_FORMAT).to_string(),
        );
    }
    for (code, value) in element.properties.iter() {
        if let Some(v) = props::to_form_value(value) {
            form.insert(format!("prop_{code}"), v);
        }
    }
    form
}

async fn load_iblock(state: &AppState, id: i64) -> AppResult<(Iblock, Vec<Property>)> {
    let iblock = repo::get_iblock(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let properties = repo::list_properties(&state.db, id).await?;
    Ok((iblock, properties))
}

fn render_form(
    state: &AppState,
    user: Access,
    iblock: Iblock,
    properties: Vec<Property>,
    element_id: Option<i64>,
    form: FormValues,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let can_write = user.iblock_level(iblock.id) >= Level::Write;
    render(
        state,
        "element_form.html",
        context! { user, iblock, properties, element_id, form, error, can_write },
    )
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Query(q): Query<PageQuery>,
) -> AppResult<Html<String>> {
    user.require_iblock(id, Level::Read)?;
    let iblock = repo::get_iblock(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let page = q.page();
    let (items, total) =
        repo::list_elements(&state.db, id, PER_PAGE, (page - 1) * PER_PAGE).await?;
    let pages = ((total + PER_PAGE - 1) / PER_PAGE).max(1);
    render(
        &state,
        "elements.html",
        context! { can_write => user.iblock_level(id) >= Level::Write, user, iblock, items, total, page, pages },
    )
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    user.require_iblock(id, Level::Write)?;
    let (iblock, properties) = load_iblock(&state, id).await?;
    let form = FormValues::from([
        ("active".into(), "on".into()),
        ("sort".into(), "500".into()),
    ]);
    render_form(&state, user, iblock, properties, None, form, None)
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    user.require_iblock(id, Level::Write)?;
    let (iblock, properties) = load_iblock(&state, id).await?;
    let form: FormValues = fields.into_iter().collect();
    let error = match build_input(&form, &properties) {
        Ok(input) => match repo::create_element(&state.db, id, &input).await {
            Ok(_) => {
                return Ok(Redirect::to(&format!("/admin/iblocks/{id}/elements")).into_response());
            }
            Err(e) if is_unique_violation(&e) => "Элемент с таким кодом уже есть".into(),
            Err(e) => return Err(e.into()),
        },
        Err(msg) => msg,
    };
    let page = render_form(&state, user, iblock, properties, None, form, Some(error))?;
    Ok(page.into_response())
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let element = repo::get_element(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    user.require_iblock(element.iblock_id, Level::Read)?;
    let (iblock, properties) = load_iblock(&state, element.iblock_id).await?;
    let form = element_to_form(&element);
    render_form(&state, user, iblock, properties, Some(id), form, None)
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let element = repo::get_element(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    user.require_iblock(element.iblock_id, Level::Write)?;
    let (iblock, properties) = load_iblock(&state, element.iblock_id).await?;
    let form: FormValues = fields.into_iter().collect();
    let error = match build_input(&form, &properties) {
        Ok(input) => match repo::update_element(&state.db, id, &input).await {
            Ok(()) => {
                let url = format!("/admin/iblocks/{}/elements", element.iblock_id);
                return Ok(Redirect::to(&url).into_response());
            }
            Err(e) if is_unique_violation(&e) => "Элемент с таким кодом уже есть".into(),
            Err(e) => return Err(e.into()),
        },
        Err(msg) => msg,
    };
    let page = render_form(
        &state,
        user,
        iblock,
        properties,
        Some(id),
        form,
        Some(error),
    )?;
    Ok(page.into_response())
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    let element = repo::get_element(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    user.require_iblock(element.iblock_id, Level::Write)?;
    let iblock_id = repo::delete_element(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Redirect::to(&format!(
        "/admin/iblocks/{iblock_id}/elements"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prop(code: &str, kind: &str, required: bool) -> Property {
        Property {
            id: 1,
            iblock_id: 1,
            code: code.into(),
            name: code.into(),
            kind: kind.into(),
            is_required: required,
            sort: 500,
        }
    }

    #[test]
    fn autogenerates_code_and_parses_props() {
        let form = FormValues::from([
            ("name".into(), "Первая новость".into()),
            ("active".into(), "on".into()),
            ("published_at".into(), "2026-09-27T10:30".into()),
            ("prop_price".into(), "99.5".into()),
        ]);
        let input = build_input(
            &form,
            &[prop("price", "number", true), prop("hot", "boolean", false)],
        )
        .unwrap();
        assert_eq!(input.code, "pervaya-novost");
        assert!(input.active);
        assert_eq!(input.properties["price"], serde_json::json!(99.5));
        assert_eq!(input.properties["hot"], serde_json::json!(false));
        assert!(input.published_at.is_some());
    }

    #[test]
    fn collects_errors() {
        let form = FormValues::from([("name".into(), "".into())]);
        let err = build_input(&form, &[prop("price", "number", true)]).unwrap_err();
        assert!(err.contains("Укажите название"));
        assert!(err.contains("обязательное"));
    }
}
