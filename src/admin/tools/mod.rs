//! Инструменты супер-администратора: SQL, таблицы БД, Rust-скрипты.
//!
//! SQL-консоль, обозреватель таблиц и хранение/редактор Rust-скриптов работают;
//! запуск скриптов — отдельная задача. Общий выключатель `ADMIN_TOOLS_ENABLED` и
//! доступ только супер-администратору.

pub(crate) mod db;
pub(crate) mod pg;
pub(crate) mod rust;
pub(crate) mod sql;

use axum::{
    Router,
    routing::{get, post},
};

use super::render;
use crate::{
    access::Access,
    error::{AppError, AppResult},
    state::AppState,
};

/// Включены ли инструменты (`ADMIN_TOOLS_ENABLED`).
pub(crate) fn enabled(state: &AppState) -> bool {
    state.config.tools_enabled
}

/// Общий пропуск на страницы инструментов: выключенный раздел — 404 (даже для супера,
/// чтобы не раскрывать его существование), не-супер — 403.
pub(crate) fn guard(state: &AppState, user: &Access) -> AppResult<()> {
    if !enabled(state) {
        return Err(AppError::NotFound);
    }
    user.require_super()
}

/// Маршруты инструментов (монтируются в админку, то есть под `/admin`).
pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/tools/sql", get(sql::page).post(sql::execute))
        .route("/tools/db", get(db::list))
        .route("/tools/db/create", post(db::table_create))
        .route("/tools/db/{schema}/{table}", get(db::table))
        .route("/tools/db/{schema}/{table}/drop", post(db::table_drop))
        .route("/tools/db/{schema}/{table}/insert", post(db::row_insert))
        .route("/tools/db/{schema}/{table}/update", post(db::row_update))
        .route("/tools/db/{schema}/{table}/delete", post(db::row_delete))
        .route(
            "/tools/db/{schema}/{table}/column-add",
            post(db::column_add),
        )
        .route(
            "/tools/db/{schema}/{table}/column-drop",
            post(db::column_drop),
        )
        .route("/tools/rust", get(rust::list).post(rust::save))
        .route("/tools/rust/new", get(rust::new_form))
        .route("/tools/rust/{id}", get(rust::edit).post(rust::update))
        .route("/tools/rust/{id}/delete", post(rust::delete))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::PgPool;

    use crate::{
        smoke_tests::get,
        state::AppState,
        test_support::{admin_cookie, permitted_cookie, test_state},
    };

    /// Состояние приложения с выключателем инструментов.
    fn state_with_tools(db: PgPool, tools_enabled: bool) -> AppState {
        let mut state = test_state(db);
        let mut config = (*state.config).clone();
        config.tools_enabled = tools_enabled;
        state.config = Arc::new(config);
        state
    }

    #[sqlx::test]
    async fn super_gets_tool_pages(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        for path in ["/admin/tools/sql", "/admin/tools/db", "/admin/tools/rust"] {
            let (status, _) = get(&app, path, &cookie).await;
            assert_eq!(status, 200, "{path}");
        }
    }

    #[sqlx::test]
    async fn non_super_forbidden(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = permitted_cookie(&db, "op", &[]).await;
        for path in ["/admin/tools/sql", "/admin/tools/db", "/admin/tools/rust"] {
            let (status, _) = get(&app, path, &cookie).await;
            assert_eq!(status, 403, "{path}");
        }
    }

    #[sqlx::test]
    async fn disabled_tools_404(db: PgPool) {
        let app = crate::app(state_with_tools(db.clone(), false));
        let super_cookie = admin_cookie(&db).await;
        let plain_cookie = permitted_cookie(&db, "op", &[]).await;
        for path in ["/admin/tools/sql", "/admin/tools/db", "/admin/tools/rust"] {
            // выключенный раздел не должен раскрывать даже факт своего существования
            for cookie in [&super_cookie, &plain_cookie] {
                let (status, _) = get(&app, path, cookie).await;
                assert_eq!(status, 404, "{path}");
            }
        }
    }

    const TOOL_LABELS: [&str; 3] = ["SQL-запрос", "Таблицы БД", "Rust-скрипты"];

    #[sqlx::test]
    async fn tools_menu_only_for_super(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (status, body) = get(&app, "/admin", &cookie).await;
        assert_eq!(status, 200);
        for label in TOOL_LABELS {
            assert!(body.contains(label), "супер не видит пункт {label}");
        }
        let (_, body) = get(&app, "/admin/palette?q=sql", &cookie).await;
        assert!(body.contains("SQL-запрос"), "палитра не нашла инструмент");

        // обычный админ с системным разделом инструменты не видит
        let cookie = permitted_cookie(&db, "op", &["settings.manage"]).await;
        let (_, body) = get(&app, "/admin", &cookie).await;
        assert!(
            body.contains("Настройки сайта"),
            "системная группа не отрисована"
        );
        for label in TOOL_LABELS {
            assert!(!body.contains(label), "обычный админ видит {label}");
        }
    }

    #[sqlx::test]
    async fn tools_menu_hidden_when_disabled(db: PgPool) {
        let app = crate::app(state_with_tools(db.clone(), false));
        let cookie = admin_cookie(&db).await;
        let (status, body) = get(&app, "/admin", &cookie).await;
        assert_eq!(status, 200);
        for label in TOOL_LABELS {
            assert!(
                !body.contains(label),
                "выключенные инструменты в меню: {label}"
            );
        }
        let (_, body) = get(&app, "/admin/palette?q=sql", &cookie).await;
        assert!(
            !body.contains("SQL-запрос"),
            "выключенный инструмент в палитре"
        );
    }
}
