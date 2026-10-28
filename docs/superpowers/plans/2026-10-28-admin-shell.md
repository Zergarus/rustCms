# Новая оболочка админки (этап 1) — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** админка в духе Linear и Notion:
- меню, где CMS на первом месте, а магазин спрятан под одну кнопку;
- выключатель модуля «Магазин»;
- вкладки открытых записей;
- командная палитра и горячие клавиши;
- уведомления и тёмная/светлая тема;
- карточка заказа на вкладках.

**Architecture:**
- **Сервер.** Реестр разделов `src/admin/nav.rs` — один источник для меню, палитры и вкладок. Middleware `require_access` собирает для каждой полной страницы контекст оболочки (`Access.shell`: меню, вкладки, настройки пользователя, уведомление), и шаблоны берут его из `user.shell`, как сейчас `user.store_control`. Новые модули: `tabs.rs` (вкладки), `palette.rs` (поиск), `flash.rs` (уведомления), `modules.rs` (выключатель).
- **Клиент.** Один файл `static/admin/admin.js` без сборки. Он отвечает за всё интерактивное: палитру, клавиши, отправку вкладок, панель сохранения и уведомления.

**Tech Stack:** Rust 2024, Axum 0.8, SQLx 0.9 (`#[sqlx::test]`), PostgreSQL 17, MiniJinja, HTMX 2.0.4, ванильный JS.

**Spec:** `docs/superpowers/specs/2026-10-28-admin-shell-design.md`

## Global Constraints

- **Миграции.** Новая миграция — `0024_admin_shell.sql`. Уже применённые `0001`–`0023` не менять.
- **Ветка** `feature/admin-shell`, от `feature/stores`.
- **API сайта** (`/api/v1/*` — это `bxapi`, а также `/api/1c/exchange`) не меняется ни в одном байте ответа.
- **Права** проверяются на сервере, как сейчас. Меню и палитра только скрывают, но не заменяют `require`.
- **Admin POST** проходят через `same_origin_guard`. В тестах используется `post_form` или новый `post_form_raw` с Origin.
- **Тексты интерфейса** только на русском.
- **Сообщения коммитов** на русском, без упоминаний Claude или ИИ.
- **Внешние зависимости.** Новых crate нет, npm нет. Из внешних ресурсов остаётся только HTMX с jsdelivr, шрифт Inter хранится локально.
- **Опция модуля:** `options(module='admin', name='module_shop')`, значения `Y`/`N`. Если записи нет, модуль включён.
- **Лимит вкладок** `10`. Палитра: до `5` результатов в группе, задержка `150` мс, запрос обрезается до `100` символов.
- **Заголовок вкладки** обрезается до `200` символов.
- **Время показа уведомления** `4` с. Ошибки висят до закрытия.
- **Узкий экран** — меньше `900` px.
- **Режим «Магазин»** включается на путях, начинающихся с `/admin/shop`, `/admin/discounts` или `/admin/exchange`.
- **Контраст** текста не ниже 4.5:1 в обеих темах.

## Review Focus

1. **Заголовок вкладки приходит от клиента.** HTML в нём (`<script>`) экранируется, длинный обрезается до 200 символов. Тест в Task 4.
2. **Адрес вкладки приходит от клиента.** Принимается только путь, начинающийся с `/admin/` и не содержащий `//`. Иначе 400. Так `HX-Location` не уведёт на чужой сайт. Тест в Task 4.
3. **Запрос палитры** из одних пробелов, длиннее 100 символов или с `%`, `_`, `\`. Ожидается: пробелы обрезаются, длинный запрос режется, спецсимволы ищутся буквально, ответ не 500. Тест в Task 5.
4. **Вкладки заказа без права** `orders.manage`: каждая вкладка, и полной страницей, и фрагментом, отвечает 403. Несуществующий заказ — 404. Тест в Task 7.
5. **Cookie уведомления.** Русский текст переживает перекодирование. Испорченное значение игнорируется, страница отвечает 200. Тест в Task 2.

---

## Файлы

| Файл | Ответственность |
|---|---|
| `migrations/0024_admin_shell.sql` | `admin_tabs`, `users.admin_prefs` |
| `src/admin/nav.rs` | реестр разделов, режим, ключ вкладки, сборка меню |
| `src/admin/flash.rs` | одноразовая cookie уведомления, `HX-Trigger` |
| `src/admin/shell.rs` | `Shell` и `Prefs`: контекст оболочки, сохранение настроек пользователя |
| `src/admin/modules.rs` | страница «Модули», проверка выключенного модуля |
| `src/admin/tabs.rs` | вкладки открытых записей |
| `src/admin/palette.rs` | поиск палитры |
| `src/admin/mod.rs` | маршруты; middleware заполняет `Access.shell`, ставит заглушку выключенного модуля, удаляет вкладки на 404 |
| `src/access.rs` | поле `shell: Option<Shell>` в `Access` |
| `src/admin/shop/orders.rs`, `order_edit.rs` | вкладки заказа, переходы после POST |
| `templates/admin/base.html` | оболочка |
| `templates/admin/_ui.html` | макросы |
| `templates/admin/_tabs.html`, `_palette.html`, `_nav.html`, `module_off.html`, `modules.html` | фрагменты и страницы |
| `templates/admin/shop/order.html` + `shop/order/_*.html` | карточка заказа |
| `static/admin/admin.css`, `admin.js`, `fonts/` | клиент |
| `src/test_support.rs` | `post_form_raw`, `htmx_get` |

---

### Task 1: Данные и реестр разделов

**Files:**
- Create: `migrations/0024_admin_shell.sql`, `src/admin/nav.rs`
- Modify: `src/admin/mod.rs` (`mod nav;`)

**Interfaces:**
- Produces:
  - `nav::Mode { Cms, Shop }` (serde `lowercase`);
  - `nav::TabKind { Content, Shop, System }` (serde `lowercase`);
  - `nav::mode_for(path: &str) -> Mode`;
  - `nav::tab_key(path: &str) -> Option<String>`;
  - `nav::tab_kind(path: &str) -> TabKind`;
  - `nav::ORDER_TABS: [(&str, &str); 6]`;
  - `nav::Section { code: &'static str, label: String, url: String, group: &'static str, mode: Mode, create: Option<(String, String)> }`;
  - `nav::sections(user: &Access, ctx: &NavCtx) -> Vec<Section>`;
  - `nav::NavCtx { shop_enabled: bool, store_control: bool, collections: Vec<CollectionLink> }`;
  - `nav::CollectionLink { id: i64, name: String, count: i64, catalog: bool, writable: bool }`;
  - `nav::load_ctx(db: &PgPool, user: &Access) -> sqlx::Result<NavCtx>`;
  - `nav::Nav` (Serialize) и `nav::build(db, user: &Access, ctx: &NavCtx, path: &str) -> sqlx::Result<Nav>`;
  - `nav::shop_enabled(db) -> sqlx::Result<bool>`;
  - `nav::set_shop_enabled(db, on: bool) -> sqlx::Result<()>`.

Миграция:

```sql
CREATE TABLE admin_tabs (
    id        BIGSERIAL PRIMARY KEY,
    user_id   BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key       TEXT NOT NULL,
    url       TEXT NOT NULL,
    title     TEXT NOT NULL,
    kind      TEXT NOT NULL CHECK (kind IN ('content', 'shop', 'system')),
    opened_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, key)
);
ALTER TABLE users ADD COLUMN admin_prefs JSONB NOT NULL DEFAULT '{}';
```

Разделы реестра — коды, названия, адреса и группы. Права берутся как в `shop/_nav.html` и `base.html`:

- **Группа `main`:** `desk` «Рабочий стол» `/admin`.
- **Группа `content`:**
  - по одному пункту на коллекцию с уровнем ≥ Read, код `collection:<id>`, адрес списка записей `/admin/collections/<id>/items`;
  - `collections` «Все коллекции» (`can_see_collections`).
- **Группа `system`:**
  - `users` «Пользователи» (`users.manage`);
  - `groups` «Группы и права» (суперадмин);
  - `modules` «Модули» (суперадмин).
- **Группа `shop_main`:**
  - `orders` «Заказы» (`orders.manage`), без действия `create`: формы создания заказа в админке нет;
  - `products` «Товары»: одна коллекция с `is_catalog` → её адрес, несколько → `/admin/collections`.
- **Группа `shop_catalog`:** `price-types` «Типы цен», `rounding` «Правила округления».
- **Группа `shop_stock`:**
  - `stores` «Склады»;
  - `rests` «Остатки» и `store-docs` «Документы» — только при `store_control`; действие «Новый документ прихода» ведёт на `/admin/shop/store-docs/new?type=A`;
  - `contractors` «Поставщики».
- **Группа `shop_marketing`** (`discounts.manage`):
  - `discounts` «Скидки» `/admin/discounts`;
  - `cumulative` «Накопительные скидки» `/admin/discounts/cumulative`. Отдельного пункта «Купоны» нет: купоны живут в карточке скидки (`/admin/discounts/{id}/coupons`).
- **Группа `shop_settings`:**
  - `statuses` «Статусы», `deliveries` «Доставки», `pay-systems` «Платёжные системы», `person-types` «Типы плательщика», `currencies` «Валюты», `currency-rates` «Курсы валют», `settings` «Настройки каталога», `carts` «Корзины»;
  - `exchange` «Обмен с 1С» `/admin/exchange` (`exchange.1c` или `exchange.1c.sale`).

Все пункты магазина скрыты, если `shop_enabled == false`. Действие коллекции — «Добавить запись в „<name>“» по адресу `/admin/collections/<id>/items/new`, только при уровне Write.

`Nav`:

```rust
pub struct Nav {
    pub mode: Mode,
    pub groups: Vec<NavGroup>,      // группы текущего режима, пустые выброшены
    pub shop: Option<NavItem>,      // кнопка «Магазин» в режиме Cms; url — первый видимый пункт магазина
    pub more_collections: bool,     // коллекций > 8: показаны первые 8 и «Ещё…»
    pub goto: Vec<(char, String)>,  // G-переходы: d, o, c, u — только доступные
}
pub struct NavGroup { pub code: &'static str, pub label: &'static str, pub items: Vec<NavItem> }
pub struct NavItem { pub code: String, pub label: String, pub url: String, pub count: Option<i64>, pub active: bool }
```

- **Активный пункт** — тот, чей `url` совпадает с `path` или является префиксом `path` по границе `/`. При нескольких совпадениях выбирается самый длинный. `/admin` активен только при точном совпадении.
- **Счётчик заказов:** `SELECT count(*) FROM orders WHERE status = 'N' AND NOT canceled` (`sale::INITIAL_STATUS`). Он стоит на `orders` и на кнопке `shop` при `orders.manage`.
- **`tab_key`:**
  - отрезает `?…`;
  - `None` для `/admin/login`, путей с последним сегментом `new`, путей с префиксом `/admin/tabs`, `/admin/palette`, `/admin/prefs`, `/admin/static`;
  - `/admin/shop/orders/{id}/{t}` при `t` из `ORDER_TABS` → `/admin/shop/orders/{id}`.
- **`ORDER_TABS`:** `[("", "Обзор"), ("items", "Состав и скидки"), ("props", "Покупатель и свойства"), ("shipment", "Отгрузка и резервы"), ("files", "Документы и 1С"), ("history", "История")]`.
- **`tab_kind`:** пути с префиксами `/admin/collections`, `/admin/items` и `/admin/sections` → `Content`, режим `Shop` → `Shop`, остальное → `System`.

- [ ] **Step 1: Тесты в `nav.rs`**

```rust
#[test] fn mode_and_keys() {
    assert_eq!(mode_for("/admin/shop/orders/5"), Mode::Shop);
    assert_eq!(mode_for("/admin/discounts/3"), Mode::Shop);
    assert_eq!(mode_for("/admin/exchange"), Mode::Shop);
    assert_eq!(mode_for("/admin/collections/1"), Mode::Cms);
    assert_eq!(mode_for("/admin/shopping"), Mode::Cms);           // граница сегмента
    assert_eq!(tab_key("/admin/shop/orders/1042/shipment?x=1").as_deref(), Some("/admin/shop/orders/1042"));
    assert_eq!(tab_key("/admin/shop/orders/1042").as_deref(), Some("/admin/shop/orders/1042"));
    assert_eq!(tab_key("/admin/shop/orders/1042/xml1c").as_deref(), Some("/admin/shop/orders/1042/xml1c"));
    assert_eq!(tab_key("/admin/users/new"), None);
    assert_eq!(tab_key("/admin/login"), None);
    assert_eq!(tab_kind("/admin/collections/3/items/9"), TabKind::Content);
    assert_eq!(tab_kind("/admin/shop/stores"), TabKind::Shop);
    assert_eq!(tab_kind("/admin/users"), TabKind::System);
}
```

Тесты `#[sqlx::test]`:
- `menu_by_permissions`:
  - с правами только `orders.manage`: в режиме Cms `shop` есть, групп `system` нет;
  - в режиме Shop видна только группа `shop_main` с `orders`;
  - без `orders.manage` и `shop.manage` `shop` равно `None`.
- `shop_disabled_hides_shop`: после `set_shop_enabled(false)` у суперадмина `shop == None`, и в `sections` нет ни одного раздела с `mode == Shop`. После `set_shop_enabled(true)` всё возвращается.
- `stock_items_follow_store_control`: `rests` и `store-docs` есть только при `store_control = true`.
- `more_collections_after_eight`: при 9 коллекциях — `more_collections == true` и 8 пунктов коллекций.
- `orders_badge_counts_new_not_canceled`: заказы N, N (отменён) и F дают `count == Some(1)`.
- `active_item_longest_prefix`: на пути `/admin/shop/store-docs/7` активен `store-docs`, а не `stores`.

- [ ] **Step 2:** `cargo test admin::nav` — FAIL (нет модуля).
- [ ] **Step 3:** Реализовать миграцию и `nav.rs` по интерфейсам выше.
- [ ] **Step 4:** `cargo test admin::nav` — PASS, затем `cargo test` целиком — PASS.
- [ ] **Step 5:** Commit «Админка: реестр разделов, режим магазина, данные вкладок».

---

### Task 2: Контекст оболочки, уведомления, модули, настройки пользователя

**Files:**
- Create: `src/admin/shell.rs`, `src/admin/flash.rs`, `src/admin/modules.rs`, `templates/admin/modules.html`, `templates/admin/module_off.html`
- Modify: `src/access.rs` (поле `shell`), `src/admin/mod.rs` (`require_access`, маршруты), `src/test_support.rs`

**Interfaces:**
- Consumes: Task 1 (`nav::*`).
- Produces:
  - `shell::Prefs { theme: Theme, tabs: bool }`, где `Theme { Dark, Light, System }` (serde `lowercase`); по умолчанию `System` и `true`. Читается из `users.admin_prefs` с заменой неизвестных значений на умолчания.
  - `shell::Shell { nav: Nav, prefs: Prefs, flash: Option<Flash>, tabs: Vec<tabs::Tab>, path: String }` (Serialize). `tabs` до Task 4 — пустой `Vec`; тип `Tab` объявляется здесь же в `tabs.rs` как заготовка с полями `id, url, title, kind, active`.
  - `Access.shell: Option<Shell>`: `Some` только для GET без заголовка `HX-Request`.
  - `flash::Flash { text: String, kind: FlashKind }`, `FlashKind { Ok, Error }`.
  - `flash::redirect(url: &str, text: &str, kind: FlashKind) -> Response` — ответ 303 с `Location` и `Set-Cookie: admin_flash=<закодировано>; Path=/admin; HttpOnly; SameSite=Lax`.
  - `flash::read(jar: &CookieJar) -> Option<Flash>` — испорченное значение даёт `None`.
  - `flash::toast(text: &str, kind: FlashKind) -> [(HeaderName, HeaderValue); 1]` — заголовок `HX-Trigger: {"toast":{"text":…,"kind":"ok|error"}}`.
  - Маршруты:
    - `GET/POST /admin/modules` (суперадмин; форма с полем `shop=on`);
    - `POST /admin/prefs` с полями `theme` (`dark|light|system`) и `tabs` (`on` или отсутствует) → `flash::redirect` на `back` (путь из поля `back`, если он начинается с `/admin/`, иначе `/admin`) с текстом «Настройки сохранены».
- Test support:
  - `post_form_raw(app, path, cookie, body) -> (StatusCode, HeaderMap, String)` — как `post_form`, но с заголовками;
  - `htmx_get(app, path, cookie, target: &str) -> (StatusCode, String)` — с заголовками `HX-Request: true` и `HX-Target`.

Что делает `require_access` после проверки входа:
1. Вычисляет `store_control` (как сейчас) и `shop_enabled`.
2. Если `mode_for(path) == Shop` и магазин выключен, рендерит `module_off.html` со статусом 404. Текст: «Модуль „Магазин“ выключен», для суперадмина — ссылка «Включить в разделе „Модули“».
3. Для GET без `HX-Request` заполняет `Access.shell`: `nav::build`, `prefs`, `flash::read`, `path`.
4. Если у запроса была cookie `admin_flash` и ответ — 200 без `HX-Request`, добавляет в ответ `Set-Cookie: admin_flash=; Max-Age=0; Path=/admin`.
5. Если ответ на GET — 404 и `tab_key(path)` есть, удаляет вкладку пользователя с этим ключом. Это нужно для Task 4: таблица уже есть с Task 1.

- [ ] **Step 1: Тесты**
  - `flash_roundtrip_cyrillic`: `redirect("/admin/x", "Заказ сохранён", Ok)` → `read` по cookie из `Set-Cookie` даёт тот же текст и `Ok`.
  - `flash_malformed_ignored`: `read` для cookie `admin_flash=zz` и для `admin_flash=ff` (не UTF-8) даёт `None`; GET `/admin` с такой cookie отвечает 200.
  - `flash_shown_once`: GET `/admin` с валидной cookie → 200; тело содержит текст; в ответе есть `Set-Cookie` со сбросом `admin_flash` (`Max-Age=0`).
  - `module_off_404`:
    - после `set_shop_enabled(false)` GET `/admin/shop/orders` суперадмином → 404, тело содержит «Модуль „Магазин“ выключен» и `/admin/modules`;
    - GET `/admin/discounts` → 404;
    - GET `/api/v1/iblock/list` отвечает тем же статусом и телом, что до выключения.
  - `modules_page_super_only`:
    - `permitted_cookie(&["shop.manage"])` → GET `/admin/modules` даёт 403;
    - суперадмин: POST `/admin/modules` без `shop` → `nav::shop_enabled` равно `false`, с `shop=on` → `true`.
  - `prefs_saved_and_validated`:
    - POST `/admin/prefs` с `theme=dark&back=/admin/users` → 303 на `/admin/users`, в `admin_prefs` записано `{"theme":"dark","tabs":false}`;
    - `theme=evil&back=https://x.test` → 303 на `/admin`, тема `system`.

- [ ] **Step 2:** `cargo test admin::flash admin::modules admin::shell` — FAIL.
- [ ] **Step 3:** Реализовать по интерфейсам. Значение cookie — строка `ok:<текст>` или `error:<текст>`, закодированная в hex (байты UTF-8, строчные цифры). Новых crate не добавлять. Если hex не читается или в нём не UTF-8, `read` возвращает `None`.
- [ ] **Step 4:** Эти тесты — PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Commit «Админка: контекст оболочки, уведомления, выключатель магазина, настройки пользователя».

---

### Task 3: Шаблон оболочки, компоненты, тема

**Files:**
- Modify: `templates/admin/base.html`, `static/admin/admin.css`, все шаблоны с `{% include "shop/_nav.html" %}`
- Create: `templates/admin/_ui.html`, `templates/admin/_nav.html` (меню), `static/admin/fonts/` (Inter 400/500/600/700, наборы cyrillic и latin, woff2 из `https://cdn.jsdelivr.net/npm/@fontsource/inter@5.1.0/files/inter-{cyrillic|latin}-{400|500|600|700}-normal.woff2`)
- Delete: `templates/admin/shop/_nav.html`

**Interfaces:**
- Consumes: `user.shell` (Task 2).
- Produces:
  - **Разметка, на которую опирается JS** (Task 6):
    - `#open-tabs` — строка вкладок;
    - `#palette` — `<dialog>`, внутри `input#palette-q` с `hx-get="/admin/palette"`, `hx-trigger="input changed delay:150ms, focus once"`, `hx-target="#palette-results"`;
    - `#toasts` — `aria-live="polite"`; уведомление из `user.shell.flash` рендерится внутри как `.toast[data-kind]`;
    - `#savebar` — скрытая панель сохранения;
    - `#hotkeys` — `<dialog>` со списком клавиш;
    - `[data-palette-open]` — кнопки открытия палитры;
    - `a[data-goto="d|o|c|u"]` — скрытые ссылки G-переходов из `nav.goto`;
    - `button[data-nav-toggle]` — кнопка ☰;
    - `details[data-nav-group="<code>"]` — группы меню;
    - `<html data-theme>` — тема из `prefs`; при `system` атрибута нет;
    - `<body data-tabs="on|off">` и `data-no-tab`, если шаблон задал `{% set no_tab = true %}`.
  - **Макросы `_ui.html`:**
    - `page_head(crumbs, actions=None)` — `crumbs` это список пар `(label, url|none)`;
    - `tabs(items, active, target)` — `items` это список `(code, label, url)`;
    - `prop(label)` (call-блок);
    - `card(title)` (call-блок);
    - `aside()` (call-блок);
    - `empty(text, action_label=None, action_url=None)`.

Правила:
- **Меню** строится только из `user.shell.nav`, без пунктов, записанных прямо в шаблоне.
  - Режим Cms: «Рабочий стол», «Контент» (с «Ещё…», если `more_collections`), «Модули» (кнопка `shop` со счётчиком), «Система».
  - Режим Shop: «‹ Все разделы» (`/admin`), заголовок «Магазин», затем группы.
  - Внизу меню — аватар с меню профиля: форма `POST /admin/prefs` (тема, переключатель вкладок) и «Выйти».
- **Подсветка.** Переменные `section` и `tab` в шаблонах больше не нужны. Активный пункт задаёт `NavItem.active`, а шаблоны, которые их ставят, продолжают работать.
- **CSS:**
  - переменные на `:root` (тёмная по умолчанию через `prefers-color-scheme`), `:root[data-theme=dark]` и `:root[data-theme=light]`;
  - тёмная палитра: `--bg #0e0e10`, `--panel #121215`, `--raised #17171b`, `--border #222227`, `--text #e4e4e7`, `--muted #8b8b94`, `--accent #5e6ad2`, `--ok #8fd19e`, `--warn #f0a35e`, `--danger #f87171`;
  - светлая палитра: `--bg #f7f7f8`, `--panel #ffffff`, `--raised #f1f1f3`, `--border #e4e4e7`, `--text #18181b`, `--muted #5f5f69`, `--accent #4f5bd5`, `--ok #1f7a3a`, `--warn #a15c07`, `--danger #c0262d`;
  - все классы, которые используют существующие шаблоны (собрать через `grep -oh 'class="[^"]*"' templates/admin -r`), сохраняют смысл.
- **`htmx-config`** из старого `base.html` (обработка 204/403) сохраняется. `alert()` убирается: ошибки теперь показывает Task 6.

- [ ] **Step 1: Тесты** (`smoke_tests.rs` или `admin/mod.rs`)
  - `shell_renders_cms_menu`: GET `/admin` суперадмином → тело содержит «Контент», «Модули», «Система», `data-palette-open`, `id="open-tabs"` и не содержит «Типы цен».
  - `shell_renders_shop_menu`: GET `/admin/shop/stores` → «Все разделы», «Типы цен», «Поставщики»; без складского учёта «Документы» нет.
  - `theme_attribute_from_prefs`: при `admin_prefs = {"theme":"light"}` тело содержит `data-theme="light"`; при пустых настройках `data-theme=` нет.
  - `old_shop_nav_gone`: в `templates/admin` нет ни одного `shop/_nav.html` — проверка через `include_str!` нескольких шаблонов либо через отсутствие файла `std::path::Path::new("templates/admin/shop/_nav.html").exists() == false`.
  - Все существующие тесты страниц админки продолжают проходить.
- [ ] **Step 2:** `cargo test shell_` — FAIL.
- [ ] **Step 3:** Реализовать шаблоны, CSS и шрифты.
- [ ] **Step 4:** `cargo test` — PASS.
- [ ] **Step 5:** Commit «Админка: новая оболочка, меню модулей, тёмная и светлая тема».

---

### Task 4: Вкладки открытых записей

**Files:**
- Modify: `src/admin/tabs.rs` (заготовка из Task 2), `src/admin/mod.rs`
- Create: `templates/admin/_tabs.html`

**Interfaces:**
- Consumes: `nav::tab_key`, `nav::tab_kind`, `Shell.tabs`.
- Produces:
  - `tabs::LIMIT: i64 = 10`;
  - `tabs::list(db, user_id, current_path: &str) -> sqlx::Result<Vec<Tab>>` — порядок по `id`; `active` у вкладки с ключом `tab_key(current_path)`;
  - `tabs::open(db, user_id, url: &str, title: &str) -> Result<Option<i64>, TabError>` — `None`, если `tab_key` даёт `None`;
  - маршрут `POST /admin/tabs` с полями `url`, `title` → фрагмент `_tabs.html`;
  - маршрут `POST /admin/tabs/{id}/close` с полем `current` → фрагмент, а если закрыта активная, ещё заголовок `HX-Location: {"path": <url соседней>, "target": "body"}` (соседняя — правая, иначе левая, иначе `/admin`);
  - `Shell.tabs` заполняется в `require_access` через `tabs::list`, если `prefs.tabs`.

Правила `open`:
- **Проверка адреса:** начинается с `/admin/`, не содержит `//`, `\`, управляющих символов. Иначе `TabError::BadUrl`, ответ 400.
- **Заголовок:** без суффикса « — CMS», обрезан до 200 символов по границе символа. Пустой заменяется на ключ.
- **Запись:** `INSERT … ON CONFLICT (user_id, key) DO UPDATE SET url, title, opened_at = now()`.
- **Лимит:** если вкладок больше `LIMIT`, удаляется самая старая по `opened_at`, кроме только что открытой.
- **Чужая вкладка** при закрытии: 404.

- [ ] **Step 1: Тесты** (`#[sqlx::test]` через `post_form_raw`)
  - `open_creates_and_reuses`: открыть `/admin/shop/orders/7`, затем `/admin/shop/orders/7/shipment` → одна вкладка, её `url` = `/admin/shop/orders/7/shipment`.
  - `open_skips_new_forms`: `/admin/users/new` не создаёт вкладку, ответ 200.
  - `limit_evicts_oldest_not_current`: 11 разных адресов → 10 вкладок, первой нет, последняя есть.
  - `close_active_moves_to_neighbor`: вкладки A, B, C; закрыть B при `current`=B → `HX-Location` с путём C. Закрыть C при `current`=C → путь A. Закрыть A при `current`=A → `/admin`.
  - `close_foreign_404`: вкладка пользователя 1, закрывает пользователь 2 → 404.
  - `title_escaped_and_capped`: заголовок `<script>x</script>` + 300 «я» → во фрагменте `&lt;script&gt;`, длина сохранённого заголовка 200 символов.
  - `bad_url_rejected`: `url=https://evil.test/admin/`, `url=//evil.test`, `url=/admin//x` → 400.
  - `deleted_record_tab_removed`: вкладка `/admin/shop/orders/999999`, GET этого адреса (404) → вкладки нет.
- [ ] **Step 2:** `cargo test admin::tabs` — FAIL.
- [ ] **Step 3:** Реализовать.
- [ ] **Step 4:** `cargo test` — PASS.
- [ ] **Step 5:** Commit «Админка: вкладки открытых записей».

---

### Task 5: Командная палитра (сервер)

**Files:**
- Create: `src/admin/palette.rs`, `templates/admin/_palette.html`
- Modify: `src/admin/mod.rs` (маршрут `GET /admin/palette`)

**Interfaces:**
- Consumes: `nav::sections`, `nav::load_ctx`, `tabs::list`.
- Produces:
  - `palette::Group { title: &'static str, items: Vec<Hit> }`;
  - `palette::Hit { label: String, hint: String, url: String }`;
  - `palette::search(db, user: &Access, ctx: &NavCtx, q: &str) -> sqlx::Result<Vec<Group>>`;
  - `palette::like_pattern(q: &str) -> String` — экранирует `\`, `%`, `_` и оборачивает в `%…%`;
  - фрагмент `_palette.html`: `<a class="pal-item" href=url>` в группах, пустой результат — «Ничего не найдено».

Правила:
- **Запрос:** `q` обрезается по краям и до 100 символов.
- **Пустой `q`:** группы «Недавние» (до 5 вкладок пользователя, новые первыми) и «Действия» (до 5).
- **Непустой `q`**, группы в таком порядке, пустые выброшены, каждая — до 5:
  1. «Разделы» — `label ILIKE`, подсказка `group`.
  2. «Действия» — `create`, подсказка «действие».
  3. «Заказы» — только при `orders.manage` и включённом магазине. Совпадение `account_number` с `q` или по префиксу, либо условие покупателя — то же выражение, что фильтр `buyer` в `orders::push_filters`. Подпись «Заказ №<номер> · <покупатель>», адрес `/admin/shop/orders/<id>`.
  4. «Записи» — `collection_items.name ILIKE`, только в коллекциях из `ctx.collections`. Подсказка — имя коллекции, адрес — `/admin/items/<id>`.
  5. «Пользователи» — при `users.manage`: `login`/`email ILIKE`, адрес `/admin/users/<id>`.

- [ ] **Step 1: Тесты**
  - `like_pattern_escapes`: `like_pattern("50%_a\\b") == "%50\\%\\_a\\\\b%"`.
  - `palette_respects_permissions`:
    - пользователь только с `collections` уровня Read на коллекцию A: «новость» находит запись из A и не находит запись из B;
    - нет групп «Заказы» и «Пользователи», даже если заказ с покупателем «новость» существует.
  - `palette_orders_by_number`: суперадмин, `q` = номер заказа → группа «Заказы» с адресом заказа.
  - `palette_shop_disabled`: магазин выключен → по «заказ» нет раздела «Заказы» и группы «Заказы».
  - `palette_weird_queries`:
    - `q` из пробелов → те же группы, что для пустого;
    - `q` из 500 символов → ответ 200 OK;
    - `q="%"` не находит запись «abc»;
    - ни один из запросов не даёт 500.
  - `palette_empty_shows_recent`: после двух открытых вкладок пустой `q` даёт «Недавние» с ними в обратном порядке.
- [ ] **Step 2:** `cargo test admin::palette` — FAIL.
- [ ] **Step 3:** Реализовать.
- [ ] **Step 4:** `cargo test` — PASS.
- [ ] **Step 5:** Commit «Админка: командная палитра».

---

### Task 6: Клиентский скрипт

**Files:**
- Create: `static/admin/admin.js` (не больше 600 строк)
- Modify: `templates/admin/base.html` (подключить `<script src="/admin/static/admin.js" defer>`)

**Interfaces:**
- Consumes: разметка из Task 3, маршруты из Task 4 и Task 5, `HX-Trigger: toast` из Task 2.

Поведение:
- **Вкладки.** На `DOMContentLoaded` и на `htmx:afterSettle` после перехода через `hx-boost` или `hx-push-url`: если `body[data-tabs=on]` и нет `data-no-tab`, отправить `htmx.ajax('POST', '/admin/tabs', {target:'#open-tabs', values:{url: location.pathname+location.search, title: document.title}})`. Крестики — `hx-post` на `/admin/tabs/{id}/close` с `hx-vals` `current` = текущий путь (в разметке `_tabs.html`).
- **Палитра.**
  - Открывается по Ctrl+K или Cmd+K и по клику на `[data-palette-open]` через `showModal()`, фокус в `#palette-q`, поле очищается.
  - ↑ и ↓ двигают класс `is-selected` по `.pal-item`; после подгрузки результатов выбран первый.
  - Enter открывает выбранный элемент (клик по ссылке). Esc или клик по фону закрывают.
- **Клавиши.**
  - Не срабатывают, если `event.target` — `input`, `textarea`, `select`, `[contenteditable]`, или открыта палитра, кроме Esc и Ctrl+K.
  - `g` + буква в течение 1000 мс → `a[data-goto=<буква>]`.
  - `c` → `[data-hotkey=c]`.
  - `/` → фокус на `[data-hotkey="/"]`.
  - `?` → `#hotkeys.showModal()`.
  - Ctrl+S или Cmd+S → отправка формы с видимой `#savebar` (`preventDefault`).
- **Панель сохранения.**
  - `form[data-dirty-guard]`: первое событие `input` или `change` показывает `#savebar`, привязанную к этой форме.
  - «Отменить» выполняет `form.reset()` и прячет панель.
  - `beforeunload` при грязной форме.
  - На `htmx:beforeRequest` от `hx-boost` при грязной форме — `confirm('Есть несохранённые изменения. Уйти?')`; при отказе `preventDefault`.
- **Уведомления.**
  - Событие `toast` (`HX-Trigger`) и `htmx:responseError` (`kind=error`, текст — первые 300 символов ответа) создают `.toast`.
  - `ok` исчезает через 4000 мс; у `error` есть кнопка закрытия.
  - Уведомления, отрисованные сервером, получают то же поведение при загрузке.
- **Меню.**
  - `details[data-nav-group]` хранит `open` в `localStorage` под ключом `admin.nav.<code>`; каждое обращение — в `try/catch`.
  - `[data-nav-toggle]` переключает класс `nav-open` у `body`.

- [ ] **Step 1:** Проверка синтаксиса: `node --check static/admin/admin.js`, если `node` установлен; иначе пропустить и записать это в отчёт. Плюс `wc -l static/admin/admin.js` ≤ 600.
- [ ] **Step 2:** Реализовать.
- [ ] **Step 3:** Ручная проверка в браузере на локальном сервере (`cargo run` с `.env`). Каждый пункт записать в отчёт как OK или FAIL:
  - Ctrl+K находит «Заказы» и Enter открывает их;
  - `G O`;
  - `?`;
  - вкладка появляется после перехода и закрывается крестиком;
  - правка формы показывает панель, а Ctrl+S сохраняет;
  - уведомление после сохранения исчезает через 4 с;
  - тема меняется в профиле;
  - на ширине 390 px меню открывается по ☰.
- [ ] **Step 4:** `cargo test` — PASS (шаблон с `<script>` не ломает тесты).
- [ ] **Step 5:** Commit «Админка: палитра, горячие клавиши, вкладки и уведомления на клиенте».

---

### Task 7: Карточка заказа на вкладках

**Files:**
- Modify: `src/admin/shop/orders.rs`, `src/admin/shop/order_edit.rs`, `src/admin/mod.rs` (маршруты), `templates/admin/shop/order.html`
- Create: `templates/admin/shop/order/_overview.html`, `_items.html`, `_props.html`, `_shipment.html`, `_files.html`, `_history.html`, `_aside.html`, `templates/admin/shop/order_fragment.html`

**Interfaces:**
- Consumes: `nav::ORDER_TABS`, `flash::redirect`, `_ui.html` (`page_head`, `tabs`, `prop`, `aside`).
- Produces:
  - `orders::card(state, user, id, tab: &str, error: Option<String>, notice: Option<String>, fragment: bool) -> AppResult<Response>`. Неизвестный `tab` → `AppError::NotFound`. При `fragment` рендерит `order_fragment.html`: вкладку и `_aside.html` с `hx-swap-oob="true"` на `#order-aside`.
  - `orders::view_tab` с `Path((id, tab))` и `orders::view`.
  - Фрагмент отдаётся, если есть заголовки `HX-Request: true` и `HX-Target: order-tab`.
  - Маршруты: к `/shop/orders/{id}/items`, `/props`, `/shipment`, `/files` добавить `get(view_tab)`; новый `GET /shop/orders/{id}/history`.
  - `fn back(id: i64, tab: &str, text: &str) -> Response` через `flash::redirect`.

Переходы после успешного POST и тексты уведомлений:

| POST | Вкладка | Текст |
|---|---|---|
| `status` | `` (Обзор) | «Статус изменён» |
| `paid` | `` | «Оплата сохранена» |
| `cancel` | `` | «Отмена заказа сохранена» |
| `items` | `items` | «Состав сохранён» |
| `discounts` | `items` | «Скидки пересчитаны» |
| `props` | `props` | «Свойства сохранены» |
| `shipment` | `shipment` | «Отгрузка сохранена» |
| `reserve` | `shipment` | «Резерв поставлен» |
| `unreserve` | `shipment` | «Резерв снят» |
| `files` | `files` | «Документы сохранены» |

Ошибка формы рендерит вкладку этого POST со статусом 200 и текстом ошибки, как сейчас карточка. Содержимое вкладок — по таблице из спеки, раздел 4. Блоки нынешнего `order.html` переносятся без изменения логики и текстов. Формы правки получают `data-dirty-guard`. Вкладки рисует `tabs(items, active, "#order-tab")`.

Существующие тесты, которые ищут в `/admin/shop/orders/{id}` блоки, переехавшие на вкладки (1С, отгрузка, резервы, история), переводятся на GET нужной вкладки. Их утверждения не ослабляются.

- [ ] **Step 1: Тесты**
  - `order_tabs_full_and_fragment`: для каждого `tab` из `ORDER_TABS`:
    - GET полной страницы → 200, есть `id="order-tab"` и все 6 подписей вкладок;
    - `htmx_get(..., "order-tab")` → 200, нет `<html`, есть `hx-swap-oob`.
  - `order_unknown_tab_404`: `/admin/shop/orders/{id}/bogus` → 404.
  - `order_tabs_require_orders_manage`: `permitted_cookie(&["shop.manage"])` → каждая вкладка, полной страницей и фрагментом, отвечает 403. Несуществующий id суперадмином → 404.
  - `order_post_redirects_to_tab`:
    - POST `/admin/shop/orders/{id}/shipment` (валидная форма) → 303, `Location` = `/admin/shop/orders/{id}/shipment`, в `Set-Cookie` есть `admin_flash`;
    - POST `status` → `Location` = `/admin/shop/orders/{id}`.
  - `order_form_error_renders_tab`: POST `status` с несуществующим статусом → 200, в теле текст ошибки и активна вкладка «Обзор» (`aria-current="page"` на ней).
  - `order_aside_totals`: в `_aside.html` есть итоговая сумма заказа из фикстуры.
- [ ] **Step 2:** `cargo test admin::shop::orders` — FAIL.
- [ ] **Step 3:** Реализовать.
- [ ] **Step 4:** `cargo test` — PASS, `cargo clippy --all-targets -- -D warnings` — чисто, `cargo fmt --check` — OK.
- [ ] **Step 5:** Commit «Карточка заказа на вкладках».

---

### Task 8: Документация и итоговая проверка

**Files:**
- Modify: `README.md`: новый раздел «Интерфейс админки» перед «Безопасность админки» (меню и модули, вкладки, палитра, клавиши, тема, вкладки заказа); в «Магазин в админке» упомянуть меню модуля.

- [ ] **Step 1:** README.
- [ ] **Step 2: Регрессия API.** Собрать release двух сборок: `feature/stores` (HEAD `56c6e7e`) и эту ветку. Запустить обе на одной копии базы (`createdb -T cms cms_shell`) на портах 18081 и 18082 с одинаковым `.env`. Сравнить тела ответов на запросы:
  - `/api/v1/iblock/list`;
  - `/api/v1/iblock/info/catalog`;
  - `/api/v1/iblock/catalog/element/list`;
  - `/api/v1/section/list?apiCode=catalog`;
  - `/api/v1/iblock/catalog/search?q=перфоратор`;
  - `/api/v1/location/current`;
  - `/api/v1/base/csrf/token` (только статус);
  - `/api/v1/basket`.

  Ожидается: все совпадают байт в байт (у токена — статус).
- [ ] **Step 3: Ручная проверка в браузере** на `cms_shell`:
  - обе темы;
  - ширина 390 px;
  - палитра и клавиши;
  - заказ: правка состава → отгрузка → история, вкладки открытых записей;
  - выключение магазина в «Модулях»: кнопка пропала, `/admin/shop/orders` отвечает 404.

  Скриншоты сохранить в рабочую папку плана.
- [ ] **Step 4:** Удалить копию базы и остановить серверы. `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.
- [ ] **Step 5:** Commit «README: интерфейс админки».
