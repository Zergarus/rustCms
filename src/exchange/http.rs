//! HTTP-протокол обмена с 1С: `checkauth`, `init`, `file`, `import`, `deactivate`, `complete`.
//!
//! Распаковка zip выполняется лениво — при первом `import` после загрузки архива.

use std::collections::HashMap;
use std::time::Instant;

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use axum_extra::extract::CookieJar;
use serde_json::Value;

use super::apply::{self, Ctx, Stats};
use super::cml;
use super::session::{self, Limits, Session};
use super::settings::{Action, Settings};
use crate::{access, state::AppState};

/// Запас на тело запроса `file`; настоящий предел — `file_limit` из настроек.
const BODY_LIMIT: usize = 1 << 30;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/bitrix/admin/1c_exchange.php", get(handle).post(handle))
        .route("/api/1c/exchange", get(handle).post(handle))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
}

/// Успешный ответ: полный текст тела и счётчики для журнала.
#[derive(Debug)]
pub(crate) struct Outcome {
    body: String,
    stats: Value,
}

impl Outcome {
    fn text(body: impl Into<String>) -> Outcome {
        Outcome {
            body: body.into(),
            stats: Value::Object(Default::default()),
        }
    }
}

pub(crate) type Reply = Result<Outcome, String>;

fn db_err(e: impl std::fmt::Display) -> String {
    format!("Ошибка базы данных: {e}")
}

const DENIED: &str = "Доступ запрещён";

async fn handle(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let mode = query.get("mode").map(String::as_str).unwrap_or("");
    let filename = query.get("filename").map(String::as_str).unwrap_or("");
    let mut set_cookie = None;
    let mut session = None;
    let reply = dispatch(
        &state,
        mode,
        &query,
        &headers,
        &body,
        &mut session,
        &mut set_cookie,
    )
    .await;
    if matches!(mode, "import" | "deactivate" | "complete") {
        let (result, stats, error) = match &reply {
            Ok(o) => ("success", o.stats.clone(), String::new()),
            Err(e) => ("failure", Value::Object(Default::default()), e.clone()),
        };
        let session: &Option<Session> = &session;
        let user_id = session.as_ref().and_then(|s| s.user_id);
        let logged = sqlx::query(
            "INSERT INTO exchange_log (user_id, mode, filename, result, stats, error, duration_ms)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(user_id)
        .bind(mode)
        .bind(filename)
        .bind(result)
        .bind(stats)
        .bind(error)
        .bind(started.elapsed().as_millis().min(i32::MAX as u128) as i32)
        .execute(&state.db)
        .await;
        if let Err(e) = logged {
            tracing::warn!("обмен с 1С: не записан журнал: {e}");
        }
    }
    let text = match reply {
        Ok(o) => o.body,
        Err(e) => format!("failure\n{e}"),
    };
    let mut response = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text,
    )
        .into_response();
    if let Some(token) = set_cookie
        && let Ok(v) =
            HeaderValue::from_str(&format!("{}={token}; Path=/; HttpOnly", session::COOKIE))
    {
        response.headers_mut().insert(header::SET_COOKIE, v);
    }
    response
}

async fn dispatch(
    state: &AppState,
    mode: &str,
    query: &HashMap<String, String>,
    headers: &HeaderMap,
    body: &Bytes,
    session_out: &mut Option<Session>,
    set_cookie: &mut Option<String>,
) -> Reply {
    if query.get("type").map(String::as_str) != Some("catalog") {
        return Err("Неподдерживаемый тип обмена".into());
    }
    if mode == "checkauth" {
        let (reply, token) = checkauth(state, headers).await?;
        *set_cookie = Some(token.clone());
        return Ok(reply);
    }
    let jar = CookieJar::from_headers(headers);
    let token = jar
        .get(session::COOKIE)
        .map(|c| c.value().to_string())
        .or_else(|| query.get("sessid").cloned())
        .filter(|t| !t.is_empty())
        .ok_or("Не выполнена авторизация обмена (checkauth)")?;
    let session = session::find(&state.db, &token)
        .await
        .map_err(db_err)?
        .ok_or("Сессия обмена не найдена, выполните checkauth")?;
    let session = session_out.insert(session);
    if session.expired {
        return Err("Сессия обмена истекла".into());
    }
    let filename = || {
        query
            .get("filename")
            .map(String::as_str)
            .filter(|f| !f.is_empty())
            .ok_or_else(|| "Не указан параметр filename".to_string())
    };
    match mode {
        "init" => init(state, session, &token).await,
        "file" => file(state, session, filename()?, body).await,
        "import" => import_file(state, session, filename()?, Limits::default(), None).await,
        "deactivate" => deactivate(state, session).await,
        "complete" => complete(state, session).await,
        other => Err(format!("Неизвестный режим «{other}»")),
    }
}

fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    use base64::Engine;
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = value.strip_prefix("Basic ")?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let raw = String::from_utf8(raw).ok()?;
    let (login, password) = raw.split_once(':')?;
    Some((login.to_string(), password.to_string()))
}

async fn checkauth(state: &AppState, headers: &HeaderMap) -> Result<(Outcome, String), String> {
    let (login, password) = basic_credentials(headers).ok_or(DENIED)?;
    let user = crate::auth::verify_login(&state.db, &login, password)
        .await
        .map_err(|_| "Ошибка проверки пароля".to_string())?
        .filter(|u| u.active)
        .ok_or(DENIED)?;
    let user_id = user.id;
    let access = access::Access::load(&state.db, user)
        .await
        .map_err(db_err)?;
    if !access.can(access::EXCHANGE_1C) {
        return Err(DENIED.into());
    }
    if let Err(e) = session::cleanup_expired(&state.db, &state.config.upload_dir).await {
        tracing::warn!("обмен с 1С: очистка старых сессий: {e}");
    }
    let (_, token) = session::start(&state.db, user_id).await.map_err(db_err)?;
    Ok((
        Outcome::text(format!("success\n{}\n{token}", session::COOKIE)),
        token,
    ))
}

async fn init(state: &AppState, session: &Session, token: &str) -> Reply {
    let settings = Settings::load(&state.db).await.map_err(db_err)?;
    // Новый обмен в той же сессии начинается с чистого листа
    let dir = session::dir(&state.config.upload_dir, session);
    if let Err(e) = tokio::fs::remove_dir_all(&dir).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(format!("Ошибка работы с файлом: {e}"));
    }
    let mut tx = state.db.begin().await.map_err(db_err)?;
    sqlx::query("DELETE FROM exchange_seen WHERE session_id = $1")
        .bind(session.id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    sqlx::query("UPDATE exchange_sessions SET state = '{}' WHERE id = $1")
        .bind(session.id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    Ok(Outcome::text(format!(
        "zip={}\nfile_limit={}\nsessid={token}\nversion=3.1",
        if settings.use_zip { "yes" } else { "no" },
        settings.file_limit
    )))
}

async fn file(state: &AppState, session: &Session, filename: &str, body: &Bytes) -> Reply {
    let settings = Settings::load(&state.db).await.map_err(db_err)?;
    if body.len() as u64 > settings.file_limit {
        return Err(format!(
            "Размер части файла превышает допустимый ({} байт)",
            settings.file_limit
        ));
    }
    let dir = session::dir(&state.config.upload_dir, session);
    session::append(&dir, filename, body)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Outcome::text("success"))
}

/// Короткий итог для ответа 1С: «Создано: items 2. Обновлено: sections 1».
fn summary(stats: &Stats) -> String {
    let value = serde_json::to_value(stats).unwrap_or_default();
    let mut parts = Vec::new();
    for (key, label) in [
        ("created", "Создано"),
        ("updated", "Обновлено"),
        ("deactivated", "Деактивировано"),
        ("deleted", "Удалено"),
        ("skipped", "Пропущено"),
    ] {
        let Some(map) = value[key].as_object().filter(|m| !m.is_empty()) else {
            continue;
        };
        let items: Vec<String> = map.iter().map(|(k, v)| format!("{k} {v}")).collect();
        parts.push(format!("{label}: {}", items.join(", ")));
    }
    if parts.is_empty() {
        "Изменений нет".into()
    } else {
        parts.join(". ")
    }
}

/// Подставляется в тестах между `apply` и коммитом, чтобы проверить откат.
pub(crate) type PostApply<'a> = &'a (dyn Fn() -> Result<(), String> + Send + Sync);

/// Один вызов `import`: файл целиком — одна транзакция (`parse`, `apply`, отметка в сессии).
pub(crate) async fn import_file(
    state: &AppState,
    session: &Session,
    filename: &str,
    limits: Limits,
    post_apply: Option<PostApply<'_>>,
) -> Reply {
    let rel = session::safe_rel_path(filename)
        .ok_or_else(|| session::SessionError::BadPath(filename.into()).to_string())?;
    let upload_dir = &state.config.upload_dir;
    let dir = session::dir(upload_dir, session);

    let unpack_dir = dir.clone();
    tokio::task::spawn_blocking(move || session::unpack_zips(&unpack_dir, limits))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;

    let data = tokio::fs::read(dir.join(&rel))
        .await
        .map_err(|_| format!("Файл «{filename}» не найден"))?;
    let parsed = tokio::task::spawn_blocking(move || cml::parse(&data))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let settings = Settings::load(&state.db).await.map_err(db_err)?;

    let mut tx = state.db.begin().await.map_err(db_err)?;
    // Блокировка строки сессии: два одновременных import одного файла не удвоят запись
    let current: Value =
        sqlx::query_scalar("SELECT state FROM exchange_sessions WHERE id = $1 FOR UPDATE")
            .bind(session.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
    let fresh = Session {
        state: current,
        ..session.clone()
    };
    if fresh.is_imported(filename) {
        return Ok(Outcome::text("success\nФайл уже загружен"));
    }

    let ctx = Ctx::new(&settings, Some(session.id), &dir, upload_dir)
        .with_hook(state.project.import_hook.as_deref());
    let result: Result<Stats, String> = async {
        let stats = apply::apply(&mut tx, &ctx, &parsed)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(check) = post_apply {
            check()?;
        }
        sqlx::query(
            "UPDATE exchange_sessions
             SET state = jsonb_set(state, '{imported}',
                                   COALESCE(state->'imported', '[]'::jsonb) || to_jsonb($2::text))
             WHERE id = $1",
        )
        .bind(session.id)
        .bind(filename)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(stats)
    }
    .await;
    match result {
        Ok(stats) => Ok(Outcome {
            body: format!("success\n{}", summary(&stats)),
            stats: serde_json::to_value(&stats).unwrap_or_default(),
        }),
        Err(e) => {
            // Транзакция откатилась, а записанные на диск файлы остались бы сиротами
            ctx.discard_written().await;
            Err(e)
        }
    }
}

/// Деактивация пропавшего по настройкам, если сессия видела полную выгрузку.
async fn deactivate_now(state: &AppState, session: &Session) -> Result<Stats, String> {
    let settings = Settings::load(&state.db).await.map_err(db_err)?;
    let mut tx = state.db.begin().await.map_err(db_err)?;
    let stats = apply::deactivate_missing(&mut tx, &settings, session.id)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query(
        "UPDATE exchange_sessions SET state = jsonb_set(state, '{deactivated}', 'true') WHERE id = $1",
    )
    .bind(session.id)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    Ok(stats)
}

async fn deactivate(state: &AppState, session: &Session) -> Reply {
    let stats = deactivate_now(state, session).await?;
    Ok(Outcome {
        body: "success".into(),
        stats: serde_json::to_value(&stats).unwrap_or_default(),
    })
}

async fn complete(state: &AppState, session: &Session) -> Reply {
    let mut stats = Value::Object(Default::default());
    let deactivated = session.state["deactivated"].as_bool().unwrap_or(false);
    if !deactivated {
        let settings = Settings::load(&state.db).await.map_err(db_err)?;
        if settings.element_action != Action::None || settings.section_action != Action::None {
            stats = serde_json::to_value(deactivate_now(state, session).await?).unwrap_or_default();
        }
    }
    let dir = session::dir(&state.config.upload_dir, session);
    if let Err(e) = tokio::fs::remove_dir_all(&dir).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %dir.display(), "обмен с 1С: не удалён каталог сессии: {e}");
    }
    sqlx::query("DELETE FROM exchange_sessions WHERE id = $1")
        .bind(session.id)
        .execute(&state.db)
        .await
        .map_err(db_err)?;
    Ok(Outcome {
        body: "success".into(),
        stats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::session::{self, Limits};
    use crate::exchange::settings::{Action, Settings};
    use crate::test_support::test_state;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, header};
    use sqlx::PgPool;
    use std::io::Write;
    use tower::ServiceExt;

    const BASE: &str = "/api/1c/exchange?type=catalog";
    const GROUPS: &str = include_str!("../../tests/fixtures/1c/groups.xml");
    const PRICES: &str = include_str!("../../tests/fixtures/1c/prices.xml");

    fn basic(login: &str, password: &str) -> String {
        use base64::Engine;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{login}:{password}"))
        )
    }

    struct Client {
        app: Router,
    }

    impl Client {
        fn new(state: &AppState) -> Client {
            Client {
                app: crate::app(state.clone()),
            }
        }

        async fn raw(
            &self,
            method: &str,
            uri: &str,
            cookie: Option<&str>,
            auth: Option<&str>,
            body: Vec<u8>,
        ) -> String {
            let mut req = Request::builder().method(method).uri(uri);
            if let Some(c) = cookie {
                req = req.header(header::COOKIE, format!("{}={c}", session::COOKIE));
            }
            if let Some(a) = auth {
                req = req.header(header::AUTHORIZATION, a);
            }
            let res = self
                .app
                .clone()
                .oneshot(req.body(Body::from(body)).unwrap())
                .await
                .unwrap();
            assert_eq!(
                res.headers()[header::CONTENT_TYPE],
                "text/plain; charset=utf-8",
                "{uri}"
            );
            let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        }

        async fn checkauth(&self, login: &str, password: &str) -> String {
            self.raw(
                "GET",
                &format!("{BASE}&mode=checkauth"),
                None,
                Some(&basic(login, password)),
                vec![],
            )
            .await
        }

        async fn get(&self, token: &str, query: &str) -> String {
            self.raw("GET", &format!("{BASE}&{query}"), Some(token), None, vec![])
                .await
        }

        async fn post(&self, token: &str, query: &str, body: Vec<u8>) -> String {
            self.raw("POST", &format!("{BASE}&{query}"), Some(token), None, body)
                .await
        }
    }

    /// Пользователь группы с правом `exchange.1c`.
    async fn exchange_user(db: &PgPool, login: &str) -> i64 {
        let user = crate::auth::create_user(db, login, "password123".into(), false)
            .await
            .unwrap();
        let (group,): (i64,) =
            sqlx::query_as("INSERT INTO groups (code, name) VALUES ($1, 'Обмен') RETURNING id")
                .bind(format!("g-{login}"))
                .fetch_one(db)
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO group_permissions (group_id, permission) VALUES ($1, 'exchange.1c')",
        )
        .bind(group)
        .execute(db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO user_groups (user_id, group_id) VALUES ($1, $2)")
            .bind(user.id)
            .bind(group)
            .execute(db)
            .await
            .unwrap();
        user.id
    }

    async fn login(c: &Client, login: &str) -> String {
        let r = c.checkauth(login, "password123").await;
        let lines: Vec<&str> = r.lines().collect();
        assert_eq!(&lines[..2], ["success", "CMS_1C_SID"], "{r}");
        assert_eq!(lines[2].len(), 64);
        lines[2].to_string()
    }

    fn goods_xml(ids: &[&str], full: bool) -> String {
        let goods: String = ids
            .iter()
            .map(|id| {
                format!("<Товар><Ид>{id}</Ид><Наименование>Товар {id}</Наименование></Товар>")
            })
            .collect();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<КоммерческаяИнформация xmlns="urn:1C.ru:commerceml_3" ВерсияСхемы="3.1"><Каталог СодержитТолькоИзменения="{}"><Ид>cat-0001</Ид><ИдКлассификатора>cls-0001</ИдКлассификатора><Наименование>Тестовый каталог</Наименование><Товары>{goods}</Товары></Каталог></КоммерческаяИнформация>"#,
            !full
        )
    }

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
        buf.into_inner()
    }

    async fn upload(c: &Client, token: &str, filename: &str, data: &[u8]) {
        for chunk in data.chunks(1024) {
            let r = c
                .post(
                    token,
                    &format!("mode=file&filename={filename}"),
                    chunk.to_vec(),
                )
                .await;
            assert_eq!(r, "success", "{filename}");
        }
    }

    async fn active_items(db: &PgPool) -> Vec<(String, bool)> {
        sqlx::query_as(
            "SELECT xml_id, active FROM collection_items WHERE xml_id <> '' ORDER BY xml_id",
        )
        .fetch_all(db)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn full_exchange_flow(db: PgPool) {
        exchange_user(&db, "exch").await;
        let state = test_state(db.clone());
        let c = Client::new(&state);
        let token = login(&c, "exch").await;

        let init = c.get(&token, "mode=init").await;
        assert_eq!(
            init,
            format!("zip=yes\nfile_limit=209715200\nsessid={token}\nversion=3.1")
        );

        let goods = goods_xml(&["good-1", "good-2"], true);
        let zip = make_zip(&[
            ("groups.xml", GROUPS.as_bytes()),
            ("goods/goods_1_1.xml", goods.as_bytes()),
            ("prices.xml", PRICES.as_bytes()),
        ]);
        upload(&c, &token, "exchange.zip", &zip).await;

        for file in ["groups.xml", "goods/goods_1_1.xml", "prices.xml"] {
            let r = c.get(&token, &format!("mode=import&filename={file}")).await;
            assert!(r.starts_with("success"), "{file}: {r}");
        }
        assert_eq!(c.get(&token, "mode=complete").await, "success");

        assert_eq!(
            active_items(&db).await,
            [("good-1".to_string(), true), ("good-2".to_string(), true)]
        );
        let sections: i64 =
            sqlx::query_scalar("SELECT count(*) FROM collection_sections WHERE xml_id <> ''")
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(sections >= 2);
        let log: Vec<(String, String, String)> =
            sqlx::query_as("SELECT mode, filename, result FROM exchange_log ORDER BY id")
                .fetch_all(&db)
                .await
                .unwrap();
        let modes: Vec<&str> = log.iter().map(|l| l.0.as_str()).collect();
        assert_eq!(modes, ["import", "import", "import", "complete"]);
        assert!(log.iter().all(|l| l.2 == "success"));
        assert_eq!(log[1].1, "goods/goods_1_1.xml");
        let (stats, user): (serde_json::Value, Option<i64>) =
            sqlx::query_as("SELECT stats, user_id FROM exchange_log WHERE id = (SELECT min(id) + 1 FROM exchange_log)")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(stats["created"]["items"], 2);
        assert!(user.is_some());
        // сессия завершена: каталог убран, токен больше не действует
        assert!(
            !state
                .config
                .upload_dir
                .join("1c_exchange")
                .join("1")
                .exists()
                || std::fs::read_dir(state.config.upload_dir.join("1c_exchange"))
                    .unwrap()
                    .next()
                    .is_none()
        );
        assert!(c.get(&token, "mode=init").await.starts_with("failure"));
    }

    #[sqlx::test]
    async fn bitrix_address_and_sessid_param(db: PgPool) {
        exchange_user(&db, "exch").await;
        let c = Client::new(&test_state(db.clone()));
        let r = c
            .raw(
                "GET",
                "/bitrix/admin/1c_exchange.php?type=catalog&mode=checkauth",
                None,
                Some(&basic("exch", "password123")),
                vec![],
            )
            .await;
        let token = r.lines().nth(2).unwrap().to_string();
        let init = c
            .raw(
                "GET",
                &format!("/bitrix/admin/1c_exchange.php?type=catalog&mode=init&sessid={token}"),
                None,
                None,
                vec![],
            )
            .await;
        assert!(init.starts_with("zip=yes"), "{init}");
    }

    #[sqlx::test]
    async fn checkauth_requires_permission(db: PgPool) {
        exchange_user(&db, "exch").await;
        crate::auth::create_user(&db, "plain", "password123".into(), false)
            .await
            .unwrap();
        crate::auth::create_user(&db, "root", "password123".into(), true)
            .await
            .unwrap();
        let c = Client::new(&test_state(db.clone()));
        let denied = "failure\nДоступ запрещён";
        assert_eq!(c.checkauth("plain", "password123").await, denied);
        assert_eq!(c.checkauth("exch", "wrong-password").await, denied);
        assert_eq!(c.checkauth("nobody", "password123").await, denied);
        let no_basic = c
            .raw("GET", &format!("{BASE}&mode=checkauth"), None, None, vec![])
            .await;
        assert_eq!(no_basic, denied);
        assert!(
            c.checkauth("root", "password123")
                .await
                .starts_with("success\nCMS_1C_SID\n")
        );
        // без сессии остальные режимы не работают
        let r = c
            .raw("GET", &format!("{BASE}&mode=init"), None, None, vec![])
            .await;
        assert!(r.starts_with("failure"), "{r}");
        let r = c
            .raw(
                "GET",
                &format!("{BASE}&mode=init"),
                Some("bogus"),
                None,
                vec![],
            )
            .await;
        assert!(r.starts_with("failure"), "{r}");
    }

    #[sqlx::test]
    async fn path_traversal_rejected(db: PgPool) {
        exchange_user(&db, "exch").await;
        let state = test_state(db.clone());
        let c = Client::new(&state);
        let token = login(&c, "exch").await;
        for name in ["../x", "%2e%2e/x", "a/../../x", "%2Fetc%2Fpasswd", "a%5Cb"] {
            let r = c
                .post(
                    &token,
                    &format!("mode=file&filename={name}"),
                    b"evil".to_vec(),
                )
                .await;
            assert!(r.starts_with("failure"), "{name}: {r}");
            let r = c.get(&token, &format!("mode=import&filename={name}")).await;
            assert!(r.starts_with("failure"), "{name}: {r}");
        }
        let r = c.post(&token, "mode=file", b"x".to_vec()).await;
        assert!(r.starts_with("failure"), "{r}");
        let base = state.config.upload_dir.join("1c_exchange");
        assert!(!base.join("x").exists());
        assert!(!state.config.upload_dir.join("x").exists());

        // zip с записью ../x
        let zip = make_zip(&[("ok.xml", b"<a/>"), ("../x", b"evil")]);
        upload(&c, &token, "bad.zip", &zip).await;
        let r = c.get(&token, "mode=import&filename=ok.xml").await;
        assert!(r.starts_with("failure"), "{r}");
        assert!(!base.join("x").exists());
        assert!(!base.join("1/ok.xml").exists());
    }

    #[sqlx::test]
    async fn zip_limits_enforced(db: PgPool) {
        let user_id = exchange_user(&db, "exch").await;
        let state = test_state(db.clone());
        let (s, _) = session::start(&db, user_id).await.unwrap();
        let dir = session::dir(&state.config.upload_dir, &s);
        std::fs::create_dir_all(&dir).unwrap();
        let big = vec![b'a'; 4096];
        std::fs::write(dir.join("a.zip"), make_zip(&[("a.xml", &big)])).unwrap();
        let small = Limits {
            max_bytes: 1000,
            max_entries: 10,
        };
        let err = import_file(&state, &s, "a.xml", small, None)
            .await
            .unwrap_err();
        assert!(err.contains("Архив превышает"), "{err}");
        assert!(!dir.join("a.xml").exists());
    }

    #[sqlx::test]
    async fn import_of_broken_zip_fails_cleanly(db: PgPool) {
        exchange_user(&db, "exch").await;
        let state = test_state(db.clone());
        let c = Client::new(&state);
        let token = login(&c, "exch").await;
        upload(&c, &token, "broken.zip", b"PK\x03\x04 definitely not a zip").await;
        let r = c.get(&token, "mode=import&filename=goods.xml").await;
        assert!(r.starts_with("failure\n"), "{r}");
        // и сам пакет с мусором вместо XML
        upload(&c, &token, "junk.xml", b"\x00\x01 not xml").await;
        let r = c.get(&token, "mode=import&filename=junk.xml").await;
        assert!(r.starts_with("failure\n"), "{r}");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM collections")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
        let failed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM exchange_log WHERE result = 'failure' AND error <> ''",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(failed, 2);
        // сессия жива: нормальный файл после этого загружается
        upload(&c, &token, "ok.xml", goods_xml(&["g1"], false).as_bytes()).await;
        let r = c.get(&token, "mode=import&filename=ok.xml").await;
        assert!(r.starts_with("success"), "{r}");
    }

    #[sqlx::test]
    async fn sessions_are_isolated(db: PgPool) {
        exchange_user(&db, "one").await;
        exchange_user(&db, "two").await;
        let state = test_state(db.clone());
        let c = Client::new(&state);
        let t1 = login(&c, "one").await;
        let t2 = login(&c, "two").await;
        assert_ne!(t1, t2);
        upload(&c, &t1, "only1.xml", goods_xml(&["g1"], false).as_bytes()).await;
        // вторая сессия файла первой не видит
        let r = c.get(&t2, "mode=import&filename=only1.xml").await;
        assert!(r.starts_with("failure"), "{r}");
        assert_eq!(active_items(&db).await, []);
        let r = c.get(&t1, "mode=import&filename=only1.xml").await;
        assert!(r.starts_with("success"), "{r}");
        // «уже загружено» тоже по сессиям
        let r = c.get(&t2, "mode=import&filename=only1.xml").await;
        assert!(r.starts_with("failure"), "{r}");
        // seen разных сессий не смешиваются
        Settings {
            element_action: Action::Deactivate,
            ..Settings::default()
        }
        .save(&db)
        .await
        .unwrap();
        upload(
            &c,
            &t1,
            "full.xml",
            goods_xml(&["g1", "g2"], true).as_bytes(),
        )
        .await;
        assert!(
            c.get(&t1, "mode=import&filename=full.xml")
                .await
                .starts_with("success")
        );
        upload(&c, &t2, "full.xml", goods_xml(&["g1"], true).as_bytes()).await;
        assert!(
            c.get(&t2, "mode=import&filename=full.xml")
                .await
                .starts_with("success")
        );
        assert_eq!(c.get(&t2, "mode=deactivate").await, "success");
        assert_eq!(
            active_items(&db).await,
            [("g1".to_string(), true), ("g2".to_string(), false)]
        );
    }

    #[sqlx::test]
    async fn repeated_import_of_same_file_is_noop(db: PgPool) {
        exchange_user(&db, "exch").await;
        let c = Client::new(&test_state(db.clone()));
        let token = login(&c, "exch").await;
        upload(&c, &token, "g.xml", goods_xml(&["g1"], false).as_bytes()).await;
        assert!(
            c.get(&token, "mode=import&filename=g.xml")
                .await
                .starts_with("success")
        );
        sqlx::query("UPDATE collection_items SET name = 'Изменено вручную'")
            .execute(&db)
            .await
            .unwrap();
        assert!(
            c.get(&token, "mode=import&filename=g.xml")
                .await
                .starts_with("success")
        );
        let name: String = sqlx::query_scalar("SELECT name FROM collection_items")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(name, "Изменено вручную");
    }

    #[sqlx::test]
    async fn full_export_deactivates_missing(db: PgPool) {
        exchange_user(&db, "exch").await;
        Settings {
            element_action: Action::Deactivate,
            ..Settings::default()
        }
        .save(&db)
        .await
        .unwrap();
        let state = test_state(db.clone());
        let c = Client::new(&state);

        let t = login(&c, "exch").await;
        upload(
            &c,
            &t,
            "g.xml",
            goods_xml(&["a", "b", "c"], true).as_bytes(),
        )
        .await;
        assert!(
            c.get(&t, "mode=import&filename=g.xml")
                .await
                .starts_with("success")
        );
        // запись, заведённая вручную в той же коллекции, не трогается
        sqlx::query(
            "INSERT INTO collection_items (collection_id, code, name)
             SELECT id, 'manual', 'Ручная' FROM collections LIMIT 1",
        )
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(c.get(&t, "mode=complete").await, "success");
        assert!(active_items(&db).await.iter().all(|i| i.1));

        // полная выгрузка без «c»: deactivate
        let t = login(&c, "exch").await;
        upload(&c, &t, "g.xml", goods_xml(&["a", "b"], true).as_bytes()).await;
        assert!(
            c.get(&t, "mode=import&filename=g.xml")
                .await
                .starts_with("success")
        );
        assert_eq!(c.get(&t, "mode=deactivate").await, "success");
        assert_eq!(
            active_items(&db).await,
            [("a".into(), true), ("b".into(), true), ("c".into(), false)]
        );
        let manual: bool =
            sqlx::query_scalar("SELECT active FROM collection_items WHERE code = 'manual'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(manual);
        let stats: serde_json::Value = sqlx::query_scalar(
            "SELECT stats FROM exchange_log WHERE mode = 'deactivate' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(stats["deactivated"]["items"], 1);

        // пакет «только изменения» ничего не деактивирует
        sqlx::query("UPDATE collection_items SET active = TRUE")
            .execute(&db)
            .await
            .unwrap();
        let t = login(&c, "exch").await;
        upload(&c, &t, "g.xml", goods_xml(&["a"], false).as_bytes()).await;
        assert!(
            c.get(&t, "mode=import&filename=g.xml")
                .await
                .starts_with("success")
        );
        assert_eq!(c.get(&t, "mode=deactivate").await, "success");
        assert!(active_items(&db).await.iter().all(|i| i.1));

        // complete без deactivate после полной выгрузки делает то же самое
        let t = login(&c, "exch").await;
        upload(&c, &t, "g.xml", goods_xml(&["a", "c"], true).as_bytes()).await;
        assert!(
            c.get(&t, "mode=import&filename=g.xml")
                .await
                .starts_with("success")
        );
        assert_eq!(c.get(&t, "mode=complete").await, "success");
        assert_eq!(
            active_items(&db).await,
            [("a".into(), true), ("b".into(), false), ("c".into(), true)]
        );
    }

    #[sqlx::test]
    async fn expired_session_rejected(db: PgPool) {
        exchange_user(&db, "exch").await;
        let state = test_state(db.clone());
        let c = Client::new(&state);
        let token = login(&c, "exch").await;
        upload(&c, &token, "g.xml", goods_xml(&["a"], false).as_bytes()).await;
        sqlx::query("UPDATE exchange_sessions SET touched_at = now() - interval '25 hours'")
            .execute(&db)
            .await
            .unwrap();
        for q in ["mode=init", "mode=import&filename=g.xml", "mode=complete"] {
            assert_eq!(
                c.get(&token, q).await,
                "failure\nСессия обмена истекла",
                "{q}"
            );
        }
        assert_eq!(active_items(&db).await, []);
        // новый checkauth убирает просроченную сессию и её каталог
        let _ = login(&c, "exch").await;
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM exchange_sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let dirs = std::fs::read_dir(state.config.upload_dir.join("1c_exchange"))
            .unwrap()
            .count();
        assert!(dirs <= 1);
    }

    /// Файлы, лежащие в `upload_dir` вне каталога обмена.
    fn stored_files(upload_dir: &std::path::Path) -> usize {
        fn walk(dir: &std::path::Path, skip: bool) -> usize {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return 0;
            };
            rd.filter_map(Result::ok)
                .map(|e| {
                    let p = e.path();
                    if p.is_dir() {
                        if skip && p.file_name().is_some_and(|n| n == "1c_exchange") {
                            0
                        } else {
                            walk(&p, false)
                        }
                    } else {
                        1
                    }
                })
                .sum()
        }
        walk(upload_dir, true)
    }

    #[sqlx::test]
    async fn failure_after_apply_discards_written_files(db: PgPool) {
        let user_id = exchange_user(&db, "exch").await;
        let state = test_state(db.clone());
        let (s, _) = session::start(&db, user_id).await.unwrap();
        let dir = session::dir(&state.config.upload_dir, &s);
        std::fs::create_dir_all(dir.join("import_files/aa")).unwrap();
        std::fs::write(
            dir.join("goods.xml"),
            include_str!("../../tests/fixtures/1c/goods.xml"),
        )
        .unwrap();
        std::fs::write(dir.join("import_files/aa/good-1_a.jpeg"), b"AAAA").unwrap();
        std::fs::write(dir.join("import_files/aa/good-1_b.jpeg"), b"BBBB").unwrap();

        let fail = || Err("сбой после записи".to_string());
        let err = import_file(&state, &s, "goods.xml", Limits::default(), Some(&fail))
            .await
            .unwrap_err();
        assert_eq!(err, "сбой после записи");
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM files")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(rows, 0);
        let items: i64 = sqlx::query_scalar("SELECT count(*) FROM collection_items")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(items, 0);
        assert_eq!(stored_files(&state.config.upload_dir), 0, "сироты на диске");
        let imported: serde_json::Value =
            sqlx::query_scalar("SELECT state FROM exchange_sessions WHERE id = $1")
                .bind(s.id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(imported["imported"].is_null());

        // контроль: без сбоя файлы на диске появляются
        import_file(&state, &s, "goods.xml", Limits::default(), None)
            .await
            .unwrap();
        assert_eq!(stored_files(&state.config.upload_dir), 2);
    }
}
