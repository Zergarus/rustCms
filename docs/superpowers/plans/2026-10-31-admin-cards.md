# Админка, этап 2: карточки на вкладках и быстрый просмотр — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** карточки товара, пользователя, коллекции и скидки на вкладках с точкой ошибки; панель
быстрого просмотра заказа и товара из списков.

**Architecture:** общий модуль `src/admin/card.rs` (ошибки по вкладкам, выбор активной
вкладки), макрос вкладок в `_ui.html` и переключение в `admin.js`; затем каждая карточка
переводится на них по очереди; затем панель `#peek` с фрагментами `…/peek`.

**Tech Stack:** Rust 2024, Axum 0.8, MiniJinja, HTMX 2.0.4, чистый JS; `#[sqlx::test]`.

**Spec:** `docs/superpowers/specs/2026-10-31-admin-cards-design.md`

## Global Constraints

- Ветка `feature/admin-cards` (от `main`). Без новых зависимостей, без миграций.
- Ключи вкладок (дословно): товар `main`, `text`, `fields`, `catalog`, `offers`; пользователь
  `profile`, `access`, `password`, `keys`, `orders`; коллекция `settings`, `fields`, `offers`;
  скидка `main`, `conditions`, `actions`, `groups`, `coupons`.
- Названия вкладок (дословно): «Основное», «Описание», «Поля», «Каталог», «Предложения»;
  «Профиль», «Доступ», «Пароль», «API-ключи», «Заказы»; «Настройки», «Поля», «Предложения»;
  «Основное», «Условия», «Действия», «Группы», «Купоны».
- Адрес активной вкладки — `?tab=<ключ>`; неизвестный ключ — первая видимая вкладка; при
  ошибках сохранения — первая вкладка с ошибкой.
- Клавиши `[` / `]` — предыдущая/следующая вкладка (не в полях ввода, как остальные клавиши
  этапа 1). Уведомление после сохранения — «Сохранено».
- Логика сохранения, тексты проверок, права и API сайта не меняются; ответы `/api/v1` и bxapi
  байт в байт.
- Без `confirm()`/`alert()`; уведомления — существующие тосты `admin.js`.
- Тексты и комментарии по-русски; коммиты по-русски, без упоминаний ИИ и `Co-Authored-By`;
  `docs/benchmark-bitrix-vs-rust.md` не коммитится.
- Проверка после задачи: `cargo test` (вывод в файл), `cargo clippy --all-targets -- -D warnings`,
  `cargo fmt --check`.

## Review Focus

1. **Ошибка на скрытой вкладке** (например, ошибка цены, а у коллекции выключен каталог, или
   вкладка скрыта правами). Ожидание: ошибка показывается над вкладками, не теряется. Тест в
   Task 1 `error_on_hidden_tab_goes_general`.
2. **Переход по вкладке с несохранёнными правками и Ctrl+S с другой вкладки.** Ожидание:
   отправляется вся форма, правки с обеих вкладок сохранены. Тест в Task 2
   `save_from_any_tab_sends_all_fields`.
3. **Поле с `required` на скрытой вкладке.** Ожидание: браузер не молчит — JS при неудачной
   проверке формы переключает на вкладку первого невалидного поля. Обработчик `invalid` — в
   Task 1 (автотестов JS в проекте нет), проверка — шаг 2 Task 8.
4. **Клик по строке списка с открытым выделением текста или по чекбоксу/ссылке внутри строки.**
   Ожидание: панель не открывается. Ручная проверка в Task 8; разметка-тест в Task 6
   `row_has_peek_attr_and_links_stay`.
5. **Действие из панели над заказом, который открыт другим менеджером и уже отменён.**
   Ожидание: красное уведомление с текстом существующей проверки, панель показывает актуальное
   состояние. Тест в Task 6 `peek_paid_on_canceled_shows_error`.

---

### Task 1: Общая механика вкладок

**Files:**
- Create: `src/admin/card.rs`
- Modify: `src/admin/mod.rs` (`mod card;`), `templates/admin/_ui.html`, `static/admin/admin.js`,
  `static/admin/admin.css`

**Interfaces:**
- Produces:
  - `pub(crate) struct TabErrors { items: Vec<(&'static str, String)> }` — `""` означает «без
    вкладки»; `new()`, `push(tab: &'static str, msg: impl Into<String>)`, `is_empty()`,
    `extend_general(msgs: Vec<String>)`.
  - `pub(crate) fn tab_context(errors: &TabErrors, visible: &[&'static str], requested:
    Option<&str>) -> minijinja::Value` — `{ active, error_tabs: [ключи], by_tab: {ключ: [тексты]},
    general: [тексты] }`; `active` = первая видимая вкладка с ошибкой, иначе `requested`, если он
    среди видимых, иначе `visible[0]`; ошибки невидимых вкладок и `""` → `general`.
  - Макросы `_ui.html`: `card_tabs(items, tabctx)` — `items` пары `(ключ, название)`; рисует
    `<nav class="tabs card-tabs" role="tablist">` с кнопками `data-tab-btn="<ключ>"`, классом
    `active`, точкой ошибки `<span class="tab-dot err">` для `tabctx.error_tabs` и пустой точкой
    «изменено» `<span class="tab-dot dirty" hidden>`; `tab_pane(key, tabctx)` — `{% call %}`
    обёртка `<section data-tab="<ключ>" {% if key != tabctx.active %}hidden{% endif %}>` с
    блоком ошибок вкладки `<div class="alert">` сверху; `card_errors(tabctx)` — общие ошибки.
  - `admin.js`: клик по `[data-tab-btn]` и `[`/`]` переключают вкладки ближайшего
    `[data-card-tabs]` без запроса, пишут `?tab=` через `history.replaceState` и обновляют URL
    записи в строке открытых записей тем же вызовом, что делает переключение подвкладок заказа;
    ввод в поле внутри `section[data-tab]` показывает точку «изменено» на её кнопке (сбрасывается
    вместе с панелью «Сохранить»); событие `invalid` на поле скрытой вкладки переключает на неё.

- [ ] **Step 1: Тесты** (`#[test]` в `card.rs`): `active_prefers_first_error_tab`
  (`visible=["main","text"]`, ошибка в `text`, `requested=Some("main")` → `active="text"`);
  `active_uses_requested_when_valid`; `unknown_requested_falls_back_first`;
  `error_on_hidden_tab_goes_general` (ошибка `catalog` при `visible=["main"]` → `general`,
  `error_tabs` пуст); `general_errors_kept`. Шаблонный тест (`#[test]` с окружением MiniJinja
  проекта, как другие тесты шаблонов, если есть; иначе рендер через маленький тестовый шаблон)
  `card_tabs_marks_active_and_error`.
- [ ] **Step 2:** `cargo test admin::card` — FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Админка: вкладки карточки — общая механика».

### Task 2: Карточка товара

**Files:**
- Modify: `src/admin/items.rs` (сборка ошибок, контекст вкладок, видимость), `templates/admin/item_form.html`

**Interfaces:**
- Consumes: Task 1.
- Produces: ошибки `build_input`/`check_references`/`save` и вкладки каталога — `TabErrors` с
  ключами: название, код, раздел, дата, внешний код → `main`; поля и файлы → `fields`; цены,
  остатки, штрихкоды, закупочная цена, тип → `catalog`; «запись изменилась»/прочие → `""`.
  Видимые вкладки: `main`, `text`; `fields` — если у коллекции есть поля; `catalog` — если
  включён каталог; `offers` — у сохранённого товара с предложениями. `edit_form` и `update`
  принимают `?tab=`; редирект после успешного сохранения — на `…?tab=<текущая>` (форма шлёт
  скрытое поле `tab`, его ставит JS при переключении), уведомление «Сохранено». Блок
  «Предложения» — вне основной формы в своей `tab_pane`. Кнопка «Удалить» в шапке (`page_head`
  actions) с тем же POST и подтверждением, блок «Удаление» внизу убран.

- [ ] **Step 1: Тесты** (`#[sqlx::test]`, HTTP): `item_card_tabs_visibility` (коллекция без
  полей и каталога → только `main`,`text`; с каталогом → и `catalog`; товар с предложениями →
  `offers`); `item_tab_query_opens_tab`; `item_error_opens_error_tab` (пустое название и
  неверная цена → активна `main`, точки на `main` и `catalog`, тексты в своих секциях);
  `item_save_returns_to_tab` (POST с `tab=catalog` → редирект `?tab=catalog`);
  `save_from_any_tab_sends_all_fields` (одна форма: изменённые поля `main` и `catalog` в одном
  POST сохранены); `new_item_only_form_tabs`; `item_delete_button_in_header`; табличный
  `item_errors_tagged` (каждая проверка → ожидаемый ключ).
- [ ] **Step 2:** `cargo test admin::items` — новые FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Админка: карточка товара на вкладках».

### Task 3: Карточка пользователя

**Files:**
- Modify: `src/admin/users.rs`, `templates/admin/user_form.html`

**Interfaces:**
- Consumes: Task 1.
- Produces: ошибки `UserForm::validate` → логин, имя, e-mail → `profile`; группы,
  суперадминистратор, активность → `access`; пароль → `password`. Вкладки `profile`, `access`,
  `password` — основная форма; `keys` (блок API-ключей, как сейчас, право `users.manage`) и
  `orders` — вне формы. `orders`: последние 20 заказов пользователя (`orders.user_id`, по
  `id DESC`): номер со ссылкой на `/admin/shop/orders/{id}`, дата, сумма, статус, оплачен;
  видна при `orders.manage`. Выпуск/отзыв ключа возвращают страницу с `?tab=keys`. Удаление —
  кнопка в шапке.

- [ ] **Step 1: Тесты:** `user_card_tabs_visibility` (без `orders.manage` нет `orders`; новая
  запись — только `profile`,`access`,`password`); `user_orders_tab_lists_last_20`;
  `user_error_opens_error_tab`; `user_save_returns_to_tab`; `issue_key_opens_keys_tab`;
  `user_errors_tagged`.
- [ ] **Step 2:** `cargo test admin::users` — новые FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Админка: карточка пользователя на вкладках».

### Task 4: Карточка коллекции

**Files:**
- Modify: `src/admin/collections.rs`, `templates/admin/collection_form.html`

**Interfaces:**
- Consumes: Task 1.
- Produces: вкладка `settings` — основная форма (ошибки названия, кода, описания → `settings`);
  `fields` (таблица и форма добавления поля, POST `/collections/{id}/fields`) и `offers`
  (POST `/collections/{id}/offers`) — вне формы, после своих POST возврат на `?tab=fields` /
  `?tab=offers`; ошибки формы поля показываются на `fields`. Новая коллекция — только
  `settings`. Удаление — кнопка в шапке.

- [ ] **Step 1: Тесты:** `collection_card_tabs`; `add_field_error_opens_fields_tab`;
  `set_offers_returns_to_offers_tab`; `collection_errors_tagged`.
- [ ] **Step 2:** `cargo test admin::collections` — новые FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Админка: карточка коллекции на вкладках».

### Task 5: Карточка скидки

**Files:**
- Modify: `src/admin/discounts.rs`, `templates/admin/discount_form.html`

**Interfaces:**
- Consumes: Task 1.
- Produces: одна форма; ошибки → название, период, приоритет, сортировка → `main`; дерево
  условий → `conditions`; дерево действий → `actions`; группы → `groups`. Вкладка `coupons` —
  вне формы: ссылка на страницу купонов скидки с числом купонов; видна у сохранённой скидки с
  включёнными купонами. Удаление — кнопка в шапке.

- [ ] **Step 1: Тесты:** `discount_card_tabs`; `discount_error_opens_error_tab` (неверное
  условие → `conditions`); `discount_save_returns_to_tab`; `discount_errors_tagged`.
- [ ] **Step 2:** `cargo test admin::discounts` — новые FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Админка: карточка скидки на вкладках».

### Task 6: Панель быстрого просмотра и заказ

**Files:**
- Create: `templates/admin/_peek.html` (рамка панели), `templates/admin/shop/order_peek.html`
- Modify: `templates/admin/base.html` (контейнер `<aside id="peek" hidden>`),
  `templates/admin/shop/orders.html` (строки), `src/admin/shop/orders.rs`, `src/admin/mod.rs`,
  `static/admin/admin.js`, `static/admin/admin.css`

**Interfaces:**
- Produces:
  - Строка списка: `<tr id="order-row-{id}" data-peek="/admin/shop/orders/{id}/peek">`; ссылки
    и кнопки в строке остаются.
  - JS: клик по `tr[data-peek]`, если цель не внутри `a, button, input, select, label` и нет
    выделенного текста, — `htmx.ajax('GET', url, '#peek')`, показать `#peek`, подсветить строку
    (`.peeked`); Esc и крестик закрывают; на ширине < 760 px панель во весь экран.
  - `GET /admin/shop/orders/{id}/peek` (`orders.manage`) — `order_peek.html`: номер, дата,
    покупатель, телефон, e-mail, сумма, до 5 позиций и «ещё N», статус (выпадающий список),
    оплата, отгрузка, доставка, «Открыть карточку».
  - Существующие `POST /shop/orders/{id}/status` и `/paid`: если запрос с `HX-Target: peek` —
    ответ = панель + OOB-замена строки `#order-row-{id}`; отказ — панель с актуальным
    состоянием и `HX-Trigger` тоста ошибки с текстом отказа. Без заголовка — поведение как
    сейчас.

- [ ] **Step 1: Тесты:** `order_peek_fragment` (номер, сумма, «ещё N» при 7 позициях);
  `order_peek_forbidden_without_right`; `peek_status_returns_panel_and_row`;
  `peek_paid_on_canceled_shows_error`; `status_without_peek_unchanged`;
  `row_has_peek_attr_and_links_stay`.
- [ ] **Step 2:** `cargo test admin::shop::orders` — новые FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Админка: быстрый просмотр заказа».

### Task 7: Быстрый просмотр товара

**Files:**
- Create: `templates/admin/item_peek.html`
- Modify: `src/admin/items.rs` (peek, active), `src/admin/mod.rs`, `templates/admin/items.html`

**Interfaces:**
- Consumes: Task 6 (панель и JS).
- Produces:
  - Строки списка записей коллекции с каталогом: `<tr id="item-row-{id}" data-peek="/admin/items/{id}/peek">`
    (у коллекций без каталога атрибута нет).
  - `GET /admin/items/{id}/peek` (право чтения коллекции): картинка (первая из анонса/детальной
    или поля картинок, как в списке), название, код, раздел, цена базового типа, остатки по
    складам и доступное количество, активность, «Открыть карточку».
  - `POST /admin/items/{id}/active` (`active=on|off`, право записи в коллекцию) — меняет
    `collection_items.active`; с `HX-Target: peek` — панель + OOB-строка; без права — 403.

- [ ] **Step 1: Тесты:** `item_peek_fragment` (цена, остатки по складам);
  `item_peek_forbidden_without_access`; `item_active_toggle` (с правом записи — меняет и
  возвращает панель и строку; только чтение — 403); `no_peek_for_non_catalog_collection`.
- [ ] **Step 2:** `cargo test admin::items` — новые FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Админка: быстрый просмотр товара».

### Task 8: Проверки по критериям

- [ ] **Step 1: Регрессия:** сборки `main` и ветки на копиях dev-базы (`pg_dump` через pipe, не
  трогая сервер на :3000), 22 ответа bxapi и `/api/v1` — байт в байт.
- [ ] **Step 2: Браузер:** четыре карточки и две панели по разделу 4 спеки: переключение кликом
  и `[` `]`, точка «изменено», Ctrl+S с любой вкладки, ошибка — точка и нужная вкладка,
  `required`-поле на скрытой вкладке переключает на неё, последняя вкладка из строки открытых
  записей, узкий экран; панель: клик по строке, по ссылке в строке, выделение текста, Esc,
  смена статуса и оплаты, переключатель «Активен».
- [ ] **Step 3:** Копии баз и временные пользователи удалены; `cargo test`, clippy, fmt — PASS.
- [ ] **Step 4:** Найденные ошибки — в фикс-раунды соответствующих задач.
