use std::sync::Arc;

use minijinja::Environment;
use sqlx::PgPool;

use crate::{
    bxapi::{project::Project, registry::Registry},
    config::Config,
};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub templates: Arc<Environment<'static>>,
    pub config: Arc<Config>,
    /// Снимок схемы коллекций для API bxapi.
    pub registry: Arc<Registry>,
    /// Проектные настройки bxapi (`BXAPI_PROJECT`).
    pub project: Arc<Project>,
}
