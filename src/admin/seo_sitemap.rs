//! «SEO → Sitemap»: типы карты сайта, как `b_seo_sitemap` в Битриксе.

use axum::{
    Extension,
    body::Bytes,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use minijinja::context;

use super::{flash, render};
use crate::{
    access::{Access, COLLECTIONS_MANAGE},
    collection::repo,
    error::{AppError, is_unique_violation},
    sitemap::{self, Input},
    state::AppState,
};

/// Разбор тела формы: `collections` приходит повторяющимся полем, поэтому
/// `serde_urlencoded` не подходит и пары читаются напрямую.
fn parse(body: &Bytes) -> Input {
    let mut input = Input {
        include: "items".into(),
        sort: 500,
        active: false,
        ..Input::default()
    };
    for (key, value) in form_urlencoded::parse(body) {
        match key.as_ref() {
            "name" => input.name = value.trim().to_string(),
            "sort" => input.sort = value.parse().unwrap_or(500),
            "path_template" => input.path_template = value.trim().to_string(),
            "include" => input.include = value.into_owned(),
            "active" => input.active = true,
            "collection" => {
                if let Ok(id) = value.parse() {
                    input.collections.push(id);
                }
            }
            _ => {}
        }
    }
    input
}

async fn options(
    state: &AppState,
) -> crate::error::AppResult<Vec<crate::collection::CollectionSummary>> {
    Ok(repo::list_collections(&state.db).await?)
}

async fn page(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: Input,
    error: Option<String>,
) -> crate::error::AppResult<Html<String>> {
    let collections = options(state).await?;
    render(
        state,
        "sitemap_form.html",
        context! { user, id, form, error, collections },
    )
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> crate::error::AppResult<Html<String>> {
    user.require(COLLECTIONS_MANAGE)?;
    let entries = sitemap::list(&state.db, false).await?;
    render(&state, "sitemap_list.html", context! { user, entries })
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> crate::error::AppResult<Html<String>> {
    user.require(COLLECTIONS_MANAGE)?;
    page(&state, user, None, Input::default(), None).await
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Html<String>> {
    user.require(COLLECTIONS_MANAGE)?;
    let entry = sitemap::get(&state.db, id).await?;
    let form = Input {
        name: entry.sitemap.name.clone(),
        active: entry.sitemap.active,
        sort: entry.sitemap.sort,
        path_template: entry.sitemap.path_template.clone(),
        include: entry.sitemap.include.clone(),
        collections: entry.sources.iter().map(|s| s.collection_id).collect(),
    };
    page(&state, user, Some(id), form, None).await
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    body: Bytes,
) -> crate::error::AppResult<Response> {
    user.require(COLLECTIONS_MANAGE)?;
    let input = parse(&body);
    if let Err(error) = input.validate() {
        return Ok(page(&state, user, None, input, Some(error))
            .await?
            .into_response());
    }
    match sitemap::save(&state.db, None, &input).await {
        Ok(_) => Ok(Redirect::to("/admin/seo/sitemap").into_response()),
        Err(e) if matches!(&e, AppError::Db(err) if is_unique_violation(err)) => Ok(page(
            &state,
            user,
            None,
            input,
            Some("Тип с таким именем уже есть".into()),
        )
        .await?
        .into_response()),
        Err(e) => Err(e),
    }
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    body: Bytes,
) -> crate::error::AppResult<Response> {
    user.require(COLLECTIONS_MANAGE)?;
    let input = parse(&body);
    if let Err(error) = input.validate() {
        return Ok(page(&state, user, Some(id), input, Some(error))
            .await?
            .into_response());
    }
    match sitemap::save(&state.db, Some(id), &input).await {
        Ok(_) => Ok(Redirect::to("/admin/seo/sitemap").into_response()),
        Err(e) if matches!(&e, AppError::Db(err) if is_unique_violation(err)) => Ok(page(
            &state,
            user,
            Some(id),
            input,
            Some("Тип с таким именем уже есть".into()),
        )
        .await?
        .into_response()),
        Err(e) => Err(e),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Response> {
    user.require(COLLECTIONS_MANAGE)?;
    sitemap::remove(&state.db, id).await?;
    Ok(flash::redirect(
        "/admin/seo/sitemap",
        "Тип удалён",
        flash::FlashKind::Ok,
    ))
}
