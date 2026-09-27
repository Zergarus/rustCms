use std::{env, path::PathBuf};

use anyhow::Context;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,
    /// Разрешённые origin'ы для публичного API (Vue-фронт). `*` — любой.
    pub cors_origins: Vec<String>,
    pub templates_dir: PathBuf,
    pub static_dir: PathBuf,
    /// Ставить флаг Secure на cookie сессии (включать за HTTPS).
    pub cookie_secure: bool,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            database_url: env::var("DATABASE_URL").context("DATABASE_URL не задан")?,
            bind_addr: var_or("BIND_ADDR", "127.0.0.1:3000"),
            cors_origins: var_or("CORS_ORIGINS", "http://localhost:5173")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            templates_dir: var_or("TEMPLATES_DIR", "templates").into(),
            static_dir: var_or("STATIC_DIR", "static").into(),
            cookie_secure: var_or("COOKIE_SECURE", "false") == "true",
        })
    }
}

fn var_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}
