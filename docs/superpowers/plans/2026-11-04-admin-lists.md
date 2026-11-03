# Админка, этап 3.1: движок списков — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Перевести списки записей коллекций и заказов на общий серверный движок с фильтрами-чипами, колонками, сортировкой, сохранёнными видами, деревом разделов и клавишами.

**Architecture:** Модуль `src/admin/list/`: спека списка (данные) → состояние из URL (чистые функции) → сборка SQL через `QueryBuilder` (значения только `push_bind`) → общий шаблон `_list.html`. Ответ — фрагмент `#list-body` для HTMX или полная страница. Виды — таблицы `admin_views`/`admin_view_pins`. Поповеры и клавиши — `static/admin/list.js`.

**Tech Stack:** Rust, axum, sqlx (Postgres), MiniJinja, HTMX 2, чистый JS.

**Spec:** `docs/superpowers/specs/2026-11-04-admin-lists-design.md`. Прототип интерфейса: `.superpowers/tmp/list-demo.html` (вне git), https://claude.ai/artifact/Nh4rsvVH7JtEHbw4q8GJTB.

## Global Constraints

- Ветка `feature/admin-lists` (от `main`).
- Сообщения коммитов на русском, в стиле `Админка: …`; без упоминаний ИИ и строк `Co-Authored-By`.
- В текст SQL попадают только выражения из спеки и целые числа (`i64` id коллекции, типа цены, склада); всё из URL — только `push_bind`.
- URL: `f.<code>=<op>[:<value>]`, `sort=<code>|-<code>`, `cols=a,b,c`, `page`, `per` ∈ {20, 50, 100} (по умолчанию 50), `section=<id>|none`, `only=1`, `view=<id>`.
- Операторы: text — `has`, `eq`, `empty`, `notempty`; number/money — `eq`, `ne`, `gt`, `lt`, `between:a,b`, `empty`; date — `today`, `d7`, `d30`, `range:YYYY-MM-DD,YYYY-MM-DD`; enum — `in:a,b`, `nin:a,b`; bool — `eq:Y|N`; link — `eq:<id>`, `empty`, `notempty`.
- `COUNT` ограничен 10 001 строкой; при превышении показывается «10 000+».
- Обязательная колонка: `name` у записей, `number` у заказов.
- Фрагмент — `HX-Request` + `HX-Target` без `HX-Boosted` и `HX-History-Restore-Request` (правило `is_full_page` в `src/admin/mod.rs`).
- Клавиши не срабатывают при открытом поповере, в полях ввода, с Alt/Ctrl/Cmd. Страницы — `{`/`}` (Shift+BracketLeft/Right), `[ ]` заняты вкладками оболочки.
- Остальные списки админки не меняются.

## Review Focus

1. Кириллица и спецсимволы (`%`, `_`, `,`, `:`, `&`) в значении текстового фильтра: разбор → запись → разбор без потерь; `%`/`_` в ILIKE экранируются, а не работают как шаблон. → тест в Task 1 и Task 2.
2. Значение enum, которого больше нет в справочнике (удалили статус или вариант поля), в URL или в виде: отбрасывается только это значение, остальные значения фильтра работают. → тест `drops_garbage` в Task 1 (`in:N,X` → `in:N`).
3. Поле коллекции удалили или сменили ему тип при открытом виде: фильтр отбрасывается, страница не падает с ошибкой приведения типа в SQL (например `'abc'::numeric`). Числовые и датовые выражения по JSONB строятся с безопасным приведением. → тест в Task 4.
4. Пользователь без права на запись открывает общий вид и меняет фильтры: «Сохранить» ему недоступно (кнопки нет и POST даёт 403), «Сохранить как…» создаёт личный вид. → тест в Task 6.
5. Быстрый просмотр меняет статус заказа, когда в списке стоит фильтр по статусу: список перезапрашивается и строка пропадает, а не остаётся со старым статусом. → проверка в Task 3 и браузерная в Task 9.

---

## Файлы

| Файл | Ответственность |
|---|---|
| `src/admin/list/mod.rs` | реэкспорт, `run_list` — общий обработчик |
| `src/admin/list/spec.rs` | типы спеки |
| `src/admin/list/state.rs` | разбор и запись URL |
| `src/admin/list/sql.rs` | сборка SELECT и COUNT |
| `src/admin/list/render.rs` | ячейки, контекст шаблона, выбор ответа |
| `src/admin/list/orders.rs` | спека заказов |
| `src/admin/list/collection.rs` | спека коллекции, дерево |
| `src/admin/list/views.rs` | виды, закрепления, маршруты |
| `migrations/0027_admin_views.sql` | таблицы видов |
| `templates/admin/_list.html`, `_list_body.html`, `_tree.html` | разметка |
| `static/admin/list.js`, `static/admin/admin.css` | поповеры, клавиши, стили |

Удаляются: `templates/admin/shop/_order_row.html`, `templates/admin/_item_row.html`, форма фильтра в `shop/orders.html`, таблица подразделов в `items.html`, `OrdersQuery`/`push_filters` в `src/admin/shop/orders.rs` (после переноса тестов), `repo::list_items`, если у него не останется вызовов.

---

### Task 1: Спека и состояние URL

**Files:**
- Create: `src/admin/list/mod.rs`, `src/admin/list/spec.rs`, `src/admin/list/state.rs`
- Modify: `src/admin/mod.rs` (добавить `mod list;`)

**Interfaces:**
- Produces (`spec.rs`):
  ```rust
  pub enum FieldType { Text, Number, Money, Date, Bool, Enum, Link, File }
  pub struct EnumOption { pub value: String, pub label: String }
  pub struct Join { pub code: String, pub sql: String }          // " LEFT JOIN … " целиком
  pub enum FilterSql {
      Expr(String),                                              // SQL-выражение значения
      JsonField { key: String, multiple: bool },                 // field_values по коду, enum/link
  }
  pub struct Column { pub code: String, pub title: String, pub expr: String, pub ty: FieldType,
      pub sortable: bool, pub default: bool, pub required: bool,
      pub join: Option<String>, pub currency_expr: Option<String> }
  pub struct Filter { pub code: String, pub label: String, pub group: String, pub ty: FieldType,
      pub sql: FilterSql, pub options: Vec<EnumOption>, pub join: Option<String> }
  pub struct ListSpec { pub key: String, pub id_expr: String, pub from: String,
      pub joins: Vec<Join>, pub columns: Vec<Column>, pub filters: Vec<Filter>,
      pub default_sort: Sort, pub tie_break: String, pub sections: bool,
      pub peek_url: Option<String>, pub open_url: String, pub can_share: bool }
  impl ListSpec { pub fn column(&self, code: &str) -> Option<&Column>;
      pub fn filter(&self, code: &str) -> Option<&Filter>;
      pub fn default_cols(&self) -> Vec<String>; }
  ```
  `from` — `"FROM … WHERE <условие>"` без пользовательских строк. `peek_url`/`open_url` — шаблоны с `{id}`. `can_share` вычисляется построителем спеки для текущего пользователя.
- Produces (`state.rs`):
  ```rust
  pub enum Op { Has, Eq, Ne, Gt, Lt, Between, Empty, NotEmpty, In, Nin, Today, D7, D30, Range }
  pub enum Val { None, One(String), Two(String, String), Many(Vec<String>) }
  pub struct Cond { pub code: String, pub op: Op, pub val: Val }
  pub struct Sort { pub code: String, pub desc: bool }
  pub enum SectionSel { Id(i64), Without }
  pub struct ListState { pub filters: Vec<Cond>, pub sort: Sort, pub cols: Vec<String>,
      pub page: i64, pub per: i64, pub section: Option<SectionSel>, pub only: bool,
      pub view: Option<i64> }
  pub struct Parsed { pub state: ListState, pub dropped: Vec<String>, pub changed: bool }
  pub fn parse(spec: &ListSpec, query: &str) -> Parsed;
  pub fn to_query(spec: &ListSpec, st: &ListState, with_page: bool) -> String;
  ```
  `dropped` — коды фильтров `f.*`, отброшенных из-за неизвестного кода; `changed` — нормализованная строка отличается от исходной (порядок параметров не учитывается). `to_query` опускает значения по умолчанию (sort, cols, per=50, page=1, only=false) и `view`; фильтры пишет в порядке `st.filters`. Кодирование и разбор — крейт `form_urlencoded` (уже в `Cargo.lock` через axum; добавить прямой зависимостью в `Cargo.toml` той же версии).

- [ ] **Step 1: Тесты `state.rs`** (в `#[cfg(test)] mod tests`, спека-заглушка `fn spec()` с фильтрами всех типов: `name` text, `price` money, `qty` number, `updated` date, `status` enum с вариантами `N`,`P`,`F`, `paid` bool, `link` link и колонками `id`, `name` (required), `price`, `updated`):
  - `roundtrip_all_ops` — для каждого оператора каждого типа `to_query(parse(q))` == `q`;
  - `cyrillic_and_specials_survive` — `f.name=has:кед%_,:&` разбирается в `Val::One("кед%_,:&")` и записывается обратно так же;
  - `drops_garbage` — `f.nope=eq:1&f.price=gt:abc&f.updated=range:2026-13-01,&f.status=in:N,X&f.paid=eq:maybe&sort=zzz&cols=zzz&per=33&page=-4` → фильтров 0 (у `status` остаётся `in:N`, т. е. 1 фильтр с `Many(["N"])`), `dropped == ["nope"]`, sort по умолчанию, cols по умолчанию, `per == 50`, `page == 1`, `changed == true`;
  - `required_column_kept` — `cols=price` → `cols == ["name","price"]`;
  - `defaults_are_empty_query` — `to_query(parse(""))` == `""`;
  - `range_open_ends` — `range:,2026-11-01` и `range:2026-11-01,` принимаются;
  - `section_and_only` — `section=15&only=1` → `Some(Id(15))`, `only`; `section=none` → `Without`; `only=1` без `section` отбрасывается; при `spec.sections == false` оба отбрасываются.
- [ ] **Step 2:** `cargo test --lib admin::list::state` → FAIL (не компилируется).
- [ ] **Step 3: Реализовать `spec.rs` и `state.rs`.** Проверка значения по типу: number/money — `f64` parse; date — `NaiveDate` `%Y-%m-%d`; bool — `Y`/`N`; link — `i64`; enum — значение должно быть в `options` (иначе выбрасывается это значение; пустой список → весь фильтр выбрасывается). Повтор `f.<code>` — берётся последний.
- [ ] **Step 4:** `cargo test --lib admin::list::state` → PASS.
- [ ] **Step 5: Commit** `Админка: списки — спека и состояние URL`.

### Task 2: Сборка SQL

**Files:**
- Create: `src/admin/list/sql.rs`

**Interfaces:**
- Consumes: Task 1.
- Produces:
  ```rust
  pub enum Scope { All, Sections(Vec<i64>), Without }   // section_id IN (…) / IS NULL
  pub const COUNT_CAP: i64 = 10_001;
  pub fn select(spec: &ListSpec, st: &ListState, scope: &Scope) -> QueryBuilder<'static, Postgres>;
  pub fn count(spec: &ListSpec, st: &ListState, scope: &Scope) -> QueryBuilder<'static, Postgres>;
  pub struct Row { pub id: i64, pub cells: serde_json::Map<String, serde_json::Value> }
  pub async fn fetch(db: &PgPool, spec: &ListSpec, st: &ListState, scope: &Scope)
      -> sqlx::Result<(Vec<Row>, i64)>;     // (строки страницы, total ≤ COUNT_CAP)
  ```
- `select`: `SELECT <id_expr> AS id, jsonb_build_object('<code>', <expr>, …, '<code>__cur', <currency_expr>) AS cells <from> <joins> <where> ORDER BY <sort expr> [DESC] NULLS LAST, <tie_break> LIMIT $ OFFSET $`. В `jsonb_build_object` — только видимые колонки. `Scope` применяется как `AND ci.section_id = ANY($)` / `AND ci.section_id IS NULL`; его использует только спека коллекции (алиас `ci`), заказы всегда передают `Scope::All`.
- `count`: `SELECT count(*) FROM (SELECT 1 <from> <joins> <where> LIMIT 10001) t`.
- Joins: подключается `Join` с кодом, если на него ссылается видимая колонка, колонка сортировки или фильтр; каждый не больше одного раза.
- Условия по `FilterSql::Expr(e)`: `has` → `e ILIKE $` с `%{экранированное}%` (`\`, `%`, `_` экранируются `\`); `eq`/`ne`/`gt`/`lt` → `e = $` и т. д. с приведением параметра к типу (`$::numeric`, `$::date`); `between` → `e BETWEEN $ AND $`; `empty` → `(e IS NULL OR e::text = '')`, `notempty` — отрицание; `today`/`d7`/`d30` → `e >= now() - interval` (`today` — `date_trunc('day', now())`); `range` → `e >= $::date` и `e < ($::date + 1)`; `in`/`nin` → `e = ANY($)` / `NOT (e = ANY($))` (массив `Vec<String>`); bool → `e = $::bool`.
- Условия по `FilterSql::JsonField { key, multiple }` (enum и link, значения — id): `in` → `(ci.field_values @> $1 OR ci.field_values @> $2 …)`, каждый параметр — `Json(json!({key: id}))` или `Json(json!({key: [id]}))` при `multiple`; `nin` → `NOT (…)`; `eq` для link — одна такая проверка; `empty` → `NOT (ci.field_values ? $key)` или значение `null`/`[]`.

- [ ] **Step 1: Тесты** (спека-заглушка из Task 1 вынесена в `#[cfg(test)] pub(crate) fn test_spec()` в `spec.rs`):
  - `select_sql_snapshot` — для состояния `f.name=has:a&f.price=between:1,5&sort=-price` `qb.sql()` равен ожидаемой строке (записать в тест после первой реализации, проверить глазами);
  - `no_url_text_in_sql` — для `f.name=has:'; DROP TABLE x;--` и `f.status=in:N` текст `qb.sql()` не содержит `DROP`, `'; ` и значений;
  - `joins_only_when_used` — join `price_1` есть в SQL, только если `price` видна, или в фильтре, или в сортировке;
  - `like_escaped` — `has:50%_` даёт параметр `%50\%\_%`.
- [ ] **Step 2: Тест с базой** `#[sqlx::test] fetch_counts_and_caps`: таблица-заглушка не нужна — спека над `generate_series(1, 10050) AS g(id)` (`from: "FROM generate_series(1, 10050) AS g(id) WHERE TRUE"`): `fetch` возвращает 50 строк и `total == 10_001`; с `f.id=lt:11` → `total == 10`.
- [ ] **Step 3:** `cargo test --lib admin::list::sql` → FAIL.
- [ ] **Step 4: Реализовать `sql.rs`.**
- [ ] **Step 5:** `cargo test --lib admin::list` → PASS.
- [ ] **Step 6: Commit** `Админка: списки — сборка SQL`.

### Task 3: Шаблон, общий обработчик и заказы на движке

**Files:**
- Create: `src/admin/list/render.rs`, `src/admin/list/orders.rs`, `templates/admin/_list.html`, `templates/admin/_list_body.html`
- Modify: `src/admin/shop/orders.rs` (обработчик `list`, удалить `OrdersQuery`, `push_filters`, `serde_urlencoded_query`, `PER_PAGE` если не используется), `templates/admin/shop/orders.html`, `templates/admin/shop/order_peek.html`, `static/admin/admin.js` (быстрый просмотр), `static/admin/admin.css`
- Delete: `templates/admin/shop/_order_row.html`

**Interfaces:**
- Consumes: Tasks 1–2.
- Produces:
  ```rust
  // render.rs
  pub struct ListPage { pub title: String, pub crumbs: Vec<(String, String)>,
      pub actions: Vec<Action>, pub tree: Option<Value> }      // доп. разметка страницы
  pub struct Action { pub label: String, pub url: String, pub primary: bool, pub hotkey: Option<char> }
  pub fn format_cell(ty: &FieldType, v: &Value, currency: Option<&str>) -> String;
  pub async fn run_list(state: &AppState, user: &Access, headers: &HeaderMap, path: &str,
      spec: &ListSpec, raw_query: &str, scope: Scope, page: ListPage,
      notices: Vec<String>) -> AppResult<Response>;
  // orders.rs
  pub async fn orders_spec(db: &PgPool, user: &Access) -> sqlx::Result<ListSpec>;
  ```
- `run_list`: `parse` → если `changed`: фрагмент получает заголовок `HX-Replace-Url: <path>?<to_query>`, полная страница — 302 на нормализованный адрес; `page` больше последней → последняя; `fetch`; рендер `_list.html` (полная) или `_list_body.html` (фрагмент). Контекст: `spec` (колонки и фильтры для JSON поповеров), `st`, `rows` (ячейки уже отформатированы `format_cell`), `total`, `capped: total == COUNT_CAP`, `pages`, `query` (`to_query` без page), `chips` (подпись, оператор, значение — подписи вариантов enum из `options`), `views` (пусто до Task 6), `page`.
- `format_cell`: money — разделитель тысяч пробел, два знака, через пробел код валюты (`1 234,50 RUB`); date — `ДД.ММ.ГГГГ ЧЧ:ММ`; bool — `да`/`нет`; null — `—`.
- `_list.html` расширяет `base.html`: крошки, заголовок с числом (`10 000+` при `capped`), действия, `<div id="list-body" hx-get="<текущий адрес>" hx-trigger="list:refresh from:body" hx-target="this">` с `_list_body.html` внутри, `<script type="application/json" id="list-spec">` (фильтры: code, label, group, ty, options; колонки: code, title, required, default; `per`), слот под дерево.
- `_list_body.html`: строка видов (Task 6), чипы, таблица (строки `data-id`, `data-open`, `data-peek` если у спеки есть `peek_url`), страницы ‹ 1 … › и `per`. Пустой результат: «Ничего не найдено. Измените фильтры или сбросьте их».
- Спека заказов: `from: "FROM orders o LEFT JOIN users u ON u.id = o.user_id LEFT JOIN order_statuses s ON s.code = o.status WHERE TRUE"`. Колонки: `number` (`o.account_number`, required), `created_at`, `buyer` (выражение покупателя из старого `row_query`), `status` (`COALESCE(s.name, o.status)`), `price` (money, `currency_expr: o.currency`), `paid`, `delivery` (подзапрос из `row_query`), `canceled` (не по умолчанию). Фильтры: `number` text (`o.account_number`), `status` enum (`o.status`, варианты из `order_statuses`), `created_at` date, `buyer` text (`BUYER_TEXT_SQL`), `paid` bool, `canceled` bool, `price` money, `pay_system` enum, `delivery` enum (варианты из соответствующих справочников). Сортировка по умолчанию `-created_at`, `tie_break: "o.id DESC"`. `peek_url: /admin/shop/orders/{id}/peek`, `open_url: /admin/shop/orders/{id}`. `can_share: user.can(ORDERS_MANAGE)`.
- Быстрый просмотр: в `order_peek.html` (и в Task 5 `item_peek.html`) вместо `<template>` со строкой — `<div hidden data-list-changed></div>`, когда раньше передавалась строка; `admin.js` после swap в `#peek` при наличии `[data-list-changed]` вызывает `htmx.trigger(document.body, 'list:refresh')`; класс `peeked` после перезагрузки восстанавливается по `data-id` открытой записи.

- [ ] **Step 1: Перенести тесты старых фильтров заказов** (`src/admin/shop/orders.rs`, тесты вокруг `push_filters`, строки ~1150–1190) на движок: `orders_filters_match_old_form` — для набора старых запросов (`number`, `status`, `from`/`to`, `buyer`, `paid`, `canceled`) эквивалентный URL движка (`f.number=eq:…`, `f.status=in:…`, `f.created_at=range:…`, `f.buyer=has:…`, `f.paid=eq:Y`, `f.canceled=eq:N`) возвращает те же id заказов на `order_fixture` + `place_order`.
- [ ] **Step 2: Тесты ответов** (`#[sqlx::test]`, через `test_state`, `admin_cookie`, `get_raw`, `htmx_get`):
  - `orders_full_page_and_fragment` — `GET /admin/shop/orders` содержит `id="list-body"` и меню; `htmx_get(…, "list-body")` содержит таблицу и не содержит `<aside class="sidebar"`;
  - `orders_normalizes_url` — `GET /admin/shop/orders?f.zzz=eq:1` → 302 на `/admin/shop/orders`; фрагмент — 200 и `HX-Replace-Url: /admin/shop/orders`;
  - `orders_page_past_end` — `?page=999` показывает последнюю страницу;
  - `orders_readonly_403` — пользователь без `orders.manage` получает 403;
  - `peek_change_triggers_refresh` — POST смены статуса из панели (`/admin/shop/orders/{id}/status` с `HX-Target: peek`) возвращает разметку с `data-list-changed`.
- [ ] **Step 3:** `cargo test --lib admin::shop::orders admin::list` → FAIL.
- [ ] **Step 4: Реализовать** `render.rs`, `orders.rs`, шаблоны, заменить обработчик `shop::orders::list` на вызов `run_list` (подпись обработчика: `State`, `Extension<Access>`, `RawQuery`, `HeaderMap`, `OriginalUri`), удалить старую форму и `_order_row.html`, поправить `order_peek.html` и `admin.js`. Стили чипов, строки видов, страниц — по прототипу, на токенах `admin.css`.
- [ ] **Step 5:** `cargo test` (весь набор) → PASS.
- [ ] **Step 6: Commit** `Админка: список заказов на движке списков`.

### Task 4: Спека коллекции

**Files:**
- Create: `src/admin/list/collection.rs`

**Interfaces:**
- Consumes: Tasks 1–2; `repo::get_collection`, `repo::list_fields`, `repo::list_collection_options`, `Access::collection_level`.
- Produces:
  ```rust
  pub async fn collection_spec(db: &PgPool, user: &Access, c: &Collection) -> sqlx::Result<ListSpec>;
  pub async fn section_scope(db: &PgPool, collection_id: i64, sel: &Option<SectionSel>, only: bool)
      -> sqlx::Result<(Scope, Option<Section>)>;   // None → Scope::All; удалённый/чужой раздел → (All, None)
  ```
- `key: "collection:<id>"`, `from: "FROM collection_items ci WHERE ci.collection_id = <id>"`, `tie_break: "ci.id DESC"`, `sections: true`, сортировка по умолчанию `-updated_at`.
- Стандартные колонки: `id`, `name` (required), `code`, `active`, `sort`, `section` (имя раздела подзапросом по `collection_sections`, `sortable: false`), `published_at`, `updated_at`. Фильтры: `name` text, `code` text, `active` bool, `id` number, `published_at` date, `updated_at` date. Группа «Запись».
- Поля коллекции (группа «Поля коллекции»): `string`/`text` → text, выражение `ci.field_values->>'<code>'`; `number` → number, `CASE WHEN jsonb_typeof(ci.field_values->'<code>') = 'number' THEN (ci.field_values->>'<code>')::numeric END`; `date` → date, `CASE WHEN (ci.field_values->>'<code>') ~ '^\d{4}-\d{2}-\d{2}' THEN (ci.field_values->>'<code>')::date END`; `boolean` → bool, `(ci.field_values->'<code>') = 'true'::jsonb`; `list` → enum, `FilterSql::JsonField`, варианты — id и подпись `collection_field_options`; колонка показывает подписи вариантов через подзапрос `string_agg` по `collection_field_options`; `element` → link, `JsonField`; `file` → только колонка (число файлов). Множественные поля — `sortable: false`. Код поля вставляется в SQL литералом; `is_valid_code` гарантирует `[a-zA-Z0-9_]` — при невалидном коде поле пропускается. Колонки полей `default: false`.
- Каталог (`c.is_catalog`, группа «Каталог»): на каждый тип цены `catalog_price_types` — join `price_<id>`: `LEFT JOIN catalog_prices p<id> ON p<id>.item_id = ci.id AND p<id>.price_type_id = <id> AND p<id>.quantity_from IS NULL`, колонка/фильтр `price_<id>` money (`currency_expr: p<id>.currency`); join `qty`: `LEFT JOIN catalog_products cp ON cp.item_id = ci.id`, колонка/фильтр `qty` number `COALESCE(cp.quantity, 0)`; на каждый активный склад — join `store_<id>` по `catalog_store_amounts`, колонка/фильтр `store_<id>`; фильтр `instock` bool `COALESCE(cp.quantity, 0) > 0` с join `qty`. Базовая цена и `qty` — `default: true`.
- `can_share`: `user.collection_level(id) >= Level::Write || user.can("collections.manage")`.
- `peek_url` только у каталога: `/admin/items/{id}/peek`; `open_url: /admin/items/{id}`.

- [ ] **Step 1: Тесты с базой** (`#[sqlx::test]`, на `content_fixture`/`cart_fixture` плюс свои вставки полей и записей):
  - `filters_by_collection_fields` — поле `list` одиночное и множественное (`in`, `nin`), `number` (`gt`), `date` (`range`), `element` (`eq`), `boolean` (`eq:Y`) возвращают ожидаемые id;
  - `bad_json_value_does_not_break` — запись с `field_values = {"weight": "abc", "made": "вчера"}` при фильтре `f.weight=gt:1` и `f.made=range:2026-01-01,` не вызывает ошибку SQL и не попадает в результат;
  - `catalog_price_stock_store` — фильтры `price_<base>` (`empty`, `gt`), `qty` (`eq:0`), `store_<id>` (`gt`), `instock=eq:Y`;
  - `section_scope_includes_children` — `section_scope` для раздела с подразделом даёт оба id; с `only` — один; `Without` → `Scope::Without`; раздел другой коллекции → `(Scope::All, None)`;
  - `deleted_field_dropped` — после `repo::delete_field` запрос `f.<code>=…` даёт `dropped == [code]`.
- [ ] **Step 2:** `cargo test --lib admin::list::collection` → FAIL.
- [ ] **Step 3: Реализовать** `collection.rs`.
- [ ] **Step 4:** `cargo test --lib admin::list` → PASS.
- [ ] **Step 5: Commit** `Админка: спека списка коллекции`.

### Task 5: Список записей и дерево разделов

**Files:**
- Create: `templates/admin/_tree.html`
- Modify: `src/admin/items.rs` (обработчик `list`, новый `tree`), `src/admin/mod.rs` (маршрут `/collections/{id}/tree`), `templates/admin/items.html`, `templates/admin/item_peek.html`, `static/admin/admin.css`
- Delete: `templates/admin/_item_row.html`; `repo::list_items`, если вызовов не осталось

**Interfaces:**
- Consumes: Tasks 3–4 (`run_list`, `collection_spec`, `section_scope`, `ListPage.tree`).
- Produces: `GET /admin/collections/{id}/tree?parent=<id>` → фрагмент `_tree.html` (дети узла); `pub async fn tree(...)` в `items.rs`.
- Список: `user.require_collection(id, Level::Read)`; заголовок — имя раздела или коллекции; крошки — цепочка разделов; действия — «Добавить запись» (`hotkey: 'c'`, с `?section=`), при `can_write`. `items.html` становится тонкой обёрткой над `_list.html` с деревом в слоте или удаляется, если `_list.html` справляется сам.
- Дерево: первый уровень и ветка до текущего раздела отрисовываются сервером; остальные — по клику через `tree`. Узлы: «Вся коллекция», разделы с числом записей вместе с подразделами (один запрос: `count(*)` по `section_id` и сложение по поддереву в Rust через `section_subtree_ids`), «Без раздела», «Добавить раздел» (при `can_write`). Клик по разделу — `hx-get` списка с `section=<id>` (без `only`), `hx-target="#list-body"`, `hx-push-url`. Меню «⋯» узла: «Изменить раздел» (`/admin/sections/{id}`), «Добавить подраздел», «Добавить запись в раздел». Галочка «только этот раздел» в `_list_body.html` — только если у раздела есть дети. Свёрнутость — класс на контейнере (сохранение — Task 8).
- Чип «Раздел: <имя> ✕» — в `chips` с признаком `section: true`.

- [ ] **Step 1: Тесты** (`#[sqlx::test]`, `content_fixture`):
  - `items_list_shows_subsection_items` — раздел с подразделом: без `only` записи обоих, с `only=1` — только свои;
  - `items_without_section` — `section=none`;
  - `items_foreign_section_dropped` — `section=<id другой коллекции>` → 302 на адрес без `section`;
  - `tree_children_fragment` — `/tree?parent=<id>` содержит детей и не содержит меню;
  - `items_readonly_user` — пользователь с доступом `read` видит список без «Добавить запись» и без «Добавить раздел»;
  - `item_peek_triggers_refresh` — действие из панели товара отдаёт `data-list-changed`.
- [ ] **Step 2:** `cargo test --lib admin::items` → FAIL.
- [ ] **Step 3: Реализовать**; удалить таблицу подразделов и `_item_row.html`.
- [ ] **Step 4:** `cargo test` → PASS (включая `smoke_tests`).
- [ ] **Step 5: Commit** `Админка: список записей на движке и дерево разделов`.

### Task 6: Сохранённые виды

**Files:**
- Create: `migrations/0027_admin_views.sql` (таблицы и индекс — дословно из раздела 3 спеки), `src/admin/list/views.rs`
- Modify: `src/admin/mod.rs` (маршруты), `src/admin/list/render.rs` (строка видов, `?view=`), `templates/admin/_list_body.html`, `src/collection/repo.rs` (`delete_collection`)

**Interfaces:**
- Produces:
  ```rust
  pub struct View { pub id: i64, pub list_key: String, pub owner_id: i64, pub name: String,
      pub query: String, pub shared: bool, pub pinned: bool }
  pub async fn visible(db: &PgPool, user_id: i64, list_key: &str) -> sqlx::Result<Vec<View>>;
      // общие, затем свои; sort, id
  pub async fn get_visible(db: &PgPool, user_id: i64, id: i64) -> sqlx::Result<Option<View>>;
  pub fn may_edit(v: &View, user_id: i64, can_share: bool) -> bool;   // свой, или общий при can_share
  ```
  Маршруты (форма `application/x-www-form-urlencoded`, ответы для HTMX — фрагмент `#list-body` с уведомлением через существующий `flash`):
  - `POST /admin/views` — `list_key`, `path`, `name`, `query`, `shared`; `shared=1` без `can_share` → 403; пустое имя → 422 с уведомлением «Введите название вида».
  - `POST /admin/views/{id}` — любые из `name`, `query`, `shared`; не `may_edit` → 403; невидимый → 404.
  - `POST /admin/views/{id}/delete` — то же правило.
  - `POST /admin/views/{id}/pin` — переключатель в `admin_view_pins` для любого видимого вида.
  `can_share` для `list_key` берётся из построителя спеки (`orders_spec`/`collection_spec`) — функция `fn spec_for_key(db, user, key) -> AppResult<ListSpec>` в `views.rs`.
- `?view=<id>`: `run_list` берёт `query` видимого вида и рендерит его состояние; адрес — `HX-Replace-Url`/302 на `path?<query>&view=<id>` (параметр `view` сохраняется в URL, чтобы строка видов знала открытый вид; `to_query` его не пишет, `run_list` дописывает). Невидимый вид → 404. `dropped` при открытии вида → уведомления «Фильтр по полю “<code>” больше недоступен».
- Строка видов: «Все» (системный, URL без параметров), общие, свои; звёздочка, «общий», меню «⋯»; «Вид изменён · Сохранить · Сохранить как… · Сбросить», когда `to_query(st)` ≠ нормализованный `query` открытого вида (для «Все» — ≠ пустой строки); «Сохранить» — только при `may_edit`.
- `delete_collection`: в той же транзакции `DELETE FROM admin_views WHERE list_key = 'collection:' || $1`.

- [ ] **Step 1: Тесты** (`#[sqlx::test]`):
  - `view_crud` — создать, переименовать, сменить `query`, удалить;
  - `shared_requires_right` — пользователь с `read` на коллекцию: `shared=1` → 403; без `shared` — 200;
  - `readonly_cannot_save_shared` — общий вид, созданный админом, пользователь с `read`: `POST /admin/views/{id}` → 403, во фрагменте нет кнопки «Сохранить», есть «Сохранить как…»;
  - `foreign_private_404` — чужой личный вид: `?view=` и POST → 404;
  - `view_dropped_field_notice` — вид с фильтром по удалённому полю открывается (200) с текстом «больше недоступен»;
  - `delete_collection_drops_views`.
- [ ] **Step 2:** `cargo test --lib admin::list::views` → FAIL.
- [ ] **Step 3: Реализовать** (`sqlx migrate` подхватывает миграцию в тестах автоматически).
- [ ] **Step 4:** `cargo test` → PASS.
- [ ] **Step 5: Commit** `Админка: сохранённые виды списков`.

### Task 7: Закреплённые виды в меню

**Files:**
- Modify: `src/admin/nav.rs` (`NavItem`, `build`), `templates/admin/_nav.html`, `static/admin/admin.css`

**Interfaces:**
- Consumes: `admin_view_pins`, `admin_views` (Task 6).
- Produces: `NavItem.views: Vec<NavItem>` (пустой по умолчанию). В `build` одним запросом читаются закреплённые видимые виды пользователя (`list_key`, `id`, `name`, `sort`); `list_key` совпадает с `Section.code` пункта меню (`orders`, `collection:<id>`) — виды кладутся в `views` этого пункта; `url` — адрес списка с `?view=<id>`; `active` — текущий путь содержит `view=<id>`. Виды пункта, скрытого из меню (коллекции сверх `MENU_COLLECTIONS`), не показываются.
- `_nav.html`: под пунктом — вложенные ссылки `class="nav sub"` со звёздочкой.

- [ ] **Step 1: Тест** `pinned_views_in_nav` — закреплённый вид коллекции и заказов выводится под своими пунктами на любой полной странице админки; чужой личный закреплённый (после смены `shared` владельцем на `false`) не выводится.
- [ ] **Step 2:** `cargo test --lib admin::nav` → FAIL.
- [ ] **Step 3: Реализовать.**
- [ ] **Step 4:** `cargo test --lib admin` → PASS.
- [ ] **Step 5: Commit** `Админка: закреплённые виды в меню`.

### Task 8: Последнее состояние списка

**Files:**
- Modify: `src/admin/list/render.rs`, `src/admin/shell.rs` (чтение и запись `admin_prefs`)

**Interfaces:**
- Consumes: функции чтения и записи `admin_prefs` в `shell.rs` (строки ~66 и ~109; при необходимости вынести `pub(crate) async fn set_pref(db, user_id, path: &[&str], value: Value)`).
- Поведение: `admin_prefs.lists.<key> = {cols, sort, per, tree_collapsed}`. Запрос списка без параметров (пустой `raw_query`) подставляет сохранённые `cols`, `sort`, `per` — без редиректа, URL остаётся пустым; при любом ответе списка `cols`/`sort`/`per` из состояния сравниваются с сохранёнными и пишутся отдельным `UPDATE`, только если отличаются. `tree_collapsed` пишет `POST /admin/collections/{id}/tree/collapsed` (`value=1|0`), вызываемый из `list.js`.
- Фильтры не сохраняются и не восстанавливаются.

- [ ] **Step 1: Тесты** `list_prefs_restore` (открыть с `cols=id,name&sort=id`, затем без параметров — в таблице колонки `ID`, `Название`, сортировка по ID; фильтры не восстановлены), `list_prefs_no_write_when_same` (повторный запрос не меняет `admin_prefs` — сравнить значение до и после; обновления проверять через `updated_at` нельзя — сравнивать JSON).
- [ ] **Step 2:** FAIL → **Step 3:** реализовать → **Step 4:** `cargo test --lib admin` PASS.
- [ ] **Step 5: Commit** `Админка: списки запоминают колонки и сортировку`.

### Task 9: Поповеры, колонки и клавиши

**Files:**
- Create: `static/admin/list.js` (подключить в `base.html` после `admin.js`)
- Modify: `templates/admin/_list_body.html` (атрибуты-крючки), `static/admin/admin.css`, окно «Горячие клавиши» в `base.html` (добавить новые клавиши)

**Interfaces:**
- Consumes: `#list-spec` JSON (Task 3), разметка чипов `[data-chip="<index>"]`, `[data-chip-remove]`, кнопки `#list-add-filter`, `#list-cols`, `[data-view-save-as]`, `[data-view-menu="<id>"]`, строки `tr[data-id]`.
- Поведение — как в прототипе:
  - «+ Фильтр» — поиск по названию, группы, Enter выбирает первый; поповер оператора и значения (enum — чекбоксы с поиском; date — быстрые варианты и период; number — поле; bool — две кнопки, применяется сразу); «Применить»/«Убрать». Применение строит URL из текущего (`URLSearchParams`, правка одного `f.<code>`, сброс `page`), вызывает `htmx.ajax('GET', url, {target: '#list-body', swap: 'innerHTML'})` и после успешного swap — `history.pushState(null, '', url)` (у `htmx.ajax` в HTMX 2 нет опции push). Ответ с `HX-Replace-Url` заменяет адрес сам.
  - «Колонки» — галочки, перетаскивание (HTML5 drag-and-drop), «По умолчанию»; `required` нельзя снять.
  - Сортировка по клику на `th[data-sort]` — цикл возрастание → убывание → по умолчанию.
  - Виды: «Сохранить как…» (название, «Общий» — только если в JSON `can_share`), меню «⋯» (переименовать, общий/личный, удалить с подтверждением в поповере — без `confirm()`), звёздочка → `POST …/pin`.
  - Клавиши: `J`/`K` (`KeyJ`/`KeyK`, работают и в русской раскладке по `e.code`) — текущая строка `tr.cur` с `scrollIntoView({block:'nearest'})`, при открытом `#peek` — открыть соседнюю; `Enter` — `open_url` строки; `Space` — клик по строке с `data-peek`; `F` — «+ Фильтр»; `/` — элемент с `data-hotkey="/"` (поповер «Название содержит»); `{`/`}` — страницы; `Escape` — закрыть поповер. Не срабатывают при открытом поповере, `dialog[open]`, в полях ввода, с модификаторами (кроме Shift для `{ }`).
  - Свёрнутость дерева — `POST …/tree/collapsed` (Task 8).
- [ ] **Step 1:** `cargo test` → PASS (регрессия шаблонов).
- [ ] **Step 2: Проверка в браузере** (dev-сервер, Chrome; `admin`-пользователь и пользователь только с чтением): заказы и каталог — добавить, изменить, убрать каждый тип фильтра; кириллица в «Название содержит»; колонки и порядок; сортировка; страницы и `per`; назад/вперёд; дерево — переход по разделам, «только этот раздел», «Без раздела», сворачивание после перезагрузки; виды — сохранить, сохранить как общий, переименовать, удалить, закрепить → пункт в меню; быстрый просмотр — Space, J/K в панели, смена статуса заказа при фильтре по статусу убирает строку; клавиши не срабатывают в поле ввода; светлая и тёмная темы; ширина 400px.
- [ ] **Step 3: Commit** `Админка: поповеры фильтров, колонки и клавиши списков`.

### Task 10: Итоговая проверка

- [ ] `cargo clippy --all-targets -- -D warnings` → без предупреждений.
- [ ] `cargo test` → PASS.
- [ ] Остальные списки (пользователи, скидки, справочники магазина) открываются как раньше.
- [ ] Обновить память `admin-redesign-remaining` (подпроект 3.1 готов) и README, если в нём описаны списки админки.
- [ ] Commit `Админка: этап 3.1 — итоговые правки`, если что-то изменилось.
