use crate::{
    access::{Access, COLLECTIONS_MANAGE, Level},
    error::AppResult,
    seo::{self, Kind, Settings, Target},
    state::AppState,
};
use axum::{
    Extension, Form,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use minijinja::context;

fn authorize(user: &Access, target: &Target) -> AppResult<()> {
    if target.kind == Kind::Collection {
        user.require(COLLECTIONS_MANAGE)
    } else {
        user.require_collection(target.collection.id, Level::Write)
    }
}
async fn page(
    state: &AppState,
    user: Access,
    target: Target,
    form: Settings,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let metadata = seo::resolve(&state.db, &target).await?;
    let kind = target.kind.path();
    let back = format!("/admin/{kind}/{}", target.id);
    super::render(
        state,
        "seo_form.html",
        context! {user,kind,id=>target.id,name=>target.name,collection=>target.collection,form,error,metadata,back},
    )
}
pub async fn form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path((kind, id)): Path<(String, i64)>,
) -> AppResult<Html<String>> {
    let target = seo::load(&state.db, Kind::parse(&kind)?, id).await?;
    authorize(&user, &target)?;
    let form = seo::settings(&state.db, &target).await?;
    page(&state, user, target, form, None).await
}
pub async fn save(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path((kind, id)): Path<(String, i64)>,
    Form(mut form): Form<Settings>,
) -> AppResult<Response> {
    let target = seo::load(&state.db, Kind::parse(&kind)?, id).await?;
    authorize(&user, &target)?;
    if let Err(error) = form.validate(&target) {
        // Общая HTMX-оболочка не отображает HTML ответов 4xx.
        return Ok(page(&state, user, target, form, Some(error))
            .await?
            .into_response());
    }
    seo::save(&state.db, &target, form).await?;
    Ok(Redirect::to(&format!("/admin/seo/{kind}/{id}")).into_response())
}
