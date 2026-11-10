//! «Журнал действий»: последние изменения в админке (только супер).

use axum::{
    Extension,
    extract::{Query, State},
    response::Html,
};
use minijinja::context;
use serde::Deserialize;

use super::render;
use crate::{access::Access, audit, error::AppResult, state::AppState};

#[derive(Deserialize, Default, serde::Serialize)]
pub struct Filters {
    #[serde(default)]
    entity: String,
    #[serde(default)]
    login: String,
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(filters): Query<Filters>,
) -> AppResult<Html<String>> {
    user.require_super()?;
    let entries = audit::list(
        &state.db,
        Some(filters.entity.as_str()),
        Some(filters.login.as_str()),
        300,
    )
    .await?;
    render(&state, "audit.html", context! { user, entries, filters })
}
