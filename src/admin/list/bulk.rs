//! Массовые действия над записями списка: описание, отбор id движком, выполнение, итог.

use std::collections::{HashMap, HashSet};

use axum::{Extension, Form, body::Body, extract::State, http::StatusCode, response::Response};
use serde::Serialize;

use super::{
    spec::{EnumOption, ListSpec},
    sql::{self, Scope},
    state::ListState,
    views,
};
use crate::{
    access::Access,
    admin::flash::FlashKind,
    error::{AppError, AppResult},
    state::AppState,
};

/// Предел «по фильтру» и числа переданных id.
pub const BULK_LIMIT: i64 = 10_000;
/// Причин в итоге показывается не больше ссылок.
const LINKS_MAX: usize = 20;
/// Причина отмены не длиннее.
const REASON_MAX: usize = 500;

#[derive(Debug, Clone)]
pub enum BulkParam {
    None,
    Choice(Vec<EnumOption>),
    Reason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    Never,
    Always,
    Danger,
}

#[derive(Debug, Clone)]
pub struct BulkAction {
    pub code: &'static str,
    #[allow(dead_code)] // читает панель интерфейса
    pub label: String,
    pub param: BulkParam,
    #[allow(dead_code)] // читает панель интерфейса
    pub confirm: Confirm,
    pub notify: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkScope {
    Ids,
    Filter,
}

#[derive(Debug, Clone)]
pub struct BulkForm {
    pub list_key: String,
    pub path: String,
    pub query: String,
    pub action: String,
    pub param: String,
    pub notify: bool,
    pub scope: BulkScope,
    pub ids: Vec<i64>,
}

impl BulkForm {
    pub fn parse(form: &HashMap<String, String>) -> BulkForm {
        let get = |k: &str| form.get(k).cloned().unwrap_or_default();
        let scope = if form.get("scope").map(String::as_str) == Some("filter") {
            BulkScope::Filter
        } else {
            BulkScope::Ids
        };
        let mut ids: Vec<i64> = Vec::new();
        if scope == BulkScope::Ids {
            let mut seen: HashSet<i64> = HashSet::new();
            for id in get("ids")
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter_map(|s| s.parse::<i64>().ok())
            {
                if seen.insert(id) {
                    ids.push(id);
                }
            }
        }
        BulkForm {
            list_key: get("list_key"),
            path: get("path"),
            query: get("query"),
            action: get("action"),
            param: get("param"),
            notify: form.get("notify").map(String::as_str) == Some("1"),
            scope,
            ids,
        }
    }
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub done: usize,
    pub failed: Vec<(i64, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryGroup {
    pub reason: String,
    /// `(подпись, адрес)`, не больше 20.
    pub links: Vec<(String, String)>,
    pub more: usize,
}

/// Итог для блока над списком.
#[derive(Debug, Clone, Serialize)]
pub struct BulkSummary {
    pub done: usize,
    pub total: usize,
    pub groups: Vec<SummaryGroup>,
}

/// Невыполненные, сгруппированные по причине в порядке первой встречи.
pub fn summarize(
    outcome: &Outcome,
    labels: &HashMap<i64, String>,
    open_url: &str,
) -> Vec<SummaryGroup> {
    let mut groups: Vec<(&str, Vec<i64>)> = Vec::new();
    for (id, reason) in &outcome.failed {
        match groups.iter_mut().find(|(r, _)| *r == reason) {
            Some((_, ids)) => ids.push(*id),
            None => groups.push((reason, vec![*id])),
        }
    }
    groups
        .into_iter()
        .map(|(reason, ids)| SummaryGroup {
            reason: reason.to_string(),
            links: ids
                .iter()
                .take(LINKS_MAX)
                .map(|id| {
                    (
                        labels.get(id).cloned().unwrap_or_else(|| id.to_string()),
                        open_url.replace("{id}", &id.to_string()),
                    )
                })
                .collect(),
            more: ids.len().saturating_sub(LINKS_MAX),
        })
        .collect()
}

/// Письмо уходит, только если оно есть и пользователь не снял «Уведомить».
pub fn mails_to_send<T>(event: Option<T>, notify: bool) -> Option<T> {
    event.filter(|_| notify)
}

pub struct BulkCtx<'a> {
    pub state: &'a AppState,
    pub user: &'a Access,
    pub notify: bool,
    /// Коллекция для списков записей.
    pub collection_id: Option<i64>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SelectError {
    Empty,
    TooMany,
}

impl SelectError {
    pub fn message(&self) -> &'static str {
        match self {
            SelectError::Empty => "Ничего не выбрано",
            SelectError::TooMany => "Слишком много записей: уточните фильтр (до 10 000)",
        }
    }
}

/// Id по условию списка; `only` пересекает с переданными.
pub async fn select_ids(
    db: &sqlx::PgPool,
    spec: &ListSpec,
    st: &ListState,
    scope: &Scope,
    only: Option<&[i64]>,
) -> AppResult<Result<Vec<i64>, SelectError>> {
    let mut qb = sql::ids(spec, st, scope, only, BULK_LIMIT + 1);
    let ids: Vec<i64> = qb.build_query_scalar().fetch_all(db).await?;
    Ok(if ids.is_empty() {
        Err(SelectError::Empty)
    } else if ids.len() as i64 > BULK_LIMIT {
        Err(SelectError::TooMany)
    } else {
        Ok(ids)
    })
}

/// Параметр действия: значение из `Choice`, причина не длиннее 500 символов.
pub fn check_param(action: &BulkAction, raw: &str) -> Result<String, &'static str> {
    match &action.param {
        BulkParam::None => Ok(String::new()),
        BulkParam::Choice(options) => options
            .iter()
            .any(|o| o.value == raw)
            .then(|| raw.to_string())
            .ok_or("Недопустимое значение параметра"),
        BulkParam::Reason => {
            let reason = raw.trim();
            if reason.chars().count() > REASON_MAX {
                Err("Причина не длиннее 500 символов")
            } else {
                Ok(reason.to_string())
            }
        }
    }
}

fn reject(status: StatusCode, text: &str) -> Response {
    views::with_toast(Response::new(Body::empty()), status, text, FlashKind::Error)
}

/// Реестр выполнения по списку.
async fn run(
    ctx: &BulkCtx<'_>,
    list_key: &str,
    action: &str,
    param: &str,
    id: i64,
) -> Result<(), String> {
    #[cfg(test)]
    if let Some(r) = tests::run_test_action(action, id) {
        return r;
    }
    if list_key == "orders" {
        crate::admin::shop::orders::bulk_run(ctx, action, param, id).await
    } else {
        crate::admin::items::bulk_run(ctx, action, param, id).await
    }
}

/// `POST /admin/lists/bulk`.
pub async fn handle(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let f = BulkForm::parse(&form);
    if f.ids.len() as i64 > BULK_LIMIT {
        return Ok(reject(
            StatusCode::UNPROCESSABLE_ENTITY,
            SelectError::TooMany.message(),
        ));
    }
    #[cfg_attr(not(test), allow(unused_mut))]
    let mut spec = views::spec_for_key(&state.db, &user, &f.list_key).await?;
    #[cfg(test)]
    spec.bulk_actions.extend(tests::test_actions());
    let expected = views::list_path(&f.list_key).ok_or(AppError::NotFound)?;
    if f.path != expected {
        return Ok(reject(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Неверный адрес списка",
        ));
    }
    let Some(action) = spec.bulk_actions.iter().find(|a| a.code == f.action) else {
        return Ok(reject(StatusCode::FORBIDDEN, "Действие недоступно"));
    };
    let param = match check_param(action, &f.param) {
        Ok(p) => p,
        Err(msg) => return Ok(reject(StatusCode::UNPROCESSABLE_ENTITY, msg)),
    };
    let notify = f.notify && action.notify;

    let parsed = views::resolve(&state.db, user.user.id, &spec, &f.query, false).await?;
    let st = parsed.parsed.state;
    let collection_id = f
        .list_key
        .strip_prefix("collection:")
        .and_then(|s| s.parse::<i64>().ok());
    let scope = match collection_id {
        Some(cid) => {
            super::collection::section_scope(&state.db, cid, &st.section, st.only)
                .await?
                .0
        }
        None => Scope::All,
    };
    let only = (f.scope == BulkScope::Ids).then_some(f.ids.as_slice());
    let ids = match select_ids(&state.db, &spec, &st, &scope, only).await? {
        Ok(ids) => ids,
        Err(e) => return Ok(reject(StatusCode::UNPROCESSABLE_ENTITY, e.message())),
    };

    let ctx = BulkCtx {
        state: &state,
        user: &user,
        notify,
        collection_id,
    };
    let mut outcome = Outcome::default();
    for &id in &ids {
        match run(&ctx, &f.list_key, &f.action, &param, id).await {
            Ok(()) => outcome.done += 1,
            Err(reason) => outcome.failed.push((id, reason)),
        }
    }

    let summary = if outcome.failed.is_empty() {
        None
    } else {
        let failed_ids: Vec<i64> = outcome.failed.iter().map(|(id, _)| *id).collect();
        let rows: Vec<(i64, String)> = sql::labels(&spec, &failed_ids)
            .build_query_as()
            .fetch_all(&state.db)
            .await?;
        let labels: HashMap<i64, String> = rows.into_iter().collect();
        Some(BulkSummary {
            done: outcome.done,
            total: ids.len(),
            groups: summarize(&outcome, &labels, &spec.open_url),
        })
    };
    let resp = views::rerender_with(
        &state,
        &user,
        &f.list_key,
        &f.path,
        &f.query,
        summary.clone(),
    )
    .await?;
    Ok(match summary {
        Some(_) => resp,
        None => views::with_toast(
            resp,
            StatusCode::OK,
            &format!("Готово: {}", outcome.done),
            FlashKind::Ok,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Тестовые действия: `noop` (чётные id — успех, нечётные — «нечётный»), с выбором, с причиной.
    pub(super) fn test_actions() -> Vec<BulkAction> {
        let action = |code, param| BulkAction {
            code,
            label: code.into(),
            param,
            confirm: Confirm::Never,
            notify: false,
        };
        let opts = ["a", "b"].map(|v| EnumOption {
            value: v.into(),
            label: v.into(),
        });
        vec![
            action("noop", BulkParam::None),
            action("noop_choice", BulkParam::Choice(opts.to_vec())),
            action("noop_reason", BulkParam::Reason),
        ]
    }

    pub(super) fn run_test_action(action: &str, id: i64) -> Option<Result<(), String>> {
        action.starts_with("noop").then(|| {
            if id % 2 == 0 {
                Ok(())
            } else {
                Err("нечётный".to_string())
            }
        })
    }

    #[test]
    fn summarize_groups_and_caps() {
        let outcome = Outcome {
            done: 0,
            failed: (1..=25).map(|i| (i, "отгружен".to_string())).collect(),
        };
        let labels: HashMap<i64, String> = (1..=25).map(|i| (i, format!("№{i}"))).collect();
        let g = summarize(&outcome, &labels, "/o/{id}");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].links.len(), 20);
        assert_eq!(g[0].more, 5);
        assert_eq!(g[0].links[0], ("№1".to_string(), "/o/1".to_string()));

        let outcome = Outcome {
            done: 1,
            failed: vec![(3, "б".into()), (1, "а".into()), (2, "б".into())],
        };
        let g = summarize(&outcome, &HashMap::new(), "/o/{id}");
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].reason, "б");
        assert_eq!(g[0].links.len(), 2);
        assert_eq!(g[0].links[0].0, "3");
        assert_eq!(g[1].reason, "а");
    }

    #[test]
    fn form_parse() {
        let mut m: HashMap<String, String> = HashMap::new();
        m.insert("ids".into(), "3, 1,x,3,,2,-5,99999999999999999999".into());
        m.insert("notify".into(), "1".into());
        m.insert("list_key".into(), "orders".into());
        let f = BulkForm::parse(&m);
        assert_eq!(f.ids, vec![3, 1, 2, -5]);
        assert_eq!(f.scope, BulkScope::Ids);
        assert!(f.notify);
        m.insert("scope".into(), "filter".into());
        m.remove("notify");
        let f = BulkForm::parse(&m);
        assert_eq!(f.scope, BulkScope::Filter);
        assert!(f.ids.is_empty());
        assert!(!f.notify);
    }

    #[test]
    fn form_parse_keeps_first_seen_order_with_many_ids() {
        let ids: Vec<String> = (0..50_000).map(|i| (i % 20_000).to_string()).collect();
        let mut m: HashMap<String, String> = HashMap::new();
        m.insert("ids".into(), ids.join(","));
        let f = BulkForm::parse(&m);
        assert_eq!(f.ids.len(), 20_000);
        assert_eq!(&f.ids[..3], &[0, 1, 2]);
    }

    #[test]
    fn mails_to_send_respects_notify() {
        assert_eq!(mails_to_send(Some(1), true), Some(1));
        assert_eq!(mails_to_send(Some(1), false), None);
        assert_eq!(mails_to_send::<i32>(None, true), None);
    }

    // ---------- с базой ----------

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{HeaderMap, Request, header},
    };
    use sqlx::PgPool;
    use tower::ServiceExt;

    use crate::admin::list::{spec::test_spec, state::parse};
    use crate::test_support::{
        admin_cookie, content_fixture, order_fixture, permitted_cookie, place_order, test_state,
    };

    fn app(db: &PgPool) -> Router {
        crate::app(test_state(db.clone()))
    }

    fn enc(s: &str) -> String {
        form_urlencoded::byte_serialize(s.as_bytes()).collect()
    }

    async fn post(app: &Router, cookie: &str, body: &str) -> (StatusCode, HeaderMap, String) {
        let req = Request::post("/admin/lists/bulk")
            .header(header::COOKIE, cookie)
            .header(header::HOST, "127.0.0.1:3000")
            .header(header::ORIGIN, "http://127.0.0.1:3000")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("hx-request", "true")
            .header("hx-target", "list-body")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    fn orders_form(action: &str, scope: &str, ids: &[i64], query: &str) -> String {
        let ids: Vec<String> = ids.iter().map(i64::to_string).collect();
        format!(
            "list_key=orders&path={}&query={}&action={action}&scope={scope}&ids={}",
            enc("/admin/shop/orders"),
            enc(query),
            ids.join(",")
        )
    }

    /// Заказы: `n` штук, у вторых по счёту статус `F`. Возвращает id (по возрастанию).
    async fn orders(db: &PgPool, n: usize) -> Vec<i64> {
        let f = order_fixture(db).await;
        sqlx::query("INSERT INTO order_statuses (code, name) VALUES ('F', 'Выполнен')")
            .execute(db)
            .await
            .unwrap();
        let mut ids = Vec::new();
        for _ in 0..n {
            ids.push(place_order(db, &f, None).await);
        }
        ids.sort_unstable();
        ids
    }

    /// Заказы, пока среди id не наберётся по `min` нечётных и чётных (результат `noop` зависит от чётности).
    async fn orders_mixed(db: &PgPool, min: usize) -> Vec<i64> {
        let f = order_fixture(db).await;
        sqlx::query("INSERT INTO order_statuses (code, name) VALUES ('F', 'Выполнен')")
            .execute(db)
            .await
            .unwrap();
        let mut ids = Vec::new();
        while odd(&ids).len() < min || ids.len() - odd(&ids).len() < min {
            ids.push(place_order(db, &f, None).await);
        }
        ids.sort_unstable();
        ids
    }

    fn odd(ids: &[i64]) -> Vec<i64> {
        ids.iter().copied().filter(|i| i % 2 != 0).collect()
    }

    #[sqlx::test]
    async fn ids_intersect_with_filter(db: PgPool) {
        let all = orders_mixed(&db, 2).await;
        let odds = odd(&all);
        let evens: Vec<i64> = all.iter().copied().filter(|i| i % 2 == 0).collect();
        // видимы (статус N): два нечётных и один чётный; остальные скрыты фильтром
        let visible = [odds[0], odds[1], evens[0]];
        let hidden: Vec<i64> = all
            .iter()
            .copied()
            .filter(|i| !visible.contains(i))
            .collect();
        sqlx::query("UPDATE orders SET status = 'F' WHERE id = ANY($1)")
            .bind(&hidden)
            .execute(&db)
            .await
            .unwrap();
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        // скрытые фильтром и несуществующий id отбрасываются: обработаны 3 заказа, 2 нечётных не выполнены
        let mut sent = all.clone();
        sent.push(999_999);
        let (st, _, body) = post(
            &app,
            &cookie,
            &orders_form("noop", "ids", &sent, "f.status=in%3AN"),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(body.contains("Выполнено 1 из 3"), "{body}");
        for id in &hidden {
            assert!(!body.contains(&format!("data-id=\"{id}\"")), "{body}");
        }
    }

    #[sqlx::test]
    async fn foreign_and_section_scope(db: PgPool) {
        let c = content_fixture(&db).await;
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name) VALUES ('o', 'O') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let mk = |coll: i64, section: Option<i64>, code: String| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "INSERT INTO collection_items (collection_id, section_id, code, name)
                     VALUES ($1, $2, $3, $3) RETURNING id",
                )
                .bind(coll)
                .bind(section)
                .bind(code)
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };
        // сдвигаем счётчик id так, чтобы запись раздела получила нечётный id
        for n in 0.. {
            if mk(other, None, format!("filler{n}")).await % 2 == 0 {
                break;
            }
        }
        let in_sec = mk(c.collection_id, Some(c.section_id), "in-sec".into()).await;
        assert!(in_sec % 2 != 0);
        let loose = mk(c.collection_id, None, "loose".into()).await;
        let foreign = mk(other, None, "foreign".into()).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let key = format!("collection:{}", c.collection_id);
        let path = format!("/admin/collections/{}/items", c.collection_id);
        let form = |scope: &str, ids: &[i64], query: &str| {
            let ids: Vec<String> = ids.iter().map(i64::to_string).collect();
            format!(
                "list_key={}&path={}&query={}&action=noop&scope={scope}&ids={}",
                enc(&key),
                enc(&path),
                enc(query),
                ids.join(",")
            )
        };
        // чужая коллекция молча отбрасывается: обработаны 3 записи из этой коллекции
        let all = [c.item_id, in_sec, loose, foreign];
        let (st, _, body) = post(&app, &cookie, &form("ids", &all, "")).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let bad = [c.item_id, in_sec, loose]
            .iter()
            .filter(|i| *i % 2 != 0)
            .count();
        assert!(
            body.contains(&format!("Выполнено {} из 3", 3 - bad)),
            "{body}"
        );
        // «все по фильтру» в разделе берёт записи раздела: 2 (первая новость и in-sec)
        let (st, _, body) = post(
            &app,
            &cookie,
            &form("filter", &[], &format!("section={}", c.section_id)),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let bad = [c.item_id, in_sec].iter().filter(|i| *i % 2 != 0).count();
        assert!(bad > 0);
        assert!(
            body.contains(&format!("Выполнено {} из 2", 2 - bad)),
            "{body}"
        );
    }

    fn series_spec(n: i64) -> ListSpec {
        let mut spec = test_spec();
        spec.from = format!("FROM generate_series(1, {n}) AS g(id) WHERE TRUE");
        spec.id_expr = "g.id".into();
        spec.joins.clear();
        for f in &mut spec.filters {
            f.join = None;
        }
        spec
    }

    #[sqlx::test]
    async fn filter_scope_limit(db: PgPool) {
        let st = parse(&test_spec(), "").state;
        let spec = series_spec(10_001);
        let r = select_ids(&db, &spec, &st, &Scope::All, None)
            .await
            .unwrap();
        assert_eq!(r, Err(SelectError::TooMany));
        assert_eq!(
            SelectError::TooMany.message(),
            "Слишком много записей: уточните фильтр (до 10 000)"
        );
        let spec = series_spec(10_000);
        let ids = select_ids(&db, &spec, &st, &Scope::All, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ids.len(), 10_000);
        // пересечение с переданными id
        let only = [5, 7, 20_000];
        let ids = select_ids(&db, &spec, &st, &Scope::All, Some(&only))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ids, vec![5, 7]);
        let r = select_ids(&db, &spec, &st, &Scope::All, Some(&[]))
            .await
            .unwrap();
        assert_eq!(r, Err(SelectError::Empty));
    }

    #[sqlx::test]
    async fn too_many_ids_422(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let many: Vec<i64> = (1..=BULK_LIMIT + 1).collect();
        let (st, h, _) = post(&app, &cookie, &orders_form("noop", "ids", &many, "")).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(h["hx-trigger"].to_str().unwrap().contains("toast"));
    }

    #[sqlx::test]
    async fn empty_selection_422(db: PgPool) {
        orders(&db, 2).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (st, h, _) = post(&app, &cookie, &orders_form("noop", "ids", &[], "")).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(h["hx-trigger"].to_str().unwrap().contains("toast"));
    }

    #[sqlx::test]
    async fn edited_view_uses_query(db: PgPool) {
        let ids = orders_mixed(&db, 1).await;
        // в статусе N остаётся один заказ с нечётным id, остальные — F
        let kept = odd(&ids)[0];
        sqlx::query("UPDATE orders SET status = 'F' WHERE id <> $1")
            .bind(kept)
            .execute(&db)
            .await
            .unwrap();
        let uid: i64 = sqlx::query_scalar("SELECT id FROM users WHERE login = 'admin'")
            .fetch_optional(&db)
            .await
            .unwrap()
            .unwrap_or(0);
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let uid: i64 = if uid == 0 {
            sqlx::query_scalar("SELECT id FROM users ORDER BY id LIMIT 1")
                .fetch_one(&db)
                .await
                .unwrap()
        } else {
            uid
        };
        // сохранённый вид: статус F (3 заказа); в запросе — другой фильтр (N: 1 заказ)
        let view: i64 = sqlx::query_scalar(
            "INSERT INTO admin_views (list_key, owner_id, name, query, shared)
             VALUES ('orders', $1, 'В', 'f.status=in%3AF', true) RETURNING id",
        )
        .bind(uid)
        .fetch_one(&db)
        .await
        .unwrap();
        let q = format!("view={view}&edited=1&f.status=in%3AN");
        let (st, _, body) = post(&app, &cookie, &orders_form("noop", "filter", &[], &q)).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(body.contains("Выполнено 0 из 1"), "{body}");
    }

    #[sqlx::test]
    async fn rejects(db: PgPool) {
        let ids = orders(&db, 2).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (st, _, _) = post(&app, &cookie, &orders_form("nope", "ids", &ids, "")).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let bad_path = orders_form("noop", "ids", &ids, "").replace("shop%2Forders", "users");
        let (st, _, _) = post(&app, &cookie, &bad_path).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let (st, _, _) = post(&app, &cookie, &orders_form("noop_choice", "ids", &ids, "")).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let long = format!(
            "{}&param={}",
            orders_form("noop_reason", "ids", &ids, ""),
            "я".repeat(501)
        );
        let (st, _, _) = post(&app, &cookie, &long).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let ok = format!("{}&param=a", orders_form("noop_choice", "ids", &ids, ""));
        let (st, _, _) = post(&app, &cookie, &ok).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[sqlx::test]
    async fn no_list_access_403(db: PgPool) {
        let ids = orders(&db, 2).await;
        let app = app(&db);
        let cookie = permitted_cookie(&db, "nobody", &[]).await;
        let (st, _, _) = post(&app, &cookie, &orders_form("noop", "ids", &ids, "")).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn summary_block_rendered(db: PgPool) {
        let ids = orders_mixed(&db, 2).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (st, h, body) = post(&app, &cookie, &orders_form("noop", "ids", &ids, "")).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(body.contains("id=\"list-body\""), "{body}");
        let failed = odd(&ids);
        assert!(!failed.is_empty());
        assert!(
            body.contains(&format!(
                "Выполнено {} из {}",
                ids.len() - failed.len(),
                ids.len()
            )),
            "{body}"
        );
        assert!(body.contains("нечётный"), "{body}");
        for id in &failed {
            assert!(
                body.contains(&format!(
                    "<a href=\"&#x2f;admin&#x2f;shop&#x2f;orders&#x2f;{id}\">"
                )),
                "{body}"
            );
        }
        assert!(
            !h.contains_key("hx-trigger") || !h["hx-trigger"].to_str().unwrap().contains("toast")
        );
        // всё выполнено — только уведомление
        let even: Vec<i64> = ids.iter().copied().filter(|i| i % 2 == 0).collect();
        assert!(!even.is_empty());
        let (st, h, body) = post(&app, &cookie, &orders_form("noop", "ids", &even, "")).await;
        assert_eq!(st, StatusCode::OK);
        assert!(!body.contains("bulk-summary"), "{body}");
        assert!(h.contains_key("hx-trigger"));
    }
}
