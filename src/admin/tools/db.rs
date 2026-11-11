//! Обозреватель таблиц: список таблиц/вью, структура и данные (только чтение).
//!
//! Имена схем/таблиц/колонок берутся из каталога PostgreSQL и экранируются;
//! пользовательский ввод задаёт только страницу и сортировку по существующей
//! колонке. Правка строк и DDL — следующие задачи.

use axum::{
    Extension,
    extract::{Path, Query, State},
    response::Html,
};
use minijinja::context;
use serde::{Deserialize, Serialize};

use super::{
    guard,
    pg::{self, ColumnInfo, TableInfo},
    render,
};
use crate::{
    access::Access,
    error::{AppError, AppResult},
    state::AppState,
};

/// Данные всегда по 50 строк.
const PER_PAGE: i64 = 50;
/// Больше 200 строк за страницу не отдаём даже при будущем изменении `per`.
const MAX_PER_PAGE: i64 = 200;
/// Сколько соседних страниц показывать вокруг текущей.
const PAGER_AROUND: i64 = 2;

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
    let info = pg::tables(&state.db)
        .await?
        .into_iter()
        .find(|known| known.schema == schema && known.name == table)
        .ok_or(AppError::NotFound)?;
    let columns = pg::columns(&state.db, &info.schema, &info.name).await?;
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

    let base_url = format!(
        "/admin/tools/db/{}/{}",
        encode_path(&info.schema),
        encode_path(&info.name)
    );
    let columns: Vec<ColumnView> = columns
        .iter()
        .map(|column| ColumnView::new(column, &base_url, sort, dir))
        .collect();
    let total = rows.total;
    let pages = total
        .map(|total| ((total + PER_PAGE - 1) / PER_PAGE).max(1))
        .unwrap_or(1);
    let pager = pager(page_no, pages, &base_url, sort, dir);

    render(
        &state,
        "tools/db_table.html",
        context! {
            user,
            schema => info.schema,
            table => info.name,
            kind_label => kind_label(&info.kind),
            columns,
            data => rows.rows,
            total,
            count_error => rows.count_error,
            page => page_no,
            pages,
            pager,
        },
    )
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
    use sqlx::PgPool;

    use crate::{
        smoke_tests::get,
        test_support::{admin_cookie, permitted_cookie, test_state},
    };

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
}
