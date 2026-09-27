use std::sync::Arc;

use minijinja::Environment;
use sqlx::PgPool;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub templates: Arc<Environment<'static>>,
    pub config: Arc<Config>,
}
