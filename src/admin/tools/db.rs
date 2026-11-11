//! Обозреватель таблиц: список таблиц/вью, структура, данные, правка строк и DDL.
//!
//! Имена схем/таблиц/колонок берутся из каталога PostgreSQL и экранируются;
//! пользовательский ввод задаёт только страницу и сортировку по существующей
//! колонке. Строки меняются только у таблиц (`r`/`p`) с первичным ключом:
//! значения уходят bind-параметрами, имена — из каталога после [`pg::quote_ident`].
//! DDL принимает имена только по `^[a-z_][a-z0-9_]*$`, типы — из allowlist,
//! default — только безопасный литерал; идентификаторы всё равно экранируются.

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
/// Таймаут одного DDL-запроса.
const DDL_TIMEOUT: &str = "15s";
/// Сколько строк колонок отрисовывает форма создания таблицы.
const CREATE_COLUMNS: usize = 5;
/// Максимальная длина идентификатора PostgreSQL.
const MAX_IDENT_CHARS: usize = 63;
/// Подсказки типов в форме создания/добавления колонки.
const COLUMN_TYPES: [&str; 13] = [
    "text",
    "varchar(n)",
    "integer",
    "bigint",
    "smallint",
    "numeric(p,s)",
    "boolean",
    "date",
    "timestamp",
    "timestamptz",
    "uuid",
    "jsonb",
    "bytea",
];

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
    let schemas = pg::schemas(&state.db).await?;
    render_list_page(
        &state,
        &user,
        tables,
        schemas,
        CreateFormView::empty(),
        None,
    )
}

/// POST `/admin/tools/db/create` — создать таблицу из формы списка.
pub async fn table_create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    guard(&state, &user)?;
    let schemas = pg::schemas(&state.db).await?;
    let submitted = CreateFormView::from_form(&form, true);
    // Схема выбирается из существующих пользовательских схем; чужое имя — 404.
    let schema = submitted.schema.clone();
    if !schemas.iter().any(|known| known == &schema) {
        return Err(AppError::NotFound);
    }
    if let Err(message) = validate_ident(&submitted.table) {
        return create_error(
            &state,
            &user,
            schemas,
            submitted,
            format!("Имя таблицы: {message}"),
        )
        .await;
    }
    let columns = match parse_create_columns(&form) {
        Ok(columns) => columns,
        Err(message) => {
            return create_error(&state, &user, schemas, submitted, message).await;
        }
    };
    let table = submitted.table.clone();
    let statement = create_table_sql(&schema, &table, &columns);
    match execute_ddl(&state, &statement).await {
        Ok(()) => {
            let details = columns
                .iter()
                .map(|column| format!("{} {}", column.name, column.data_type))
                .collect::<Vec<_>>()
                .join(", ");
            audit_ddl(
                &state,
                &user,
                "db-table-create",
                &format!("{schema}.{table}"),
                &format!("колонки: {details}"),
            )
            .await;
            let url = format!(
                "/admin/tools/db/{}/{}",
                encode_path(&schema),
                encode_path(&table)
            );
            Ok(flash::redirect(&url, "Таблица создана", FlashKind::Ok))
        }
        Err(error) => create_error(&state, &user, schemas, submitted, pg::db_message(&error)).await,
    }
}

/// Ошибка создания таблицы: список таблиц с alert и сохранённой формой.
async fn create_error(
    state: &AppState,
    user: &Access,
    schemas: Vec<String>,
    form: CreateFormView,
    message: String,
) -> AppResult<Response> {
    let tables: Vec<TableView> = pg::tables(&state.db)
        .await?
        .iter()
        .map(TableView::new)
        .collect();
    Ok(render_list_page(state, user, tables, schemas, form, Some(message))?.into_response())
}

/// Рендер списка таблиц с формой создания и (необязательной) ошибкой.
fn render_list_page(
    state: &AppState,
    user: &Access,
    tables: Vec<TableView>,
    schemas: Vec<String>,
    create: CreateFormView,
    error: Option<String>,
) -> AppResult<Html<String>> {
    render(
        state,
        "tools/db_list.html",
        context! {
            user,
            tables,
            schemas,
            ddl_types => COLUMN_TYPES,
            create,
            error,
        },
    )
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

/// POST `/admin/tools/db/{schema}/{table}/drop` — удалить таблицу.
///
/// Подтверждение — точное имя таблицы в поле `confirm`; `DROP TABLE` без CASCADE,
/// поэтому зависимости показываются ошибкой БД.
pub async fn table_drop(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path((schema, table)): Path<(String, String)>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    guard(&state, &user)?;
    let info = find_table(&state, &schema, &table).await?;
    if !is_table(&info) {
        return Err(AppError::NotFound);
    }
    let columns = pg::columns(&state.db, &info.schema, &info.name).await?;
    let confirm = form.get("confirm").map(|value| value.trim()).unwrap_or("");
    if confirm != info.name {
        let page = PageForm {
            error: Some(format!(
                "Введите имя таблицы «{}», чтобы подтвердить удаление.",
                info.name
            )),
            ..PageForm::default()
        };
        return page_error(&state, &user, info, columns, page).await;
    }
    match execute_ddl(&state, &format!("DROP TABLE {}", qualified(&info))).await {
        Ok(()) => {
            record_ddl(&state, &user, "db-table-drop", &info, "").await;
            Ok(flash::redirect(
                "/admin/tools/db",
                "Таблица удалена",
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

/// POST `/admin/tools/db/{schema}/{table}/column-add` — добавить колонку.
pub async fn column_add(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path((schema, table)): Path<(String, String)>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    guard(&state, &user)?;
    let info = find_table(&state, &schema, &table).await?;
    if !is_table(&info) {
        return Err(AppError::NotFound);
    }
    let columns = pg::columns(&state.db, &info.schema, &info.name).await?;
    let name = form.get("name").map(|value| value.trim()).unwrap_or("");
    let data_type = form.get("type").map(|value| value.trim()).unwrap_or("");
    let default = form.get("default").map(String::as_str).unwrap_or("");
    let nullable = form.contains_key("nullable");

    let parsed = validate_ident(name)
        .and_then(|()| parse_column_type(data_type))
        .and_then(|data_type| parse_default(default).map(|default| (data_type, default)));
    let (data_type, default) = match parsed {
        Ok(parsed) => parsed,
        Err(message) => {
            let page = PageForm {
                error: Some(message),
                ..PageForm::default()
            };
            return page_error(&state, &user, info, columns, page).await;
        }
    };
    if columns.iter().any(|column| column.name == name) {
        let page = PageForm {
            error: Some(format!("Колонка «{name}» уже существует.")),
            ..PageForm::default()
        };
        return page_error(&state, &user, info, columns, page).await;
    }

    let mut statement = format!(
        "ALTER TABLE {} ADD COLUMN {} {}",
        qualified(&info),
        pg::quote_ident(name),
        data_type
    );
    if !nullable {
        statement.push_str(" NOT NULL");
    }
    if let Some(default) = &default {
        statement.push_str(&format!(" DEFAULT {default}"));
    }
    match execute_ddl(&state, &statement).await {
        Ok(()) => {
            record_ddl(
                &state,
                &user,
                "db-column-add",
                &info,
                &format!("колонка {name} {data_type}"),
            )
            .await;
            Ok(flash::redirect(
                &table_url(&info),
                "Колонка добавлена",
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

/// POST `/admin/tools/db/{schema}/{table}/column-drop` — удалить колонку.
///
/// Подтверждение — точное имя колонки в поле `confirm`; имя проверяется по каталогу.
pub async fn column_drop(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path((schema, table)): Path<(String, String)>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    guard(&state, &user)?;
    let info = find_table(&state, &schema, &table).await?;
    if !is_table(&info) {
        return Err(AppError::NotFound);
    }
    let columns = pg::columns(&state.db, &info.schema, &info.name).await?;
    let name = form.get("name").map(|value| value.trim()).unwrap_or("");
    if let Err(message) = validate_ident(name) {
        let page = PageForm {
            error: Some(message),
            ..PageForm::default()
        };
        return page_error(&state, &user, info, columns, page).await;
    }
    if !columns.iter().any(|column| column.name == name) {
        return Err(AppError::NotFound);
    }
    let confirm = form.get("confirm").map(|value| value.trim()).unwrap_or("");
    if confirm != name {
        let page = PageForm {
            error: Some(format!(
                "Введите имя колонки «{name}», чтобы подтвердить удаление."
            )),
            ..PageForm::default()
        };
        return page_error(&state, &user, info, columns, page).await;
    }
    match execute_ddl(
        &state,
        &format!(
            "ALTER TABLE {} DROP COLUMN {}",
            qualified(&info),
            pg::quote_ident(name)
        ),
    )
    .await
    {
        Ok(()) => {
            record_ddl(
                &state,
                &user,
                "db-column-drop",
                &info,
                &format!("колонка {name}"),
            )
            .await;
            Ok(flash::redirect(
                &table_url(&info),
                "Колонка удалена",
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

/// Настоящая таблица (`r`/`p`), а не представление: DDL доступен только им.
fn is_table(info: &TableInfo) -> bool {
    matches!(info.kind.as_str(), "r" | "p")
}

/// Значения формы создания таблицы, в том числе после ошибки.
#[derive(Debug, Serialize)]
struct CreateFormView {
    schema: String,
    table: String,
    columns: Vec<CreateColumnView>,
    /// Раскрыть `<details>` после ошибки.
    open: bool,
}

/// Одна строка формы создания таблицы.
#[derive(Debug, Serialize)]
struct CreateColumnView {
    name: String,
    type_name: String,
    nullable: bool,
    pk: bool,
    default_value: String,
}

impl CreateFormView {
    /// Пустая форма: схема `public`, [`CREATE_COLUMNS`] пустых строк.
    fn empty() -> Self {
        Self::from_form(&HashMap::new(), false)
    }

    fn from_form(form: &HashMap<String, String>, open: bool) -> Self {
        let schema = form
            .get("schema")
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .unwrap_or("public")
            .to_string();
        let table = form
            .get("table")
            .map(|value| value.trim().to_string())
            .unwrap_or_default();
        let columns = (0..CREATE_COLUMNS)
            .map(|index| CreateColumnView {
                name: form
                    .get(&format!("col_name_{index}"))
                    .map(|value| value.trim().to_string())
                    .unwrap_or_default(),
                type_name: form
                    .get(&format!("col_type_{index}"))
                    .map(|value| value.trim().to_string())
                    .unwrap_or_default(),
                nullable: form.contains_key(&format!("col_nullable_{index}")),
                pk: form.contains_key(&format!("col_pk_{index}")),
                default_value: form
                    .get(&format!("col_default_{index}"))
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect();
        Self {
            schema,
            table,
            columns,
            open,
        }
    }
}

/// Проверенная колонка новой таблицы.
#[derive(Debug)]
struct ColumnDraft {
    name: String,
    data_type: String,
    nullable: bool,
    pk: bool,
    default: Option<String>,
}

/// Разбирает строки формы создания: имя/тип/default валидируются, пустые строки
/// пропускаются; хотя бы одна колонка обязательна.
fn parse_create_columns(form: &HashMap<String, String>) -> Result<Vec<ColumnDraft>, String> {
    let mut columns = Vec::new();
    for index in 0..CREATE_COLUMNS {
        let name = form
            .get(&format!("col_name_{index}"))
            .map(|value| value.trim())
            .unwrap_or("");
        let type_raw = form
            .get(&format!("col_type_{index}"))
            .map(|value| value.trim())
            .unwrap_or("");
        let default_raw = form
            .get(&format!("col_default_{index}"))
            .map(String::as_str)
            .unwrap_or("");
        let nullable = form.contains_key(&format!("col_nullable_{index}"));
        let pk = form.contains_key(&format!("col_pk_{index}"));
        let position = index + 1;
        if name.is_empty() {
            if !type_raw.is_empty() || !default_raw.trim().is_empty() || pk {
                return Err(format!("Колонка {position}: укажите имя."));
            }
            continue;
        }
        validate_ident(name).map_err(|message| format!("Колонка {position}: {message}"))?;
        if columns
            .iter()
            .any(|column: &ColumnDraft| column.name == name)
        {
            return Err(format!("Колонка «{name}» указана дважды."));
        }
        let data_type = parse_column_type(type_raw)
            .map_err(|message| format!("Колонка «{name}»: {message}"))?;
        let default =
            parse_default(default_raw).map_err(|message| format!("Колонка «{name}»: {message}"))?;
        columns.push(ColumnDraft {
            name: name.to_string(),
            data_type,
            nullable,
            pk,
            default,
        });
    }
    if columns.is_empty() {
        return Err("Добавьте хотя бы одну колонку.".into());
    }
    Ok(columns)
}

/// Идентификатор: `^[a-z_][a-z0-9_]*$` и не длиннее 63 символов.
fn validate_ident(raw: &str) -> Result<(), String> {
    let message = "Имя: только латинские строчные буквы, цифры и _, первый символ — буква или _.";
    let mut chars = raw.chars();
    let first = chars
        .next()
        .ok_or_else(|| "Имя не может быть пустым.".to_string())?;
    if !(first.is_ascii_lowercase() || first == '_') {
        return Err(message.into());
    }
    if !chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_') {
        return Err(message.into());
    }
    if raw.chars().count() > MAX_IDENT_CHARS {
        return Err(format!(
            "Имя длиннее {MAX_IDENT_CHARS} символов — Postgres его обрежет."
        ));
    }
    Ok(())
}

/// Тип колонки только из allowlist; возвращает нормализованный SQL-тип.
fn parse_column_type(raw: &str) -> Result<String, String> {
    let compact: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    match compact.as_str() {
        "text" | "integer" | "bigint" | "smallint" | "boolean" | "date" | "timestamp"
        | "timestamptz" | "uuid" | "jsonb" | "bytea" | "numeric" => return Ok(compact),
        _ => {}
    }
    if let Some(args) = compact
        .strip_prefix("varchar(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let length = parse_unsigned(args).ok_or_else(|| {
            format!("Недопустимая длина varchar «{args}»: нужно целое от 1 до 10485760.")
        })?;
        if !(1..=10_485_760).contains(&length) {
            return Err("Длина varchar должна быть от 1 до 10485760.".into());
        }
        return Ok(format!("varchar({length})"));
    }
    if let Some(args) = compact
        .strip_prefix("numeric(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let mut parts = args.split(',');
        let precision = parts
            .next()
            .and_then(parse_unsigned)
            .ok_or_else(|| "numeric: точность — целое от 1 до 1000.".to_string())?;
        let scale = match parts.next() {
            Some(raw) => Some(
                parse_unsigned(raw).ok_or_else(|| "numeric: масштаб — целое от 0.".to_string())?,
            ),
            None => None,
        };
        if parts.next().is_some() {
            return Err("numeric: ожидается numeric(p,s).".into());
        }
        if !(1..=1000).contains(&precision) {
            return Err("numeric: точность должна быть от 1 до 1000.".into());
        }
        let scale = scale.unwrap_or(0);
        if scale > precision {
            return Err("numeric: масштаб не может быть больше точности.".into());
        }
        return Ok(format!("numeric({precision},{scale})"));
    }
    Err(format!(
        "Недопустимый тип «{raw}». Разрешены: {}.",
        COLUMN_TYPES.join(", ")
    ))
}

/// Неотрицательное целое без знака и пробелов.
fn parse_unsigned(raw: &str) -> Option<u32> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// Default только из безопасного allowlist; возвращает SQL-литерал.
fn parse_default(raw: &str) -> Result<Option<String>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.eq_ignore_ascii_case("null") {
        return Ok(Some("NULL".into()));
    }
    if raw.eq_ignore_ascii_case("true") {
        return Ok(Some("true".into()));
    }
    if raw.eq_ignore_ascii_case("false") {
        return Ok(Some("false".into()));
    }
    if raw.eq_ignore_ascii_case("now()") {
        return Ok(Some("now()".into()));
    }
    if raw.eq_ignore_ascii_case("current_timestamp") {
        return Ok(Some("CURRENT_TIMESTAMP".into()));
    }
    if is_numeric_literal(raw) {
        return Ok(Some(raw.to_string()));
    }
    // Строка в одинарных кавычках: кавычки внутри экранируются удвоением.
    if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        let inner = &raw[1..raw.len() - 1];
        return Ok(Some(format!("'{}'", inner.replace('\'', "''"))));
    }
    Err(format!(
        "Недопустимое значение по умолчанию «{raw}». Разрешены NULL, true/false, \
         целые и дробные числа, строки в одинарных кавычках, now() и CURRENT_TIMESTAMP."
    ))
}

/// Целое или дробное число без знака `+`, экспоненты и прочих форм.
fn is_numeric_literal(raw: &str) -> bool {
    let body = raw.strip_prefix('-').unwrap_or(raw);
    let mut parts = body.split('.');
    let integer = parts.next().unwrap_or("");
    let fraction = parts.next();
    if parts.next().is_some() {
        return false;
    }
    if integer.is_empty() || !integer.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match fraction {
        None => true,
        Some(fraction) => {
            !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit())
        }
    }
}

/// `CREATE TABLE "schema"."table" (...)` с экранированными идентификаторами.
fn create_table_sql(schema: &str, table: &str, columns: &[ColumnDraft]) -> String {
    let definitions = columns
        .iter()
        .map(|column| {
            let mut definition = format!("{} {}", pg::quote_ident(&column.name), column.data_type);
            if !column.nullable && !column.pk {
                definition.push_str(" NOT NULL");
            }
            if let Some(default) = &column.default {
                definition.push_str(&format!(" DEFAULT {default}"));
            }
            if column.pk {
                definition.push_str(" PRIMARY KEY");
            }
            definition
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE TABLE {}.{} ({definitions})",
        pg::quote_ident(schema),
        pg::quote_ident(table)
    )
}

/// Выполняет DDL в транзакции с таймаутом; ошибка откатывает изменение.
async fn execute_ddl(state: &AppState, statement: &str) -> Result<(), sqlx::Error> {
    let mut tx = state.db.begin().await?;
    let outcome: Result<(), sqlx::Error> = async {
        sqlx::query(AssertSqlSafe(format!(
            "SET LOCAL statement_timeout = '{DDL_TIMEOUT}'"
        )))
        .execute(&mut *tx)
        .await?;
        // Строковые default экранируют кавычки удвоением; режим с обратным слэшем
        // выключен, чтобы `\` в литерале не мог закрыть строку.
        sqlx::query("SET LOCAL standard_conforming_strings = on")
            .execute(&mut *tx)
            .await?;
        sqlx::query(AssertSqlSafe(statement.to_string()))
            .execute(&mut *tx)
            .await?;
        Ok(())
    }
    .await;
    match outcome {
        Ok(()) => {
            tx.commit().await?;
            Ok(())
        }
        Err(error) => {
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}

/// Пишет в журнал `db-table-*`/`db-column-*` с таблицей и подробностями.
async fn audit_ddl(state: &AppState, user: &Access, action: &str, table: &str, details: &str) {
    let summary = if details.is_empty() {
        table.to_string()
    } else {
        format!("{table}; {details}")
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

/// [`audit_ddl`] для записи, известной из каталога.
async fn record_ddl(
    state: &AppState,
    user: &Access,
    action: &str,
    info: &TableInfo,
    details: &str,
) {
    audit_ddl(
        state,
        user,
        action,
        &format!("{}.{}", info.schema, info.name),
        details,
    )
    .await;
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
                    .filter(|(_, column)| !column.pk && !column.generated)
                    .map(|(index, column)| {
                        let submitted = if open {
                            form.update_values.as_ref()
                        } else {
                            None
                        };
                        let value = submitted
                            .and_then(|values| values.get(&column.name))
                            .cloned()
                            .unwrap_or_else(|| row[index].input.clone());
                        let is_null = submitted.map_or(row[index].null, |values| {
                            values.contains_key(&format!("null_{}", column.name))
                        });
                        FieldView::new(column, value, false, is_null)
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
            .filter(|column| !column.generated)
            .map(|column| {
                let submitted = form.insert_values.as_ref();
                let value = submitted
                    .and_then(|values| values.get(&column.name))
                    .cloned()
                    .unwrap_or_default();
                let is_null = submitted
                    .is_some_and(|values| values.contains_key(&format!("null_{}", column.name)));
                FieldView::new(column, value, true, is_null)
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
            is_table,
            ddl_types => COLUMN_TYPES,
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
    /// У колонки есть галочка `null_<имя>`: она отличает NULL от пустой строки.
    nullable: bool,
    /// Текущее значение — NULL, галочка отмечена.
    is_null: bool,
}

impl FieldView {
    fn new(column: &ColumnInfo, value: String, insert: bool, is_null: bool) -> Self {
        let (input_type, boolean, multiline) = match pg::input_kind(column) {
            InputKind::Number => ("number", false, false),
            InputKind::Date => ("date", false, false),
            InputKind::Boolean => ("text", true, false),
            InputKind::Multiline => ("text", false, true),
            InputKind::Text => ("text", false, false),
        };
        let hint = hint(column, insert);
        Self {
            name: column.name.clone(),
            input_type,
            step: pg::input_step(column),
            boolean,
            multiline,
            value,
            hint,
            nullable: column.nullable,
            is_null,
        }
    }
}

/// Подсказка под полем: что означает пустой ввод и галочка NULL.
fn hint(column: &ColumnInfo, insert: bool) -> String {
    if pg::is_textual(column) {
        if !column.nullable && column.default.is_some() {
            return format!(
                "пусто = DEFAULT ({})",
                pg::truncate(column.default.as_deref().unwrap_or_default(), 60)
            );
        }
        if column.nullable {
            return "пусто = пустая строка; галочка NULL — NULL".into();
        }
        return "пусто = пустая строка".into();
    }
    if insert && column.default.is_some() {
        return format!(
            "пусто = DEFAULT ({})",
            pg::truncate(column.default.as_deref().unwrap_or_default(), 60)
        );
    }
    if insert && column.identity_default {
        return "пусто = DEFAULT".into();
    }
    if column.nullable {
        return "пусто = NULL".into();
    }
    if !insert && (column.default.is_some() || column.identity_default) {
        return "пусто = DEFAULT".into();
    }
    "обязательно".into()
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
///
/// `null_checked` — галочка `null_<колонка>`: пустой ввод с ней пишет NULL.
/// Identity `BY DEFAULT` ведёт себя как колонка с default: пусто — сервер сам
/// подставит значение, явное значение разрешено.
fn insert_bound(
    column: &ColumnInfo,
    raw: Option<&str>,
    null_checked: bool,
) -> Result<Bound, String> {
    if null_checked {
        return if column.nullable {
            Ok(Bound::Null)
        } else {
            Err(format!("Колонка «{}» не допускает NULL", column.name))
        };
    }
    let raw = raw.unwrap_or_default();
    if is_blank(column, raw) {
        // Пустая строка в текстовой колонке — это значение `''`: DEFAULT для
        // nullable-колонки не подставляем, иначе нельзя сохранить пустую строку.
        if pg::is_textual(column) {
            if !column.nullable && column.default.is_some() {
                return Ok(Bound::Default);
            }
            return Ok(Bound::Value(String::new()));
        }
        if column.default.is_some() || column.identity_default {
            return Ok(Bound::Default);
        }
        if column.nullable {
            return Ok(Bound::Null);
        }
        return Err(format!("Колонка «{}» обязательна", column.name));
    }
    Ok(Bound::Value(normalized(column, raw)))
}

/// Пустой ввод при правке: пустая строка, NULL, DEFAULT или ошибка.
///
/// Пустая строка в текстовой колонке — это значение `''`, а не NULL: иначе
/// сохранение нетронутой строки молча меняло бы данные. NULL ставится только
/// галочкой `null_<колонка>`.
fn update_bound(
    column: &ColumnInfo,
    raw: Option<&str>,
    null_checked: bool,
) -> Result<Bound, String> {
    if null_checked {
        return if column.nullable {
            Ok(Bound::Null)
        } else {
            Err(format!("Колонка «{}» не допускает NULL", column.name))
        };
    }
    let raw = raw.unwrap_or_default();
    if is_blank(column, raw) {
        if pg::is_textual(column) {
            if !column.nullable && column.default.is_some() {
                return Ok(Bound::Default);
            }
            return Ok(Bound::Value(String::new()));
        }
        if column.nullable {
            return Ok(Bound::Null);
        }
        if column.default.is_some() || column.identity_default {
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
        // Значение generated-колонки вычисляет сервер: в INSERT его не передаём.
        if column.generated {
            continue;
        }
        match insert_bound(
            column,
            form.get(&column.name).map(String::as_str),
            form.contains_key(&format!("null_{}", column.name)),
        )? {
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
    for column in columns
        .iter()
        .filter(|column| !column.pk && !column.generated)
    {
        match update_bound(
            column,
            form.get(&column.name).map(String::as_str),
            form.contains_key(&format!("null_{}", column.name)),
        )? {
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
    /// Значение вычисляет сервер — править нельзя.
    generated: bool,
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
            generated: column.generated,
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

    /// Существует ли отношение в каталоге.
    async fn table_exists(db: &PgPool, schema: &str, table: &str) -> bool {
        sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_class c
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1 AND c.relname = $2
             )",
        )
        .bind(schema)
        .bind(table)
        .fetch_one(db)
        .await
        .unwrap()
    }

    /// Колонки таблицы в порядке объявления.
    async fn column_names(db: &PgPool, schema: &str, table: &str) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT a.attname
               FROM pg_attribute a
               JOIN pg_class c ON c.oid = a.attrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = $1 AND c.relname = $2
                AND a.attnum > 0 AND NOT a.attisdropped
              ORDER BY a.attnum",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(db)
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
            body.contains("Создать таблицу"),
            "нет формы создания таблицы: {body}"
        );
        assert!(
            body.contains("name=\"col_name_4\""),
            "форма создания должна содержать пять строк колонок: {body}"
        );
        assert!(
            body.contains("value=\"public\""),
            "схема public не выбрана по умолчанию: {body}"
        );
        assert!(
            body.contains("id=\"ddl-types\""),
            "нет подсказок типов: {body}"
        );
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

    #[sqlx::test]
    async fn generated_columns_are_read_only(db: PgPool) {
        sqlx::query(
            "CREATE TABLE tools_generated (
                 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                 name text NOT NULL,
                 name_len integer GENERATED ALWAYS AS (length(name)) STORED
             )",
        )
        .execute(&db)
        .await
        .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, "/admin/tools/db/public/tools_generated", &cookie).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("вычисляется"),
            "нет пометки generated: {body}"
        );
        assert!(
            !body.contains("name=\"name_len\""),
            "generated-колонка попала в форму: {body}"
        );

        // Вставка: id (identity ALWAYS) и name_len вычисляются сервером.
        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_generated/insert",
            &cookie,
            &[("name", "Альфа")],
        )
        .await;
        assert_eq!(status, 303, "вставка с generated не прошла: {body}");
        let (id, name, name_len): (i64, String, i32) =
            sqlx::query_as("SELECT id, name, name_len FROM tools_generated")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!((id, name.as_str(), name_len), (1, "Альфа", 5));

        // Правка обычной колонки не должна попадать в generated.
        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_generated/update",
            &cookie,
            &[("id", "1"), ("name", "Бета")],
        )
        .await;
        assert_eq!(status, 303, "правка с generated не прошла: {body}");
        let (name, name_len): (String, i32) =
            sqlx::query_as("SELECT name, name_len FROM tools_generated WHERE id = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!((name.as_str(), name_len), ("Бета", 4));
    }

    #[sqlx::test]
    async fn blank_text_inputs_follow_hints(db: PgPool) {
        sqlx::query(
            "CREATE TABLE tools_blank (
                 id bigint PRIMARY KEY,
                 opt text,
                 req text NOT NULL DEFAULT 'fallback',
                 plain text NOT NULL
             )",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO tools_blank (id, opt, req, plain) VALUES (1, 'x', 'y', 'z')")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, "/admin/tools/db/public/tools_blank", &cookie).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("галочка NULL"),
            "нет подсказки про галочку NULL: {body}"
        );
        // pg_get_expr отдаёт default с приведением типа: 'fallback'::text.
        assert!(
            body.contains("пусто = DEFAULT (") && body.contains("fallback"),
            "нет подсказки DEFAULT: {body}"
        );
        assert!(
            body.contains("пусто = пустая строка"),
            "нет подсказки пустой строки: {body}"
        );

        // Правка: nullable text пусто → '', not-null text с default пусто → DEFAULT,
        // not-null text без default пусто → пустая строка.
        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_blank/update",
            &cookie,
            &[("id", "1"), ("opt", ""), ("req", ""), ("plain", "")],
        )
        .await;
        assert_eq!(status, 303, "{body}");
        let (opt, req, plain): (Option<String>, String, String) =
            sqlx::query_as("SELECT opt, req, plain FROM tools_blank WHERE id = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            opt.as_deref(),
            Some(""),
            "nullable text пусто должен стать ''"
        );
        assert_eq!(req, "fallback", "default text пусто должен дать DEFAULT");
        assert_eq!(plain, "");

        // Вставка: nullable text пусто → '', default → DEFAULT, без default → ''.
        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_blank/insert",
            &cookie,
            &[("id", "2"), ("opt", ""), ("req", ""), ("plain", "")],
        )
        .await;
        assert_eq!(status, 303, "{body}");
        let (opt, req, plain): (Option<String>, String, String) =
            sqlx::query_as("SELECT opt, req, plain FROM tools_blank WHERE id = 2")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(opt.as_deref(), Some(""));
        assert_eq!(req, "fallback");
        assert_eq!(plain, "");
    }

    #[sqlx::test]
    async fn identity_by_default_allows_blank_and_explicit(db: PgPool) {
        sqlx::query(
            "CREATE TABLE tools_identity (
                 id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
                 name text NOT NULL
             )",
        )
        .execute(&db)
        .await
        .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        // identity BY DEFAULT — обычная колонка формы: значение можно не указывать.
        let (status, body) = get(&app, "/admin/tools/db/public/tools_identity", &cookie).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("name=\"id\""),
            "identity BY DEFAULT не попала в форму вставки: {body}"
        );

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_identity/insert",
            &cookie,
            &[("id", ""), ("name", "Первый")],
        )
        .await;
        assert_eq!(
            status, 303,
            "пустой identity BY DEFAULT не вставился: {body}"
        );
        let (id, name): (i64, String) =
            sqlx::query_as("SELECT id, name FROM tools_identity ORDER BY id")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!((id, name.as_str()), (1, "Первый"));

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_identity/insert",
            &cookie,
            &[("id", "50"), ("name", "Второй")],
        )
        .await;
        assert_eq!(
            status, 303,
            "явный identity BY DEFAULT не вставился: {body}"
        );
        let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM tools_identity ORDER BY id")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(ids, [1, 50]);
    }

    #[sqlx::test]
    async fn empty_text_round_trips_and_null_is_explicit(db: PgPool) {
        sqlx::query(
            "CREATE TABLE tools_nullable (
                 id bigint PRIMARY KEY,
                 note text,
                 req text NOT NULL DEFAULT 'fallback',
                 opt_def text DEFAULT 'default-text'
             )",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO tools_nullable (id, note, req) VALUES (1, '', 'kept')")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        // Пустая строка в БД не должна выглядеть как NULL: галочка снята.
        let (status, body) = get(&app, "/admin/tools/db/public/tools_nullable", &cookie).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("галочка NULL"),
            "нет подсказки про галочку NULL: {body}"
        );
        assert!(
            !body.contains("name=\"null_note\" value=\"1\" checked"),
            "пустая строка показана как NULL: {body}"
        );

        // Сохранение без изменений: пустая строка остаётся пустой строкой,
        // not-null с default по-прежнему получает DEFAULT.
        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_nullable/update",
            &cookie,
            &[("id", "1"), ("note", ""), ("req", ""), ("opt_def", "")],
        )
        .await;
        assert_eq!(status, 303, "{body}");
        let (note, req, opt_def): (Option<String>, String, Option<String>) =
            sqlx::query_as("SELECT note, req, opt_def FROM tools_nullable WHERE id = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(note.as_deref(), Some(""), "пустая строка стала NULL");
        assert_eq!(req, "fallback", "not-null с default не получил DEFAULT");
        assert_eq!(
            opt_def.as_deref(),
            Some(""),
            "nullable text с default не сохранил пустую строку"
        );

        // Вставка пустого nullable text — пустая строка, а не NULL/DEFAULT.
        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_nullable/insert",
            &cookie,
            &[("id", "2"), ("note", ""), ("opt_def", "")],
        )
        .await;
        assert_eq!(status, 303, "{body}");
        let (note, opt_def): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT note, opt_def FROM tools_nullable WHERE id = 2")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(note.as_deref(), Some(""), "вставка записала NULL вместо ''");
        assert_eq!(
            opt_def.as_deref(),
            Some(""),
            "nullable text с default получил DEFAULT вместо ''"
        );

        // Галочка NULL — явный NULL.
        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_nullable/insert",
            &cookie,
            &[("id", "3"), ("note", ""), ("null_note", "1")],
        )
        .await;
        assert_eq!(status, 303, "{body}");
        let note: Option<String> =
            sqlx::query_scalar("SELECT note FROM tools_nullable WHERE id = 3")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(note, None, "галочка NULL не записала NULL");

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_nullable/update",
            &cookie,
            &[
                ("id", "1"),
                ("note", ""),
                ("null_note", "1"),
                ("req", "kept"),
            ],
        )
        .await;
        assert_eq!(status, 303, "{body}");
        let note: Option<String> =
            sqlx::query_scalar("SELECT note FROM tools_nullable WHERE id = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(note, None, "галочка NULL при правке не записала NULL");

        let (_, body) = get(&app, "/admin/tools/db/public/tools_nullable", &cookie).await;
        assert!(
            body.contains("name=\"null_note\" value=\"1\" checked"),
            "NULL-значение не отмечено галочкой: {body}"
        );
    }

    #[sqlx::test]
    async fn create_table_with_types_defaults_and_pk(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post(
            &app,
            "/admin/tools/db/create",
            &cookie,
            &[
                ("schema", "public"),
                ("table", "tools_ddl"),
                ("col_name_0", "id"),
                ("col_type_0", "bigint"),
                ("col_pk_0", "1"),
                ("col_name_1", "name"),
                ("col_type_1", "varchar(20)"),
                ("col_nullable_1", "1"),
                ("col_default_1", "'без имени'"),
                ("col_name_2", "amount"),
                ("col_type_2", "numeric(10,2)"),
                ("col_nullable_2", "1"),
                ("col_name_3", "created"),
                ("col_type_3", "timestamptz"),
                ("col_nullable_3", "1"),
                ("col_default_3", "now()"),
                ("col_name_4", "author"),
                ("col_type_4", "text"),
                ("col_nullable_4", "1"),
                ("col_default_4", "'О'Рейли'"),
            ],
        )
        .await;
        assert_eq!(status, 303, "таблица не создана: {body}");
        assert!(table_exists(&db, "public", "tools_ddl").await);

        let columns: Vec<(String, String, bool, Option<String>)> = sqlx::query_as(
            "SELECT a.attname,
                    format_type(a.atttypid, a.atttypmod),
                    NOT a.attnotnull,
                    pg_get_expr(d.adbin, d.adrelid)
               FROM pg_attribute a
               LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
              WHERE a.attrelid = 'public.tools_ddl'::regclass
                AND a.attnum > 0 AND NOT a.attisdropped
              ORDER BY a.attnum",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        let names: Vec<&str> = columns.iter().map(|row| row.0.as_str()).collect();
        assert_eq!(names, ["id", "name", "amount", "created", "author"]);
        assert_eq!(columns[0].1, "bigint");
        assert!(!columns[0].2, "PK-колонка не должна быть nullable");
        assert_eq!(columns[1].1, "character varying(20)");
        assert!(columns[1].2, "колонка без PK должна быть nullable");
        assert!(
            columns[1]
                .3
                .as_deref()
                .is_some_and(|d| d.contains("без имени")),
            "default строки не применился: {:?}",
            columns[1].3
        );
        assert_eq!(columns[2].1, "numeric(10,2)");
        assert_eq!(columns[3].1, "timestamp with time zone");
        assert!(
            columns[3].3.as_deref().is_some_and(|d| d.contains("now()")),
            "default now() не применился: {:?}",
            columns[3].3
        );

        // PK отмечен на колонке, а не отдельным ограничением таблицы.
        let pk_columns: Vec<String> = sqlx::query_scalar(
            "SELECT a.attname
               FROM pg_constraint con
               CROSS JOIN LATERAL unnest(con.conkey) AS k(attnum)
               JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum
              WHERE con.conrelid = 'public.tools_ddl'::regclass
                AND con.contype = 'p'",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(pk_columns, ["id"]);

        // Значения по умолчанию, включая экранированную кавычку, работают.
        sqlx::query("INSERT INTO tools_ddl (id) VALUES (1)")
            .execute(&db)
            .await
            .unwrap();
        let (name, author, created): (String, String, Option<String>) =
            sqlx::query_as("SELECT name, author, created::text FROM tools_ddl WHERE id = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(name, "без имени");
        assert_eq!(author, "О'Рейли");
        assert!(created.is_some(), "timestamptz default now() не сработал");

        let (status, body) = get(&app, "/admin/tools/db/public/tools_ddl", &cookie).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("created"),
            "нет новой таблицы на странице: {body}"
        );
        assert!(body.contains("timestamptz"), "нет типа колонки: {body}");
        assert!(
            body.contains("/column-add"),
            "нет формы добавления колонки: {body}"
        );
        assert!(
            body.contains("/column-drop"),
            "нет формы удаления колонки: {body}"
        );
        assert!(body.contains("/drop"), "нет формы удаления таблицы: {body}");

        let (action, entity, summary) = last_audit(&db).await;
        assert_eq!(action, "db-table-create");
        assert_eq!(entity, "tools");
        assert!(summary.contains("public.tools_ddl"), "{summary}");
    }

    #[sqlx::test]
    async fn create_uses_existing_user_schema(db: PgPool) {
        sqlx::query("CREATE SCHEMA tools_schema")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post(
            &app,
            "/admin/tools/db/create",
            &cookie,
            &[
                ("schema", "tools_schema"),
                ("table", "tools_in_schema"),
                ("col_name_0", "id"),
                ("col_type_0", "integer"),
                ("col_pk_0", "1"),
                ("col_name_1", "note"),
                ("col_type_1", "text"),
                ("col_nullable_1", "1"),
                ("col_default_1", "NULL"),
            ],
        )
        .await;
        assert_eq!(status, 303, "таблица в своей схеме не создана: {body}");
        assert!(table_exists(&db, "tools_schema", "tools_in_schema").await);

        // Несуществующая схема — 404, таблицы нет.
        let (status, _) = post(
            &app,
            "/admin/tools/db/create",
            &cookie,
            &[
                ("schema", "no_such_schema"),
                ("table", "tools_ghost"),
                ("col_name_0", "id"),
                ("col_type_0", "integer"),
            ],
        )
        .await;
        assert_eq!(status, 404, "несуществующая схема должна быть 404");
        assert!(!table_exists(&db, "no_such_schema", "tools_ghost").await);
    }

    #[sqlx::test]
    async fn invalid_create_input_shows_alert_and_creates_nothing(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let valid = [
            ("schema", "public"),
            ("table", "tools_bad"),
            ("col_name_0", "id"),
            ("col_type_0", "integer"),
        ];
        let cases: [(&str, Vec<(&str, &str)>); 7] = [
            (
                "неверное имя таблицы",
                vec![
                    ("schema", "public"),
                    ("table", "bad-name"),
                    ("col_name_0", "id"),
                    ("col_type_0", "integer"),
                ],
            ),
            (
                "неверное имя колонки",
                vec![
                    ("schema", "public"),
                    ("table", "tools_bad"),
                    ("col_name_0", "1bad"),
                    ("col_type_0", "integer"),
                ],
            ),
            (
                "тип вне allowlist",
                vec![
                    ("schema", "public"),
                    ("table", "tools_bad"),
                    ("col_name_0", "id"),
                    ("col_type_0", "serial"),
                ],
            ),
            (
                "инъекция в тип",
                vec![
                    ("schema", "public"),
                    ("table", "tools_bad"),
                    ("col_name_0", "id"),
                    ("col_type_0", "text); DROP TABLE tools_test; --"),
                ],
            ),
            (
                "default вне allowlist",
                vec![
                    ("schema", "public"),
                    ("table", "tools_bad"),
                    ("col_name_0", "id"),
                    ("col_type_0", "integer"),
                    ("col_default_0", "nextval('seq')"),
                ],
            ),
            (
                "незакрытая строка default",
                vec![
                    ("schema", "public"),
                    ("table", "tools_bad"),
                    ("col_name_0", "id"),
                    ("col_type_0", "text"),
                    ("col_default_0", "'x"),
                ],
            ),
            (
                "нет колонок",
                vec![("schema", "public"), ("table", "tools_bad")],
            ),
        ];

        for (label, pairs) in cases {
            let (status, body) = post(&app, "/admin/tools/db/create", &cookie, &pairs).await;
            assert_eq!(status, 200, "{label}: ожидался alert: {body}");
            assert!(body.contains("Ошибка"), "{label}: нет alert: {body}");
            assert!(
                !table_exists(&db, "public", "tools_bad").await,
                "{label}: таблица создана вопреки ошибке"
            );
        }

        // Дубликат таблицы — ошибка БД на странице без изменения старой таблицы.
        sqlx::query("CREATE TABLE tools_bad (id integer PRIMARY KEY)")
            .execute(&db)
            .await
            .unwrap();
        let mut pairs = valid.to_vec();
        pairs[1] = ("table", "tools_bad");
        let (status, body) = post(&app, "/admin/tools/db/create", &cookie, &pairs).await;
        assert_eq!(status, 200, "дубликат должен дать alert: {body}");
        assert!(body.contains("Ошибка"), "нет alert дубликата: {body}");
        assert_eq!(column_names(&db, "public", "tools_bad").await, ["id"]);
    }

    #[sqlx::test]
    async fn add_and_drop_column_with_confirmation(db: PgPool) {
        sqlx::query("CREATE TABLE tools_ddl (id bigint PRIMARY KEY)")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_ddl/column-add",
            &cookie,
            &[
                ("name", "note"),
                ("type", "varchar(30)"),
                ("nullable", "1"),
                ("default", "'x'"),
            ],
        )
        .await;
        assert_eq!(status, 303, "колонка не добавлена: {body}");
        assert_eq!(
            column_names(&db, "public", "tools_ddl").await,
            ["id", "note"]
        );

        let (action, entity, summary) = last_audit(&db).await;
        assert_eq!(action, "db-column-add");
        assert_eq!(entity, "tools");
        assert!(summary.contains("public.tools_ddl"), "{summary}");
        assert!(summary.contains("note"), "{summary}");

        let (status, body) = get(&app, "/admin/tools/db/public/tools_ddl", &cookie).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("note"), "новая колонка не видна: {body}");

        // Неверное подтверждение — alert 200, колонка жива.
        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_ddl/column-drop",
            &cookie,
            &[("name", "note"), ("confirm", "nope")],
        )
        .await;
        assert_eq!(status, 200, "неверный confirm: {body}");
        assert!(body.contains("Ошибка"), "нет alert: {body}");
        assert_eq!(
            column_names(&db, "public", "tools_ddl").await,
            ["id", "note"],
            "колонка удалена при неверном подтверждении"
        );

        // Неверное имя колонки — alert 200.
        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_ddl/column-add",
            &cookie,
            &[("name", "bad-name"), ("type", "text"), ("nullable", "1")],
        )
        .await;
        assert_eq!(status, 200, "неверное имя колонки: {body}");
        assert!(body.contains("Ошибка"), "нет alert: {body}");

        // Неизвестная колонка — 404.
        let (status, _) = post(
            &app,
            "/admin/tools/db/public/tools_ddl/column-drop",
            &cookie,
            &[("name", "ghost"), ("confirm", "ghost")],
        )
        .await;
        assert_eq!(status, 404, "несуществующая колонка должна быть 404");

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_ddl/column-drop",
            &cookie,
            &[("name", "note"), ("confirm", "note")],
        )
        .await;
        assert_eq!(status, 303, "колонка не удалена: {body}");
        assert_eq!(column_names(&db, "public", "tools_ddl").await, ["id"]);

        let (action, entity, summary) = last_audit(&db).await;
        assert_eq!(action, "db-column-drop");
        assert_eq!(entity, "tools");
        assert!(summary.contains("public.tools_ddl"), "{summary}");
        assert!(summary.contains("note"), "{summary}");

        // Нет таблицы — 404.
        let (status, _) = post(
            &app,
            "/admin/tools/db/public/tools_missing/column-add",
            &cookie,
            &[("name", "note"), ("type", "text")],
        )
        .await;
        assert_eq!(status, 404);
    }

    #[sqlx::test]
    async fn drop_table_requires_exact_name(db: PgPool) {
        sqlx::query("CREATE TABLE tools_ddl (id bigint PRIMARY KEY)")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_ddl/drop",
            &cookie,
            &[("confirm", "tools_dd")],
        )
        .await;
        assert_eq!(status, 200, "неверный confirm: {body}");
        assert!(body.contains("Ошибка"), "нет alert: {body}");
        assert!(
            table_exists(&db, "public", "tools_ddl").await,
            "таблица удалена при неверном подтверждении"
        );

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_ddl/drop",
            &cookie,
            &[("confirm", "tools_ddl")],
        )
        .await;
        assert_eq!(status, 303, "таблица не удалена: {body}");
        assert!(!table_exists(&db, "public", "tools_ddl").await);

        let (status, _) = get(&app, "/admin/tools/db/public/tools_ddl", &cookie).await;
        assert_eq!(status, 404, "страница удалённой таблицы должна быть 404");

        let (action, entity, summary) = last_audit(&db).await;
        assert_eq!(action, "db-table-drop");
        assert_eq!(entity, "tools");
        assert!(summary.contains("public.tools_ddl"), "{summary}");

        // Системные схемы и вью удалять нельзя.
        let (status, _) = post(
            &app,
            "/admin/tools/db/pg_catalog/pg_class/drop",
            &cookie,
            &[("confirm", "pg_class")],
        )
        .await;
        assert_eq!(status, 404);
        let (status, _) = post(
            &app,
            "/admin/tools/db/public/tools_ddl/drop",
            &cookie,
            &[("confirm", "tools_ddl")],
        )
        .await;
        assert_eq!(status, 404, "повторное удаление — 404");
    }

    #[sqlx::test]
    async fn drop_table_with_dependencies_reports_error(db: PgPool) {
        sqlx::query("CREATE TABLE tools_parent (id bigint PRIMARY KEY)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE tools_child (
                 id bigint PRIMARY KEY,
                 parent_id bigint REFERENCES tools_parent(id)
             )",
        )
        .execute(&db)
        .await
        .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post(
            &app,
            "/admin/tools/db/public/tools_parent/drop",
            &cookie,
            &[("confirm", "tools_parent")],
        )
        .await;
        assert_eq!(status, 200, "DROP без CASCADE должен дать alert: {body}");
        assert!(body.contains("Ошибка"), "нет alert: {body}");
        assert!(
            table_exists(&db, "public", "tools_parent").await,
            "таблица с зависимостями удалена"
        );
    }

    #[sqlx::test]
    async fn non_super_cannot_run_ddl(db: PgPool) {
        sqlx::query("CREATE TABLE tools_ddl (id bigint PRIMARY KEY, note text)")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = permitted_cookie(&db, "op", &[]).await;

        for (path, pairs) in [
            (
                "/admin/tools/db/create",
                vec![
                    ("schema", "public"),
                    ("table", "tools_forbidden"),
                    ("col_name_0", "id"),
                    ("col_type_0", "integer"),
                ],
            ),
            (
                "/admin/tools/db/public/tools_ddl/drop",
                vec![("confirm", "tools_ddl")],
            ),
            (
                "/admin/tools/db/public/tools_ddl/column-add",
                vec![("name", "x"), ("type", "text")],
            ),
            (
                "/admin/tools/db/public/tools_ddl/column-drop",
                vec![("name", "note"), ("confirm", "note")],
            ),
        ] {
            let (status, body) = post(&app, path, &cookie, &pairs).await;
            assert_eq!(status, 403, "{path}: {body}");
        }

        assert!(
            table_exists(&db, "public", "tools_ddl").await,
            "таблица удалена не-супером"
        );
        assert_eq!(
            column_names(&db, "public", "tools_ddl").await,
            ["id", "note"]
        );
    }
}
