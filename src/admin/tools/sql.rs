//! SQL-консоль: выполнить произвольный запрос и увидеть результат.
//!
//! Запрос идёт в транзакции: без подтверждения — `READ ONLY`, с подтверждением —
//! обычной, с `COMMIT` (read-only откатывается). На транзакцию ставится
//! `statement_timeout = 15s`, вывод ограничен 200 строками и 256 КБ.
//!
//! Выполнение — через extended protocol (`sqlx::query`/`prepare`), поэтому сервер
//! сам отклоняет несколько команд в одном prepared statement. Отдельно запрещены
//! одиночные `SET/RESET/CALL/BEGIN/COMMIT/ROLLBACK/…`, иначе можно выйти из
//! READ ONLY или сбросить таймаут. Значения приходят в binary-формате и
//! форматируются по типу колонки.

use std::time::Instant;

use axum::{Extension, Form, extract::State, response::Html};
use futures_util::TryStreamExt;
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, Column, Executor, PgConnection, Row, SqlSafeStr, Statement};

use super::{guard, pg, render};
use crate::{access::Access, audit, error::AppResult, state::AppState};

/// Сколько строк результата показываем.
const MAX_ROWS: usize = 200;
/// Сколько байт значений показываем.
const MAX_BYTES: usize = 256 * 1024;
/// Сколько символов SQL попадает в журнал действий.
const SQL_PREVIEW_CHARS: usize = 500;

/// Поля формы SQL-консоли.
#[derive(Debug, Deserialize)]
pub struct SqlForm {
    #[serde(default)]
    sql: String,
    /// Чекбокс «Запрос может изменить данные» (`value="1"`).
    #[serde(default)]
    confirm: Option<String>,
}

/// Итог одного выполнения: либо набор строк, либо число затронутых строк, либо ошибка.
#[derive(Debug, Default, Serialize)]
pub struct SqlResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub affected: Option<u64>,
    pub error: Option<String>,
    pub duration_ms: u64,
    /// Вывод обрезан по числу строк или объёму.
    pub truncated: bool,
}

/// GET `/admin/tools/sql` — форма без результата.
pub async fn page(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    guard(&state, &user)?;
    render(&state, "tools/sql.html", context! { user })
}

/// POST `/admin/tools/sql` — выполнить запрос и показать результат/ошибку.
pub async fn execute(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(input): Form<SqlForm>,
) -> AppResult<Html<String>> {
    guard(&state, &user)?;
    let confirmed = input.confirm.as_deref() == Some("1");
    let started = Instant::now();
    let mut result = run(&state, &input.sql, confirmed).await;
    result.duration_ms = started.elapsed().as_millis() as u64;
    let summary = audit_summary(&input.sql, &result);
    audit::record(
        &state.db,
        user.user.id,
        &user.user.login,
        "sql",
        "tools",
        "",
        summary,
    )
    .await;
    render(
        &state,
        "tools/sql.html",
        context! {
            user,
            sql => input.sql,
            confirm => confirmed,
            result,
        },
    )
}

/// Текст ошибки до обращения к БД.
fn rejected(message: impl Into<String>) -> SqlResult {
    SqlResult {
        error: Some(message.into()),
        ..SqlResult::default()
    }
}

/// Выполняет SQL в транзакции с таймаутом и собирает [`SqlResult`].
///
/// Разрешён ровно один statement: extended protocol сам отклоняет несколько
/// команд в prepared statement, а одиночные опасные команды отсекает блоклист.
async fn run(state: &AppState, sql: &str, confirmed: bool) -> SqlResult {
    let statement = sql.trim();
    // Один хвостовой `;` — обычная привычка; больше не трогаем, иначе можно
    // склеить команды (`;;`).
    let statement = statement.strip_suffix(';').unwrap_or(statement).trim_end();
    if statement.is_empty() {
        return rejected("Пустой запрос");
    }
    if is_transaction_control(&first_keyword(statement)) {
        return rejected(
            "Управление транзакцией и настройками сессии запрещено: \
             SET/RESET/CALL/BEGIN/START/COMMIT/ROLLBACK/END.",
        );
    }
    let mut tx = match state.db.begin().await {
        Ok(tx) => tx,
        Err(error) => return rejected(pg::db_message(&error)),
    };
    if let Err(error) = setup(&mut tx, confirmed).await {
        let _ = tx.rollback().await;
        return rejected(pg::db_message(&error));
    }
    // Таймаут и режим чтения выставлены до пользовательского statement.
    let outcome = if is_query(statement) {
        fetch(&mut tx, statement).await
    } else {
        execute_statement(&mut tx, statement)
            .await
            .map(|affected| SqlResult {
                affected: Some(affected),
                ..SqlResult::default()
            })
    };
    let mut result = match outcome {
        Ok(result) => result,
        Err(error) => {
            let _ = tx.rollback().await;
            return rejected(pg::db_message(&error));
        }
    };
    if confirmed {
        if let Err(error) = tx.commit().await {
            result.error = Some(pg::db_message(&error));
        }
    } else if let Err(error) = tx.rollback().await {
        result.error = Some(pg::db_message(&error));
    }
    result
}

/// Настраивает транзакцию: режим чтения и таймаут запроса.
async fn setup(conn: &mut PgConnection, confirmed: bool) -> Result<(), sqlx::Error> {
    if !confirmed {
        sqlx::query("SET TRANSACTION READ ONLY")
            .execute(&mut *conn)
            .await?;
    }
    // 15 секунд на запрос: тяжёлый SELECT не должен держать пул.
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Запрос возвращает строки: `SELECT`, CTE, `SHOW`, `EXPLAIN`, `VALUES`, `TABLE`.
///
/// Подготовка (prepared statement) даёт колонки даже для пустого результата и
/// гарантирует, что сервер принял ровно одну команду.
async fn fetch(conn: &mut PgConnection, sql: &str) -> Result<SqlResult, sqlx::Error> {
    let prepared = (&mut *conn)
        .prepare(AssertSqlSafe(sql).into_sql_str())
        .await?;
    let mut result = SqlResult {
        columns: prepared
            .columns()
            .iter()
            .map(|column| column.name().to_string())
            .collect(),
        ..SqlResult::default()
    };
    let mut used = 0usize;
    let mut stream = prepared.query().fetch(&mut *conn);
    while let Some(row) = stream.try_next().await? {
        let values: Vec<String> = (0..row.len())
            .map(|index| pg::format_value(&row, index))
            .collect();
        let size: usize = values.iter().map(String::len).sum();
        if result.rows.len() >= MAX_ROWS || used + size > MAX_BYTES {
            result.truncated = true;
            break;
        }
        used += size;
        result.rows.push(values);
    }
    Ok(result)
}

/// Запрос не возвращает строк: `INSERT`/`UPDATE`/`DELETE`/DDL и прочее.
async fn execute_statement(conn: &mut PgConnection, sql: &str) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(AssertSqlSafe(sql)).execute(&mut *conn).await?;
    Ok(result.rows_affected())
}

/// Классификация: вернёт ли запрос строки.
fn is_query(sql: &str) -> bool {
    let head = first_keyword(sql);
    matches!(
        head.as_str(),
        "select" | "with" | "show" | "explain" | "values" | "table"
    )
}

/// Управление транзакцией и настройками сессии: запрещено, иначе можно выйти из
/// READ ONLY или сбросить `statement_timeout`. `CALL` умеет коммитить изнутри
/// процедуры, поэтому тоже запрещён.
fn is_transaction_control(keyword: &str) -> bool {
    matches!(
        keyword,
        "set" | "reset" | "call" | "begin" | "start" | "commit" | "rollback" | "end"
    )
}

/// Первое слово запроса без ведущих пробелов и комментариев (`--`, `/* */`).
fn first_keyword(sql: &str) -> String {
    skip_leading(sql)
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .next()
        .unwrap_or("")
        .to_lowercase()
}

/// Убирает ведущие пробелы и комментарии; блок-комментарии могут быть вложенными.
fn skip_leading(sql: &str) -> &str {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.find('\n').map_or("", |at| &after[at + 1..]);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = skip_block_comment(after);
        } else {
            return rest;
        }
    }
}

/// Хвост после закрывающего `*/` (с учётом вложенности) или `""`, если не закрыт.
fn skip_block_comment(after: &str) -> &str {
    let mut depth = 1;
    let mut chars = after.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        if ch == '/' && chars.peek().is_some_and(|(_, next)| *next == '*') {
            chars.next();
            depth += 1;
        } else if ch == '*' && chars.peek().is_some_and(|(_, next)| *next == '/') {
            chars.next();
            depth -= 1;
            if depth == 0 {
                return &after[at + 2..];
            }
        }
    }
    ""
}

/// Сводка для журнала: первые 500 символов SQL и статус.
fn audit_summary(sql: &str, result: &SqlResult) -> String {
    let head: String = sql.trim().chars().take(SQL_PREVIEW_CHARS).collect();
    match &result.error {
        Some(error) => {
            let error: String = error.chars().take(200).collect();
            format!("{head}: error: {error}")
        }
        None => {
            let count = result.affected.unwrap_or(result.rows.len() as u64);
            format!("{head}: ok, {count} строк")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{first_keyword, is_query, is_transaction_control};

    use crate::admin::tools::pg;

    use sqlx::PgPool;

    use crate::{
        smoke_tests::{get, post_form},
        test_support::{admin_cookie, permitted_cookie, test_state},
    };

    const PATH: &str = "/admin/tools/sql";

    /// Сводки всех записей журнала действий с `action = 'sql'`.
    async fn audit_summaries(db: &PgPool) -> Vec<String> {
        sqlx::query_scalar("SELECT summary FROM audit_log WHERE action = 'sql' ORDER BY id")
            .fetch_all(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn select_shows_rows_and_writes_audit(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(&app, PATH, &cookie, "sql=SELECT+1+AS+one").await;

        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("<th>one</th>"),
            "нет колонки результата: {body}"
        );
        assert!(body.contains(">1<"), "нет значения результата: {body}");

        let summaries = audit_summaries(&db).await;
        assert_eq!(summaries.len(), 1, "{summaries:?}");
        assert!(summaries[0].contains("SELECT 1 AS one"), "{summaries:?}");
        assert!(summaries[0].contains("ok"), "{summaries:?}");
        assert!(summaries[0].contains('1'), "{summaries:?}");
    }

    #[sqlx::test]
    async fn database_error_is_shown_and_audited(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(&app, PATH, &cookie, "sql=SELEC+1").await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("Ошибка"), "нет блока ошибки: {body}");
        assert!(body.contains("SELEC 1"), "SQL не сохранён в форме: {body}");

        let summaries = audit_summaries(&db).await;
        assert_eq!(summaries.len(), 1, "{summaries:?}");
        assert!(summaries[0].contains("error"), "{summaries:?}");
    }

    #[sqlx::test]
    async fn unconfirmed_changes_run_read_only(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(
            &app,
            PATH,
            &cookie,
            "sql=INSERT+INTO+audit_log+(action,+entity)+VALUES+('probe','probe')",
        )
        .await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("Ошибка"), "нет блока ошибки: {body}");
        let probes: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'probe'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(probes, 0, "запрос без подтверждения изменил данные");
        let summaries = audit_summaries(&db).await;
        assert_eq!(summaries.len(), 1, "{summaries:?}");
        assert!(summaries[0].contains("error"), "{summaries:?}");
    }

    #[sqlx::test]
    async fn confirmed_changes_are_committed(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(
            &app,
            PATH,
            &cookie,
            "sql=CREATE+TABLE+sql_console_probe+(id+int)&confirm=1",
        )
        .await;

        assert_eq!(status, 200, "{body}");
        assert!(!body.contains("Ошибка"), "неожиданная ошибка: {body}");
        let table: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('sql_console_probe')::text")
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(table.is_some(), "подтверждённый DDL не закоммичен");
    }

    #[sqlx::test]
    async fn values_are_formatted(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let sql = "SELECT NULL::int AS n, TRUE AS b, 1.5::numeric AS num, \
                   -0.25::numeric AS neg, 12345.6789::numeric AS big, 'abc'::bytea AS bytes, \
                   '0f8fad5b-d9cb-469f-a165-70867728950e'::uuid AS id, \
                   '{\"a\": 1}'::jsonb AS doc, '2026-01-02'::date AS day, \
                   '2026-01-02 03:04:05+00'::timestamptz AS ts";

        let (status, body) = post_form(&app, PATH, &cookie, &sql_body(sql)).await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains(">NULL<"), "NULL: {body}");
        assert!(body.contains(">true<"), "bool: {body}");
        assert!(body.contains(">1.5<"), "numeric: {body}");
        assert!(body.contains(">-0.25<"), "отрицательный numeric: {body}");
        assert!(body.contains(">12345.6789<"), "дробный numeric: {body}");
        assert!(body.contains(">616263<"), "bytea: {body}");
        assert!(
            body.contains(">0f8fad5b-d9cb-469f-a165-70867728950e<"),
            "uuid: {body}"
        );
        assert!(
            body.contains("<td class=\"mono\">{&quot;a&quot;:1}</td>"),
            "jsonb: {body}"
        );
        assert!(body.contains(">2026-01-02<"), "date: {body}");
        assert!(
            body.contains(">2026-01-02 03:04:05 UTC<"),
            "timestamptz: {body}"
        );
    }

    #[sqlx::test]
    async fn page_warns_that_data_is_not_masked(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, PATH, &cookie).await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("Данные не маскируются"), "{body}");
    }

    #[sqlx::test]
    async fn multiple_statements_are_rejected(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(
            &app,
            PATH,
            &cookie,
            "sql=SELECT+1;+INSERT+INTO+audit_log+(action,+entity)+VALUES+('multi','probe')",
        )
        .await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("по одному"), "нет ошибки о запросах: {body}");
        let probes: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'multi'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(probes, 0, "многооператорный запрос выполнился");
    }

    #[sqlx::test]
    async fn statements_cannot_escape_read_only(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(
            &app,
            PATH,
            &cookie,
            "sql=SELECT+1;+COMMIT;+INSERT+INTO+audit_log+(action,+entity)+VALUES+('escaped','probe')",
        )
        .await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("по одному"), "нет ошибки о запросах: {body}");
        let escaped: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'escaped'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(escaped, 0, "COMMIT внутри запроса вышел из READ ONLY");
    }

    #[sqlx::test]
    async fn session_settings_cannot_reset_timeout(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(
            &app,
            PATH,
            &cookie,
            "sql=SELECT+1;+SET+statement_timeout=0;+SELECT+pg_sleep(0.05)",
        )
        .await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("по одному"), "нет ошибки о запросах: {body}");
    }

    #[sqlx::test]
    async fn single_transaction_control_is_rejected(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(&app, PATH, &cookie, "sql=SET+statement_timeout=0").await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("Ошибка"), "нет блока ошибки: {body}");
        assert!(body.contains("запрещено"), "нет причины отказа: {body}");
    }

    #[sqlx::test]
    async fn empty_confirmation_is_not_confirmation(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(
            &app,
            PATH,
            &cookie,
            "sql=INSERT+INTO+audit_log+(action,+entity)+VALUES+('empty','probe')&confirm=",
        )
        .await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("Ошибка"), "нет блока ошибки: {body}");
        let probes: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'empty'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(probes, 0, "пустой confirm считается подтверждением");
    }

    #[sqlx::test]
    async fn statement_timeout_is_set_before_user_sql(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(&app, PATH, &cookie, "sql=SHOW+statement_timeout").await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains(">15s<"), "таймаут не применён: {body}");
    }

    #[sqlx::test]
    async fn confirmed_single_update_commits(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO audit_log (action, entity, summary) VALUES ('fixture', 'probe', 'old') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();

        let (status, body) = post_form(
            &app,
            PATH,
            &cookie,
            &format!("sql=UPDATE+audit_log+SET+summary='new'+WHERE+id={id}&confirm=1"),
        )
        .await;

        assert_eq!(status, 200, "{body}");
        assert!(!body.contains("Ошибка"), "неожиданная ошибка: {body}");
        let summary: String = sqlx::query_scalar("SELECT summary FROM audit_log WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(summary, "new");
    }

    /// Тело формы POST для произвольного SQL.
    fn sql_body(sql: &str) -> String {
        let encoded = sql
            .replace('%', "%25")
            .replace('&', "%26")
            .replace('+', "%2B")
            .replace(' ', "+");
        format!("sql={encoded}")
    }

    #[sqlx::test]
    async fn e_string_quote_cannot_smuggle_commands(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let sql = "SELECT E'\\''; COMMIT; INSERT INTO audit_log (action, entity) VALUES ('esmuggle','probe')";

        let (status, body) = post_form(&app, PATH, &cookie, &sql_body(sql)).await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("по одному"), "нет отказа: {body}");
        let probes: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'esmuggle'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(probes, 0, "E-string обошёл проверку");
    }

    #[sqlx::test]
    async fn dollar_like_identifier_cannot_smuggle_commands(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let sql = "SELECT 1 AS foo$bar$; COMMIT; INSERT INTO audit_log (action, entity) VALUES ('dsmuggle','probe')";

        let (status, body) = post_form(&app, PATH, &cookie, &sql_body(sql)).await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("по одному"), "нет отказа: {body}");
        let probes: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'dsmuggle'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(probes, 0, "dollar-подобный идентификатор обошёл проверку");
    }

    #[sqlx::test]
    async fn valid_e_string_works(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) =
            post_form(&app, PATH, &cookie, &sql_body("SELECT E'a\\';b' AS v")).await;

        assert_eq!(status, 200, "{body}");
        assert!(
            !body.contains("Ошибка"),
            "валидный E-string отклонён: {body}"
        );
        assert!(body.contains("a&#x27;;b"), "нет значения E-string: {body}");
    }

    #[sqlx::test]
    async fn trailing_semicolon_works(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(&app, PATH, &cookie, &sql_body("SELECT 5 AS five;")).await;

        assert_eq!(status, 200, "{body}");
        assert!(
            !body.contains("Ошибка"),
            "точка с запятой сломала запрос: {body}"
        );
        assert!(body.contains(">5<"), "нет результата: {body}");
    }

    #[sqlx::test]
    async fn empty_result_keeps_columns(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(
            &app,
            PATH,
            &cookie,
            &sql_body("SELECT 1 AS one WHERE false"),
        )
        .await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("<th>one</th>"), "нет колонки: {body}");
        assert!(body.contains("Строк: 0"), "нет счётчика строк: {body}");
    }

    #[sqlx::test]
    async fn non_super_cannot_execute(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = permitted_cookie(&db, "op", &[]).await;

        let (status, _) = post_form(&app, PATH, &cookie, "sql=SELECT+1").await;

        assert_eq!(status, 403);
    }

    #[test]
    fn query_classification() {
        for sql in [
            "SELECT 1",
            "  select * from users",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "SHOW server_version",
            "EXPLAIN SELECT 1",
            "VALUES (1), (2)",
            "TABLE users",
            "-- комментарий\nSELECT 1",
            "/* комментарий */ SELECT 1",
        ] {
            assert!(is_query(sql), "{sql}");
        }
        for sql in [
            "INSERT INTO users DEFAULT VALUES",
            "UPDATE users SET login = 'x'",
            "DELETE FROM users",
            "CREATE TABLE t (id int)",
            "  set statement_timeout = '1s'",
        ] {
            assert!(!is_query(sql), "{sql}");
        }
        assert_eq!(first_keyword("  /* a */ -- b\n SeLeCt 1"), "select");
    }

    #[test]
    fn truncation_marks_the_tail() {
        assert_eq!(pg::truncate("abc", 3), "abc");
        assert_eq!(pg::truncate("abcd", 3), "abc…");
    }

    #[test]
    fn transaction_control_is_blocked() {
        for keyword in [
            "set", "reset", "call", "begin", "start", "commit", "rollback", "end",
        ] {
            assert!(is_transaction_control(keyword), "{keyword}");
        }
        for keyword in ["select", "with", "insert", "show", "explain"] {
            assert!(!is_transaction_control(keyword), "{keyword}");
        }
    }
}
