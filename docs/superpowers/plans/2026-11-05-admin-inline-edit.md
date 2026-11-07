# Админка, этап 3.3: правка в таблице — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Редактирование значений прямо в таблице списка записей коллекции: двойной клик по ячейке, сохранение через тот же разбор и проверки, что в карточке, с защитой от параллельной правки.

**Architecture:** Движок списков получает дескриптор редактируемости колонки (`CellEdit`) и SQL сырого значения. Новый модуль `src/admin/list/edit.rs` принимает `POST /admin/lists/cell`, проверяет доступ и значение, атомарно обновляет одну колонку/поле с условием по `updated_at` и перерисовывает `#list-body`. Интерфейс — в `static/admin/list.js` на `data-*`-атрибутах.

**Tech Stack:** Rust, axum, sqlx (Postgres), MiniJinja, HTMX 2, чистый JS.

**Spec:** `docs/superpowers/specs/2026-11-05-admin-inline-edit-design.md`

## Статус на 2026-11-08

- Tasks 1–3 реализованы коммитами `7e75ade`, `86f22d9`, `3c7fff6`.
- Итоговое ревью выявило пустой `data-edit`: в `CellView` отсутствовал код колонки. Добавлена передача кода и регрессионная проверка точных атрибутов для `name`, `code`, `active`. До исправления тест воспроизвёл ошибку.
- README дополнен описанием правки в таблице.
- Браузерная проверка Task 3 / Step 6 остаётся открытой: инструмент вернул `The browser access host is unavailable`. Автоматические проверки не заменяют этот пункт.

## Global Constraints

- Ветка `feature/admin-inline-edit` (от `main`): перед Task 1 — `git switch -c feature/admin-inline-edit`. Коммиты на русском в стиле `Админка: …`, без упоминаний ИИ и `Co-Authored-By`.
- Тесты: `cargo test <путь>` (бинарный крейт, без `--lib`); перед коммитом задачи — `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`; при правке JS — `node --check static/admin/list.js`.
- Редактируются только одиночные поля типов `string`, `number`, `boolean`, `date`, `list` и стандартные `name`, `code`, `sort`, `active`. `text`, `file`, `element`, `multiple`, `published_at`, `id`, `updated_at`, цены/остатки — не редактируются.
- Редактирование доступно только при `collection_level >= Write` и праве записи на родительскую коллекцию.
- Значения из формы — только через `push_bind`; в текст SQL — только выражения спеки.
- Значение проверяется теми же правилами, что в карточке (`collection::fields`), пустое обязательное — 422.
- Ответ — перерисованный `#list-body`; 4xx список не подменяют, уведомления — `flash::toast`.
- Заказы и остальные списки админки не меняются; существующие тесты остаются зелёными без правок ожиданий.

## Review Focus

1. Правка колонки, участвующей в текущем фильтре/сортировке: строка может выпасть из фильтра или переехать; это не ошибка, список после ответа консистентен. → тест в Task 2.
2. Две вкладки: устаревший `updated_at` не затирает чужую правку — 409, значение в БД прежнее. → тест в Task 2.
3. Некорректное значение (пустое обязательное, чужой вариант списка, занятый код): 422, запись не меняется, редактор остаётся открытым. → тесты в Task 2.
4. Пользователь только с чтением: в HTML нет `data-edit`/`data-version`, `edit_columns` пуст, прямой POST — 403. → тесты в Task 2.
5. Первая колонка (`name`) — ссылка: одиночный клик открывает карточку, двойной правит; случайного перехода при двойном клике нет. → проверка в Chrome в Task 3.

---

## Файлы

| Файл | Ответственность |
|---|---|
| `src/admin/list/spec.rs` | `EditKind`, `EditTarget`, `StdField`, `CellEdit`; поля `Column.editable`, `Column.edit_expr`, `ListSpec.version_expr` |
| `src/admin/list/collection.rs` | заполнение `editable`/`edit_expr`/`version_expr` по правам и типам полей |
| `src/admin/list/orders.rs`, `src/admin/list/spec.rs` (`test_spec`) | новые поля структур — `None` |
| `src/admin/list/sql.rs` | `__edit` и `__version` в выборке строк |
| `src/admin/list/render.rs` | `CellView.edit/value`, `RowView.version`, `edit_columns` в `#list-spec` |
| `templates/admin/_list_body.html` | `data-edit`/`data-kind`/`data-value`/`data-first`, `data-version` на строке |
| `src/collection/fields.rs` | `parse_single` для одного значения |
| `src/admin/list/edit.rs` | маршрут `POST /admin/lists/cell`, проверки, сохранение |
| `src/admin/list/views.rs` | общий `reject` (перенос из `bulk.rs`) |
| `src/admin/mod.rs`, `src/admin/list/mod.rs` | маршрут и модуль |
| `static/admin/list.js`, `static/admin/admin.css` | редакторы, клавиши, 409, стили |
| `README.md` | абзац о правке в таблице |

---

### Task 1: Редактируемость в спеке и разметке

**Files:**
- Modify: `src/admin/list/spec.rs`, `src/admin/list/collection.rs`, `src/admin/list/orders.rs`, `src/admin/list/sql.rs`, `src/admin/list/render.rs`, `templates/admin/_list_body.html`
- Test: `src/admin/list/collection.rs` (тесты), `src/admin/list/render.rs` (тесты)

**Interfaces:**
- Produces:
  ```rust
  pub enum EditKind { Text, Number, Bool, Date, List }
  pub enum StdField { Name, Code, Sort, Active }
  pub enum EditTarget { Standard(StdField), Field(i64) }
  pub struct CellEdit { pub target: EditTarget, pub kind: EditKind, pub required: bool, pub options: Vec<EnumOption> }
  // Column: pub editable: Option<CellEdit>, pub edit_expr: Option<String>
  // ListSpec: pub version_expr: Option<String>
  fn field_entries(f: &Field, options: &[EnumOption], can_edit: bool, out_cols: &mut Vec<Column>, out_filters: &mut Vec<Filter>)
  ```
- `select()` кладёт в `cells` пары `'<код>__edit'` (из `edit_expr`) и `'__version'` (из `version_expr`).
- `CellView` получает `edit: Option<&'static str>` и `value: String`; `RowView` — `version: Option<String>`.
- `#list-spec` получает `edit_columns: [{code, kind, required, options:[{value,label}]}]`.

- [ ] **Step 1: Тест спеки**

В `src/admin/list/collection.rs` (модуль `tests`), используя существующие `field`, `spec_of`, `content_fixture`:

```rust
#[sqlx::test]
async fn editable_columns_by_kind(db: PgPool) {
    let f = content_fixture(&db).await;
    let c = f.collection_id;
    field(&db, c, "s", "string", false).await;
    field(&db, c, "s_multi", "string", true).await;
    field(&db, c, "t", "text", false).await;
    field(&db, c, "n", "number", false).await;
    field(&db, c, "b", "boolean", false).await;
    field(&db, c, "d", "date", false).await;
    let spec = spec_of(&db, c).await;
    let edit = |code: &str| spec.column(code).and_then(|c| c.editable.as_ref());
    assert!(spec.version_expr.is_some());
    for code in ["name", "code", "sort", "active", "s", "n", "b", "d", "color"] {
        assert!(edit(code).is_some(), "{code}");
    }
    assert!(edit("s_multi").is_none());
    assert!(edit("t").is_none());
    assert!(edit("link").is_none());
    assert!(edit("id").is_none());
    assert!(edit("published_at").is_none());
    assert_eq!(edit("color").unwrap().options[0].label, "Красный");
    assert!(spec.column("color").unwrap().edit_expr.is_some());
    assert!(spec.column("t").unwrap().edit_expr.is_none());
}
```

- [ ] **Step 2:** `cargo test admin::list::collection::tests::editable_columns_by_kind` → FAIL.
- [ ] **Step 3:** Реализовать структуры и заполнение.

Derive: `EditKind`, `StdField`, `EditTarget` — `Debug, Clone, Copy, PartialEq, Eq`; `CellEdit` — `Debug, Clone` (этого требуют derive у `Column`).

В `collection_spec` вычислить до цикла полей:
```rust
let can_edit = user.collection_level(id) >= Level::Write
    && c.product_collection_id.is_none_or(|p| user.collection_level(p) >= Level::Write);
```
и передавать `can_edit` в `field_entries`; `parent_ok`/`bulk_actions` ниже заменить на тот же `can_edit`.

Стандартные колонки (после `let mut columns = vec![…]`):
- `name`: `Standard(Name)`, `Text`, `required = true`, `edit_expr = "ci.name"`;
- `code`: `Standard(Code)`, `Text`, `edit_expr = "ci.code"`;
- `active`: `Standard(Active)`, `Bool`, `edit_expr = "ci.active"`;
- `sort`: `Standard(Sort)`, `Number`, `edit_expr = "ci.sort"`.

`field_entries` при `can_edit` и `!f.multiple`:
- `string` → `Text`, `edit_expr = text`; `number` → `Number`; `date` → `Date`;
  `boolean` → `Bool`; `list` → `List` с `options.to_vec()`; `target = Field(f.id)`,
  `required = f.is_required`. `text`, `element`, `file` и множественные — без `editable`.

`version_expr = can_edit.then(|| "ci.updated_at".to_string())`.
`orders.rs` и `test_spec`: новые поля — `None`. `col()` во всех трёх местах — `editable: None, edit_expr: None`.

- [ ] **Step 4:** `cargo test admin::list::collection::tests::editable_columns_by_kind` → PASS.

- [ ] **Step 5: Тест разметки**

В `src/admin/list/render.rs` (модуль `tests`):

```rust
#[test]
fn spec_json_has_edit_columns() {
    use crate::admin::list::spec::{CellEdit, EditKind, EditTarget, StdField, test_spec};
    let mut spec = test_spec();
    spec.columns[1].editable = Some(CellEdit {
        target: EditTarget::Standard(StdField::Name),
        kind: EditKind::Text,
        required: true,
        options: vec![],
    });
    let st = crate::admin::list::state::parse(&spec, "").state;
    let json = spec_json(&spec, &st, "/p", "");
    assert!(json.contains("\"edit_columns\""));
    assert!(json.contains("\"kind\":\"text\""));
    let json = spec_json(&test_spec(), &st, "/p", "");
    assert!(json.contains("\"edit_columns\":[]"));
}
```

`"kind":"text"` есть только в `edit_columns` (в массиве `columns` поля `kind` нет).

- [ ] **Step 6:** `cargo test admin::list::render::tests::spec_json_has_edit_columns` → FAIL.

- [ ] **Step 7:** реализовать `__edit`/`__version` в `sql.rs` (в цикле по `visible` и после него), `CellView`/`RowView` и `edit_columns` в `render.rs`; в `_list_body.html`:

```html
<tr data-id="{{ r.id }}"{% if r.version %} data-version="{{ r.version }}"{% endif %} …>
…
<td{% if c.num %} class="num"{% endif %}{% if c.edit %} data-edit="{{ c.code }}" data-kind="{{ c.edit }}" data-value="{{ c.value }}"{% if c.first %} data-first="1"{% endif %} title="Двойной клик — изменить"{% endif %}>…
```

`CellView.value`: `null` → `""`, bool → `"true"`/`"false"`, иначе `to_string()`; `RowView.version` — из `__version` через `as_str()`.

- [ ] **Step 8:** `cargo test admin::list::render`, `cargo test admin::list::collection` → PASS. `cargo fmt`.

- [ ] **Step 9: Commit** `Админка: редактируемые колонки списка`.

### Task 2: Маршрут сохранения ячейки

**Files:**
- Create: `src/admin/list/edit.rs`
- Modify: `src/admin/list/mod.rs` (`pub(crate) mod edit;`), `src/admin/mod.rs` (`.route("/lists/cell", post(list::edit::handle))`), `src/admin/list/views.rs` (`reject`), `src/collection/fields.rs` (`parse_single`), `src/admin/list/bulk.rs` (использовать `views::reject`)
- Test: `src/admin/list/edit.rs` (модуль `tests`), `src/collection/fields.rs` (модуль `tests`)

**Interfaces:**
- Consumes: `views::spec_for_key`, `views::list_path`, `views::rerender`, `views::with_toast`, `views::reject`, `collection::is_valid_slug`, `fields::parse_single`.
- Produces:
  ```rust
  pub async fn handle(State<AppState>, Extension<Access>, Form<HashMap<String,String>>) -> AppResult<Response>;
  // fields.rs
  pub fn parse_single(kind: &str, required: bool, name: &str, options: &[FieldOption], raw: &str) -> Result<Value, String>;
  // views.rs
  pub(crate) fn reject(status: StatusCode, text: &str) -> Response;
  ```

- [ ] **Step 1: Тест `parse_single`**

В `src/collection/fields.rs` (в модуле `tests` добавить `use serde_json::json;`):
```rust
#[test]
fn single_cell_values() {
    let opts = [FieldOption { id: 3, field_id: 1, value: "v".into(), xml_id: "x".into(), sort: 0, is_default: false }];
    assert_eq!(parse_single("string", false, "S", &[], ""), Ok(Value::Null));
    assert_eq!(parse_single("string", false, "S", &[], " текст "), Ok(json!("текст")));
    assert_eq!(parse_single("number", false, "N", &[], "1,5"), Ok(json!(1.5)));
    assert_eq!(parse_single("date", false, "D", &[], "2026-05-06"), Ok(json!("2026-05-06")));
    assert_eq!(parse_single("boolean", false, "B", &[], "true"), Ok(json!(true)));
    assert_eq!(parse_single("boolean", false, "B", &[], ""), Ok(json!(false)));
    assert_eq!(parse_single("list", false, "L", &opts, "3"), Ok(json!(3)));
    assert!(parse_single("string", true, "S", &[], "").is_err());
    assert!(parse_single("list", false, "L", &opts, "9").is_err());
    assert!(parse_single("date", false, "D", &[], "06.05.2026").is_err());
}
```
`parse_single` использует те же ветки, что `parse_item` (вынести общий разбор в `parse_item_kind(kind, name, s, options)`), плюс пустое значение: `required` → ошибка; `boolean` → `Value::Bool(false)` (как в карточке); иначе `Value::Null`.

- [ ] **Step 2:** `cargo test collection::fields::tests::single_cell_values` → FAIL.

- [ ] **Step 3: Тесты маршрута**

В `src/admin/list/edit.rs` (модуль `tests`) — хелперы `app`, `post`, `enc`, `version`, `field`, `cell_form` (по образцу `bulk.rs::tests`; POST на `/admin/lists/cell` с `hx-request: true`, `hx-target: list-body`):

```rust
async fn version(db: &PgPool, id: i64) -> String {
    sqlx::query_scalar("SELECT updated_at::text FROM collection_items WHERE id = $1")
        .bind(id).fetch_one(db).await.unwrap()
}

async fn field(db: &PgPool, c: i64, code: &str, kind: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO collection_fields (collection_id, code, name, kind) VALUES ($1,$2,$2,$3) RETURNING id",
    ).bind(c).bind(code).bind(kind).fetch_one(db).await.unwrap()
}
```

Тесты (все `#[sqlx::test]`, `content_fixture`; перед каждым POST — свежая `version(&db, id)`, потому что сохранение меняет `updated_at`):
- `cell_save_standard_and_custom`: `name` → БД и тело содержат новое; `code` → `code`; `sort` → `5`; `active` → `false`; затем добавить поля `s`/`n`/`b`/`d` через `field(...)` и сохранить `s="x"`, `n="2,5"`, `b="true"`, `d="2026-05-06"`, `color=<id варианта>`; проверять `SELECT field_values->>'…'`.
- `cell_required_empty_422`: `name=""` → 422, имя в БД прежнее.
- `cell_foreign_option_422`: `color=999999` → 422.
- `cell_code_conflict_422`: второй item с кодом `other`, сохранить ему `pervaya` → 422.
- `cell_filtered_row_may_disappear`: `query="f.active=eq:Y"`, сохранить `active=false` → 200, в теле нет строки с `data-id="<id>"` (Review Focus 1).
- `cell_stale_version_409`: сохранить версию, `UPDATE … SET updated_at = now() + interval '1 hour'`, POST со старой версией → 409, значение в БД прежнее (Review Focus 2).
- `cell_missing_404`, `cell_foreign_collection_404`.
- `cell_readonly_403_and_no_attrs`: пользователь `permitted_cookie(&db, "reader", &[])` + `collection_access` read для коллекции; GET списка не содержит `data-edit` и `data-version`, `edit_columns` пуст; POST → 403; у админа `data-edit`/`data-version` есть, `edit_columns` не пуст (Review Focus 4).
- `cell_response_is_list_body`: успешный POST → тело содержит `id="list-body"` и новый текст.
- `cell_bad_path_422`: `path="/admin/shop/orders"` для `collection:<id>` → 422.

- [ ] **Step 4:** `cargo test admin::list::edit` → FAIL.

- [ ] **Step 5: Реализовать `parse_single` и `handle`**

Разбор формы в структуру; шаги:
1. `row` — не число → 422 «Неверная запись».
2. `spec_for_key` (403/404); `path == views::list_path(&list_key)` иначе 422.
3. `column` ищется в `spec.columns`; `editable` нет → `views::reject(403, "Поле недоступно")`.
4. `collection_id` из `list_key` (`collection:<id>`), иначе 404.
5. `version` пустая → 422 «Обновите список».
6. Значение:
   - `Standard(Name)`: `trim`, пусто → 422 «Укажите название»; `Value::String`.
   - `Standard(Code)`: `trim`; непустое и `!is_valid_slug` → 422; `Value::String` (пустое допустимо).
   - `Standard(Sort)`: `trim().parse::<i32>()` → 422 «Сортировка: ожидается число»; `Value::from`.
   - `Standard(Active)`: `matches!(trim.to_lowercase(), "1"|"true"|"on")` → `Value::Bool`.
   - `Field(_)`: `fields::parse_single(kind_str, required, title, options, raw)`, где `kind_str` — `Text→"string"`, `Number→"number"`, `Bool→"boolean"`, `Date→"date"`, `List→"list"`.
7. `UPDATE` (bind только значения):
   - стандартное — `SET <колонка> = $1, updated_at = now()`;
   - поле — `SET field_values = jsonb_set(field_values, ARRAY[$1], $2::jsonb, true), updated_at = now()`;
   - `WHERE id = $ AND collection_id = $ AND updated_at = $::timestamptz`.
8. `rows_affected == 1` → успех. `0` → `SELECT 1 … WHERE id=$ AND collection_id=$`: нет → 404 «Запись не найдена», есть → 409 «Запись изменилась, список обновлён».
9. `23505` (`collection_items_code_key`) → 422 «Код уже занят», прочее — `AppError` (500, `tracing::error!`).
10. Успех — `views::rerender(...)` + `views::with_toast(..., OK, "Сохранено", FlashKind::Ok)`.

`views::reject` — перенос из `bulk.rs` (`HX-Reswap: none` + `with_toast`), `bulk.rs` вызывает `views::reject`.

- [ ] **Step 6:** `cargo test` → PASS (все существующие тесты тоже). `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.

- [ ] **Step 7: Commit** `Админка: сохранение ячейки списка`.

### Task 3: Интерфейс правки ячейки

**Files:**
- Modify: `static/admin/list.js`, `static/admin/admin.css`
- Test: браузерная проверка

**Interfaces:**
- Consumes: `#list-spec` (`edit_columns`, `list_key`, `path`, `query`), `td[data-edit][data-kind][data-value][data-first]`, `tr[data-version]`, `POST /admin/lists/cell`.

- [ ] **Step 1: Открытие и закрытие редактора**

В `list.js` добавить карту `editDef(code)` из `spec().edit_columns` и состояние `editing` (ячейка, исходная разметка, редактор). Делегирование:
- `dblclick` по `td[data-edit]`, кроме `data-kind="bool"` — заменить содержимое редактором: `input` для `text`/`number`, `input[type=date]` для `date`, `select` для `list` (пустой пункт при `!required`); фокус и выделение;
- `click` по `td[data-kind="bool"]` — сохранить противоположное значение (без редактора);
- `keydown` в редакторе: `Enter`/`blur` — сохранить, `Esc` — отменить и вернуть исходную разметку, `Tab` — сохранить и открыть следующую редактируемую ячейку;
- не начинать правку при открытом поповере/`dialog[open]` и при `busy`.

- [ ] **Step 2: Сохранение и ошибки**

`POST /admin/lists/cell` через `htmx.ajax(..., {target:'#list-body', swap:'outerHTML', values:{list_key, path, query, row, column, value, version}})`; `version` — из `tr.dataset.version`, `path`/`query`/`list_key` — из `spec()`. На время запроса — класс `saving`, поля заблокированы. `htmx:responseError`: 422 — редактор остаётся и разблокируется; 409/404 — `htmx.trigger(document.body, 'list:refresh')` и закрыть редактор.

- [ ] **Step 3: Первая колонка и сохранение позиции**

Ссылка внутри `td[data-first]`: `click` перехватывается и через ~200 мс открывает карточку; второй клик отменяет таймер, `dblclick` открывает редактор (флаг, чтобы программный клик после задержки не перехватывался). После замены `#list-body` — восстановить `window.scrollY` и класс `.cur` по сохранённому id.

- [ ] **Step 4: Стили**

`admin.css`: курсор/подчёркивание у `td[data-edit]`, стили `.cell-edit input/select`, состояние `.saving`; на узком экране редактор не выходит за таблицу.

- [ ] **Step 5:** `node --check static/admin/list.js` → PASS.

- [ ] **Step 6: Проверка в Chrome** (dev-сервер на свободном порту, не 3000): двойной клик по названию (карточка не открывается), одиночный клик по названию (открывается), `Enter`/`Esc`/`Tab`, «да/нет» одним кликом, список, ошибка обязательного/чужого варианта (редактор остаётся), 409 (две вкладки — список обновляется, значение не затирается), сохранение при активном фильтре (строка выпадает), сброс выбора строк, узкий экран.

- [ ] **Step 7: Commit** `Админка: правка ячейки в списке`.

### Task 4: Итоговая проверка

- [x] `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `node --check static/admin/list.js` → PASS (2026-11-08: 1129 тестов).
- [x] `README.md`, раздел «Интерфейс админки» → «Списки»: абзац о правке в таблице (двойной клик, `Enter`/`Esc`/`Tab`, типы полей, только при праве записи, 409).
- [ ] Commit `Админка: этап 3.3 — итоговые правки`, если что-то изменилось.
