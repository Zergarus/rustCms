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
/// Разрешён ровно один statement: `raw_sql` шлёт весь текст одним simple Query,
/// поэтому второй оператор мог бы выйти из READ ONLY или сбросить таймаут.
async fn run(state: &AppState, sql: &str, confirmed: bool) -> SqlResult {
    let statements = split_statements(sql);
    if statements.is_empty() {
        return rejected("Пустой запрос");
    }
    if statements.len() > 1 {
        return rejected(format!(
            "Выполняйте по одному запросу (получено {}).",
            statements.len()
        ));
    }
    let statement = &statements[0];
    if is_transaction_control(&first_keyword(statement)) {
        return rejected(
            "Управление транзакцией и настройками сессии запрещено: \
             SET/RESET/BEGIN/START/COMMIT/ROLLBACK/END.",
        );
    }
    let mut tx = match state.db.begin().await {
        Ok(tx) => tx,
        Err(error) => return rejected(db_message(&error)),
    };
    if let Err(error) = setup(&mut tx, confirmed).await {
        let _ = tx.rollback().await;
        return rejected(db_message(&error));
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
            return rejected(db_message(&error));
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

/// Управление транзакцией и настройками сессии: запрещено, иначе можно выйти из
/// READ ONLY или сбросить `statement_timeout`.
fn is_transaction_control(keyword: &str) -> bool {
    matches!(
        keyword,
        "set" | "reset" | "begin" | "start" | "commit" | "rollback" | "end"
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

/// Делит текст на top-level statements по `;`, не трогая кавычки, dollar-quoting
/// и комментарии. Postgres понимает вложенные `/* */`, поэтому глубина считается.
fn split_statements(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '\'' | '"' => {
                let quote = chars[index];
                current.push(quote);
                index += 1;
                while index < chars.len() {
                    let ch = chars[index];
                    current.push(ch);
                    index += 1;
                    if ch == quote {
                        // Удвоенная кавычка — экранирование внутри литерала.
                        if index < chars.len() && chars[index] == quote {
                            current.push(quote);
                            index += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            '-' if chars.get(index + 1) == Some(&'-') => {
                while index < chars.len() {
                    let ch = chars[index];
                    current.push(ch);
                    index += 1;
                    if ch == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.get(index + 1) == Some(&'*') => {
                current.push('/');
                current.push('*');
                index += 2;
                let mut depth = 1;
                while index < chars.len() && depth > 0 {
                    if chars[index] == '/' && chars.get(index + 1) == Some(&'*') {
                        current.push('/');
                        current.push('*');
                        index += 2;
                        depth += 1;
                    } else if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
                        current.push('*');
                        current.push('/');
                        index += 2;
                        depth -= 1;
                    } else {
                        current.push(chars[index]);
                        index += 1;
                    }
                }
            }
            '$' => {
                // Dollar-quoting: `$$...$$` или `$tag$...$tag$`.
                let mut end = index + 1;
                while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
                    end += 1;
                }
                if chars.get(end) == Some(&'$') {
                    let delimiter: String = chars[index..=end].iter().collect();
                    let delimiter_chars: Vec<char> = delimiter.chars().collect();
                    current.push_str(&delimiter);
                    index = end + 1;
                    while index < chars.len() {
                        if chars[index..].starts_with(&delimiter_chars) {
                            current.push_str(&delimiter);
                            index += delimiter_chars.len();
                            break;
                        }
                        current.push(chars[index]);
                        index += 1;
                    }
                } else {
                    current.push('$');
                    index += 1;
                }
            }
            ';' => {
                if !skip_leading(&current).is_empty() {
                    statements.push(current.trim().to_string());
                }
                current.clear();
                index += 1;
            }
            ch => {
                current.push(ch);
                index += 1;
            }
        }
    }
    if !skip_leading(&current).is_empty() {
        statements.push(current.trim().to_string());
    }
    statements
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
    use super::{first_keyword, is_query, is_transaction_control, split_statements, truncate};

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

    #[test]
    fn splitting_respects_quoting_and_comments() {
        let cases = [
            ("SELECT ';'", vec!["SELECT ';'"]),
            ("SELECT 'it''s; fine'", vec!["SELECT 'it''s; fine'"]),
            ("SELECT \"a;b\"", vec!["SELECT \"a;b\""]),
            ("SELECT \"a\"\"b;c\"", vec!["SELECT \"a\"\"b;c\""]),
            ("SELECT $$a;b$$", vec!["SELECT $$a;b$$"]),
            ("SELECT $tag$ a;b $tag$", vec!["SELECT $tag$ a;b $tag$"]),
            (
                "SELECT /* ; /* nested ; */ */ 1",
                vec!["SELECT /* ; /* nested ; */ */ 1"],
            ),
            (
                "SELECT 1 -- ;\n; UPDATE t SET x = 1",
                vec!["SELECT 1 -- ;", "UPDATE t SET x = 1"],
            ),
            ("SELECT 1; ; -- tail", vec!["SELECT 1"]),
            ("  ", vec![]),
        ];
        for (sql, expected) in cases {
            assert_eq!(split_statements(sql), expected, "{sql}");
        }
    }

    #[test]
    fn transaction_control_is_blocked() {
        for keyword in [
            "set", "reset", "begin", "start", "commit", "rollback", "end",
        ] {
            assert!(is_transaction_control(keyword), "{keyword}");
        }
        for keyword in ["select", "with", "insert", "show", "explain"] {
            assert!(!is_transaction_control(keyword), "{keyword}");
        }
    }
}
