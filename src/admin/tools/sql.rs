//! SQL-консоль: выполнить произвольный запрос и увидеть результат.
//!
//! Запрос идёт в транзакции: без подтверждения — `READ ONLY`, с подтверждением —
//! обычной, с `COMMIT` (read-only откатывается). На транзакцию ставится
//! `statement_timeout = 15s`, вывод ограничен 200 строками и 256 КБ.
//! Форматирование значений — по текстовому представлению Postgres (simple query
//! protocol), поэтому отдельный модуль форматирования не нужен.

use std::time::Instant;

use axum::{Extension, Form, extract::State, response::Html};
use futures_util::TryStreamExt;
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, Column, PgConnection, Row, TypeInfo, ValueRef, postgres::PgRow};

use super::{guard, render};
use crate::{access::Access, audit, error::AppResult, state::AppState};

/// Сколько строк результата показываем.
const MAX_ROWS: usize = 200;
/// Сколько байт значений показываем.
const MAX_BYTES: usize = 256 * 1024;
/// Предохранитель от одного гигантского значения (1 ГБ в поле Postgres).
const MAX_CELL_CHARS: usize = 8192;
/// Сколько байт `bytea` показываем в hex.
const MAX_BYTEA_HEX_CHARS: usize = 128;
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
    let started = Instant::now();
    let mut result = run(&state, &input.sql, input.confirm.is_some()).await;
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
            confirm => input.confirm.is_some(),
            result,
        },
    )
}

/// Выполняет SQL в транзакции с таймаутом и собирает [`SqlResult`].
async fn run(state: &AppState, sql: &str, confirmed: bool) -> SqlResult {
    if sql.trim().is_empty() {
        return SqlResult {
            error: Some("Пустой запрос".into()),
            ..SqlResult::default()
        };
    }
    let mut tx = match state.db.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            return SqlResult {
                error: Some(db_message(&error)),
                ..SqlResult::default()
            };
        }
    };
    if let Err(error) = setup(&mut tx, confirmed).await {
        let _ = tx.rollback().await;
        return SqlResult {
            error: Some(db_message(&error)),
            ..SqlResult::default()
        };
    }
    let outcome = if is_query(sql) {
        fetch(&mut tx, sql).await
    } else {
        execute_statement(&mut tx, sql)
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
            return SqlResult {
                error: Some(db_message(&error)),
                ..SqlResult::default()
            };
        }
    };
    if confirmed {
        if let Err(error) = tx.commit().await {
            result.error = Some(db_message(&error));
        }
    } else if let Err(error) = tx.rollback().await {
        result.error = Some(db_message(&error));
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
async fn fetch(conn: &mut PgConnection, sql: &str) -> Result<SqlResult, sqlx::Error> {
    let mut result = SqlResult::default();
    let mut used = 0usize;
    let mut stream = sqlx::raw_sql(AssertSqlSafe(sql)).fetch(&mut *conn);
    while let Some(row) = stream.try_next().await? {
        if result.columns.is_empty() {
            result.columns = row
                .columns()
                .iter()
                .map(|column| column.name().to_string())
                .collect();
        }
        let values: Vec<String> = (0..row.len())
            .map(|index| format_value(&row, index))
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
    let result = sqlx::raw_sql(AssertSqlSafe(sql))
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected())
}

/// Текстовое представление значения ячейки (Postgres отдаёт его в text-формате).
pub(crate) fn format_value(row: &PgRow, index: usize) -> String {
    let Ok(raw) = row.try_get_raw(index) else {
        return "…".into();
    };
    if raw.is_null() {
        return "NULL".into();
    }
    let type_name = row.column(index).type_info().name();
    match raw.as_str() {
        Ok(text) => truncate(&format_text(type_name, text), MAX_CELL_CHARS),
        Err(_) => "…".into(),
    }
}

/// Приведение текста Postgres к человекочитаемому виду для известных типов.
fn format_text(type_name: &str, text: &str) -> String {
    match type_name {
        "BOOL" => match text {
            "t" => "true".into(),
            "f" => "false".into(),
            _ => text.into(),
        },
        // Postgres печатает bytea как `\x...`; показываем сам hex.
        "BYTEA" => {
            let hex = text.strip_prefix("\\x").unwrap_or(text);
            if hex.len() > MAX_BYTEA_HEX_CHARS {
                format!("{}…", &hex[..MAX_BYTEA_HEX_CHARS])
            } else {
                hex.to_string()
            }
        }
        _ => text.to_string(),
    }
}

/// Обрезает строку по числу символов, помечая хвост многоточием.
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut short: String = text.chars().take(max_chars).collect();
    short.push('…');
    short
}

/// Классификация: вернёт ли запрос строки.
fn is_query(sql: &str) -> bool {
    let head = first_keyword(sql);
    matches!(
        head.as_str(),
        "select" | "with" | "show" | "explain" | "values" | "table"
    )
}

/// Первое слово запроса без ведущих пробелов и комментариев (`--`, `/* */`).
fn first_keyword(sql: &str) -> String {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        } else {
            break;
        }
    }
    rest.split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .next()
        .unwrap_or("")
        .to_lowercase()
}

/// Текст ошибки Postgres без обвязки sqlx.
fn db_message(error: &sqlx::Error) -> String {
    match error {
        sqlx::Error::Database(database) => database.message().to_string(),
        other => other.to_string(),
    }
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
    use super::{first_keyword, is_query, truncate};

    use sqlx::PgPool;

    use crate::{
        smoke_tests::post_form,
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

        let (status, body) = post_form(
            &app,
            PATH,
            &cookie,
            "sql=SELECT+NULL::int+AS+n,+TRUE+AS+b,+1.5::numeric+AS+num,\
             +'abc'::bytea+AS+bytes,+'0f8fad5b-d9cb-469f-a165-70867728950e'::uuid+AS+id,\
             +'{\"a\": 1}'::jsonb+AS+doc,+'2026-01-02'::date+AS+day",
        )
        .await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains(">NULL<"), "NULL: {body}");
        assert!(body.contains(">true<"), "bool: {body}");
        assert!(body.contains(">1.5<"), "numeric: {body}");
        assert!(body.contains(">616263<"), "bytea: {body}");
        assert!(
            body.contains(">0f8fad5b-d9cb-469f-a165-70867728950e<"),
            "uuid: {body}"
        );
        assert!(body.contains("&quot;a&quot;: 1"), "jsonb: {body}");
        assert!(body.contains(">2026-01-02<"), "date: {body}");
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
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "abc…");
    }
}
