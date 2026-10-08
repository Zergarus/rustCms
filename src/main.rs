mod access;
mod admin;
mod api;
mod auth;
mod bxapi;
mod cart;
mod catalog;
mod config;
mod error;
mod files;
mod groups;
mod iblock;
mod import;
mod mail;
mod passwords;
mod sale;
mod state;
#[cfg(test)]
mod test_support;
mod uploads;
mod users;

use std::{io::Write, sync::Arc, time::Duration};

use anyhow::{Context, bail};
use axum::{
    Router,
    http::{HeaderValue, header},
    response::Redirect,
    routing::get,
};
use minijinja::{Environment, path_loader};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower_http::{
    cors::{Any, CorsLayer},
    set_header::SetResponseHeaderLayer,
    trace::TraceLayer,
};
use tracing_subscriber::EnvFilter;

use crate::{config::Config, state::AppState};

const USAGE: &str = "\
Использование:
  cms [serve]                 запустить сервер (по умолчанию)
  cms migrate                 применить миграции и выйти
  cms create-admin <login>    создать администратора или сменить ему пароль
                              (пароль спрашивается в консоли или берётся из CMS_ADMIN_PASSWORD)
  cms import-bitrix <mysql-url> [--upload <каталог>] [--replace] [--api-code ID=код ...]
                              перенести инфоблоки, HL-блоки, каталог и пользователей из базы
                              Битрикса; --upload — каталог upload сайта (файлы линкуются в
                              UPLOAD_DIR), --replace — удалить уже существующие инфоблоки,
                              --api-code — apiCode инфоблока, если API_CODE в базе не заполнен";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("cms=debug,tower_http=info,sqlx=warn")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("serve");
    if matches!(command, "-h" | "--help" | "help") {
        println!("{USAGE}");
        return Ok(());
    }

    let config = Config::from_env()?;
    let db = connect(&config).await?;

    match command {
        "serve" => serve(config, db).await,
        "migrate" => {
            tracing::info!("миграции применены");
            Ok(())
        }
        "create-admin" => {
            let login = args.get(1).context(USAGE)?;
            create_admin(&db, login).await
        }
        "import-bitrix" => {
            let opts = import_options(&args[1..], &config).context(USAGE)?;
            import::run(&db, opts).await
        }
        other => bail!("неизвестная команда `{other}`\n\n{USAGE}"),
    }
}

async fn connect(config: &Config) -> anyhow::Result<PgPool> {
    let db = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&config.database_url)
        .await
        .context("не удалось подключиться к PostgreSQL")?;
    sqlx::migrate!().run(&db).await.context("ошибка миграций")?;
    Ok(db)
}

fn import_options(args: &[String], config: &Config) -> anyhow::Result<import::Options> {
    let mut mysql_url = None;
    let mut bitrix_upload = None;
    let mut replace = false;
    let mut api_codes = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--replace" => replace = true,
            "--api-code" => {
                let raw = it.next().context("--api-code без значения")?;
                let (id, code) = raw
                    .split_once('=')
                    .context("--api-code: ожидается ID=код")?;
                let id = id
                    .trim()
                    .parse()
                    .context("--api-code: ID должен быть числом")?;
                api_codes.push((id, code.trim().to_string()));
            }
            "--upload" => bitrix_upload = Some(it.next().context("--upload без каталога")?.into()),
            other if mysql_url.is_none() && !other.starts_with("--") => {
                mysql_url = Some(other.to_string())
            }
            other => bail!("неизвестный аргумент `{other}`"),
        }
    }
    Ok(import::Options {
        mysql_url: mysql_url.context("не указан адрес MySQL")?,
        bitrix_upload,
        upload_dir: config.upload_dir.clone(),
        replace,
        api_codes,
    })
}

async fn create_admin(db: &PgPool, login: &str) -> anyhow::Result<()> {
    let password = match std::env::var("CMS_ADMIN_PASSWORD") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            print!("Пароль для {login}: ");
            std::io::stdout().flush()?;
            rpassword::read_password()?
        }
    };
    if password.chars().count() < 8 {
        bail!("пароль должен быть не короче 8 символов");
    }
    let user = auth::create_user(db, login, password, true).await?;
    println!("Администратор `{}` (id {}) сохранён", user.login, user.id);
    Ok(())
}

fn cors_layer(config: &Config) -> CorsLayer {
    let layer = CorsLayer::new().allow_methods(Any).allow_headers(Any);
    if config.cors_origins.iter().any(|o| o == "*") {
        layer.allow_origin(Any)
    } else {
        let origins: Vec<HeaderValue> = config
            .cors_origins
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect();
        layer.allow_origin(origins)
    }
}

async fn serve(config: Config, db: PgPool) -> anyhow::Result<()> {
    let mut env = Environment::new();
    env.set_loader(path_loader(config.templates_dir.join("admin")));
    let state = AppState {
        db,
        templates: Arc::new(env),
        config: Arc::new(config),
        registry: Arc::default(),
        project: bxapi::project::Project::from_env(),
    };

    let app = Router::new()
        .route("/", get(|| async { Redirect::to("/admin") }))
        .nest("/api", api::router().layer(cors_layer(&state.config)))
        .nest("/api/v1", bxapi::router().layer(cors_layer(&state.config)))
        .nest("/admin", admin::router(state.clone()))
        .nest(
            "/upload",
            uploads::router().layer(SetResponseHeaderLayer::overriding(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            )),
        )
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&state.config.bind_addr)
        .await
        .with_context(|| format!("не удалось занять адрес {}", state.config.bind_addr))?;
    tracing::info!("CMS запущена: http://{}/admin", state.config.bind_addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;
    Ok(())
}
