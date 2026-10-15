//! HTTP-протокол обмена с 1С: `checkauth`, `init`, `file`, `import`, `deactivate`, `complete`.
//!
//! Распаковка zip выполняется лениво — при первом `import` после загрузки архива.

use std::collections::HashMap;
use std::time::Instant;

use axum::{
    Router,
    body::Body,
    extract::{Query, State},
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

pub fn router() -> Router<AppState> {
    Router::new().route("/api/1c/exchange", get(handle).post(handle))
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
    body: Body,
) -> Response {
    // Запрос обрабатывается в отдельной задаче: если 1С оборвёт соединение, импорт всё равно
    // закончится целиком (транзакция, уборка файлов, журнал), а не прервётся посередине.
    match tokio::spawn(process(state, query, headers, body)).await {
        Ok(response) => response,
        Err(e) => {
            tracing::error!("обмен с 1С: обработчик запроса упал: {e}");
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "failure\nВнутренняя ошибка обмена",
            )
                .into_response()
        }
    }
}

async fn process(
    state: AppState,
    query: HashMap<String, String>,
    headers: HeaderMap,
    body: Body,
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
        body,
        &mut session,
        &mut set_cookie,
    )
    .await;
    if let (Some(session), Err(e)) = (&session, &reply)
        && mode == "import"
    {
        // Файл с ошибкой блокирует деактивацию, пока он не загрузится успешно
        let marked = sqlx::query(
            "UPDATE exchange_sessions
             SET state = jsonb_set(state, '{failed}',
                     (COALESCE(state->'failed', '[]'::jsonb) - $2::text) || to_jsonb($2::text))
             WHERE id = $1",
        )
        .bind(session.id)
        .bind(truncate(filename, 255))
        .execute(&state.db)
        .await;
        if let Err(err) = marked {
            tracing::warn!("обмен с 1С: не отмечена ошибка импорта: {err}");
        }
        tracing::warn!(session = session.id, "обмен с 1С: import «{filename}»: {e}");
    }
    match (&session, &reply) {
        // Журнал пишется только для проверенной сессии
        (Some(session), _) if matches!(mode, "import" | "deactivate" | "complete") => {
            let (result, stats, error) = match &reply {
                Ok(o) => ("success", o.stats.clone(), String::new()),
                Err(e) => ("failure", Value::Object(Default::default()), e.clone()),
            };
            let logged = sqlx::query(
                "INSERT INTO exchange_log (user_id, mode, filename, result, stats, error, duration_ms)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(session.user_id)
            .bind(mode)
            .bind(truncate(filename, 255))
            .bind(result)
            .bind(stats)
            .bind(truncate(&error, 2000))
            .bind(started.elapsed().as_millis().min(i32::MAX as u128) as i32)
            .execute(&state.db)
            .await;
            if let Err(e) = logged {
                tracing::warn!("обмен с 1С: не записан журнал: {e}");
            }
        }
        (None, Err(e)) => tracing::warn!(mode, "обмен с 1С: отказ до входа: {e}"),
        _ => {}
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
    let secure = if state.config.cookie_secure {
        "; Secure"
    } else {
        ""
    };
    if let Some(token) = set_cookie
        && let Ok(v) = HeaderValue::from_str(&format!(
            "{}={token}; Path=/; HttpOnly; SameSite=Strict{secure}",
            session::COOKIE
        ))
    {
        response.headers_mut().insert(header::SET_COOKIE, v);
    }
    response
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

async fn dispatch(
    state: &AppState,
    mode: &str,
    query: &HashMap<String, String>,
    headers: &HeaderMap,
    body: Body,
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
    if session.expired {
        return Err("Сессия обмена истекла".into());
    }
    authorize(state, session.user_id).await?;
    let session = session_out.insert(session);
    let filename = || {
        query
            .get("filename")
            .map(String::as_str)
            .filter(|f| !f.is_empty())
            .ok_or_else(|| "Не указан параметр filename".to_string())
    };
    match mode {
        "init" => init(state, session, &token).await,
        "file" => file(state, session, filename()?, headers, body).await,
        "import" => import_file(state, session, filename()?, Limits::default(), None).await,
        "deactivate" => deactivate(state, session).await,
        "complete" => complete(state, session).await,
        other => Err(format!("Неизвестный режим «{other}»")),
    }
}

/// Пользователь сессии всё ещё существует, активен и имеет право на обмен.
async fn authorize(state: &AppState, user_id: Option<i64>) -> Result<(), String> {
    let user_id = user_id.ok_or(DENIED)?;
    let user: Option<crate::auth::User> =
        sqlx::query_as("SELECT id, login, email, is_admin, active FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.db)
            .await
            .map_err(db_err)?;
    let user = user.filter(|u| u.active).ok_or(DENIED)?;
    let access = access::Access::load(&state.db, user)
        .await
        .map_err(db_err)?;
    if access.can(access::EXCHANGE_1C) {
        Ok(())
    } else {
        Err(DENIED.into())
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

async fn file(
    state: &AppState,
    session: &Session,
    filename: &str,
    headers: &HeaderMap,
    body: Body,
) -> Reply {
    let settings = Settings::load(&state.db).await.map_err(db_err)?;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > settings.file_limit) {
        return Err(session::SessionError::TooLarge(settings.file_limit).to_string());
    }
    let dir = session::dir(&state.config.upload_dir, session);
    session::append_body(&dir, filename, settings.file_limit, body)
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
    // Импорты разных сессий не пишут каталог одновременно
    apply::lock_exchange(&mut tx).await.map_err(db_err)?;
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
             SET state = jsonb_set(
                     jsonb_set(state, '{imported}',
                               COALESCE(state->'imported', '[]'::jsonb) || to_jsonb($2::text)),
                     '{failed}', COALESCE(state->'failed', '[]'::jsonb) - $2::text)
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
const SKIPPED_NOTE: &str = "деактивация пропущена: были ошибки импорта";

/// Деактивация пропавшего по настройкам. Если в сессии были неудачные импорты, пакет
/// неполон и «пропавшее» может оказаться просто невыгруженным - тогда ничего не трогаем.
async fn deactivate_now(state: &AppState, session: &Session) -> Reply {
    let settings = Settings::load(&state.db).await.map_err(db_err)?;
    let has_failures = session.state["failed"]
        .as_array()
        .is_some_and(|f| !f.is_empty());
    if has_failures {
        let would_act =
            settings.element_action != Action::None || settings.section_action != Action::None;
        if would_act {
            return Ok(Outcome {
                body: format!("success\n{SKIPPED_NOTE}"),
                stats: serde_json::json!({ "note": SKIPPED_NOTE }),
            });
        }
    }
    let mut tx = state.db.begin().await.map_err(db_err)?;
    apply::lock_exchange(&mut tx).await.map_err(db_err)?;
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
    Ok(Outcome {
        body: "success".into(),
        stats: serde_json::to_value(&stats).unwrap_or_default(),
    })
}

async fn deactivate(state: &AppState, session: &Session) -> Reply {
    deactivate_now(state, session).await
}

async fn complete(state: &AppState, session: &Session) -> Reply {
    let mut outcome = Outcome::text("success");
    let deactivated = session.state["deactivated"].as_bool().unwrap_or(false);
    if !deactivated {
        let settings = Settings::load(&state.db).await.map_err(db_err)?;
        if settings.element_action != Action::None || settings.section_action != Action::None {
            outcome = deactivate_now(state, session).await?;
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
    Ok(outcome)
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
    async fn session_cookie_is_strict(db: PgPool) {
        exchange_user(&db, "exch").await;
        for secure in [false, true] {
            let mut state = test_state(db.clone());
            let mut config = (*state.config).clone();
            config.cookie_secure = secure;
            state.config = std::sync::Arc::new(config);
            let req = Request::builder()
                .uri(format!("{BASE}&mode=checkauth"))
                .header(header::AUTHORIZATION, basic("exch", "password123"))
                .body(Body::empty())
                .unwrap();
            let res = crate::app(state).oneshot(req).await.unwrap();
            let cookie = res.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .to_string();
            assert!(cookie.starts_with("CMS_1C_SID="), "{cookie}");
            assert!(cookie.contains("HttpOnly"), "{cookie}");
            assert!(cookie.contains("SameSite=Strict"), "{cookie}");
            assert_eq!(cookie.contains("Secure"), secure, "{cookie}");
        }
    }

    #[sqlx::test]
    async fn sessid_param(db: PgPool) {
        exchange_user(&db, "exch").await;
        let c = Client::new(&test_state(db.clone()));
        let r = c
            .raw(
                "GET",
                "/api/1c/exchange?type=catalog&mode=checkauth",
                None,
                Some(&basic("exch", "password123")),
                vec![],
            )
            .await;
        let token = r.lines().nth(2).unwrap().to_string();
        let init = c
            .raw(
                "GET",
                &format!("/api/1c/exchange?type=catalog&mode=init&sessid={token}"),
                None,
                None,
                vec![],
            )
            .await;
        assert!(init.starts_with("zip=yes"), "{init}");
    }

    #[sqlx::test]
    async fn bitrix_address_not_served(db: PgPool) {
        let res = crate::app(test_state(db))
            .oneshot(
                Request::builder()
                    .uri("/bitrix/admin/1c_exchange.php?type=catalog&mode=checkauth")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
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
        assert_eq!(r, "failure\nАрхив не дочитан или повреждён");
        // и сам пакет с мусором вместо XML — в новой сессии, без битого архива
        let token2 = login(&c, "exch").await;
        upload(&c, &token2, "junk.xml", b"\x00\x01 not xml").await;
        let r = c.get(&token2, "mode=import&filename=junk.xml").await;
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
        // недогруженный архив не удаляется
        assert!(
            state
                .config
                .upload_dir
                .join("1c_exchange")
                .read_dir()
                .unwrap()
                .any(|d| d.unwrap().path().join("broken.zip").exists())
        );
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
        let t3 = login(&c, "two").await;
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
        upload(&c, &t3, "full.xml", goods_xml(&["g1"], true).as_bytes()).await;
        assert!(
            c.get(&t3, "mode=import&filename=full.xml")
                .await
                .starts_with("success")
        );
        assert_eq!(c.get(&t3, "mode=deactivate").await, "success");
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

    fn counting_body(
        chunks: usize,
        size: usize,
        counter: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Body {
        use futures_util::StreamExt;
        Body::from_stream(futures_util::stream::iter(0..chunks).map(move |_| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![b'x'; size]))
        }))
    }

    impl Client {
        async fn post_body(&self, cookie: Option<&str>, query: &str, body: Body) -> String {
            let mut req = Request::builder()
                .method("POST")
                .uri(format!("{BASE}&{query}"));
            if let Some(c) = cookie {
                req = req.header(header::COOKIE, format!("{}={c}", session::COOKIE));
            }
            let res = self
                .app
                .clone()
                .oneshot(req.body(body).unwrap())
                .await
                .unwrap();
            String::from_utf8(
                to_bytes(res.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap()
        }
    }

    fn exchange_files(state: &AppState) -> usize {
        let base = state.config.upload_dir.join("1c_exchange");
        fn walk(d: &std::path::Path) -> usize {
            std::fs::read_dir(d)
                .map(|rd| {
                    rd.filter_map(Result::ok)
                        .map(|e| {
                            if e.path().is_dir() {
                                walk(&e.path())
                            } else {
                                1
                            }
                        })
                        .sum()
                })
                .unwrap_or(0)
        }
        walk(&base)
    }

    #[sqlx::test]
    async fn file_body_is_not_read_without_session(db: PgPool) {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        exchange_user(&db, "exch").await;
        let state = test_state(db.clone());
        let c = Client::new(&state);
        for cookie in [None, Some("bogus")] {
            let counter = Arc::new(AtomicUsize::new(0));
            let body = counting_body(64, 1 << 20, counter.clone());
            let r = c.post_body(cookie, "mode=file&filename=x.bin", body).await;
            assert!(r.starts_with("failure"), "{r}");
            assert_eq!(counter.load(Ordering::SeqCst), 0, "тело прочитано до входа");
        }
        // другие режимы тело тоже не читают
        let token = login(&c, "exch").await;
        let counter = Arc::new(AtomicUsize::new(0));
        let body = counting_body(64, 1 << 20, counter.clone());
        let r = c.post_body(Some(&token), "mode=init", body).await;
        assert!(r.starts_with("zip="), "{r}");
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert_eq!(exchange_files(&state), 0);
    }

    #[sqlx::test]
    async fn file_over_limit_fails_and_removes_partial(db: PgPool) {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        exchange_user(&db, "exch").await;
        Settings {
            file_limit: 10,
            ..Settings::default()
        }
        .save(&db)
        .await
        .unwrap();
        let state = test_state(db.clone());
        let c = Client::new(&state);
        let token = login(&c, "exch").await;
        assert_eq!(
            c.post(&token, "mode=file&filename=a.bin", b"12345".to_vec())
                .await,
            "success"
        );
        let counter = Arc::new(AtomicUsize::new(0));
        let body = counting_body(64, 8, counter.clone());
        let r = c
            .post_body(Some(&token), "mode=file&filename=a.bin", body)
            .await;
        assert!(r.starts_with("failure"), "{r}");
        assert!(counter.load(Ordering::SeqCst) <= 3, "читали дальше лимита");
        let dir = state.config.upload_dir.join("1c_exchange");
        let file = std::fs::read_dir(&dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("a.bin");
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"12345",
            "частичная запись убрана"
        );
        // новый файл целиком сверх лимита не остаётся
        let r = c
            .post(&token, "mode=file&filename=b.bin", vec![b'x'; 11])
            .await;
        assert!(r.starts_with("failure"), "{r}");
        assert!(!file.with_file_name("b.bin").exists());
    }

    #[sqlx::test]
    async fn pre_auth_requests_are_not_logged(db: PgPool) {
        exchange_user(&db, "exch").await;
        let c = Client::new(&test_state(db.clone()));
        for cookie in [None, Some("bogus")] {
            for q in [
                "mode=import&filename=a.xml",
                "mode=deactivate",
                "mode=complete",
            ] {
                let r = c
                    .raw("GET", &format!("{BASE}&{q}"), cookie, None, vec![])
                    .await;
                assert!(r.starts_with("failure"), "{r}");
            }
        }
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM exchange_log")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
        // с сессией длинные имя и текст ошибки обрезаются
        let token = login(&c, "exch").await;
        let long = "a".repeat(400);
        let r = c
            .get(&token, &format!("mode=import&filename={long}.xml"))
            .await;
        assert!(r.starts_with("failure"), "{r}");
        let (f, e): (String, String) = sqlx::query_as("SELECT filename, error FROM exchange_log")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(f.chars().count(), 255);
        assert!(e.chars().count() <= 2000);
    }

    #[sqlx::test]
    async fn truncated_zip_waits_for_the_rest(db: PgPool) {
        exchange_user(&db, "exch").await;
        let c = Client::new(&test_state(db.clone()));
        let token = login(&c, "exch").await;
        let ids: Vec<String> = (0..300).map(|i| format!("g-{:x}", i * 7919 + 13)).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let xml = goods_xml(&refs, false);
        let zip = make_zip(&[("g.xml", xml.as_bytes())]);
        let (first, rest) = zip.split_at(zip.len() / 2);
        upload(&c, &token, "ex.zip", first).await;
        let r = c.get(&token, "mode=import&filename=g.xml").await;
        assert_eq!(r, "failure\nАрхив не дочитан или повреждён");
        upload(&c, &token, "ex.zip", rest).await;
        let r = c.get(&token, "mode=import&filename=g.xml").await;
        assert!(r.starts_with("success"), "{r}");
        assert_eq!(active_items(&db).await.len(), 300);
    }

    #[sqlx::test]
    async fn revoked_access_rejects_session(db: PgPool) {
        exchange_user(&db, "exch").await;
        exchange_user(&db, "gone").await;
        crate::auth::create_user(&db, "root", "password123".into(), true)
            .await
            .unwrap();
        let c = Client::new(&test_state(db.clone()));
        let t = login(&c, "exch").await;
        let t_gone = login(&c, "gone").await;
        let t_root = login(&c, "root").await;
        upload(&c, &t, "a.xml", goods_xml(&["a"], false).as_bytes()).await;
        sqlx::query("DELETE FROM group_permissions")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET active = FALSE WHERE login = 'gone'")
            .execute(&db)
            .await
            .unwrap();
        let denied = "failure\nДоступ запрещён";
        for tok in [&t, &t_gone] {
            assert_eq!(c.get(tok, "mode=init").await, denied);
            assert_eq!(
                c.post(tok, "mode=file&filename=b.xml", b"x".to_vec()).await,
                denied
            );
            assert_eq!(c.get(tok, "mode=import&filename=a.xml").await, denied);
        }
        assert_eq!(active_items(&db).await, []);
        assert!(c.get(&t_root, "mode=init").await.starts_with("zip="));
    }

    /// Сессия с файлом `name` в каталоге.
    async fn session_with_file(state: &AppState, user_id: i64, name: &str, data: &[u8]) -> Session {
        let (s, _) = session::start(&state.db, user_id).await.unwrap();
        let dir = session::dir(&state.config.upload_dir, &s);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), data).unwrap();
        s
    }

    /// Обработчик товаров, который ждёт разрешения продолжить.
    struct Gate {
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    impl crate::bxapi::project::ImportHook for Gate {
        fn after_item<'a>(
            &'a self,
            _conn: &'a mut sqlx::PgConnection,
            _item_id: i64,
            _goods: &'a cml::Goods,
        ) -> crate::bxapi::project::BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.started.notify_one();
                self.release.notified().await;
                Ok(())
            })
        }
    }

    #[sqlx::test]
    async fn import_finishes_when_client_disconnects(db: PgPool) {
        use std::sync::Arc;
        exchange_user(&db, "exch").await;
        let gate = Arc::new(Gate {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let mut state = test_state(db.clone());
        state.project = Arc::new(crate::bxapi::project::Project {
            import_hook: Some(gate.clone()),
            ..crate::bxapi::project::Project::default()
        });
        let c = Client::new(&state);
        let token = login(&c, "exch").await;
        upload(&c, &token, "g.xml", goods_xml(&["a"], false).as_bytes()).await;

        let req = Request::builder()
            .uri(format!("{BASE}&mode=import&filename=g.xml"))
            .header(header::COOKIE, format!("{}={token}", session::COOKIE))
            .body(Body::empty())
            .unwrap();
        let request = c.app.clone().oneshot(req);
        tokio::select! {
            _ = request => panic!("импорт не должен был закончиться до разрешения"),
            _ = gate.started.notified() => {}
        }
        // 1С оборвала соединение: future запроса уничтожен, импорт должен доработать
        gate.release.notify_one();
        let mut logged = None;
        for _ in 0..100 {
            logged = sqlx::query_scalar::<_, String>(
                "SELECT result FROM exchange_log WHERE mode = 'import'",
            )
            .fetch_optional(&db)
            .await
            .unwrap();
            if logged.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(logged.as_deref(), Some("success"));
        assert_eq!(active_items(&db).await, [("a".to_string(), true)]);
        let st: serde_json::Value = sqlx::query_scalar("SELECT state FROM exchange_sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(st["imported"], serde_json::json!(["g.xml"]));
    }

    #[sqlx::test]
    async fn concurrent_imports_do_not_duplicate(db: PgPool) {
        let user_id = exchange_user(&db, "exch").await;
        let state = test_state(db.clone());
        let ids: Vec<String> = (0..150).map(|i| format!("g-{i}")).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let goods = goods_xml(&refs, false);
        let offers: String = ids
            .iter()
            .map(|id| {
                format!(
                    "<Предложение><Ид>{id}</Ид><Цены><Цена><ИдТипаЦены>pt-1</ИдТипаЦены>\
                     <ЦенаЗаЕдиницу>10</ЦенаЗаЕдиницу><Валюта>RUB</Валюта></Цена></Цены></Предложение>"
                )
            })
            .collect();
        let prices = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<КоммерческаяИнформация xmlns="urn:1C.ru:commerceml_3" ВерсияСхемы="3.1"><ПакетПредложений СодержитТолькоИзменения="true"><ИдКаталога>cat-0001</ИдКаталога><ИдКлассификатора>cls-0001</ИдКлассификатора><Предложения>{offers}</Предложения></ПакетПредложений></КоммерческаяИнформация>"#
        );
        let mut sessions = Vec::new();
        for _ in 0..2 {
            let s = session_with_file(&state, user_id, "goods.xml", goods.as_bytes()).await;
            let dir = session::dir(&state.config.upload_dir, &s);
            std::fs::write(dir.join("prices.xml"), prices.as_bytes()).unwrap();
            sessions.push(s);
        }
        for file in ["goods.xml", "prices.xml"] {
            let (a, b) = tokio::join!(
                import_file(&state, &sessions[0], file, Limits::default(), None),
                import_file(&state, &sessions[1], file, Limits::default(), None),
            );
            a.unwrap();
            b.unwrap();
        }
        let dup_items: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM (SELECT xml_id FROM collection_items
                                   GROUP BY xml_id HAVING count(*) > 1) d",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(dup_items, 0);
        let (collections, items): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM collections), (SELECT count(*) FROM collection_items)",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!((collections, items), (1, 150));
        let dup_prices: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM (SELECT item_id, price_type_id FROM catalog_prices
                                   GROUP BY 1, 2 HAVING count(*) > 1) d",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(dup_prices, 0);
        let types: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_price_types")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(types, 1);
    }

    #[sqlx::test]
    async fn failed_import_blocks_deactivation(db: PgPool) {
        exchange_user(&db, "exch").await;
        Settings {
            element_action: Action::Deactivate,
            ..Settings::default()
        }
        .save(&db)
        .await
        .unwrap();
        let c = Client::new(&test_state(db.clone()));
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
        assert_eq!(c.get(&t, "mode=complete").await, "success");

        let note = "success\nдеактивация пропущена: были ошибки импорта";
        for finish in ["deactivate", "complete"] {
            let t = login(&c, "exch").await;
            upload(&c, &t, "g.xml", goods_xml(&["a"], true).as_bytes()).await;
            assert!(
                c.get(&t, "mode=import&filename=g.xml")
                    .await
                    .starts_with("success")
            );
            upload(&c, &t, "bad.xml", b"\x00 not xml").await;
            assert!(
                c.get(&t, "mode=import&filename=bad.xml")
                    .await
                    .starts_with("failure")
            );
            assert_eq!(c.get(&t, &format!("mode={finish}")).await, note);
            assert!(active_items(&db).await.iter().all(|i| i.1), "{finish}");
            let stats: serde_json::Value = sqlx::query_scalar(
                "SELECT stats FROM exchange_log WHERE mode = $1 ORDER BY id DESC LIMIT 1",
            )
            .bind(finish)
            .fetch_one(&db)
            .await
            .unwrap();
            assert!(stats["note"].as_str().unwrap().contains("пропущена"));
        }

        // ошибка снимается, когда тот же файл потом загрузился
        let t = login(&c, "exch").await;
        assert!(
            c.get(&t, "mode=import&filename=late.xml")
                .await
                .starts_with("failure")
        );
        upload(&c, &t, "late.xml", goods_xml(&["a"], true).as_bytes()).await;
        assert!(
            c.get(&t, "mode=import&filename=late.xml")
                .await
                .starts_with("success")
        );
        assert_eq!(c.get(&t, "mode=deactivate").await, "success");
        assert_eq!(
            active_items(&db).await,
            [("a".into(), true), ("b".into(), false), ("c".into(), false)]
        );
    }
}
