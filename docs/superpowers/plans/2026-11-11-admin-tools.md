# Инструменты супер-администратора Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Добавить в «Систему» три инструмента для супер-админов: SQL-консоль, обозреватель таблиц с правкой/DDL и запуск произвольных Rust-скриптов.

**Architecture:** Новый модуль `src/admin/tools/` с подмодулями `sql`, `db`, `rust`, `pg`; маршруты `/admin/tools/*` регистрируются в `src/admin/mod.rs`. Все страницы требуют `Access::require_super()` и выключатель `ADMIN_TOOLS_ENABLED`. SQL и DDL идут через `sqlx` с таймаутами и аудитом; Rust-скрипты собираются во временном crate и запускаются отдельным процессом.

**Tech Stack:** Rust, Axum 0.8, SQLx 0.9/PostgreSQL, MiniJinja, HTMX, `tokio::process`, `cargo`/`rustc`.

**Spec:** `docs/superpowers/specs/2026-11-11-admin-tools-design.md`

## Global Constraints

- Доступ только `Access::require_super()`; группам право не выдаётся.
- `ADMIN_TOOLS_ENABLED=0` → страницы 404 и пункт меню скрыт; по умолчанию `1`.
- SQL-таймаут 15 с, вывод 200 строк/256 КБ; Rust: сборка 180 с, запуск 30 с, вывод 64 КБ.
- Никакого маскирования данных; предупреждение об этом на страницах.
- Все SQL/Rust/DDL/правки строк пишутся в `audit_log` через `crate::audit::record`.
- Идентификаторы таблиц/колонок — только после проверки по каталогу и экранирования; значения — bind-параметрами.
- Русские сообщения интерфейса, коммиты по-русски без упоминания ассистента.

## Review Focus

- SQL, меняющий данные, при снятом подтверждении должен выполняться в `READ ONLY` транзакции и падать, а не менять БД.
- Имя таблицы/колонки с кавычкой или точкой не должно ломать SQL и не должно попадать в запрос без экранирования.
- Пустая таблица, таблица без PK и таблица с составным PK: просмотр работает, правка либо недоступна с объяснением, либо корректна.
- Таймаут SQL/Rust не должен оставлять висеть соединение/процесс; следующий запуск должен работать.
- Не-супер и выключенный `ADMIN_TOOLS_ENABLED` не должны видеть ни меню, ни страницы, ни данные.

---

### Task 1: Каркас инструментов, доступ, меню, миграция

**Files:**
- Create: `migrations/0042_admin_tools.sql`
- Create: `src/admin/tools/mod.rs`
- Modify: `src/admin/mod.rs` (маршруты, `mod tools;`)
- Modify: `src/admin/nav.rs` (`NavCtx.tools_enabled`, пункт «Инструменты»)
- Modify: `src/admin/shell.rs` (прокинуть `tools_enabled` в контекст, если нужно)
- Modify: `src/config.rs`, `src/test_support.rs`
- Create: `templates/admin/tools/sql.html`, `db_list.html`, `rust.html` (заглушки)

**Interfaces:**
- Produces: `crate::admin::tools::enabled() -> bool`, `tools::guard(&Access) -> AppResult<()>`, `tools::router() -> Router<AppState>`.
- Produces: `NavCtx { tools_enabled: bool, .. }`.

- [ ] **Step 1: Написать падающий тест доступа**

В `src/admin/tools/mod.rs` (cfg(test)): супер получает 200 на `/admin/tools/sql`, обычный админ — 403, при `tools_enabled=false` супер получает 404.

- [ ] **Step 2: Запустить тест, убедиться, что маршрутов нет**

Run: `DATABASE_URL=postgres://cms:cms@localhost:5433/cms cargo test admin::tools -q`
Expected: FAIL (404/compile error).

- [ ] **Step 3: Миграция и каркас**

`0042_admin_tools.sql`: `admin_rust_scripts(id BIGSERIAL PK, name TEXT UNIQUE NOT NULL, code TEXT NOT NULL DEFAULT '', created_by BIGINT, updated_by BIGINT, created_at/updated_at TIMESTAMPTZ DEFAULT now())`; `admin_rust_runs(id BIGSERIAL PK, script_id BIGINT REFERENCES admin_rust_scripts ON DELETE SET NULL, user_id BIGINT, user_login TEXT, started_at TIMESTAMPTZ, duration_ms INT, ok BOOL, output TEXT, error TEXT)`, индексы `(script_id, started_at DESC)`.

`Config.tools_enabled` из `ADMIN_TOOLS_ENABLED != "0"`; `test_state` — `true`.
`tools::enabled()` читает `state.config.tools_enabled` в хендлерах; для навигации — `NavCtx.tools_enabled`, выставляется в `build_shell` из `state.config.tools_enabled`.
`nav::build`: если `user.is_super() && ctx.tools_enabled`, добавить в группу `system` пункты `sql` («SQL-запрос»), `tools-db` («Таблицы БД»), `tools-rust` («Rust-скрипты»).

- [ ] **Step 4: Маршруты-заглушки и шаблоны**

`/admin/tools/sql`, `/admin/tools/db`, `/admin/tools/rust` → простые страницы с `{% extends "base.html" %}` и заголовком. `guard` проверяет `enabled()` и `require_super()`.

- [ ] **Step 5: Тест и коммит**

Run: `DATABASE_URL=... cargo test admin::tools -q` → PASS.
Commit: `git add ... && git commit -m "Инструменты: каркас, доступ и меню"`.

### Task 2: SQL-консоль

**Files:**
- Modify: `src/admin/tools/sql.rs`, `templates/admin/tools/sql.html`, `src/admin/tools/mod.rs` (роут POST)
- Test: `src/admin/tools/sql.rs` (cfg(test))

**Interfaces:**
- Produces: `sql::page(State, Extension<Access>) -> AppResult<Html<String>>`
- Produces: `sql::execute(State, Extension<Access>, Form<SqlForm>) -> AppResult<Html<String>>`
- Produces: `SqlResult { columns: Vec<String>, rows: Vec<Vec<String>>, affected: Option<u64>, error: Option<String>, duration_ms: u64 }`.

- [ ] **Step 1: Падающий тест SELECT и ошибки**

Тест: POST `/admin/tools/sql` с `sql=SELECT 1 AS one` → 200, HTML содержит `one` и `1`; с `sql=SELEC 1` → 200, содержит «Ошибка». Проверить запись `audit_log` с `action='sql'`.

- [ ] **Step 2: Запустить — FAIL.**

- [ ] **Step 3: Реализовать выполнение**

`SqlForm { sql: String, confirm: Option<String> }`. Если `confirm` пуст — `BEGIN READ ONLY`, иначе `BEGIN`; `SET LOCAL statement_timeout='15s'`; выбор `fetch_all` для запросов, начинающихся с `select/with/show/explain/values/table` (без учёта регистра), иначе `execute`. Результат: колонки из `PgRow::columns`, значения через `pg::format_value(row, i)`; обрезать 200 строк/256 КБ. Ошибку показывать в alert. Аудит: 500 символов SQL + статус.

- [ ] **Step 4: Шаблон**

Textarea (моноширинный), чекбокс «Запрос может изменить данные», кнопка «Выполнить», таблица результата, блок ошибки, время выполнения. Форма POST с CSRF-полем, как в остальных админ-формах.

- [ ] **Step 5: Тест, коммит**

Run: `cargo test admin::tools::sql -q`.
Commit: `git commit -m "Инструменты: SQL-консоль"`.

### Task 3: Обозреватель таблиц — список, структура, данные

**Files:**
- Create: `src/admin/tools/pg.rs`, `src/admin/tools/db.rs`, `templates/admin/tools/db_list.html`, `db_table.html`
- Modify: `src/admin/tools/mod.rs`

**Interfaces:**
- Produces: `pg::quote_ident(&str) -> String`, `pg::format_value(&PgRow, usize) -> String`, `pg::tables(&PgPool) -> AppResult<Vec<TableInfo>>`, `pg::columns(&PgPool, schema, table) -> AppResult<Vec<ColumnInfo>>`, `pg::rows(&PgPool, schema, table, page, per, sort) -> AppResult<RowsPage>`.
- `TableInfo { schema, name, kind, rows: i64, size: String, columns: i64, has_pk: bool }`
- `ColumnInfo { name, data_type, nullable, default, pk: bool }`

- [ ] **Step 1: Тест списка и чтения**

Создать тестовую таблицу `tools_test(id bigint pk, name text)`, вставить строку; GET `/admin/tools/db` содержит `tools_test`; GET `/admin/tools/db/public/tools_test` содержит `name` и значение. GET с именем `x"; DROP` → 404.

- [ ] **Step 2: FAIL.**

- [ ] **Step 3: Реализовать каталог и чтение**

`pg::tables`: `pg_class`/`pg_namespace`/`pg_stat_user_tables`, исключить `pg_catalog/information_schema/pg_toast`; `pg::columns`: `information_schema.columns` + PK из `pg_index`; `pg::rows`: `SELECT * FROM "schema"."table" ORDER BY "col" LIMIT $per OFFSET $off`, count(*) отдельно с таймаутом. Разрешать только существующие таблицы из `pg::tables`.

- [ ] **Step 4: Шаблоны**

Список таблиц: схема, имя, строки, размер, колонки, PK. Таблица: карточка структуры + таблица данных, сортировка ссылками, пагинация, кнопка «Назад».

- [ ] **Step 5: Тест, коммит.**

### Task 4: Правка строк

**Files:**
- Modify: `src/admin/tools/db.rs`, `db_table.html`
- Test: `db.rs`

**Interfaces:**
- Produces: `db::row_update`, `db::row_insert`, `db::row_delete`; `pg::PrimaryKey` с именами колонок.

- [ ] **Step 1: Тесты вставки/правки/удаления**

Через POST: добавить строку, изменить name, удалить; каждый раз GET показывает результат; строка с несуществующим PK → 404; таблица без PK → форма правки отсутствует.

- [ ] **Step 2: FAIL.**

- [ ] **Step 3: Реализовать**

Определить PK (одна или несколько колонок). Формы: inputs по типам (`pg::input_type`), bind-параметры; `UPDATE ... WHERE pk=$n`; `DELETE ... WHERE pk=$n`; `INSERT (cols) VALUES (bind)`. Для default-колонок пустой ввод = `DEFAULT`. Аудит `db-row-insert/update/delete` с `schema.table` и PK.

- [ ] **Step 4: Шаблон и проверка, коммит.**

### Task 5: DDL

**Files:**
- Modify: `src/admin/tools/db.rs`, `db_table.html`, `db_list.html`
- Test: `db.rs`

**Interfaces:**
- Produces: `db::table_create`, `db::table_drop`, `db::column_add`, `db::column_drop`.

- [ ] **Step 1: Тесты create/drop/add/drop column**

POST создаёт `tools_ddl` с колонками, GET показывает; добавить колонку, удалить; drop с неверным подтверждением → ошибка и таблица жива; удалить с точным именем → 404 на странице.

- [ ] **Step 2: FAIL.**

- [ ] **Step 3: Реализовать**

Типы из allowlist (`text, varchar(n), integer, bigint, numeric(p,s), boolean, date, timestamp, timestamptz, uuid, jsonb`); имя/колонки валидировать regex `^[a-z_][a-z0-9_]*$`; `quote_ident`. Подтверждение drop — ввод имени таблицы. Аудит `db-table-create/drop`, `db-column-add/drop`.

- [ ] **Step 4: Шаблон, тест, коммит.**

### Task 6: Rust-скрипты — хранение и редактор

**Files:**
- Create: `src/admin/tools/rust.rs`, `templates/admin/tools/rust.html`
- Test: `rust.rs`

**Interfaces:**
- Produces: `rust::list`, `rust::edit`, `rust::save`, `rust::run` (запуск в Task 7).
- `RustScript { id, name, code, updated_at }`.

- [ ] **Step 1: Тесты сохранения**

GET `/admin/tools/rust` содержит форму; POST name/code сохраняет и редиректит; пустое имя → ошибка; не-супер 403.

- [ ] **Step 2: FAIL.**

- [ ] **Step 3: Реализовать CRUD**, уникальность имени (`ON CONFLICT`/ошибка), аудит `rust-save`.

- [ ] **Step 4: Шаблон** — список слева, редактор/textarea, «Сохранить», «Запустить», история запусков.

- [ ] **Step 5: Тест, коммит.**

### Task 7: Rust-скрипты — компиляция и запуск

**Files:**
- Create: `src/admin/tools/runner.rs`
- Modify: `rust.rs`, `Cargo.toml` (если нужны фичи)
- Test: `runner.rs`

**Interfaces:**
- Produces: `runner::run(code: &str) -> Result<RunOutput, String>`, `RunOutput { stdout: String, stderr: String, duration_ms: u64 }`.
- Produces: `runner::wrapper(code) -> String`.

- [ ] **Step 1: Тест wrapper и таймаута**

`wrapper("let x = 1;")` содержит `#[tokio::main]`, `PgPool::connect`, код; запуск `loop {}` завершается ошибкой таймаута (тест с коротким таймаутом через параметр/переменную, `#[ignore]`, если нет cargo).

- [ ] **Step 2: FAIL.**

- [ ] **Step 3: Реализовать**

Временный каталог `temp_dir()/cms-rust/<uuid>`, `Cargo.toml` с зависимостями, `src/main.rs` = wrapper; `tokio::process::Command::new("cargo") build --quiet` с `CARGO_TARGET_DIR`; затем бинарник; таймауты, `kill_on_drop`, лимит вывода; mutex; env только `DATABASE_URL/PATH/HOME/CARGO_HOME/RUSTUP_HOME`. Сохранить run в `admin_rust_runs` (обрезка старых), аудит `rust-run`.

- [ ] **Step 4: Тест на реальном скрипте** `println!("ok")` (если toolchain есть) и запись run.

- [ ] **Step 5: Коммит.**

### Task 8: Финальная проверка и документация

**Files:**
- Modify: `docs/design.md` (раздел «Инструменты»), `docs/tech-debt.md` (риски/ограничения)
- Test: все

- [ ] **Step 1: Прогнать `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, полный `cargo test`.**
- [ ] **Step 2: Проверить вручную на dev: SQL SELECT/UPDATE, таблицы, Rust `println!`, снимки.**
- [ ] **Step 3: Обновить документацию и коммит `Инструменты: SQL, таблицы и Rust-скрипты`.**
