//! Сохранённые виды списков: личные и общие, закрепление, проверки прав.

use std::collections::{BTreeSet, HashMap};

use axum::{
    Extension, Form,
    extract::{OriginalUri, Path, RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri},
    response::Response,
};
use serde::Serialize;
use sqlx::PgPool;

use super::{
    spec::ListSpec,
    state::{self, ListState, Parsed, to_query},
};
use crate::{
    access::{Access, Level},
    admin::flash::{FlashKind, toast},
    collection::repo,
    error::{AppError, AppResult},
    state::AppState,
};

const NAME_MAX: usize = 100;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct View {
    pub id: i64,
    pub list_key: String,
    pub owner_id: i64,
    pub name: String,
    pub query: String,
    pub shared: bool,
    pub pinned: bool,
}

const SELECT: &str = "SELECT v.id, v.list_key, v.owner_id, v.name, v.query, v.shared,
        (p.user_id IS NOT NULL) AS pinned
     FROM admin_views v
     LEFT JOIN admin_view_pins p ON p.view_id = v.id AND p.user_id = $1";

/// Общие, затем свои; внутри — по `sort`, `id`.
pub async fn visible(db: &PgPool, user_id: i64, list_key: &str) -> sqlx::Result<Vec<View>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE v.list_key = $2 AND (v.shared OR v.owner_id = $1)
         ORDER BY v.shared DESC, v.sort, v.id"
    )))
    .bind(user_id)
    .bind(list_key)
    .fetch_all(db)
    .await
}

pub async fn get_visible(db: &PgPool, user_id: i64, id: i64) -> sqlx::Result<Option<View>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE v.id = $2 AND (v.shared OR v.owner_id = $1)"
    )))
    .bind(user_id)
    .bind(id)
    .fetch_optional(db)
    .await
}

/// Общий вид правится при праве делиться (даже владельцем), личный — только владельцем.
pub fn may_edit(v: &View, user_id: i64, can_share: bool) -> bool {
    if v.shared {
        can_share
    } else {
        v.owner_id == user_id
    }
}

/// В строке запроса нет ничего, кроме размера и номера страницы.
pub fn is_blank(query: &str) -> bool {
    form_urlencoded::parse(query.as_bytes()).all(|(k, _)| k == "per" || k == "page")
}

/// Нормализованная строка запроса без страницы и вида.
fn normalize(spec: &ListSpec, query: &str) -> String {
    let parsed = state::parse(spec, query);
    to_query(spec, &parsed.state, false)
}

pub fn list_path(list_key: &str) -> Option<String> {
    if list_key == "orders" {
        return Some("/admin/shop/orders".into());
    }
    let id: i64 = list_key.strip_prefix("collection:")?.parse().ok()?;
    Some(format!("/admin/collections/{id}/items"))
}

/// Спека списка по ключу вида с проверкой прав на сам список.
pub async fn spec_for_key(db: &PgPool, user: &Access, key: &str) -> AppResult<ListSpec> {
    if key == "orders" {
        crate::admin::shop::orders::require_orders(user)?;
        return Ok(super::orders::orders_spec(db, user).await?);
    }
    let id: i64 = key
        .strip_prefix("collection:")
        .and_then(|s| s.parse().ok())
        .ok_or(AppError::NotFound)?;
    user.require_collection(id, Level::Read)?;
    let collection = repo::get_collection(db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(super::collection::collection_spec(db, user, &collection).await?)
}

pub struct Resolved {
    pub parsed: Parsed,
    pub view: Option<View>,
    pub notices: Vec<String>,
}

fn pairs_of(q: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(q.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn ser(pairs: &[(String, String)]) -> String {
    let mut s = form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

/// Состояние списка по адресу: с `view=<id>` — открытый вид (сохранённый запрос, пока в
/// адресе нет собственных параметров), иначе обычный разбор.
pub async fn resolve(
    db: &PgPool,
    user_id: i64,
    spec: &ListSpec,
    raw_query: &str,
) -> AppResult<Resolved> {
    let pairs = pairs_of(raw_query);
    let view_id = pairs
        .iter()
        .rev()
        .find(|(k, _)| k == "view")
        .and_then(|(_, v)| v.parse::<i64>().ok());
    let Some(id) = view_id else {
        return Ok(Resolved {
            parsed: state::parse(spec, raw_query),
            view: None,
            notices: Vec::new(),
        });
    };
    let view = get_visible(db, user_id, id)
        .await?
        .filter(|v| v.list_key == spec.key)
        .ok_or(AppError::NotFound)?;

    let rest: Vec<(String, String)> = pairs
        .into_iter()
        .filter(|(k, _)| k != "view" && k != "edited")
        .collect();
    let edited = pairs_of(raw_query).iter().any(|(k, _)| k == "edited");
    let own = rest.iter().any(|(k, _)| k != "page" && k != "per");
    let opened = !(edited || own);
    let composed = if opened {
        let url_size: Vec<_> = rest
            .iter()
            .filter(|(k, _)| k == "page" || k == "per")
            .collect();
        let mut stored = pairs_of(&view.query);
        stored.retain(|(k, _)| k != "page" && !url_size.iter().any(|(u, _)| u == k));
        stored.extend(url_size.into_iter().cloned());
        ser(&stored)
    } else {
        ser(&rest)
    };
    let mut parsed = state::parse(spec, &composed);
    parsed.state.view = Some(id);
    let notices = if opened {
        parsed
            .dropped
            .iter()
            .map(|code| format!("Фильтр по полю “{code}” больше недоступен"))
            .collect()
    } else {
        Vec::new()
    };
    let norm = to_query(spec, &parsed.state, true);
    let as_set = |q: &str| -> BTreeSet<(String, String)> { pairs_of(q).into_iter().collect() };
    let url_set: BTreeSet<_> = rest.into_iter().collect();
    parsed.changed = url_set != as_set(&norm) || is_blank(&norm) != edited;
    Ok(Resolved {
        parsed,
        view: Some(view),
        notices,
    })
}

// ---------- строка видов ----------

#[derive(Serialize)]
struct ViewItem {
    id: i64,
    name: String,
    shared: bool,
    pinned: bool,
    current: bool,
    may_edit: bool,
    can_toggle_shared: bool,
    url: String,
}

#[derive(Serialize)]
struct OpenView {
    id: i64,
    name: String,
    shared: bool,
    may_edit: bool,
}

#[derive(Serialize)]
pub struct ViewsBar {
    list_key: String,
    path: String,
    all_url: String,
    all_current: bool,
    items: Vec<ViewItem>,
    open: Option<OpenView>,
    dirty: bool,
    reset_url: String,
    can_share: bool,
    /// Текущее состояние без страницы и вида — то, что сохраняется в вид.
    query: String,
    /// Запрос текущего списка целиком: после действия список перерисовывается по нему.
    back: String,
}

pub async fn bar(
    db: &PgPool,
    user_id: i64,
    spec: &ListSpec,
    path: &str,
    st: &ListState,
    current_url: &str,
) -> sqlx::Result<ViewsBar> {
    let all = visible(db, user_id, &spec.key).await?;
    let open = st.view.and_then(|id| all.iter().find(|v| v.id == id));
    let query = to_query(spec, st, false);
    let dirty = match open {
        Some(v) => query != normalize(spec, &v.query),
        None => !query.is_empty(),
    };
    let items = all
        .iter()
        .map(|v| ViewItem {
            id: v.id,
            name: v.name.clone(),
            shared: v.shared,
            pinned: v.pinned,
            current: Some(v.id) == open.map(|o| o.id),
            may_edit: may_edit(v, user_id, spec.can_share),
            can_toggle_shared: spec.can_share && may_edit(v, user_id, spec.can_share),
            url: format!("{path}?view={}", v.id),
        })
        .collect();
    Ok(ViewsBar {
        list_key: spec.key.clone(),
        path: path.to_string(),
        all_url: path.to_string(),
        all_current: open.is_none(),
        items,
        reset_url: match open {
            Some(v) => format!("{path}?view={}", v.id),
            None => path.to_string(),
        },
        open: open.map(|v| OpenView {
            id: v.id,
            name: v.name.clone(),
            shared: v.shared,
            may_edit: may_edit(v, user_id, spec.can_share),
        }),
        dirty,
        can_share: spec.can_share,
        query,
        back: current_url
            .split_once('?')
            .map_or(String::new(), |(_, q)| q.to_string()),
    })
}

// ---------- обработчики ----------

fn field<'a>(form: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    form.get(key).map(String::as_str)
}

fn check_path(form: &HashMap<String, String>, list_key: &str) -> AppResult<String> {
    let expected = list_path(list_key).ok_or(AppError::NotFound)?;
    match field(form, "path") {
        Some(p) if p != expected => Err(AppError::BadRequest("Неверный адрес списка".into())),
        _ => Ok(expected),
    }
}

/// Перерисовывает список по запросу `query`: вид, которого уже нет, из него убирается.
async fn rerender(
    state: &AppState,
    user: &Access,
    list_key: &str,
    path: &str,
    query: &str,
) -> AppResult<Response> {
    let mut pairs = pairs_of(query);
    let viewed = pairs
        .iter()
        .rev()
        .find(|(k, _)| k == "view")
        .and_then(|(_, v)| v.parse::<i64>().ok());
    if let Some(id) = viewed {
        let ok = get_visible(&state.db, user.user.id, id)
            .await?
            .is_some_and(|v| v.list_key == list_key);
        if !ok {
            pairs.retain(|(k, _)| k != "view" && k != "edited");
        }
    }
    let query = ser(&pairs);
    let mut headers = HeaderMap::new();
    headers.insert("hx-request", HeaderValue::from_static("true"));
    headers.insert("hx-target", HeaderValue::from_static("list-body"));
    let mut resp = if list_key == "orders" {
        let uri: Uri = path.parse().map_err(|_| AppError::NotFound)?;
        crate::admin::shop::orders::list(
            State(state.clone()),
            Extension(user.clone()),
            RawQuery(Some(query.clone())),
            headers,
            OriginalUri(uri),
        )
        .await?
    } else {
        let id: i64 = list_key
            .strip_prefix("collection:")
            .and_then(|s| s.parse().ok())
            .ok_or(AppError::NotFound)?;
        crate::admin::items::list(
            State(state.clone()),
            Extension(user.clone()),
            Path(id),
            RawQuery(Some(query.clone())),
            headers,
        )
        .await?
    };
    if !resp.headers().contains_key("hx-replace-url") {
        let url = if query.is_empty() {
            path.to_string()
        } else {
            format!("{path}?{query}")
        };
        if let Ok(v) = HeaderValue::from_str(&url) {
            resp.headers_mut().insert("hx-replace-url", v);
        }
    }
    Ok(resp)
}

fn with_toast(mut resp: Response, status: StatusCode, text: &str, kind: FlashKind) -> Response {
    let [(name, value)] = toast(text, kind);
    resp.headers_mut().insert(name, value);
    *resp.status_mut() = status;
    resp
}

fn clean_name(raw: Option<&str>) -> Result<String, &'static str> {
    let name = raw.unwrap_or("").trim();
    if name.is_empty() {
        Err("Введите название вида")
    } else if name.chars().count() > NAME_MAX {
        Err("Название вида слишком длинное")
    } else {
        Ok(name.to_string())
    }
}

fn with_view_id(query: &str, id: i64) -> String {
    let mut pairs = pairs_of(query);
    pairs.retain(|(k, _)| k != "view" && k != "page" && k != "edited");
    pairs.push(("view".into(), id.to_string()));
    ser(&pairs)
}

/// `POST /admin/views` — «Сохранить как…».
pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let key = field(&form, "list_key").unwrap_or("");
    let spec = spec_for_key(&state.db, &user, key).await?;
    let path = check_path(&form, key)?;
    let shared = field(&form, "shared") == Some("1");
    if shared && !spec.can_share {
        return Err(AppError::Forbidden);
    }
    let back = field(&form, "back").unwrap_or("");
    let query = normalize(&spec, field(&form, "query").unwrap_or(""));
    let name = match clean_name(field(&form, "name")) {
        Ok(n) => n,
        Err(msg) => {
            let resp = rerender(&state, &user, key, &path, back).await?;
            return Ok(with_toast(
                resp,
                StatusCode::UNPROCESSABLE_ENTITY,
                msg,
                FlashKind::Error,
            ));
        }
    };
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO admin_views (list_key, owner_id, name, query, shared)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(key)
    .bind(user.user.id)
    .bind(&name)
    .bind(&query)
    .bind(shared)
    .fetch_one(&state.db)
    .await?;
    let resp = rerender(&state, &user, key, &path, &with_view_id(&query, id)).await?;
    Ok(with_toast(
        resp,
        StatusCode::OK,
        &format!("Вид «{name}» сохранён"),
        FlashKind::Ok,
    ))
}

/// Видимый вид с проверкой права правки; возвращает вид, спеку и путь списка.
async fn editable(
    state: &AppState,
    user: &Access,
    id: i64,
    form: &HashMap<String, String>,
) -> AppResult<(View, ListSpec, String)> {
    let view = get_visible(&state.db, user.user.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let spec = spec_for_key(&state.db, user, &view.list_key).await?;
    let path = check_path(form, &view.list_key)?;
    if !may_edit(&view, user.user.id, spec.can_share) {
        return Err(AppError::Forbidden);
    }
    Ok((view, spec, path))
}

/// `POST /admin/views/{id}` — переименование, смена запроса и признака «общий».
pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let (view, spec, path) = editable(&state, &user, id, &form).await?;
    let back = match (field(&form, "back"), field(&form, "query")) {
        (Some(b), _) => b.to_string(),
        (None, Some(q)) => with_view_id(&normalize(&spec, q), id),
        (None, None) => format!("view={id}"),
    };
    let mut name = view.name.clone();
    if form.contains_key("name") {
        match clean_name(field(&form, "name")) {
            Ok(n) => name = n,
            Err(msg) => {
                let resp = rerender(&state, &user, &view.list_key, &path, &back).await?;
                return Ok(with_toast(
                    resp,
                    StatusCode::UNPROCESSABLE_ENTITY,
                    msg,
                    FlashKind::Error,
                ));
            }
        }
    }
    let query = match field(&form, "query") {
        Some(q) => normalize(&spec, q),
        None => view.query.clone(),
    };
    let shared = match field(&form, "shared") {
        Some(s) => s == "1",
        None => view.shared,
    };
    if shared != view.shared && !spec.can_share {
        return Err(AppError::Forbidden);
    }
    sqlx::query("UPDATE admin_views SET name = $2, query = $3, shared = $4 WHERE id = $1")
        .bind(id)
        .bind(&name)
        .bind(&query)
        .bind(shared)
        .execute(&state.db)
        .await?;
    let resp = rerender(&state, &user, &view.list_key, &path, &back).await?;
    Ok(with_toast(
        resp,
        StatusCode::OK,
        "Вид обновлён",
        FlashKind::Ok,
    ))
}

/// `POST /admin/views/{id}/delete`.
pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let (view, _, path) = editable(&state, &user, id, &form).await?;
    sqlx::query("DELETE FROM admin_views WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    let back = field(&form, "back").unwrap_or("");
    let resp = rerender(&state, &user, &view.list_key, &path, back).await?;
    Ok(with_toast(
        resp,
        StatusCode::OK,
        "Вид удалён",
        FlashKind::Ok,
    ))
}

/// `POST /admin/views/{id}/pin` — переключатель закрепления для любого видимого вида.
pub async fn pin(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let view = get_visible(&state.db, user.user.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    spec_for_key(&state.db, &user, &view.list_key).await?;
    let path = check_path(&form, &view.list_key)?;
    let removed = sqlx::query("DELETE FROM admin_view_pins WHERE user_id = $1 AND view_id = $2")
        .bind(user.user.id)
        .bind(id)
        .execute(&state.db)
        .await?
        .rows_affected()
        > 0;
    if !removed {
        sqlx::query("INSERT INTO admin_view_pins (user_id, view_id) VALUES ($1, $2)")
            .bind(user.user.id)
            .bind(id)
            .execute(&state.db)
            .await?;
    }
    let back = field(&form, "back").unwrap_or("");
    let resp = rerender(&state, &user, &view.list_key, &path, back).await?;
    let text = if removed {
        "Вид откреплён"
    } else {
        "Вид закреплён в меню"
    };
    Ok(with_toast(resp, StatusCode::OK, text, FlashKind::Ok))
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{HeaderMap, Request, StatusCode, header},
    };
    use sqlx::PgPool;
    use tower::ServiceExt;

    use super::*;
    use crate::test_support::{
        admin_cookie, content_fixture, permitted_cookie, test_state, user_cookie,
    };

    fn app(db: &PgPool) -> Router {
        crate::app(test_state(db.clone()))
    }

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let res = app.clone().oneshot(req).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    /// HTMX-запрос во фрагмент `#list-body`.
    async fn hx_get(app: &Router, path: &str, cookie: &str) -> (StatusCode, HeaderMap, String) {
        let req = Request::get(path)
            .header(header::COOKIE, cookie)
            .header("hx-request", "true")
            .header("hx-target", "list-body")
            .body(Body::empty())
            .unwrap();
        send(app, req).await
    }

    async fn hx_post(
        app: &Router,
        path: &str,
        cookie: &str,
        body: &str,
    ) -> (StatusCode, HeaderMap, String) {
        let req = Request::post(path)
            .header(header::COOKIE, cookie)
            .header(header::HOST, "127.0.0.1:3000")
            .header(header::ORIGIN, "http://127.0.0.1:3000")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("hx-request", "true")
            .header("hx-target", "list-body")
            .body(Body::from(body.to_string()))
            .unwrap();
        send(app, req).await
    }

    fn enc(s: &str) -> String {
        form_urlencoded::byte_serialize(s.as_bytes()).collect()
    }

    fn list_path(cid: i64) -> String {
        format!("/admin/collections/{cid}/items")
    }

    fn create_body(cid: i64, name: &str, query: &str, shared: bool) -> String {
        format!(
            "list_key={}&path={}&name={}&query={}{}",
            enc(&format!("collection:{cid}")),
            enc(&list_path(cid)),
            enc(name),
            enc(query),
            if shared { "&shared=1" } else { "" }
        )
    }

    async fn insert_view(db: &PgPool, key: &str, owner: i64, query: &str, shared: bool) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO admin_views (list_key, owner_id, name, query, shared)
             VALUES ($1, $2, 'Вид', $3, $4) RETURNING id",
        )
        .bind(key)
        .bind(owner)
        .bind(query)
        .bind(shared)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn read_only_user(db: &PgPool, cid: i64, login: &str) -> (i64, String) {
        let cookie = permitted_cookie(db, login, &[]).await;
        let gid: i64 = sqlx::query_scalar("SELECT id FROM groups WHERE code = $1")
            .bind(format!("g_{login}"))
            .fetch_one(db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO collection_access (collection_id, group_id, level) VALUES ($1, $2, 'read')",
        )
        .bind(cid)
        .bind(gid)
        .execute(db)
        .await
        .unwrap();
        let uid: i64 = sqlx::query_scalar("SELECT id FROM users WHERE login = $1")
            .bind(login)
            .fetch_one(db)
            .await
            .unwrap();
        (uid, cookie)
    }

    #[sqlx::test]
    async fn view_crud(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (st, headers, body) = hx_post(
            &app,
            "/admin/views",
            &cookie,
            &create_body(
                c.collection_id,
                "Первые",
                "f.name=has%3A%D0%9F%D0%B5%D1%80",
                false,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(headers.contains_key("hx-trigger"));
        assert!(body.contains("id=\"list-body\""), "{body}");
        let id: i64 = sqlx::query_scalar("SELECT id FROM admin_views")
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(
            headers["hx-replace-url"]
                .to_str()
                .unwrap()
                .ends_with(&format!("view={id}"))
        );

        let (st, _, _) = hx_post(
            &app,
            &format!("/admin/views/{id}"),
            &cookie,
            &format!(
                "path={}&name={}",
                enc(&list_path(c.collection_id)),
                enc("Новое имя")
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let name: String = sqlx::query_scalar("SELECT name FROM admin_views WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(name, "Новое имя");

        let (st, _, _) = hx_post(
            &app,
            &format!("/admin/views/{id}"),
            &cookie,
            &format!(
                "path={}&query={}&back={}",
                enc(&list_path(c.collection_id)),
                enc("f.name=eq%3Aabc&page=3&view=99"),
                enc(&format!("view={id}"))
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let q: String = sqlx::query_scalar("SELECT query FROM admin_views WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(q, "f.name=eq%3Aabc");

        let (st, _, _) = hx_post(
            &app,
            &format!("/admin/views/{id}/delete"),
            &cookie,
            &format!(
                "path={}&back={}",
                enc(&list_path(c.collection_id)),
                enc(&format!("view={id}"))
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_views")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[sqlx::test]
    async fn empty_name_is_422(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (st, headers, _) = hx_post(
            &app,
            "/admin/views",
            &cookie,
            &create_body(c.collection_id, "  ", "", false),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let trig = headers["hx-trigger"].to_str().unwrap();
        assert!(
            trig.contains("\\u0412\\u0432\\u0435\\u0434\\u0438\\u0442\\u0435"),
            "{trig}"
        );
    }

    #[sqlx::test]
    async fn bad_path_rejected(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let body = format!(
            "list_key={}&path={}&name=x&query=",
            enc(&format!("collection:{}", c.collection_id)),
            enc("/admin/users")
        );
        let (st, _, _) = hx_post(&app, "/admin/views", &cookie, &body).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn shared_requires_right(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let (_, cookie) = read_only_user(&db, c.collection_id, "ro").await;
        let (st, _, _) = hx_post(
            &app,
            "/admin/views",
            &cookie,
            &create_body(c.collection_id, "Общий", "", true),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, _, _) = hx_post(
            &app,
            "/admin/views",
            &cookie,
            &create_body(c.collection_id, "Личный", "", false),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
    }

    #[sqlx::test]
    async fn readonly_cannot_save_shared(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let (admin_id, _) = user_cookie(&db, "boss", true).await;
        let key = format!("collection:{}", c.collection_id);
        let id = insert_view(&db, &key, admin_id, "f.name=has%3Ax", true).await;
        let (_, cookie) = read_only_user(&db, c.collection_id, "ro").await;

        let (st, _, _) = hx_post(
            &app,
            &format!("/admin/views/{id}"),
            &cookie,
            &format!(
                "path={}&query={}",
                enc(&list_path(c.collection_id)),
                enc("f.name=has%3Ay")
            ),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, _, _) = hx_post(&app, &format!("/admin/views/{id}/delete"), &cookie, "").await;
        assert_eq!(st, StatusCode::FORBIDDEN);

        let url = format!("{}?f.name=has%3Ay&view={id}", list_path(c.collection_id));
        let (st, _, body) = hx_get(&app, &url, &cookie).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.contains("Вид изменён"), "{body}");
        assert!(body.contains("Сохранить как…"), "{body}");
        assert!(!body.contains("data-view-save "), "{body}");
        assert!(!body.contains(">Сохранить</button>"), "{body}");
    }

    #[sqlx::test]
    async fn foreign_private_404(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let (admin_id, _) = user_cookie(&db, "boss", true).await;
        let key = format!("collection:{}", c.collection_id);
        let id = insert_view(&db, &key, admin_id, "", false).await;
        let (_, cookie) = read_only_user(&db, c.collection_id, "ro").await;
        let (st, _, _) = hx_get(
            &app,
            &format!("{}?view={id}", list_path(c.collection_id)),
            &cookie,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        for p in [
            format!("/admin/views/{id}"),
            format!("/admin/views/{id}/delete"),
            format!("/admin/views/{id}/pin"),
        ] {
            let (st, _, _) = hx_post(&app, &p, &cookie, "name=x").await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{p}");
        }
    }

    #[sqlx::test]
    async fn view_dropped_field_notice(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (uid, _) = user_cookie(&db, "other", true).await;
        let key = format!("collection:{}", c.collection_id);
        let id = insert_view(&db, &key, uid, "f.gone=has%3Ax", true).await;
        let (st, headers, body) = hx_get(
            &app,
            &format!("{}?view={id}", list_path(c.collection_id)),
            &cookie,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(
            body.contains("Фильтр по полю “gone” больше недоступен"),
            "{body}"
        );
        assert!(
            headers["hx-replace-url"]
                .to_str()
                .unwrap()
                .ends_with(&format!("view={id}"))
        );
    }

    #[sqlx::test]
    async fn opening_view_redirects_to_canonical(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (uid, _) = user_cookie(&db, "other", true).await;
        let key = format!("collection:{}", c.collection_id);
        let id = insert_view(&db, &key, uid, "f.name=has%3Ax", true).await;
        let req = Request::get(format!("{}?view={id}", list_path(c.collection_id)))
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let (st, headers, _) = send(&app, req).await;
        assert_eq!(st, StatusCode::FOUND);
        let loc = headers[header::LOCATION].to_str().unwrap();
        assert!(
            loc.contains("f.name=has%3Ax") && loc.ends_with(&format!("view={id}")),
            "{loc}"
        );
    }

    #[sqlx::test]
    async fn dirty_only_when_state_differs(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (uid, _) = user_cookie(&db, "other", true).await;
        let key = format!("collection:{}", c.collection_id);
        let id = insert_view(&db, &key, uid, "f.name=has%3Ax&stale=1", true).await;
        let base = list_path(c.collection_id);
        let (_, _, clean) =
            hx_get(&app, &format!("{base}?f.name=has%3Ax&view={id}"), &cookie).await;
        assert!(!clean.contains("Вид изменён"), "{clean}");
        assert!(clean.contains("data-view-menu"), "{clean}");
        let (_, _, dirty) =
            hx_get(&app, &format!("{base}?f.name=has%3Ay&view={id}"), &cookie).await;
        assert!(dirty.contains("Вид изменён"), "{dirty}");
        assert!(dirty.contains("data-view-save-as"), "{dirty}");
        assert!(dirty.contains("data-view-save "), "{dirty}");
        // Сбросить — только ?view=<id>
        let href = format!("href=\"{base}?view={id}\"").replace('/', "&#x2f;");
        assert!(dirty.contains(&href), "{dirty}");
    }

    #[sqlx::test]
    async fn pin_toggle(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (uid, _) = user_cookie(&db, "other", true).await;
        let key = format!("collection:{}", c.collection_id);
        let id = insert_view(&db, &key, uid, "", true).await;
        let body = format!("path={}", enc(&list_path(c.collection_id)));
        for expected in [1i64, 0] {
            let (st, _, _) = hx_post(&app, &format!("/admin/views/{id}/pin"), &cookie, &body).await;
            assert_eq!(st, StatusCode::OK);
            let n: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_view_pins")
                .fetch_one(&db)
                .await
                .unwrap();
            assert_eq!(n, expected);
        }
    }

    #[sqlx::test]
    async fn delete_collection_drops_views(db: PgPool) {
        let c = content_fixture(&db).await;
        let (uid, _) = user_cookie(&db, "boss", true).await;
        let key = format!("collection:{}", c.collection_id);
        insert_view(&db, &key, uid, "", true).await;
        insert_view(&db, "orders", uid, "", true).await;
        crate::collection::repo::delete_collection(&db, c.collection_id)
            .await
            .unwrap();
        let keys: Vec<String> = sqlx::query_scalar("SELECT list_key FROM admin_views")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(keys, ["orders"]);
    }

    #[test]
    fn edit_rules() {
        let v = |shared, owner| View {
            id: 1,
            list_key: "orders".into(),
            owner_id: owner,
            name: "x".into(),
            query: String::new(),
            shared,
            pinned: false,
        };
        assert!(may_edit(&v(false, 5), 5, false));
        assert!(!may_edit(&v(false, 5), 6, true));
        assert!(may_edit(&v(true, 5), 6, true));
        assert!(!may_edit(&v(true, 5), 5, false));
    }
}
