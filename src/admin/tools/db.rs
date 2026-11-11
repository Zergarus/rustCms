//! Обозреватель таблиц: список таблиц/вью, структура, данные и правка строк.
//!
//! Имена схем/таблиц/колонок берутся из каталога PostgreSQL и экранируются;
//! пользовательский ввод задаёт только страницу и сортировку по существующей
//! колонке. Строки меняются только у таблиц (`r`/`p`) с первичным ключом:
//! значения уходят bind-параметрами, имена — из каталога после [`pg::quote_ident`].
//! DDL — следующая задача.

use std::collections::HashMap;

use axum::{
    Extension, Form,
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Response},
};
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, Row};

use super::{
    guard,
    pg::{self, ColumnInfo, InputKind, PrimaryKey, TableInfo},
    render,
};
use crate::{
    access::Access,
    admin::flash::{self, FlashKind},
    audit,
    error::{AppError, AppResult},
    state::AppState,
};

/// Данные всегда по 50 строк.
const PER_PAGE: i64 = 50;
/// Больше 200 строк за страницу не отдаём даже при будущем изменении `per`.
const MAX_PER_PAGE: i64 = 200;
/// Сколько соседних страниц показывать вокруг текущей.
const PAGER_AROUND: i64 = 2;
/// Таймаут одного INSERT/UPDATE/DELETE.
const WRITE_TIMEOUT: &str = "15s";

/// Параметры страницы таблицы.
#[derive(Debug, Default, Deserialize)]
pub struct TableQuery {
    /// Номер страницы; мусор и значения меньше 1 трактуются как первая.
    page: Option<String>,
    /// Колонка сортировки; допускается только существующая.
    sort: Option<String>,
    /// `asc`/`desc`; всё остальное — `asc`.
    dir: Option<String>,
}

/// GET `/admin/tools/db` — список таблиц и представлений.
pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    guard(&state, &user)?;
    let tables: Vec<TableView> = pg::tables(&state.db)
        .await?
        .iter()
        .map(TableView::new)
        .collect();
    render(&state, "tools/db_list.html", context! { user, tables })
}

/// GET `/admin/tools/db/{schema}/{table}` — структура и страница данных.
pub async fn table(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path((schema, table)): Path<(String, String)>,
    Query(input): Query<TableQuery>,
) -> AppResult<Html<String>> {
    guard(&state, &user)?;
    // Таблица должна быть в каталоге пользовательских таблиц: это же отсекает
    // системные схемы и мусор в имени до построения SQL.
    let info = find_table(&state, &schema, &table).await?;
    let columns = pg::columns(&state.db, &info.schema, &info.name).await?;
    let data = table_data(&state, info, columns, input).await?;
    render_table_page(&state, &user, data, PageForm::default()).await
}

/// POST `/admin/tools/db/{schema}/{table}/insert` — добавить строку.
pub async fn row_insert(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path((schema, table)): Path<(String, String)>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    guard(&state, &user)?;
    let (info, columns, pk) = editable_table(&state, &schema, &table).await?;
    let statement = match insert_statement(&qualified(&info), &columns, &pk, &form) {
        Ok(statement) => statement,
        Err(message) => {
            return insert_error(&state, &user, info, columns, form, message).await;
        }
    };
    match execute_write(&state, &statement, true).await {
        Ok((_, values)) => {
            let keys = pk_pairs(&pk, &values);
            audit_row(&state, &user, "db-row-insert", &info, &keys).await;
            Ok(flash::redirect(
                &table_url(&info),
                "Строка добавлена",
                FlashKind::Ok,
            ))
        }
        Err(error) => {
            insert_error(&state, &user, info, columns, form, pg::db_message(&error)).await
        }
    }
}

/// POST `/admin/tools/db/{schema}/{table}/update` — изменить строку по PK.
pub async fn row_update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path((schema, table)): Path<(String, String)>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    guard(&state, &user)?;
    let (info, columns, pk) = editable_table(&state, &schema, &table).await?;
    // PK приходит только hidden-полями; без него менять нечего.
    let keys = pk_keys(&pk, &form)
        .ok_or_else(|| AppError::BadRequest("Не переданы значения первичного ключа".into()))?;
    let statement = match update_statement(&qualified(&info), &columns, &keys, &form) {
        Ok(statement) => statement,
        Err(message) => {
            return update_error(&state, &user, info, columns, &keys, form, message).await;
        }
    };
    match execute_write(&state, &statement, false).await {
        Ok((0, _)) => Err(AppError::NotFound),
        Ok(_) => {
            audit_row(&state, &user, "db-row-update", &info, &keys).await;
            Ok(flash::redirect(
                &table_url(&info),
                "Строка изменена",
                FlashKind::Ok,
            ))
        }
        Err(error) => {
            update_error(
                &state,
                &user,
                info,
                columns,
                &keys,
                form,
                pg::db_message(&error),
            )
            .await
        }
    }
}

/// POST `/admin/tools/db/{schema}/{table}/delete` — удалить строку по PK.
pub async fn row_delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path((schema, table)): Path<(String, String)>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    guard(&state, &user)?;
    let (info, columns, pk) = editable_table(&state, &schema, &table).await?;
    let keys = pk_keys(&pk, &form)
        .ok_or_else(|| AppError::BadRequest("Не переданы значения первичного ключа".into()))?;
    let statement = match delete_statement(&qualified(&info), &columns, &keys) {
        Ok(statement) => statement,
        Err(message) => {
            let page = PageForm {
                error: Some(message),
                ..PageForm::default()
            };
            return page_error(&state, &user, info, columns, page).await;
        }
    };
    match execute_write(&state, &statement, false).await {
        Ok((0, _)) => Err(AppError::NotFound),
        Ok(_) => {
            audit_row(&state, &user, "db-row-delete", &info, &keys).await;
            Ok(flash::redirect(
                &table_url(&info),
                "Строка удалена",
                FlashKind::Ok,
            ))
        }
        Err(error) => {
            let page = PageForm {
                error: Some(pg::db_message(&error)),
                ..PageForm::default()
            };
            page_error(&state, &user, info, columns, page).await
        }
    }
}

/// Таблица из каталога пользовательских отношений или 404.
async fn find_table(state: &AppState, schema: &str, table: &str) -> AppResult<TableInfo> {
    pg::tables(&state.db)
        .await?
        .into_iter()
        .find(|known| known.schema == schema && known.name == table)
        .ok_or(AppError::NotFound)
}

/// Таблица, которую можно менять: только `r`/`p` и только с PK.
async fn editable_table(
    state: &AppState,
    schema: &str,
    table: &str,
) -> AppResult<(TableInfo, Vec<ColumnInfo>, PrimaryKey)> {
    let info = find_table(state, schema, table).await?;
    if !matches!(info.kind.as_str(), "r" | "p") {
        return Err(AppError::NotFound);
    }
    let columns = pg::columns(&state.db, &info.schema, &info.name).await?;
    let pk = PrimaryKey::of(&columns).ok_or(AppError::NotFound)?;
    Ok((info, columns, pk))
}

/// Страница таблицы, загруженная для показа: структура, строки и пейджер.
struct TableData {
    info: TableInfo,
    columns: Vec<ColumnInfo>,
    rows: pg::RowsPage,
    page: i64,
    pages: i64,
    pager: Vec<PagerItem>,
    base_url: String,
    sort: Option<String>,
    dir: &'static str,
}

/// Читает страницу данных по параметрам запроса (проверяя сортировку по структуре).
async fn table_data(
    state: &AppState,
    info: TableInfo,
    columns: Vec<ColumnInfo>,
    input: TableQuery,
) -> AppResult<TableData> {
    let page_no = input
        .page
        .as_deref()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let dir = normalize_dir(input.dir.as_deref());
    let sort = input
        .sort
        .as_deref()
        .filter(|column| columns.iter().any(|known| known.name == *column));
    let rows = pg::rows(
        &state.db,
        &info.schema,
        &info.name,
        page_no,
        PER_PAGE.min(MAX_PER_PAGE),
        sort.map(|column| (column, dir)),
    )
    .await?;

    let base_url = table_url(&info);
    let total = rows.total;
    let pages = total
        .map(|total| ((total + PER_PAGE - 1) / PER_PAGE).max(1))
        .unwrap_or(1);
    let pager = pager(page_no, pages, &base_url, sort, dir);
    Ok(TableData {
        info,
        columns,
        rows,
        page: page_no,
        pages,
        pager,
        base_url,
        sort: sort.map(str::to_string),
        dir,
    })
}

/// Ссылка на страницу таблицы (без query-параметров).
fn table_url(info: &TableInfo) -> String {
    format!(
        "/admin/tools/db/{}/{}",
        encode_path(&info.schema),
        encode_path(&info.name)
    )
}

/// Полное имя таблицы для SQL: схема и имя из каталога, оба в кавычках.
fn qualified(info: &TableInfo) -> String {
    format!(
        "{}.{}",
        pg::quote_ident(&info.schema),
        pg::quote_ident(&info.name)
    )
}

/// Что показать на странице таблицы после неудачной попытки правки.
#[derive(Debug, Default)]
struct PageForm {
    /// Текст ошибки в alert; HTTP-статус страницы остаётся 200.
    error: Option<String>,
    /// Введённые значения формы вставки.
    insert_values: Option<HashMap<String, String>>,
    /// Раскрыть форму вставки (после ошибки).
    insert_open: bool,
    /// PK строки, форму которой нужно раскрыть (после ошибки правки).
    open_pk: Option<Vec<String>>,
    /// Введённые значения формы правки для раскрытой строки.
    update_values: Option<HashMap<String, String>>,
}

/// Рендер страницы таблицы с данными и (необязательной) формой/ошибкой.
async fn render_table_page(
    state: &AppState,
    user: &Access,
    data: TableData,
    form: PageForm,
) -> AppResult<Html<String>> {
    let TableData {
        info,
        columns,
        rows,
        page,
        pages,
        pager,
        base_url,
        sort,
        dir,
    } = data;
    let primary_key = PrimaryKey::of(&columns);
    let is_table = matches!(info.kind.as_str(), "r" | "p");
    let editable = is_table && primary_key.is_some();
    let readonly_reason = if !is_table {
        Some("Представление — только для чтения: правка доступна только таблицам.")
    } else if primary_key.is_none() {
        Some("У таблицы нет первичного ключа — правка строк недоступна, таблица только для чтения.")
    } else {
        None
    };

    let sort_ref = sort.as_deref();
    let columns_view: Vec<ColumnView> = columns
        .iter()
        .map(|column| ColumnView::new(column, &base_url, sort_ref, dir))
        .collect();
    let pk_indexes: Vec<usize> = columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.pk)
        .map(|(index, _)| index)
        .collect();
    let open_pk = form.open_pk.as_deref();
    let rows_view: Vec<RowView> = rows
        .rows
        .iter()
        .map(|row| {
            let keys: Vec<HiddenView> = pk_indexes
                .iter()
                .map(|&index| HiddenView {
                    name: columns[index].name.clone(),
                    value: row[index].input.clone(),
                })
                .collect();
            let key_values: Vec<String> = keys.iter().map(|key| key.value.clone()).collect();
            let open = open_pk.is_some_and(|values| values == key_values.as_slice());
            let fields = if editable {
                columns
                    .iter()
                    .enumerate()
                    .filter(|(_, column)| !column.pk)
                    .map(|(index, column)| {
                        let value = if open {
                            form.update_values
                                .as_ref()
                                .and_then(|values| values.get(&column.name))
                                .cloned()
                                .unwrap_or_else(|| row[index].input.clone())
                        } else {
                            row[index].input.clone()
                        };
                        FieldView::new(column, value, false)
                    })
                    .collect()
            } else {
                Vec::new()
            };
            RowView {
                cells: row.iter().map(|cell| cell.display.clone()).collect(),
                keys,
                fields,
                open,
            }
        })
        .collect();
    let insert_fields: Vec<FieldView> = if editable {
        columns
            .iter()
            .map(|column| {
                let value = form
                    .insert_values
                    .as_ref()
                    .and_then(|values| values.get(&column.name))
                    .cloned()
                    .unwrap_or_default();
                FieldView::new(column, value, true)
            })
            .collect()
    } else {
        Vec::new()
    };

    render(
        state,
        "tools/db_table.html",
        context! {
            user,
            schema => info.schema,
            table => info.name,
            kind_label => kind_label(&info.kind),
            columns => columns_view,
            data => rows_view,
            total => rows.total,
            count_error => rows.count_error,
            page,
            pages,
            pager,
            base_url,
            editable,
            readonly_reason,
            error => form.error,
            insert_fields,
            insert_open => form.insert_open,
        },
    )
}

/// Страница таблицы с ошибкой правки: alert + сохранённый ввод, статус 200.
async fn page_error(
    state: &AppState,
    user: &Access,
    info: TableInfo,
    columns: Vec<ColumnInfo>,
    form: PageForm,
) -> AppResult<Response> {
    let data = table_data(state, info, columns, TableQuery::default()).await?;
    Ok(render_table_page(state, user, data, form)
        .await?
        .into_response())
}

/// Ошибка вставки: раскрытая форма с введёнными значениями.
async fn insert_error(
    state: &AppState,
    user: &Access,
    info: TableInfo,
    columns: Vec<ColumnInfo>,
    values: HashMap<String, String>,
    message: String,
) -> AppResult<Response> {
    let form = PageForm {
        error: Some(message),
        insert_values: Some(values),
        insert_open: true,
        ..PageForm::default()
    };
    page_error(state, user, info, columns, form).await
}

/// Ошибка правки: строка с этим PK раскрыта, поля содержат отправленный ввод.
async fn update_error(
    state: &AppState,
    user: &Access,
    info: TableInfo,
    columns: Vec<ColumnInfo>,
    keys: &[(String, String)],
    values: HashMap<String, String>,
    message: String,
) -> AppResult<Response> {
    let form = PageForm {
        error: Some(message),
        open_pk: Some(keys.iter().map(|(_, value)| value.clone()).collect()),
        update_values: Some(values),
        ..PageForm::default()
    };
    page_error(state, user, info, columns, form).await
}

/// Строка данных с формами правки/удаления.
#[derive(Debug, Serialize)]
struct RowView {
    cells: Vec<String>,
    /// PK-колонки, которые уходят hidden-полями в update/delete.
    keys: Vec<HiddenView>,
    /// Не-PK колонки формы правки.
    fields: Vec<FieldView>,
    /// Форма правки раскрыта (после ошибки).
    open: bool,
}

/// Hidden-поле формы: имя колонки и её значение.
#[derive(Debug, Serialize)]
struct HiddenView {
    name: String,
    value: String,
}

/// Поле формы строки: input по типу колонки.
#[derive(Debug, Serialize)]
struct FieldView {
    name: String,
    input_type: &'static str,
    step: Option<&'static str>,
    boolean: bool,
    multiline: bool,
    value: String,
    hint: String,
}

impl FieldView {
    fn new(column: &ColumnInfo, value: String, insert: bool) -> Self {
        let (input_type, boolean, multiline) = match pg::input_kind(column) {
            InputKind::Number => ("number", false, false),
            InputKind::Date => ("date", false, false),
            InputKind::Boolean => ("text", true, false),
            InputKind::Multiline => ("text", false, true),
            InputKind::Text => ("text", false, false),
        };
        let hint = if insert && column.default.is_some() {
            format!(
                "пусто = DEFAULT ({})",
                pg::truncate(column.default.as_deref().unwrap_or_default(), 60)
            )
        } else if column.nullable {
            "пусто = NULL".into()
        } else if !insert && column.default.is_some() {
            "пусто = DEFAULT".into()
        } else if pg::is_textual(column) {
            "пусто = пустая строка".into()
        } else {
            "обязательно".into()
        };
        Self {
            name: column.name.clone(),
            input_type,
            step: pg::input_step(column),
            boolean,
            multiline,
            value,
            hint,
        }
    }
}

/// Значение колонки в INSERT/UPDATE.
enum Bound {
    Value(String),
    Null,
    Default,
}

/// Готовый INSERT/UPDATE/DELETE: SQL с плейсхолдерами и значения для bind.
struct Statement {
    sql: String,
    params: Vec<String>,
}

/// Пустой ввод для колонки: у символьных пустая строка — значение, у остальных — нет.
fn is_blank(column: &ColumnInfo, raw: &str) -> bool {
    if pg::is_textual(column) {
        raw.is_empty()
    } else {
        raw.trim().is_empty()
    }
}

/// Значение для bind: символьные не обрезаем, остальные — по краям.
fn normalized(column: &ColumnInfo, raw: &str) -> String {
    if pg::is_textual(column) {
        raw.to_string()
    } else {
        raw.trim().to_string()
    }
}

/// Пустой ввод при вставке: DEFAULT, пустая строка, NULL или ошибка.
fn insert_bound(column: &ColumnInfo, raw: Option<&str>) -> Result<Bound, String> {
    let raw = raw.unwrap_or_default();
    if is_blank(column, raw) {
        if column.default.is_some() {
            return Ok(Bound::Default);
        }
        if pg::is_textual(column) {
            return Ok(Bound::Value(String::new()));
        }
        if column.nullable {
            return Ok(Bound::Null);
        }
        return Err(format!("Колонка «{}» обязательна", column.name));
    }
    Ok(Bound::Value(normalized(column, raw)))
}

/// Пустой ввод при правке: пустая строка, NULL, DEFAULT или ошибка.
fn update_bound(column: &ColumnInfo, raw: Option<&str>) -> Result<Bound, String> {
    let raw = raw.unwrap_or_default();
    if is_blank(column, raw) {
        if pg::is_textual(column) {
            return Ok(Bound::Value(String::new()));
        }
        if column.nullable {
            return Ok(Bound::Null);
        }
        if column.default.is_some() {
            return Ok(Bound::Default);
        }
        return Err(format!("Колонка «{}» обязательна", column.name));
    }
    Ok(Bound::Value(normalized(column, raw)))
}

/// Значения PK из hidden-полей в порядке ключа; `None`, если колонки нет.
fn pk_keys(pk: &PrimaryKey, form: &HashMap<String, String>) -> Option<Vec<(String, String)>> {
    pk.columns
        .iter()
        .map(|name| form.get(name).map(|value| (name.clone(), value.clone())))
        .collect()
}

/// Пары «колонка PK = значение» из результата `RETURNING`.
fn pk_pairs(pk: &PrimaryKey, values: &[String]) -> Vec<(String, String)> {
    pk.columns
        .iter()
        .cloned()
        .zip(values.iter().cloned())
        .collect()
}

/// INSERT по всем колонкам; пустые default-колонки не попадают в список.
fn insert_statement(
    qualified: &str,
    columns: &[ColumnInfo],
    pk: &PrimaryKey,
    form: &HashMap<String, String>,
) -> Result<Statement, String> {
    let mut names = Vec::new();
    let mut exprs = Vec::new();
    let mut params = Vec::new();
    for column in columns {
        match insert_bound(column, form.get(&column.name).map(String::as_str))? {
            Bound::Default => continue,
            Bound::Null => {
                names.push(pg::quote_ident(&column.name));
                exprs.push("NULL".to_string());
            }
            Bound::Value(value) => {
                params.push(value);
                names.push(pg::quote_ident(&column.name));
                exprs.push(format!("CAST(${} AS {})", params.len(), column.data_type));
            }
        }
    }
    let returning = pk
        .columns
        .iter()
        .map(|column| format!("{}::text", pg::quote_ident(column)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = if exprs.is_empty() {
        format!("INSERT INTO {qualified} DEFAULT VALUES")
    } else {
        format!(
            "INSERT INTO {qualified} ({}) VALUES ({})",
            names.join(", "),
            exprs.join(", ")
        )
    };
    sql.push_str(&format!(" RETURNING {returning}"));
    Ok(Statement { sql, params })
}

/// UPDATE не-PK колонок по всем колонкам PK в `WHERE`.
fn update_statement(
    qualified: &str,
    columns: &[ColumnInfo],
    keys: &[(String, String)],
    form: &HashMap<String, String>,
) -> Result<Statement, String> {
    let mut params = Vec::new();
    let mut sets = Vec::new();
    for column in columns.iter().filter(|column| !column.pk) {
        match update_bound(column, form.get(&column.name).map(String::as_str))? {
            Bound::Default => {
                sets.push(format!("{} = DEFAULT", pg::quote_ident(&column.name)));
            }
            Bound::Null => {
                sets.push(format!("{} = NULL", pg::quote_ident(&column.name)));
            }
            Bound::Value(value) => {
                params.push(value);
                sets.push(format!(
                    "{} = CAST(${} AS {})",
                    pg::quote_ident(&column.name),
                    params.len(),
                    column.data_type
                ));
            }
        }
    }
    if sets.is_empty() {
        return Err("У таблицы нет колонок, доступных для изменения".into());
    }
    let where_sql = where_clause(columns, keys, &mut params)?;
    Ok(Statement {
        sql: format!(
            "UPDATE {qualified} SET {} WHERE {where_sql}",
            sets.join(", ")
        ),
        params,
    })
}

/// DELETE по всем колонкам PK.
fn delete_statement(
    qualified: &str,
    columns: &[ColumnInfo],
    keys: &[(String, String)],
) -> Result<Statement, String> {
    let mut params = Vec::new();
    let where_sql = where_clause(columns, keys, &mut params)?;
    Ok(Statement {
        sql: format!("DELETE FROM {qualified} WHERE {where_sql}"),
        params,
    })
}

/// `WHERE pk1 = CAST($n AS type) AND pk2 = ...`; все колонки PK обязательны.
fn where_clause(
    columns: &[ColumnInfo],
    keys: &[(String, String)],
    params: &mut Vec<String>,
) -> Result<String, String> {
    let mut parts = Vec::new();
    for (name, value) in keys {
        let column = columns
            .iter()
            .find(|column| &column.name == name)
            .ok_or_else(|| format!("Неизвестная колонка ключа «{name}»"))?;
        params.push(normalized(column, value));
        parts.push(format!(
            "{} = CAST(${} AS {})",
            pg::quote_ident(name),
            params.len(),
            column.data_type
        ));
    }
    if parts.is_empty() {
        return Err("Не передан первичный ключ".into());
    }
    Ok(parts.join(" AND "))
}

/// Выполняет INSERT/UPDATE/DELETE в транзакции с таймаутом.
///
/// `returning` — INSERT с `RETURNING` значений PK в текстовом виде.
async fn execute_write(
    state: &AppState,
    statement: &Statement,
    returning: bool,
) -> Result<(u64, Vec<String>), sqlx::Error> {
    let mut tx = state.db.begin().await?;
    let outcome: Result<(u64, Vec<String>), sqlx::Error> = async {
        sqlx::query(AssertSqlSafe(format!(
            "SET LOCAL statement_timeout = '{WRITE_TIMEOUT}'"
        )))
        .execute(&mut *tx)
        .await?;
        let mut query = sqlx::query(AssertSqlSafe(statement.sql.clone()));
        for value in &statement.params {
            query = query.bind(value.as_str());
        }
        if returning {
            let row = query.fetch_one(&mut *tx).await?;
            let mut values = Vec::with_capacity(row.len());
            for index in 0..row.len() {
                values.push(row.try_get::<String, _>(index).unwrap_or_default());
            }
            Ok((1, values))
        } else {
            let result = query.execute(&mut *tx).await?;
            Ok((result.rows_affected(), Vec::new()))
        }
    }
    .await;
    match outcome {
        Ok(result) => {
            tx.commit().await?;
            Ok(result)
        }
        Err(error) => {
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}

/// Пишет в журнал `db-row-*` с таблицей и PK; ошибка журнала не ломает действие.
async fn audit_row(
    state: &AppState,
    user: &Access,
    action: &str,
    info: &TableInfo,
    keys: &[(String, String)],
) {
    let table = format!("{}.{}", info.schema, info.name);
    let summary = if keys.is_empty() {
        table
    } else {
        let pairs = keys
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{table}; PK {pairs}")
    };
    audit::record(
        &state.db,
        user.user.id,
        &user.user.login,
        action,
        "tools",
        "",
        pg::truncate(&summary, 500),
    )
    .await;
}

/// Подпись типа отношения из каталога.
fn kind_label(kind: &str) -> &'static str {
    match kind {
        "v" => "представление",
        "m" => "материализованное представление",
        _ => "таблица",
    }
}

/// Направление сортировки: всё, кроме `desc`, — по возрастанию.
fn normalize_dir(raw: Option<&str>) -> &'static str {
    if raw.is_some_and(|dir| dir.eq_ignore_ascii_case("desc")) {
        "desc"
    } else {
        "asc"
    }
}

/// Кодирует значение query-параметра (`application/x-www-form-urlencoded`).
fn encode_query(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// Кодирует сегмент пути: пробел из `byte_serialize` — `%20`, а не `+`.
fn encode_path(value: &str) -> String {
    encode_query(value).replace('+', "%20")
}

/// Строка списка таблиц: каталог + ссылка на страницу.
#[derive(Debug, Serialize)]
struct TableView {
    schema: String,
    name: String,
    kind_label: &'static str,
    /// Оценка строк; у вью точной оценки нет.
    rows_label: String,
    size: String,
    columns: i64,
    has_pk: bool,
    url: String,
}

impl TableView {
    fn new(info: &TableInfo) -> Self {
        Self {
            schema: info.schema.clone(),
            name: info.name.clone(),
            kind_label: kind_label(&info.kind),
            rows_label: match info.kind.as_str() {
                "v" | "m" => "—".into(),
                _ => format!("≈ {}", info.rows),
            },
            size: info.size.clone(),
            columns: info.columns,
            has_pk: info.has_pk,
            url: format!(
                "/admin/tools/db/{}/{}",
                encode_path(&info.schema),
                encode_path(&info.name)
            ),
        }
    }
}

/// Колонка структуры и ссылка на сортировку по ней.
#[derive(Debug, Serialize)]
struct ColumnView {
    name: String,
    data_type: String,
    nullable: bool,
    default: Option<String>,
    pk: bool,
    /// Ссылка сортировки: первый клик — `asc`, повторный — `desc`.
    sort_url: String,
    /// Направление, если таблица отсортирована по этой колонке.
    sort_dir: Option<&'static str>,
}

impl ColumnView {
    fn new(column: &ColumnInfo, base_url: &str, sort: Option<&str>, dir: &'static str) -> Self {
        let current = sort == Some(column.name.as_str());
        let next = if current && dir == "asc" {
            "desc"
        } else {
            "asc"
        };
        Self {
            name: column.name.clone(),
            data_type: column.data_type.clone(),
            nullable: column.nullable,
            default: column.default.clone(),
            pk: column.pk,
            sort_url: format!("{base_url}?sort={}&dir={next}", encode_query(&column.name)),
            sort_dir: current.then_some(dir),
        }
    }
}

/// Элемент постраничной навигации.
#[derive(Debug, Serialize)]
struct PagerItem {
    label: String,
    url: Option<String>,
    current: bool,
}

/// Компактный пейджер: первая, последняя и соседние страницы, между ними «…».
fn pager(page: i64, pages: i64, base_url: &str, sort: Option<&str>, dir: &str) -> Vec<PagerItem> {
    if pages <= 1 {
        return Vec::new();
    }
    let mut items = Vec::new();
    let mut previous = 0;
    for number in 1..=pages {
        if number != 1 && number != pages && (number - page).abs() > PAGER_AROUND {
            continue;
        }
        if previous != 0 && number - previous > 1 {
            items.push(PagerItem {
                label: "…".into(),
                url: None,
                current: false,
            });
        }
        items.push(PagerItem {
            label: number.to_string(),
            url: (number != page).then(|| page_url(base_url, number, sort, dir)),
            current: number == page,
        });
        previous = number;
    }
    items
}

/// Ссылка на страницу с сохранением сортировки.
fn page_url(base_url: &str, page: i64, sort: Option<&str>, dir: &str) -> String {
    let mut url = format!("{base_url}?page={page}");
    if let Some(sort) = sort {
        url.push_str(&format!("&sort={}&dir={dir}", encode_query(sort)));
    }
    url
}

#[cfg(test)]
mod tests {
    use axum::{Router, http::StatusCode};
    use sqlx::PgPool;

    use crate::{
        smoke_tests::get,
        test_support::{admin_cookie, permitted_cookie, test_state},
    };

    /// Тело `application/x-www-form-urlencoded` из пар; значения кодируются.
    fn form_body(pairs: &[(&str, &str)]) -> String {
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        for (name, value) in pairs {
            serializer.append_pair(name, value);
        }
        serializer.finish()
    }

    /// POST формы таблицы: статус и тело (у редиректа тело пустое).
    async fn post(
        app: &Router,
        path: &str,
        cookie: &str,
        pairs: &[(&str, &str)],
    ) -> (StatusCode, String) {
        crate::smoke_tests::post_form(app, path, cookie, &form_body(pairs)).await
    }

    /// Последняя запись журнала действий.
    async fn last_audit(db: &PgPool) -> (String, String, String) {
        sqlx::query_as("SELECT action, entity, summary FROM audit_log ORDER BY id DESC LIMIT 1")
            .fetch_one(db)
            .await
            .unwrap()
    }

    /// Таблица с данными для проверок: PK, NOT NULL + DEFAULT, numeric.
    async fn tools_fixture(db: &PgPool) {
        sqlx::query(
            "CREATE TABLE tools_test (
                 id bigint PRIMARY KEY,
                 name text NOT NULL DEFAULT 'без имени',
                 amount numeric(10,2)
             )",
        )
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tools_test (id, name, amount)
             VALUES (1, 'Альфа', 12.5), (2, 'Бета', NULL)",
        )
        .execute(db)
        .await
        .unwrap();
    }

    async fn table_count(db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM tools_test")
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn list_shows_user_tables_and_views(db: PgPool) {
        tools_fixture(&db).await;
        sqlx::query("CREATE VIEW tools_view AS SELECT id, name FROM tools_test")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, "/admin/tools/db", &cookie).await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("tools_test"), "нет таблицы в списке: {body}");
        assert!(body.contains("tools_view"), "нет вью в списке: {body}");
        assert!(body.contains("public"), "нет схемы: {body}");
        assert!(body.contains("PK"), "нет признака первичного ключа: {body}");
        assert!(
            !body.contains("pg_catalog"),
            "системная схема попала в список"
        );
    }

    #[sqlx::test]
    async fn table_shows_structure_and_data(db: PgPool) {
        tools_fixture(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, "/admin/tools/db/public/tools_test", &cookie).await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("tools_test"), "нет имени таблицы: {body}");
        assert!(body.contains("Структура"), "нет блока структуры: {body}");
        assert!(body.contains("name"), "нет колонки структуры: {body}");
        assert!(body.contains("bigint"), "нет типа колонки: {body}");
        assert!(body.contains("Альфа"), "нет значения строки: {body}");
        assert!(body.contains("12.50"), "numeric не отформатирован: {body}");
        assert!(body.contains("без имени"), "нет default: {body}");
        assert!(
            body.contains("Данные не маскируются"),
            "нет предупреждения о маскировании: {body}"
        );
    }

    #[sqlx::test]
    async fn missing_table_schema_and_garbage_are_404(db: PgPool) {
        tools_fixture(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        for path in [
            "/admin/tools/db/public/nope",
            "/admin/tools/db/nope/tools_test",
            "/admin/tools/db/pg_catalog/pg_class",
            "/admin/tools/db/public/x%22%3B%20DROP%20TABLE%20tools_test",
            "/admin/tools/db/public%22%3B%20DROP%20TABLE%20tools_test/tools_test",
        ] {
            let (status, body) = get(&app, path, &cookie).await;
            assert_eq!(status, 404, "{path}: {body}");
        }

        assert_eq!(table_count(&db).await, 2, "таблица пострадала от инъекции");
    }

    #[sqlx::test]
    async fn sorts_only_by_table_columns(db: PgPool) {
        tools_fixture(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(
            &app,
            "/admin/tools/db/public/tools_test?sort=name&dir=asc",
            &cookie,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.find("Альфа").unwrap() < body.find("Бета").unwrap(),
            "сортировка asc не применилась: {body}"
        );

        let (status, body) = get(
            &app,
            "/admin/tools/db/public/tools_test?sort=name&dir=desc",
            &cookie,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.find("Бета").unwrap() < body.find("Альфа").unwrap(),
            "сортировка desc не применилась: {body}"
        );

        // Неизвестная колонка и мусор в имени не попадают в SQL: страница работает,
        // данные на месте, таблица цела.
        for sort in [
            "amount%3B%20DROP%20TABLE%20tools_test",
            "not_a_column",
            "amount%2C%20name",
        ] {
            let path = format!("/admin/tools/db/public/tools_test?sort={sort}&dir=asc");
            let (status, body) = get(&app, &path, &cookie).await;
            assert_eq!(status, 200, "{path}: {body}");
            assert!(body.contains("Альфа"), "{path}: {body}");
        }
        assert_eq!(table_count(&db).await, 2, "сортировка изменила таблицу");
    }

    #[sqlx::test]
    async fn paginates_fifty_rows_with_exact_count(db: PgPool) {
        sqlx::query("CREATE TABLE tools_page (id bigint PRIMARY KEY, name text NOT NULL)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO tools_page (id, name)
             SELECT g, 'r' || lpad(g::text, 3, '0') FROM generate_series(1, 55) AS g",
        )
        .execute(&db)
        .await
        .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(
            &app,
            "/admin/tools/db/public/tools_page?sort=id&dir=asc",
            &cookie,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("Всего строк: 55"), "нет count(*): {body}");
        assert!(body.contains(">r001<"), "нет первой строки: {body}");
        assert!(
            !body.contains(">r051<"),
            "на странице больше 50 строк: {body}"
        );
        assert!(
            body.contains("page=2"),
            "нет ссылки на вторую страницу: {body}"
        );

        let (status, body) = get(
            &app,
            "/admin/tools/db/public/tools_page?page=2&sort=id&dir=asc",
            &cookie,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains(">r051<"),
            "нет строки второй страницы: {body}"
        );
        assert!(body.contains(">r055<"), "нет последней строки: {body}");
        assert!(
            !body.contains(">r001<"),
            "первая страница повторилась: {body}"
        );
    }

    #[sqlx::test]
    async fn empty_table_keeps_headers(db: PgPool) {
        sqlx::query("CREATE TABLE tools_empty (id bigint PRIMARY KEY, note text)")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, "/admin/tools/db/public/tools_empty", &cookie).await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains(">id<"), "нет заголовка id: {body}");
        assert!(body.contains(">note<"), "нет заголовка note: {body}");
        assert!(
            body.contains("Всего строк: 0"),
            "нет нулевого count: {body}"
        );
        assert!(
            body.contains("В таблице пока нет строк"),
            "нет сообщения о пустой таблице: {body}"
        );
    }

    #[sqlx::test]
    async fn view_is_readable(db: PgPool) {
        tools_fixture(&db).await;
        sqlx::query("CREATE VIEW tools_view AS SELECT id, name FROM tools_test")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, "/admin/tools/db/public/tools_view", &cookie).await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("Альфа"), "нет данных вью: {body}");
    }

    #[sqlx::test]
    async fn non_super_cannot_open_tables(db: PgPool) {
        tools_fixture(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = permitted_cookie(&db, "op", &[]).await;

        for path in ["/admin/tools/db", "/admin/tools/db/public/tools_test"] {
            let (status, body) = get(&app, path, &cookie).await;
            assert_eq!(status, 403, "{path}: {body}");
        }
    }

    #[sqlx::test]
    async fn insert_row_uses_default_and_redirects(db: PgPool) {
        tools_fixture(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_test/insert",
            &cookie,
            &[("id", "3"), ("name", ""), ("amount", "7.25")],
        )
        .await;

        assert_eq!(status, 303, "вставка должна редиректить: {body}");
        let (name, amount): (String, Option<String>) =
            sqlx::query_as("SELECT name, amount::text FROM tools_test WHERE id = 3")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(name, "без имени", "пустой default не применился");
        assert_eq!(amount.as_deref(), Some("7.25"));

        let (status, body) = get(
            &app,
            "/admin/tools/db/public/tools_test?sort=id&dir=desc",
            &cookie,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("7.25"), "новая строка не видна: {body}");

        let (action, entity, summary) = last_audit(&db).await;
        assert_eq!(action, "db-row-insert");
        assert_eq!(entity, "tools");
        assert!(summary.contains("public.tools_test"), "{summary}");
        assert!(summary.contains("id=3"), "{summary}");
    }

    #[sqlx::test]
    async fn insert_allows_empty_pk_with_default(db: PgPool) {
        sqlx::query("CREATE TABLE tools_serial (id bigserial PRIMARY KEY, name text NOT NULL)")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_serial/insert",
            &cookie,
            &[("id", ""), ("name", "Новая")],
        )
        .await;

        assert_eq!(status, 303, "PK с default можно оставить пустым: {body}");
        let (id, name): (i64, String) = sqlx::query_as("SELECT id, name FROM tools_serial")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(id, 1);
        assert_eq!(name, "Новая");
    }

    #[sqlx::test]
    async fn update_row_changes_values(db: PgPool) {
        tools_fixture(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_test/update",
            &cookie,
            &[("id", "1"), ("name", "Гамма"), ("amount", "")],
        )
        .await;

        assert_eq!(status, 303, "правка должна редиректить: {body}");
        let (name, amount): (String, Option<String>) =
            sqlx::query_as("SELECT name, amount::text FROM tools_test WHERE id = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(name, "Гамма");
        assert_eq!(amount, None, "пустой nullable не стал NULL");

        let (_, body) = get(&app, "/admin/tools/db/public/tools_test", &cookie).await;
        assert!(body.contains("Гамма"), "правка не видна: {body}");

        let (action, entity, summary) = last_audit(&db).await;
        assert_eq!(action, "db-row-update");
        assert_eq!(entity, "tools");
        assert!(summary.contains("public.tools_test"), "{summary}");
        assert!(summary.contains("id=1"), "{summary}");
    }

    #[sqlx::test]
    async fn delete_row_removes_it(db: PgPool) {
        tools_fixture(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_test/delete",
            &cookie,
            &[("id", "2")],
        )
        .await;

        assert_eq!(status, 303, "удаление должно редиректить: {body}");
        assert_eq!(table_count(&db).await, 1);
        let (_, body) = get(&app, "/admin/tools/db/public/tools_test", &cookie).await;
        assert!(!body.contains("Бета"), "удалённая строка видна: {body}");

        let (action, entity, summary) = last_audit(&db).await;
        assert_eq!(action, "db-row-delete");
        assert_eq!(entity, "tools");
        assert!(summary.contains("public.tools_test"), "{summary}");
        assert!(summary.contains("id=2"), "{summary}");
    }

    #[sqlx::test]
    async fn unknown_and_missing_pk_are_rejected_without_changes(db: PgPool) {
        tools_fixture(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let update = "/admin/tools/db/public/tools_test/update";
        let delete = "/admin/tools/db/public/tools_test/delete";

        let (status, body) = post(
            &app,
            update,
            &cookie,
            &[("id", "999"), ("name", "Призрак"), ("amount", "")],
        )
        .await;
        assert_eq!(status, 404, "несуществующий PK: {body}");

        let (status, body) = post(&app, delete, &cookie, &[("id", "999")]).await;
        assert_eq!(status, 404, "несуществующий PK при удалении: {body}");

        // PK-колонки нет в форме — 400, без изменений.
        let (status, body) = post(&app, update, &cookie, &[("name", "Без ключа")]).await;
        assert_eq!(status, 400, "нет PK в форме правки: {body}");
        let (status, body) = post(&app, delete, &cookie, &[]).await;
        assert_eq!(status, 400, "нет PK в форме удаления: {body}");

        assert_eq!(table_count(&db).await, 2);
        let name: String = sqlx::query_scalar("SELECT name FROM tools_test WHERE id = 1")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(name, "Альфа");
    }

    #[sqlx::test]
    async fn table_without_pk_is_read_only(db: PgPool) {
        sqlx::query("CREATE TABLE tools_nopk (name text NOT NULL)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO tools_nopk (name) VALUES ('Один')")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, "/admin/tools/db/public/tools_nopk", &cookie).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("нет первичного ключа"),
            "нет объяснения read-only: {body}"
        );
        for form in ["/insert", "/update", "/delete"] {
            assert!(
                !body.contains(form),
                "таблица без PK показывает форму {form}: {body}"
            );
        }

        let (status, _) = post(
            &app,
            "/admin/tools/db/public/tools_nopk/insert",
            &cookie,
            &[("name", "Два")],
        )
        .await;
        assert_eq!(
            status, 404,
            "вставка в таблицу без PK должна быть запрещена"
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tools_nopk")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[sqlx::test]
    async fn view_has_no_edit_forms(db: PgPool) {
        tools_fixture(&db).await;
        sqlx::query("CREATE VIEW tools_view AS SELECT id, name FROM tools_test")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, "/admin/tools/db/public/tools_view", &cookie).await;
        assert_eq!(status, 200, "{body}");
        for form in ["/insert", "/update", "/delete"] {
            assert!(!body.contains(form), "вью показывает форму {form}: {body}");
        }

        let (status, _) = post(
            &app,
            "/admin/tools/db/public/tools_view/insert",
            &cookie,
            &[("id", "9"), ("name", "Из вью")],
        )
        .await;
        assert_eq!(status, 404, "вставка во вью должна быть запрещена");
        assert_eq!(table_count(&db).await, 2);
    }

    #[sqlx::test]
    async fn pk_injection_does_not_change_other_rows(db: PgPool) {
        sqlx::query("CREATE TABLE tools_text_pk (code text PRIMARY KEY, note text NOT NULL)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO tools_text_pk (code, note) VALUES ('a', 'one'), ('b', 'two')")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, _) = post(
            &app,
            "/admin/tools/db/public/tools_text_pk/update",
            &cookie,
            &[("code", "' OR '1'='1"), ("note", "взлом")],
        )
        .await;
        assert_eq!(status, 404, "инъекция в PK не должна находить строки");

        let (status, _) = post(
            &app,
            "/admin/tools/db/public/tools_text_pk/delete",
            &cookie,
            &[("code", "a' OR '1'='1")],
        )
        .await;
        assert_eq!(status, 404);

        let notes: Vec<String> = sqlx::query_scalar("SELECT note FROM tools_text_pk ORDER BY code")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(notes, ["one", "two"]);
    }

    #[sqlx::test]
    async fn invalid_pk_type_is_error_page_without_changes(db: PgPool) {
        tools_fixture(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_test/update",
            &cookie,
            &[("id", "1 OR 1=1"), ("name", "Взлом"), ("amount", "")],
        )
        .await;

        assert_eq!(status, 200, "ошибка типа — страница с alert: {body}");
        assert!(body.contains("Ошибка"), "нет alert: {body}");
        assert_eq!(table_count(&db).await, 2);
        let name: String = sqlx::query_scalar("SELECT name FROM tools_test WHERE id = 1")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(name, "Альфа", "инъекция изменила строку");
    }

    #[sqlx::test]
    async fn duplicate_pk_shows_form_with_error(db: PgPool) {
        tools_fixture(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_test/insert",
            &cookie,
            &[("id", "1"), ("name", "Дубль"), ("amount", "")],
        )
        .await;

        assert_eq!(status, 200, "ожидаемая ошибка БД — HTTP 200: {body}");
        assert!(body.contains("Ошибка"), "нет alert: {body}");
        assert!(
            body.contains("value=\"1\""),
            "форма вставки не сохранила ввод: {body}"
        );
        assert_eq!(table_count(&db).await, 2);
    }

    #[sqlx::test]
    async fn composite_pk_uses_all_columns(db: PgPool) {
        sqlx::query(
            "CREATE TABLE tools_pair (
                 a integer NOT NULL,
                 b text NOT NULL,
                 value text,
                 PRIMARY KEY (a, b)
             )",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tools_pair (a, b, value)
             VALUES (1, 'x', 'one'), (1, 'y', 'two'), (2, 'x', 'three')",
        )
        .execute(&db)
        .await
        .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, "/admin/tools/db/public/tools_pair", &cookie).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("/update"), "нет формы правки: {body}");

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_pair/update",
            &cookie,
            &[("a", "1"), ("b", "y"), ("value", "changed")],
        )
        .await;
        assert_eq!(status, 303, "{body}");
        let values: Vec<(i32, String, Option<String>)> =
            sqlx::query_as("SELECT a, b, value FROM tools_pair ORDER BY a, b")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(
            values,
            [
                (1, "x".into(), Some("one".into())),
                (1, "y".into(), Some("changed".into())),
                (2, "x".into(), Some("three".into())),
            ]
        );

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_pair/delete",
            &cookie,
            &[("a", "1"), ("b", "x")],
        )
        .await;
        assert_eq!(status, 303, "{body}");
        let left: Vec<(i32, String)> = sqlx::query_as("SELECT a, b FROM tools_pair ORDER BY a, b")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(left, [(1, "y".into()), (2, "x".into())]);
    }

    #[sqlx::test]
    async fn pk_include_columns_are_not_key_columns(db: PgPool) {
        // INCLUDE-колонка входит в индекс PK, но не в сам ключ: её можно править.
        sqlx::query(
            "CREATE TABLE tools_include (
                 id bigint,
                 note text,
                 PRIMARY KEY (id) INCLUDE (note)
             )",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO tools_include (id, note) VALUES (1, 'one')")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_include/update",
            &cookie,
            &[("id", "1"), ("note", "two")],
        )
        .await;

        assert_eq!(
            status, 303,
            "INCLUDE-колонка не должна быть частью ключа: {body}"
        );
        let note: String = sqlx::query_scalar("SELECT note FROM tools_include WHERE id = 1")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(note, "two");
    }

    #[sqlx::test]
    async fn typed_columns_are_edited_through_casts(db: PgPool) {
        sqlx::query(
            "CREATE TABLE tools_typed (
                 id bigint PRIMARY KEY,
                 flag boolean NOT NULL DEFAULT true,
                 day date,
                 ts timestamp,
                 tstz timestamptz,
                 ident uuid,
                 doc jsonb,
                 price numeric(8,2)
             )",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tools_typed (id, flag, day, ts, tstz, ident, doc, price)
             VALUES (1, true, '2024-05-01', '2024-05-01 10:00:00',
                     '2024-05-01 10:00:00+00',
                     '11111111-1111-1111-1111-111111111111', '{\"a\": 1}', 1.50)",
        )
        .execute(&db)
        .await
        .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, "/admin/tools/db/public/tools_typed", &cookie).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("type=\"date\""), "нет input date: {body}");
        assert!(body.contains("type=\"number\""), "нет input number: {body}");
        assert!(
            body.contains("<select name=\"flag\""),
            "boolean не select: {body}"
        );
        assert!(
            body.contains("<textarea class=\"mono\" name=\"doc\""),
            "jsonb не textarea: {body}"
        );
        assert!(
            body.contains("value=\"2024-05-01\""),
            "значение date не подставлено: {body}"
        );

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_typed/update",
            &cookie,
            &[
                ("id", "1"),
                ("flag", "false"),
                ("day", "2024-06-02"),
                ("ts", "2024-06-02 03:04:05"),
                ("tstz", "2024-06-02 03:04:05+00"),
                ("ident", "22222222-2222-2222-2222-222222222222"),
                ("doc", "{\"a\": 2}"),
                ("price", "3.50"),
            ],
        )
        .await;
        assert_eq!(status, 303, "типизированная правка не прошла: {body}");

        let row: (bool, String, String, String, String, String, String) = sqlx::query_as(
            "SELECT flag, day::text, ts::text, tstz::text, ident::text,
                    doc->>'a', price::text
               FROM tools_typed WHERE id = 1",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(!row.0);
        assert_eq!(row.1, "2024-06-02");
        assert_eq!(row.2, "2024-06-02 03:04:05");
        assert!(
            row.3.starts_with("2024-06-02 03:04:05"),
            "timestamptz: {}",
            row.3
        );
        assert_eq!(row.4, "22222222-2222-2222-2222-222222222222");
        assert_eq!(row.5, "2");
        assert_eq!(row.6, "3.50");
    }
}
