//! Каталог PostgreSQL и чтение таблиц для обозревателя.
//!
//! Схемы, таблицы и колонки берутся из системного каталога, а не из пользовательского
//! ввода. Наружу идентификаторы попадают только после [`quote_ident`], параметры
//! страницы — bind-параметрами. Значения приходят в binary-формате, поэтому
//! форматируются по типу колонки; этим же форматтером пользуется SQL-консоль.

use serde::Serialize;
use sqlx::{AssertSqlSafe, Column, PgPool, Row, TypeInfo, ValueRef, postgres::PgRow};

use crate::error::AppResult;

/// Предохранитель от одного гигантского значения (1 ГБ в поле Postgres).
const MAX_CELL_CHARS: usize = 8192;
/// Сколько байт `bytea` показываем в hex.
const MAX_BYTEA_HEX_CHARS: usize = 128;
/// Таймаут точного `count(*)`: тяжёлый счёт не должен держать страницу.
const COUNT_TIMEOUT: &str = "5s";
/// Таймаут выборки страницы данных.
const ROWS_TIMEOUT: &str = "15s";

/// Таблица или представление пользовательской схемы.
#[derive(Debug, Clone, Serialize)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    /// `relkind`: `r`/`p` — таблица, `v` — вью, `m` — материализованное вью.
    pub kind: String,
    /// Оценка числа строк (`pg_class.reltuples`), не точный `count(*)`.
    pub rows: i64,
    pub size: String,
    pub columns: i64,
    pub has_pk: bool,
}

/// Колонка таблицы: тип, NULL, default и признак первичного ключа.
#[derive(Debug, Clone, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub pk: bool,
}

/// Одна страница данных таблицы.
#[derive(Debug, Default, Serialize)]
pub struct RowsPage {
    pub rows: Vec<Vec<String>>,
    /// Точное число строк; `None`, если `count(*)` не уложился в таймаут.
    pub total: Option<i64>,
    pub count_error: Option<String>,
}

/// Экранирует идентификатор PostgreSQL двойными кавычками.
pub(crate) fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Пользовательские таблицы и представления с оценкой строк, размером и PK.
pub(crate) async fn tables(db: &PgPool) -> AppResult<Vec<TableInfo>> {
    let rows: Vec<TableRow> = sqlx::query_as(
        "SELECT n.nspname AS schema,
                c.relname AS name,
                c.relkind::text AS kind,
                GREATEST(c.reltuples, 0)::bigint AS rows,
                pg_size_pretty(pg_total_relation_size(c.oid)) AS size,
                (SELECT count(*)
                   FROM pg_attribute a
                  WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped) AS columns,
                EXISTS (SELECT 1
                          FROM pg_index i
                         WHERE i.indrelid = c.oid AND i.indisprimary) AS has_pk
           FROM pg_class c
           JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE c.relkind IN ('r', 'p', 'v', 'm')
            AND n.nspname NOT IN ('pg_catalog', 'information_schema')
            AND n.nspname !~ '^pg_(toast|temp)'
          ORDER BY n.nspname, c.relname",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| TableInfo {
            schema: row.schema,
            name: row.name,
            kind: row.kind,
            rows: row.rows,
            size: row.size,
            columns: row.columns,
            has_pk: row.has_pk,
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct TableRow {
    schema: String,
    name: String,
    kind: String,
    rows: i64,
    size: String,
    columns: i64,
    has_pk: bool,
}

/// Колонки существующей таблицы в порядке объявления.
pub(crate) async fn columns(db: &PgPool, schema: &str, table: &str) -> AppResult<Vec<ColumnInfo>> {
    let rows: Vec<ColumnRow> = sqlx::query_as(
        "SELECT a.attname AS name,
                format_type(a.atttypid, a.atttypmod) AS data_type,
                NOT a.attnotnull AS nullable,
                pg_get_expr(d.adbin, d.adrelid) AS \"default\",
                EXISTS (SELECT 1
                          FROM pg_index i
                         WHERE i.indrelid = a.attrelid
                           AND i.indisprimary
                           AND a.attnum = ANY (i.indkey)) AS pk
           FROM pg_attribute a
           JOIN pg_class c ON c.oid = a.attrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
           LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
          WHERE n.nspname = $1 AND c.relname = $2
            AND a.attnum > 0 AND NOT a.attisdropped
          ORDER BY a.attnum",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ColumnInfo {
            name: row.name,
            data_type: row.data_type,
            nullable: row.nullable,
            default: row.default,
            pk: row.pk,
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct ColumnRow {
    name: String,
    data_type: String,
    nullable: bool,
    default: Option<String>,
    pk: bool,
}

/// Страница данных: до `per` строк (не больше 200), точный `count(*)` с таймаутом.
///
/// `sort` — только имя существующей колонки и направление `asc`/`desc`; неизвестная
/// колонка молча отбрасывается, чтобы в SQL не попали пользовательские данные.
pub(crate) async fn rows(
    db: &PgPool,
    schema: &str,
    table: &str,
    page: i64,
    per: i64,
    sort: Option<(&str, &str)>,
) -> AppResult<RowsPage> {
    let per = per.clamp(1, 200);
    let page = page.max(1);
    let columns = columns(db, schema, table).await?;
    let sort = sort.and_then(|(column, dir)| {
        let known = columns.iter().any(|known| known.name == column);
        let dir = if dir.eq_ignore_ascii_case("desc") {
            "DESC"
        } else {
            "ASC"
        };
        known.then_some((quote_ident(column), dir))
    });

    let qualified = format!("{}.{}", quote_ident(schema), quote_ident(table));
    let (total, count_error) = count(db, &qualified).await;

    let mut sql = format!("SELECT * FROM {qualified}");
    if let Some((column, dir)) = &sort {
        sql.push_str(&format!(" ORDER BY {column} {dir}"));
    }
    sql.push_str(" LIMIT $1 OFFSET $2");

    let mut tx = db.begin().await?;
    sqlx::query(AssertSqlSafe(format!(
        "SET LOCAL statement_timeout = '{ROWS_TIMEOUT}'"
    )))
    .execute(&mut *tx)
    .await?;
    let fetched = sqlx::query(AssertSqlSafe(sql))
        .bind(per)
        .bind(page.saturating_sub(1).saturating_mul(per))
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;

    let rows = fetched
        .iter()
        .map(|row| {
            (0..row.len())
                .map(|index| format_value(row, index))
                .collect()
        })
        .collect();
    Ok(RowsPage {
        rows,
        total,
        count_error,
    })
}

/// Точный `count(*)` на отдельном соединении с коротким таймаутом.
///
/// Ошибка счёта (например, таймаут) не ломает страницу: данные всё равно покажем,
/// а вместо числа выведем причину.
async fn count(db: &PgPool, qualified: &str) -> (Option<i64>, Option<String>) {
    let sql = format!("SELECT count(*) FROM {qualified}");
    let mut tx = match db.begin().await {
        Ok(tx) => tx,
        Err(error) => return (None, Some(db_message(&error))),
    };
    if let Err(error) = sqlx::query(AssertSqlSafe(format!(
        "SET LOCAL statement_timeout = '{COUNT_TIMEOUT}'"
    )))
    .execute(&mut *tx)
    .await
    {
        let _ = tx.rollback().await;
        return (None, Some(db_message(&error)));
    }
    match sqlx::query_scalar::<_, i64>(AssertSqlSafe(sql))
        .fetch_one(&mut *tx)
        .await
    {
        Ok(total) => {
            let _ = tx.commit().await;
            (Some(total), None)
        }
        Err(error) => {
            let _ = tx.rollback().await;
            (None, Some(db_message(&error)))
        }
    }
}

/// Представление значения ячейки: значения приходят в binary-формате, поэтому
/// тип колонки важен.
pub(crate) fn format_value(row: &PgRow, index: usize) -> String {
    let Ok(raw) = row.try_get_raw(index) else {
        return "…".into();
    };
    if raw.is_null() {
        return "NULL".into();
    }
    let type_name = row.column(index).type_info().name();
    let formatted: Option<String> = match type_name {
        "BOOL" => row
            .try_get::<bool, _>(index)
            .ok()
            .map(|value| value.to_string()),
        "INT2" => row
            .try_get::<i16, _>(index)
            .ok()
            .map(|value| value.to_string()),
        "INT4" => row
            .try_get::<i32, _>(index)
            .ok()
            .map(|value| value.to_string()),
        "INT8" => row
            .try_get::<i64, _>(index)
            .ok()
            .map(|value| value.to_string()),
        "FLOAT4" => row
            .try_get::<f32, _>(index)
            .ok()
            .map(|value| value.to_string()),
        "FLOAT8" => row
            .try_get::<f64, _>(index)
            .ok()
            .map(|value| value.to_string()),
        "NUMERIC" => row
            .try_get_raw(index)
            .ok()
            .and_then(|raw| raw.as_bytes().ok())
            .and_then(format_numeric),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "UNKNOWN" | "CITEXT" => {
            row.try_get::<String, _>(index).ok()
        }
        "JSON" | "JSONB" => row
            .try_get::<serde_json::Value, _>(index)
            .ok()
            .map(|value| value.to_string()),
        "DATE" => row
            .try_get::<chrono::NaiveDate, _>(index)
            .ok()
            .map(|value| value.to_string()),
        "TIME" => row
            .try_get::<chrono::NaiveTime, _>(index)
            .ok()
            .map(|value| value.to_string()),
        "TIMESTAMP" => row
            .try_get::<chrono::NaiveDateTime, _>(index)
            .ok()
            .map(|value| value.to_string()),
        "TIMESTAMPTZ" => row
            .try_get::<chrono::DateTime<chrono::Utc>, _>(index)
            .ok()
            .map(|value| value.to_string()),
        "UUID" => row
            .try_get_raw(index)
            .ok()
            .and_then(|raw| raw.as_bytes().ok())
            .and_then(|bytes| uuid::Uuid::from_slice(bytes).ok())
            .map(|id| id.to_string()),
        "BYTEA" => row
            .try_get::<Vec<u8>, _>(index)
            .ok()
            .map(|bytes| format_bytea(&bytes)),
        _ => None,
    };
    truncate(&formatted.unwrap_or_else(|| "…".into()), MAX_CELL_CHARS)
}

/// `bytea` — hex, обрезанный по [`MAX_BYTEA_HEX_CHARS`].
fn format_bytea(bytes: &[u8]) -> String {
    let mut hex = hex::encode(bytes);
    if hex.len() > MAX_BYTEA_HEX_CHARS {
        hex.truncate(MAX_BYTEA_HEX_CHARS);
        hex.push('…');
    }
    hex
}

/// `NUMERIC` из binary-формата Postgres: знак, база-10000 цифры, вес и scale.
fn format_numeric(bytes: &[u8]) -> Option<String> {
    const SIGN_NEG: u16 = 0x4000;
    const SIGN_NAN: u16 = 0xC000;
    const SIGN_PINF: u16 = 0xD000;
    const SIGN_NINF: u16 = 0xF000;
    if bytes.len() < 8 {
        return None;
    }
    let digits_count = i16::from_be_bytes([bytes[0], bytes[1]]);
    let weight = i16::from_be_bytes([bytes[2], bytes[3]]);
    let sign = u16::from_be_bytes([bytes[4], bytes[5]]);
    let scale = i16::from_be_bytes([bytes[6], bytes[7]]);
    match sign {
        SIGN_NAN => return Some("NaN".into()),
        SIGN_PINF => return Some("Infinity".into()),
        SIGN_NINF => return Some("-Infinity".into()),
        _ => {}
    }
    if digits_count < 0 || scale < 0 {
        return None;
    }
    let digits_count = digits_count as usize;
    if bytes.len() < 8 + digits_count * 2 {
        return None;
    }
    let digits: Vec<u16> = bytes[8..8 + digits_count * 2]
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    let mut out = String::new();
    if sign == SIGN_NEG {
        out.push('-');
    }
    let mut started = false;
    if weight >= 0 {
        for exponent in (0..=weight).rev() {
            let group = digits
                .get((weight - exponent) as usize)
                .copied()
                .unwrap_or(0);
            if !started {
                if group == 0 {
                    continue;
                }
                out.push_str(&group.to_string());
                started = true;
            } else {
                out.push_str(&format!("{group:04}"));
            }
        }
    }
    if !started {
        out.push('0');
    }
    let scale = scale as usize;
    if scale > 0 {
        let mut fraction = String::with_capacity(scale.div_ceil(4) * 4);
        for offset in 1..=scale.div_ceil(4) {
            let position = weight as i64 + offset as i64;
            let group = if position >= 0 {
                digits.get(position as usize).copied().unwrap_or(0)
            } else {
                0
            };
            fraction.push_str(&format!("{group:04}"));
        }
        fraction.truncate(scale);
        out.push('.');
        out.push_str(&fraction);
    }
    Some(out)
}

/// Обрезает строку по числу символов, помечая хвост многоточием.
pub(crate) fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut short: String = text.chars().take(max_chars).collect();
    short.push('…');
    short
}

/// Текст ошибки Postgres без обвязки sqlx.
pub(crate) fn db_message(error: &sqlx::Error) -> String {
    let message = match error {
        sqlx::Error::Database(database) => database.message().to_string(),
        other => other.to_string(),
    };
    // Extended protocol отклоняет несколько команд до выполнения: показываем
    // понятную подсказку вместо текста драйвера.
    if message.contains("cannot insert multiple commands into a prepared statement") {
        return "Выполняйте по одному запросу.".into();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::quote_ident;

    #[test]
    fn identifiers_are_quoted_and_escaped() {
        assert_eq!(quote_ident("users"), "\"users\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(
            quote_ident("x; DROP TABLE users"),
            "\"x; DROP TABLE users\""
        );
        assert_eq!(quote_ident(""), "\"\"");
    }
}
