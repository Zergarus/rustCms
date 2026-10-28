//! Полоса открытых записей: вкладки хранятся на пользователя в `admin_tabs`.

use axum::{
    Extension, Form,
    extract::{Path, State},
    http::{HeaderValue, header::HeaderName},
    response::{Html, IntoResponse, Response},
};
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::{
    nav::{self, TabKind},
    render,
};
use crate::{
    access::Access,
    error::{AppError, AppResult},
    state::AppState,
};

/// Сколько вкладок хранится на пользователя.
pub(crate) const LIMIT: i64 = 10;
/// Суффикс `<title>` страниц админки.
const TITLE_SUFFIX: &str = " — CMS";
const TITLE_MAX: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Tab {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub kind: String,
    pub active: bool,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TabError {
    #[error("недопустимый адрес вкладки")]
    BadUrl,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl From<TabError> for AppError {
    fn from(e: TabError) -> Self {
        match e {
            TabError::BadUrl => AppError::BadRequest(e.to_string()),
            TabError::Db(e) => AppError::Db(e),
        }
    }
}

/// Вкладки пользователя по порядку открытия; активна та, что соответствует `current_path`.
pub(crate) async fn list(db: &PgPool, user_id: i64, current_path: &str) -> sqlx::Result<Vec<Tab>> {
    let current = nav::tab_key(current_path);
    let rows: Vec<(i64, String, String, String, String)> = sqlx::query_as(
        "SELECT id, key, url, title, kind FROM admin_tabs WHERE user_id = $1 ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, key, url, title, kind)| Tab {
            id,
            active: current.as_deref() == Some(key.as_str()),
            url,
            title,
            kind,
        })
        .collect())
}

fn url_ok(url: &str) -> bool {
    url.starts_with("/admin/")
        && !url.contains("//")
        && !url.contains('\\')
        && !url.chars().any(char::is_control)
}

/// Браузер присылает `document.title`, а страница уже отрисована с экранированием
/// MiniJinja (`& < > " ' /`); храним простой текст, шаблон экранирует его заново.
fn unescape_html(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&#x2f;", "/")
        .replace("&amp;", "&")
}

fn clean_title(title: &str, key: &str) -> String {
    let title = unescape_html(title);
    let title = title.trim();
    let title = title.strip_suffix(TITLE_SUFFIX).unwrap_or(title).trim();
    if title.is_empty() {
        return key.chars().take(TITLE_MAX).collect();
    }
    title.chars().take(TITLE_MAX).collect()
}

/// Открывает (или обновляет) вкладку. `None` — адрес во вкладки не попадает.
pub(crate) async fn open(
    db: &PgPool,
    user_id: i64,
    url: &str,
    title: &str,
) -> Result<Option<i64>, TabError> {
    if !url_ok(url) {
        return Err(TabError::BadUrl);
    }
    let Some(key) = nav::tab_key(url) else {
        return Ok(None);
    };
    let kind = match nav::tab_kind(url) {
        TabKind::Content => "content",
        TabKind::Shop => "shop",
        TabKind::System => "system",
    };
    let title = clean_title(title, &key);
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO admin_tabs (user_id, key, url, title, kind) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (user_id, key)
         DO UPDATE SET url = EXCLUDED.url, title = EXCLUDED.title, kind = EXCLUDED.kind,
                       opened_at = now()
         RETURNING id",
    )
    .bind(user_id)
    .bind(&key)
    .bind(url)
    .bind(&title)
    .bind(kind)
    .fetch_one(db)
    .await?;
    // сверх лимита вытесняются самые старые, но не только что открытая
    sqlx::query(
        "DELETE FROM admin_tabs WHERE id IN (
             SELECT id FROM admin_tabs WHERE user_id = $1 AND id <> $2
             ORDER BY opened_at, id
             LIMIT GREATEST((SELECT count(*) FROM admin_tabs WHERE user_id = $1) - $3, 0))",
    )
    .bind(user_id)
    .bind(id)
    .bind(LIMIT)
    .execute(db)
    .await?;
    Ok(Some(id))
}

fn fragment(state: &AppState, tabs: Vec<Tab>) -> AppResult<Html<String>> {
    render(state, "_tabs.html", context! { tabs })
}

#[derive(Deserialize)]
pub(super) struct OpenForm {
    url: String,
    #[serde(default)]
    title: String,
}

/// `POST /admin/tabs`: открыть вкладку для текущей страницы.
pub(super) async fn open_tab(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<OpenForm>,
) -> AppResult<Html<String>> {
    open(&state.db, user.user.id, &form.url, &form.title).await?;
    let tabs = list(&state.db, user.user.id, &form.url).await?;
    fragment(&state, tabs)
}

#[derive(Deserialize)]
pub(super) struct CloseForm {
    #[serde(default)]
    current: String,
}

/// `POST /admin/tabs/{id}/close`: закрыть вкладку; если она была активной,
/// браузер переходит на соседнюю (правую, иначе левую, иначе на рабочий стол).
pub(super) async fn close_tab(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<CloseForm>,
) -> AppResult<Response> {
    let before = list(&state.db, user.user.id, &form.current).await?;
    let Some(pos) = before.iter().position(|t| t.id == id) else {
        return Err(AppError::NotFound);
    };
    sqlx::query("DELETE FROM admin_tabs WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(user.user.id)
        .execute(&state.db)
        .await?;
    let target = before[pos].active.then(|| {
        before
            .get(pos + 1)
            .or_else(|| pos.checked_sub(1).and_then(|p| before.get(p)))
            .map_or("/admin", |t| t.url.as_str())
            .to_string()
    });
    let mut rest = before;
    rest.remove(pos);
    let mut res = fragment(&state, rest)?.into_response();
    if let Some(path) = target {
        let value = serde_json::json!({"path": path, "target": "body"}).to_string();
        if let Ok(v) = HeaderValue::from_str(&value) {
            res.headers_mut()
                .insert(HeaderName::from_static("hx-location"), v);
        }
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use sqlx::PgPool;

    use crate::test_support::{admin_cookie, get_raw, post_form_raw, test_state, user_cookie};

    fn enc(s: &str) -> String {
        s.bytes().fold(String::new(), |mut acc, b| {
            if b.is_ascii_alphanumeric() {
                acc.push(b as char);
            } else {
                acc.push_str(&format!("%{b:02X}"));
            }
            acc
        })
    }

    async fn open_tab(
        app: &axum::Router,
        cookie: &str,
        url: &str,
        title: &str,
    ) -> (StatusCode, String) {
        let body = format!("url={}&title={}", enc(url), enc(title));
        let (s, _, b) = post_form_raw(app, "/admin/tabs", cookie, &body).await;
        (s, b)
    }

    async fn count(db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM admin_tabs")
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn open_creates_and_reuses(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        open_tab(&app, &cookie, "/admin/shop/orders/7", "Заказ 7").await;
        let (status, body) =
            open_tab(&app, &cookie, "/admin/shop/orders/7/shipment", "Заказ 7").await;
        assert_eq!(status, 200);
        assert!(body.contains("orders&#x2f;7&#x2f;shipment"), "{body}");
        assert_eq!(count(&db).await, 1);
        let url: String = sqlx::query_scalar("SELECT url FROM admin_tabs")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(url, "/admin/shop/orders/7/shipment");
    }

    #[sqlx::test]
    async fn open_skips_new_forms(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (status, _) = open_tab(&app, &cookie, "/admin/users/new", "Новый").await;
        assert_eq!(status, 200);
        assert_eq!(count(&db).await, 0);
    }

    #[sqlx::test]
    async fn limit_evicts_oldest_not_current(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        for i in 1..=11 {
            open_tab(&app, &cookie, &format!("/admin/items/{i}"), "Запись").await;
        }
        assert_eq!(count(&db).await, 10);
        let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM admin_tabs")
            .fetch_all(&db)
            .await
            .unwrap();
        assert!(!keys.contains(&"/admin/items/1".to_string()));
        assert!(keys.contains(&"/admin/items/11".to_string()));
    }

    #[sqlx::test]
    async fn close_active_moves_to_neighbor(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        for n in ["a", "b", "c"] {
            open_tab(&app, &cookie, &format!("/admin/items/{n}"), n).await;
        }
        let id = |n: &str| {
            let db = db.clone();
            let key = format!("/admin/items/{n}");
            async move {
                sqlx::query_scalar::<_, i64>("SELECT id FROM admin_tabs WHERE key = $1")
                    .bind(key)
                    .fetch_one(&db)
                    .await
                    .unwrap()
            }
        };
        let loc = |h: &axum::http::HeaderMap| -> serde_json::Value {
            serde_json::from_str(h["hx-location"].to_str().unwrap()).unwrap()
        };
        let (s, h, _) = post_form_raw(
            &app,
            &format!("/admin/tabs/{}/close", id("b").await),
            &cookie,
            "current=%2Fadmin%2Fitems%2Fb",
        )
        .await;
        assert_eq!(s, 200);
        assert_eq!(
            loc(&h),
            serde_json::json!({"path": "/admin/items/c", "target": "body"})
        );
        let (_, h, _) = post_form_raw(
            &app,
            &format!("/admin/tabs/{}/close", id("c").await),
            &cookie,
            "current=%2Fadmin%2Fitems%2Fc",
        )
        .await;
        assert_eq!(loc(&h)["path"], "/admin/items/a");
        let (_, h, _) = post_form_raw(
            &app,
            &format!("/admin/tabs/{}/close", id("a").await),
            &cookie,
            "current=%2Fadmin%2Fitems%2Fa",
        )
        .await;
        assert_eq!(loc(&h)["path"], "/admin");
    }

    #[sqlx::test]
    async fn close_inactive_has_no_redirect(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        open_tab(&app, &cookie, "/admin/items/a", "a").await;
        let id: i64 = sqlx::query_scalar("SELECT id FROM admin_tabs")
            .fetch_one(&db)
            .await
            .unwrap();
        let (s, h, _) = post_form_raw(
            &app,
            &format!("/admin/tabs/{id}/close"),
            &cookie,
            "current=%2Fadmin%2Fusers",
        )
        .await;
        assert_eq!(s, 200);
        assert!(h.get("hx-location").is_none());
        assert_eq!(count(&db).await, 0);
    }

    #[sqlx::test]
    async fn close_foreign_404(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let owner = admin_cookie(&db).await;
        let (_, other) = user_cookie(&db, "other", true).await;
        open_tab(&app, &owner, "/admin/items/a", "a").await;
        let id: i64 = sqlx::query_scalar("SELECT id FROM admin_tabs")
            .fetch_one(&db)
            .await
            .unwrap();
        let (s, _, _) =
            post_form_raw(&app, &format!("/admin/tabs/{id}/close"), &other, "current=").await;
        assert_eq!(s, 404);
        assert_eq!(count(&db).await, 1);
    }

    #[sqlx::test]
    async fn title_escaped_and_capped(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let title = format!("<script>x</script>{} — CMS", "я".repeat(300));
        let (_, body) = open_tab(&app, &cookie, "/admin/items/1", &title).await;
        assert!(body.contains("&lt;script&gt;"));
        assert!(!body.contains("<script>"));
        let stored: String = sqlx::query_scalar("SELECT title FROM admin_tabs")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(stored.chars().count(), 200);
    }

    #[sqlx::test]
    async fn bad_url_rejected(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        for url in ["https://evil.test/admin/", "//evil.test", "/admin//x"] {
            let (status, _) = open_tab(&app, &cookie, url, "x").await;
            assert_eq!(status, 400, "{url}");
        }
        assert_eq!(count(&db).await, 0);
    }

    #[sqlx::test]
    async fn deleted_record_tab_removed(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        open_tab(&app, &cookie, "/admin/shop/orders/999999", "Заказ").await;
        assert_eq!(count(&db).await, 1);
        let (status, _, _) = get_raw(&app, "/admin/shop/orders/999999", &cookie).await;
        assert_eq!(status, 404);
        assert_eq!(count(&db).await, 0);
    }

    /// JSON из атрибута `hx-vals` вкладки на странице.
    fn tab_vals(body: &str) -> serde_json::Value {
        let tail = body.split("hx-vals='").nth(1).expect("hx-vals");
        serde_json::from_str(tail.split('\'').next().unwrap()).unwrap()
    }

    #[sqlx::test]
    async fn page_renders_trigger_with_own_url_and_title(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (status, _, body) = get_raw(&app, "/admin/shop/stores?a=1", &cookie).await;
        let body = String::from_utf8_lossy(&body);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(r#"hx-post="/admin/tabs""#));
        assert!(body.contains("<title>Склады"));
        let vals = tab_vals(&body);
        assert_eq!(vals["url"], "/admin/shop/stores?a=1");
        assert!(vals["title"].as_str().unwrap().starts_with("Склады"));
    }

    #[sqlx::test]
    async fn title_with_special_chars_roundtrips_once_escaped(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name) VALUES ('c1', 'A & \"B\"') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let url = format!("/admin/collections/{id}");
        let (status, _, body) = get_raw(&app, &url, &cookie).await;
        assert_eq!(status, 200);
        let vals = tab_vals(&String::from_utf8_lossy(&body));
        let sent = vals["title"].as_str().unwrap();
        assert!(sent.starts_with("A &amp; &quot;B&quot;"), "{sent}");
        let (_, frag) = open_tab(&app, &cookie, &url, sent).await;
        let stored: String = sqlx::query_scalar("SELECT title FROM admin_tabs")
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(stored.starts_with("A & \"B\""), "{stored}");
        assert!(frag.contains("A &amp; &quot;B&quot;"), "{frag}");
        assert!(!frag.contains("&amp;amp;"));
    }

    #[sqlx::test]
    async fn module_off_page_has_no_tab_trigger(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        crate::admin::nav::set_shop_enabled(&db, false)
            .await
            .unwrap();
        let (status, _, body) = get_raw(&app, "/admin/shop/stores", &cookie).await;
        let body = String::from_utf8_lossy(&body);
        assert_eq!(status, 404);
        assert!(!body.contains(r#"hx-post="/admin/tabs""#), "{body}");
    }
}
