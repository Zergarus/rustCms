//! Контекст оболочки админки: меню, настройки пользователя, уведомление, вкладки.

use axum::{Extension, extract::State, response::Response};
use axum_extra::extract::Form;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;

use super::{
    flash::{self, Flash, FlashKind},
    nav::Nav,
    tabs::Tab,
};
use crate::{access::Access, error::AppResult, state::AppState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Theme {
    Dark,
    Light,
    #[default]
    System,
}

impl Theme {
    fn parse(s: &str) -> Option<Theme> {
        match s {
            "dark" => Some(Theme::Dark),
            "light" => Some(Theme::Light),
            "system" => Some(Theme::System),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Prefs {
    pub theme: Theme,
    pub tabs: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            theme: Theme::System,
            tabs: true,
        }
    }
}

impl Prefs {
    /// Неизвестные значения заменяются умолчаниями.
    pub(crate) fn from_json(v: &Value) -> Prefs {
        Prefs {
            theme: v
                .get("theme")
                .and_then(Value::as_str)
                .and_then(Theme::parse)
                .unwrap_or_default(),
            tabs: v.get("tabs").and_then(Value::as_bool).unwrap_or(true),
        }
    }
}

pub(crate) async fn load_prefs(db: &PgPool, user_id: i64) -> sqlx::Result<Prefs> {
    let v: Option<Value> = sqlx::query_scalar("SELECT admin_prefs FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(db)
        .await?;
    Ok(v.as_ref().map(Prefs::from_json).unwrap_or_default())
}

/// Всё, что нужно базовому шаблону полной страницы (`user.shell`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Shell {
    pub nav: Nav,
    pub prefs: Prefs,
    pub flash: Option<Flash>,
    pub tabs: Vec<Tab>,
    pub path: String,
    /// Адрес запроса вместе с query: его открывает вкладка.
    pub url: String,
}

#[derive(Deserialize)]
pub(super) struct PrefsForm {
    theme: Option<String>,
    tabs: Option<String>,
    back: Option<String>,
}

/// `POST /admin/prefs`: тема и полоса вкладок.
pub(super) async fn save_prefs(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<PrefsForm>,
) -> AppResult<Response> {
    let theme = form
        .theme
        .as_deref()
        .and_then(Theme::parse)
        .unwrap_or_default();
    let prefs = Prefs {
        theme,
        tabs: form.tabs.as_deref() == Some("on"),
    };
    sqlx::query("UPDATE users SET admin_prefs = $2 WHERE id = $1")
        .bind(user.user.id)
        .bind(serde_json::json!({"theme": prefs.theme, "tabs": prefs.tabs}))
        .execute(&state.db)
        .await?;
    let back = form
        .back
        .as_deref()
        .filter(|b| b.starts_with("/admin/"))
        .unwrap_or("/admin");
    Ok(flash::redirect(back, "Настройки сохранены", FlashKind::Ok))
}

#[cfg(test)]
mod tests {
    use axum::http::header;
    use sqlx::PgPool;

    use crate::test_support::{admin_cookie, post_form_raw, test_state};

    #[sqlx::test]
    async fn prefs_saved_and_validated(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (status, headers, _) = post_form_raw(
            &app,
            "/admin/prefs",
            &cookie,
            "theme=dark&back=/admin/users",
        )
        .await;
        assert_eq!(status, 303);
        assert_eq!(headers[header::LOCATION], "/admin/users");
        let prefs: serde_json::Value =
            sqlx::query_scalar("SELECT admin_prefs FROM users WHERE login = 'smoke'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(prefs, serde_json::json!({"theme": "dark", "tabs": false}));

        let (status, headers, _) = post_form_raw(
            &app,
            "/admin/prefs",
            &cookie,
            "theme=evil&tabs=on&back=https://x.test",
        )
        .await;
        assert_eq!(status, 303);
        assert_eq!(headers[header::LOCATION], "/admin");
        let prefs: serde_json::Value =
            sqlx::query_scalar("SELECT admin_prefs FROM users WHERE login = 'smoke'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(prefs, serde_json::json!({"theme": "system", "tabs": true}));
    }
}

#[cfg(test)]
mod page_tests {
    use axum::http::header;
    use sqlx::PgPool;

    use crate::{
        admin::nav,
        smoke_tests::get,
        test_support::{admin_cookie, get_raw, htmx_get, test_state},
    };

    fn with_flash(cookie: &str, value: &str) -> String {
        format!("{cookie}; admin_flash={value}")
    }

    fn hex(s: &str) -> String {
        s.bytes().map(|b| format!("{b:02x}")).collect()
    }

    #[sqlx::test]
    async fn flash_malformed_ignored_on_page(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        for bad in ["zz", "ff"] {
            let (status, _) = get(&app, "/admin", &with_flash(&cookie, bad)).await;
            assert_eq!(status, 200);
        }
    }

    #[sqlx::test]
    async fn flash_shown_once(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db);
        let cookie = with_flash(&cookie.await, &hex("ok:Заказ сохранён"));
        let (status, headers, body) = get_raw(&app, "/admin", &cookie).await;
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&body).contains("Заказ сохранён"));
        let cleared = headers.get_all(header::SET_COOKIE).iter().any(|v| {
            let v = v.to_str().unwrap();
            v.starts_with("admin_flash=;") && v.contains("Max-Age=0")
        });
        assert!(cleared);
        // фрагмент HTMX не показывает и не гасит уведомление
        let (status, body) = htmx_get(&app, "/admin", &cookie, "main").await;
        assert_eq!(status, 200);
        assert!(!body.contains("Заказ сохранён"));
    }

    #[sqlx::test]
    async fn shell_renders_cms_menu(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (status, body) = get(&app, "/admin", &cookie).await;
        assert_eq!(status, 200);
        for s in [
            "Контент",
            "Модули",
            "Система",
            "data-palette-open",
            "id=\"open-tabs\"",
        ] {
            assert!(body.contains(s), "нет {s}");
        }
        assert!(!body.contains("Типы цен"));
    }

    #[sqlx::test]
    async fn shell_renders_shop_menu(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (status, body) = get(&app, "/admin/shop/stores", &cookie).await;
        assert_eq!(status, 200);
        for s in ["Все разделы", "Типы цен", "Поставщики"] {
            assert!(body.contains(s), "нет {s}");
        }
        assert!(!body.contains("Документы"));
    }

    #[sqlx::test]
    async fn shell_theme_attribute_from_prefs(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (_, body) = get(&app, "/admin", &cookie).await;
        assert!(!body.contains("data-theme="));
        sqlx::query("UPDATE users SET admin_prefs = '{\"theme\":\"light\"}' WHERE login = 'smoke'")
            .execute(&db)
            .await
            .unwrap();
        let (_, body) = get(&app, "/admin", &cookie).await;
        assert!(body.contains("data-theme=\"light\""));
    }

    #[test]
    fn shell_old_shop_nav_gone() {
        assert!(!std::path::Path::new("templates/admin/shop/_nav.html").exists());
    }

    #[sqlx::test]
    async fn module_off_404(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (api_status, api_body) = get(&app, "/api/v1/iblock/list", &cookie).await;
        nav::set_shop_enabled(&db, false).await.unwrap();
        let (status, body) = get(&app, "/admin/shop/orders", &cookie).await;
        assert_eq!(status, 404);
        assert!(body.contains("Модуль „Магазин“ выключен"));
        assert!(body.contains("/admin/modules"));
        assert_eq!(get(&app, "/admin/discounts", &cookie).await.0, 404);
        let (status, body) = get(&app, "/api/v1/iblock/list", &cookie).await;
        assert_eq!((status, body), (api_status, api_body));
    }
}
