# Админка, этап 3.2: массовые действия — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Массовые действия в списках записей коллекций и заказов: выбор строк или всех по фильтру (до 10 000), выполнение по одной записи с итогом, поведение как у одиночных действий.

**Architecture:** Побочные эффекты одиночных действий выносятся в общие функции (`sale::actions`, `collection::repo`, `items::delete_one`). `src/admin/list/bulk.rs` описывает действия (`ListSpec.bulk_actions`), отбирает id тем же движком списка, выполняет их по одному через реестр по `list_key` и возвращает обновлённый `#list-body` с итогом. Выбор и панель — в `static/admin/list.js`.

**Tech Stack:** Rust, axum, sqlx (Postgres), MiniJinja, HTMX 2, чистый JS.

**Spec:** `docs/superpowers/specs/2026-11-05-admin-bulk-actions-design.md`

## Global Constraints

- Ветка `feature/admin-bulk` (от `main`). Коммиты на русском в стиле `Админка: …`, без упоминаний ИИ и `Co-Authored-By`.
- Тесты: `cargo test <путь>` (бинарный крейт, без `--lib`); перед коммитом задачи — полный `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`; при правке JS — `node --check static/admin/list.js`.
- Лимит «по фильтру» — 10 000 записей; 10 001-я → 422 «Слишком много записей: уточните фильтр (до 10 000)». Пустой выбор → 422 «Ничего не выбрано».
- Причина отмены — не длиннее 500 символов.
- Значения из формы — только через `push_bind`; в текст SQL — только выражения спеки.
- Карточки заказа и записи ведут себя как раньше: существующие тесты остаются зелёными без правок ожиданий.
- Ответы 4xx не подменяют список; уведомления — `flash::toast`.
- Остальные списки и маршруты не меняются.

## Review Focus

1. Выбор `scope=ids` с id, которые сейчас скрыты фильтром или из другой коллекции/списка: они не обрабатываются и не попадают в итог. → тест в Task 2.
2. Смена статуса заказа на тот же статус, что уже стоит: письмо не уходит (как в карточке) и запись считается выполненной. → тест в Task 1.
3. Удаление записи, которую между выбором и выполнением уже удалили или переместили в другую коллекцию: причина «запись не найдена», остальные выполняются. → тест в Task 3.
4. Повторная отправка той же формы (двойной клик / повтор запроса): действия идемпотентны по смыслу — повторная деактивация не ошибка, повторная отмена не шлёт второе письмо. → тест в Task 4.
5. Действие «все по фильтру» при открытом виде с изменёнными фильтрами (`edited=1`): берётся текущее состояние из `query`, а не сохранённый вид. → тест в Task 2.

---

## Файлы

| Файл | Ответственность |
|---|---|
| `src/sale/actions.rs` | смена статуса, оплаты, отмены заказа + какое письмо слать |
| `src/admin/shop/orders.rs` | обработчики карточки на `sale::actions`; действия списка заказов |
| `src/collection/repo.rs` | `set_item_active`, `move_item` |
| `src/admin/items.rs` | `delete_one`; действия списка записей |
| `src/admin/list/bulk.rs` | типы, форма, отбор id, выполнение, итог, маршрут |
| `src/admin/list/spec.rs`, `sql.rs` | поле `bulk_actions`; выборка id |
| `templates/admin/_list_body.html`, `_bulk.html` | колонка чекбоксов, данные панели, блок итога |
| `static/admin/list.js`, `admin.css`, `base.html` | выбор, панель, подтверждения, клавиши X |

---

### Task 1: Общие функции действий над заказом

**Files:**
- Create: `src/sale/actions.rs` (подключить в `src/sale/mod.rs`)
- Modify: `src/admin/shop/orders.rs` (`set_status` ~652, `set_paid` ~708, `set_canceled` ~726, `send` ~570)

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum MailEvent { StatusChanged(String), Paid, Canceled }
  impl MailEvent { pub fn event_name(&self) -> String }  // "SALE_STATUS_CHANGED_<код>", "SALE_ORDER_PAID", "SALE_ORDER_CANCEL"
  pub enum ActionError { NotFound, Rejected(String), Db(sqlx::Error) }
  pub async fn change_status(db: &PgPool, id: i64, status: &str, user_id: Option<i64>, comment: &str) -> Result<Option<MailEvent>, ActionError>;
  pub async fn change_paid(db: &PgPool, id: i64, paid: bool) -> Result<Option<MailEvent>, ActionError>;
  pub async fn change_canceled(db: &PgPool, id: i64, cancel: bool, reason: &str) -> Result<Option<MailEvent>, ActionError>;
  ```
  Правила писем — как сейчас в обработчиках: статус — `Some` только если `repo::set_status` вернул «изменено» и у статуса `notify`; оплата — только при изменении и `paid = true`; отмена — только при изменении и `cancel = true`. Неизвестный статус → `Rejected("Нет такого статуса")`; `StockError::Rejected(m)` → `Rejected(m.join("; "))`; несуществующий заказ → `NotFound`.
- `send(state, event, id)` в `orders.rs` принимает `&MailEvent` (`event_name()`).

- [ ] **Step 1: Тесты** в `src/sale/actions.rs` (`#[sqlx::test]`, `test_support::order_fixture` + `place_order`):
  - `status_mail_only_when_changed_and_notify` — статус с `notify = true` → `Some(StatusChanged(code))`; тот же статус повторно → `None`; статус с `notify = false` → `None`; `"nope"` → `Rejected("Нет такого статуса")`;
  - `paid_mail_only_on_set` — `change_paid(true)` → `Some(Paid)`; повторно → `None`; `false` → `None`;
  - `cancel_rejected_when_shipped` — заказ с отгрузкой `deducted = TRUE` → `Rejected(..)`; обычный → `Some(Canceled)`; повторно → `None`;
  - `missing_order` — несуществующий id → `NotFound` во всех трёх.
- [ ] **Step 2:** `cargo test sale::actions` → FAIL.
- [ ] **Step 3:** Реализовать; переписать три обработчика карточки на эти функции без изменения ответов.
- [ ] **Step 4:** `cargo test` → PASS (включая все тесты `admin::shop::orders`).
- [ ] **Step 5: Commit** `Админка: общие функции действий над заказом`.

### Task 2: Ядро массовых действий

**Files:**
- Create: `src/admin/list/bulk.rs`, `templates/admin/_bulk.html` (блок итога)
- Modify: `src/admin/list/spec.rs` (поле `bulk_actions: Vec<BulkAction>`, пусто в `test_spec` и в существующих построителях), `src/admin/list/sql.rs` (выборка id), `src/admin/list/views.rs` (`rerender` → `pub(crate)`), `src/admin/list/mod.rs`, `src/admin/mod.rs` (маршрут `POST /admin/lists/bulk`), `templates/admin/_list_body.html` (место под блок итога)

**Interfaces:**
- Consumes: `views::spec_for_key`, `views::list_path`, `views::resolve(db, user_id, spec, raw_query, restore=false)`, `views::rerender`, `collection::section_scope`, `sql::push_where`.
- Produces:
  ```rust
  // bulk.rs
  pub enum BulkParam { None, Choice(Vec<EnumOption>), Reason }
  pub enum Confirm { Never, Always, Danger }
  pub struct BulkAction { pub code: &'static str, pub label: String, pub param: BulkParam, pub confirm: Confirm, pub notify: bool }
  pub const BULK_LIMIT: i64 = 10_000;
  pub struct BulkForm { list_key, path, query, action, param, notify: bool, scope: Scope, ids: Vec<i64> }  // scope: Ids | Filter
  pub struct Outcome { pub done: usize, pub failed: Vec<(i64, String)> }
  pub struct SummaryGroup { pub reason: String, pub links: Vec<(String, String)>, pub more: usize } // (подпись, url), max 20
  pub fn summarize(outcome: &Outcome, labels: &HashMap<i64, String>, open_url: &str) -> Vec<SummaryGroup>;
  pub fn mails_to_send<T>(event: Option<T>, notify: bool) -> Option<T>;
  pub struct BulkCtx<'a> { pub state: &'a AppState, pub user: &'a Access, pub notify: bool, pub collection_id: Option<i64> }
  pub async fn handle(State, Extension<Access>, Form<HashMap<String,String>>) -> AppResult<Response>; // маршрут
  // sql.rs
  pub fn ids(spec: &ListSpec, st: &ListState, scope: &Scope, only: Option<&[i64]>, limit: i64) -> QueryBuilder<Postgres>;
  ```
  `handle`: шаги 1–6 раздела 3 спеки. Реестр — `async fn run(ctx, list_key, action, param, id) -> Result<(), String>` в `bulk.rs`, диспетчер по `list_key` на `orders::bulk_run` (Task 4) и `items::bulk_run` (Task 3); до этих задач оба возвращают `Err("неизвестное действие")`. Подписи записей для итога — один запрос `SELECT id, <label_expr>` по id невыполненных: `label_expr` — новое поле `ListSpec.label_expr: String` (заказы: `COALESCE(o.account_number, o.id::text)`, записи: `ci.name`, `test_spec`: `id::text`).
  Ответ: `views::rerender` для `path?query` + либо `flash::toast("Готово: N")`, либо блок `_bulk.html` в начале `#list-body` (передать список групп в контекст рендера; `run_list` получает необязательный `bulk_summary` — добавить в `ListPage` или параметр рендера).

- [ ] **Step 1: Юнит-тесты** `bulk.rs`: `summarize_groups_and_caps` (25 одинаковых причин → одна группа, 20 ссылок, `more = 5`; две причины → две группы в порядке первой встречи); `form_parse` (ids с мусором и повторами → уникальные числа; `scope=filter` игнорирует ids; `notify=1` → true); `mails_to_send` (Some+true → Some, Some+false → None).
- [ ] **Step 2: Тесты с базой** (`test_spec` над `generate_series` и через маршрут на заказах с действием-заглушкой, которое тест подставляет через `#[cfg(test)]` код действия `noop` в реестре — возвращает `Ok` для чётных id и `Err("нечётный")` для нечётных):
  - `ids_intersect_with_filter` — `scope=ids` с id вне фильтра и из чужого списка → обработаны только видимые (Review Focus 1);
  - `filter_scope_limit` — 10 001 строка → 422 с текстом из Global Constraints; 10 000 → выполнено;
  - `empty_selection_422`;
  - `edited_view_uses_query` — открытый вид + `edited=1` и другой фильтр в `query` → берутся записи `query`, не вида (Review Focus 5);
  - `unknown_action_403`, `bad_path_422`, `no_list_access_403`;
  - `summary_block_rendered` — ответ содержит `#list-body`, текст «Выполнено K из N» и ссылки на нечётные.
- [ ] **Step 3:** FAIL → **Step 4:** реализовать → **Step 5:** `cargo test` PASS.
- [ ] **Step 6: Commit** `Админка: ядро массовых действий списков`.

### Task 3: Действия записей коллекций

**Files:**
- Modify: `src/collection/repo.rs` (`set_item_active`, `move_item`), `src/admin/items.rs` (`delete_one`, обработчик `delete` на нём, `bulk_run`), `src/admin/list/collection.rs` (`bulk_actions` и `label_expr` в спеке)

**Interfaces:**
- Produces:
  ```rust
  pub async fn set_item_active(db: &PgPool, id: i64, active: bool) -> sqlx::Result<bool>;          // false — нет записи
  pub async fn move_item(db: &PgPool, id: i64, section_id: Option<i64>) -> sqlx::Result<bool>;
  pub async fn delete_one(db: &PgPool, user: &Access, id: i64) -> AppResult<crate::collection::Item>; // удалённая запись (для редиректа)
  pub async fn bulk_run(ctx: &BulkCtx<'_>, action: &str, param: &str, id: i64) -> Result<(), String>;
  ```
  `delete_one` — проверки и удаление из нынешнего `items::delete` (`require_collection(Write)`, `require_parent_write`, `repo::delete_item`); ошибки — `AppError::NotFound` / `AppError::Forbidden`. Обработчик `delete` использует `delete_one` (редирект как раньше по `product_id`/`section_id` вернувшейся записи). `bulk_run` переводит `NotFound` → «запись не найдена», `Forbidden` → «нет права», `Db` → «ошибка сервера» + `tracing::error!`.
  `bulk_run`: каждый вызов проверяет, что запись принадлежит `ctx.collection_id` (иначе `Err("запись не найдена")`); `move` — `param` `none` или id раздела этой коллекции (проверка в `handle` и повторно здесь); `activate`/`deactivate` идемпотентны (повтор — `Ok`).
  Спека коллекции: `bulk_actions` только при `collection_level >= Write` — `activate`, `deactivate` (`Confirm::Never`), `move` (`Choice`: «Без раздела» = `none`, разделы деревом с отступом `"— "` × глубина, `Confirm::Always`), `delete` (`Confirm::Danger`). `label_expr: "ci.name"`.

- [ ] **Step 1: Тесты** (`#[sqlx::test]`, `content_fixture`):
  - `bulk_activate_deactivate_move` — по `ids` и по `filter`; повтор — без ошибок;
  - `bulk_move_foreign_section_422`;
  - `bulk_delete_like_single` — товар с предложениями удаляется так же, как одиночным удалением (сравнить состояние после `delete` и после `bulk delete`);
  - `bulk_delete_missing_record` — одна запись удалена до выполнения → в итоге «запись не найдена», остальные удалены (Review Focus 3);
  - `bulk_readonly_user` — пользователь с `read`: в HTML списка нет колонки чекбоксов, POST → 403.
- [ ] **Step 2:** FAIL → **Step 3:** реализовать → **Step 4:** `cargo test` PASS.
- [ ] **Step 5: Commit** `Админка: массовые действия записей коллекций`.

### Task 4: Действия заказов

**Files:**
- Modify: `src/admin/shop/orders.rs` (`bulk_run`), `src/admin/list/orders.rs` (`bulk_actions`, `label_expr`)

**Interfaces:**
- Consumes: Task 1 (`sale::actions`, `send`), Task 2 (`BulkCtx`, `mails_to_send`).
- Produces: `pub async fn bulk_run(ctx: &BulkCtx<'_>, action: &str, param: &str, id: i64) -> Result<(), String>`.
  Действия: `status` (`Choice` из `order_statuses` по `sort, code`; `notify: true`), `paid` (`notify: true`), `unpaid`, `cancel` (`Reason`, `notify: true`), `uncancel`; все `Confirm::Always`; только при `orders.manage`. Письмо — `send(state, &event, id)` если `mails_to_send(event, ctx.notify)`. `ActionError::Rejected(t)` → `Err(t)`, `NotFound` → `Err("заказ не найден")`, `Db` → `Err("ошибка сервера")` + `tracing::error!`. `label_expr: "COALESCE(o.account_number, o.id::text)"`.

- [ ] **Step 1: Тесты** (`#[sqlx::test]`, `order_fixture`):
  - `bulk_status_change` — три заказа, `status` → все с новым статусом; итог «Готово: 3»;
  - `bulk_cancel_skips_shipped` — один заказ отгружен → в итоге с причиной из `StockError`, остальные отменены;
  - `bulk_repeat_is_harmless` — повторная отмена/оплата тех же заказов → `Ok`, `sale::actions` вернули `None` (второго письма нет) (Review Focus 4);
  - `bulk_reason_too_long_422` (501 символ);
  - `bulk_orders_without_permission_403`.
- [ ] **Step 2:** FAIL → **Step 3:** реализовать → **Step 4:** `cargo test` PASS.
- [ ] **Step 5: Commit** `Админка: массовые действия заказов`.

### Task 5: Интерфейс выбора и панель действий

**Files:**
- Modify: `templates/admin/_list_body.html` (колонка чекбоксов при непустом `bulk_actions`; `data-id` уже у строк; данные для панели — `total`, `capped`, `bulk_actions` в `#list-spec` JSON), `static/admin/list.js`, `static/admin/admin.css`, `templates/admin/base.html` (клавиши X / Shift+X в окне «Горячие клавиши»)

**Interfaces:**
- Consumes: `#list-spec` (добавить `bulk_actions: [{code,label,param:{kind,options},confirm,notify}]`, `list_key`, `path`), `POST /admin/lists/bulk` (Task 2).
- Поведение — раздел 4 спеки: чекбоксы, Shift+клик и Shift+X диапазоны, X — текущая строка, шапка с `indeterminate`, подсветка выбранных, сброс выбора на каждом swap `#list-body`, панель внизу («Выбрано: N · Выбрать все N по фильтру · Действия ▾ · ✕»; при `capped` — «Больше 10 000 — уточните фильтр»), Escape снимает выбор, поповер подтверждения (параметр, «Уведомить покупателей» по умолчанию включена, `Danger` — красная кнопка «Удалить N»; «все по фильтру» — подтверждение всегда с числом), «Выполняется…» с блокировкой, отправка — `htmx.ajax('POST', '/admin/lists/bulk', {target:'#list-body', swap:'outerHTML', values:{…}})`, без `pushState`. Клавиши X/Shift+X не работают в полях ввода, при открытом поповере, `dialog[open]`.
- [ ] **Step 1:** `cargo test` (шаблоны) и `node --check static/admin/list.js` → PASS.
- [ ] **Step 2: Тест** `bulk_checkbox_column_only_with_actions` — список у пользователя с правом записи содержит колонку чекбоксов, у пользователя с `read` — нет.
- [ ] **Step 3: Проверка в Chrome** (dev-сервер на свободном порту, не трогая 3000): чекбоксы, Shift+клик, X/Shift+X, шапка, панель, «Выбрать все по фильтру», подтверждение статуса с галочкой, удаление с красной кнопкой, итог с ошибками (отгруженный заказ), сброс выбора при смене фильтра, узкий экран без горизонтальной прокрутки страницы.
- [ ] **Step 4: Commit** `Админка: выбор строк и панель массовых действий`.

### Task 6: Итоговая проверка

- [ ] `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `cargo test` → PASS.
- [ ] README: в «Интерфейс админки» → «Списки» — абзац о массовых действиях (выбор, «все по фильтру» до 10 000, действия записей и заказов, галочка «Уведомить покупателей», итог).
- [ ] Commit `Админка: этап 3.2 — итоговые правки`, если что-то изменилось.
