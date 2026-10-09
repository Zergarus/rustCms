# Коллекции вместо инфоблоков — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Админка, код Rust, схема БД и README говорят «коллекция / запись / поле / вариант» вместо «инфоблок / элемент / свойство / значение списка»; поведение и внешние API не меняются.

**Architecture:** Сначала страховочные тесты, которые прогоняют страницы админки, наш API и bxapi через роутер приложения и закрепляют текущее поведение. Затем миграция `0014` (только `ALTER … RENAME`) вместе с SQL в коде, затем переименования Rust, затем адреса и тексты админки, затем документация и сквозная проверка.

**Tech Stack:** Rust 2024, Axum 0.8, SQLx 0.9 (PostgreSQL 17), MiniJinja.

**Spec:** `docs/superpowers/specs/2026-10-09-collections-rename-design.md`

## Global Constraints

- Словарь: Инфоблок → Коллекция (`collection`, `collections`); Элемент → Запись (`item`, `collection_items`); Раздел → Раздел (`section`, `collection_sections`); Свойство → Поле (`field`, `collection_fields`); Значение списка → Вариант (`option`, `collection_field_options`).
- Не меняются: наш API (`/api/v1/iblocks`, `/api/v1/iblocks/{code}`, `/api/v1/iblocks/{code}/elements[/{id}]`, ключи JSON `iblock`, `elements`, `properties`); bxapi (пути `/api/v1/iblock/...`, поля ответа, настройки контракта); запросы импорта к `b_iblock*`; миграции 0001–0013.
- Внутри bxapi, `src/api.rs` и `src/import.rs` меняются только SQL и то, что требует компилятор (имена типов и функций из переименованных модулей); локальные имена с `iblock` там остаются.
- `property_id` свойств заказа (`order_properties`, `order_property_values`, `order_property_variants`, `order_property_relations`) не трогается.
- Тесты: `rtk proxy cargo test` (лог — в файл, читать хвост); `cargo fmt`; `cargo clippy --all-targets` без предупреждений; то же без `projects/` (перенести папку, `touch build.rs`; вернуть, `touch projects`).
- Коммиты на русском, без упоминаний Claude/ИИ и строк `Co-Authored-By`.
- В локальный Битрикс запросы с побочными эффектами не отправлять; тестовые заказы и данные dev-базы не удалять.

## Review Focus

1. Шаблон обращается к переименованному полю структуры (`prop.iblock_id`, `ib.element_count`) — MiniJinja в режиме по умолчанию молча выводит пусто. Ожидается: каждая страница админки показывает данные фикстуры (тест `admin_pages_render`, Задача 1, проверяет конкретные строки на каждой странице).
2. Права по коллекциям в форме группы (`iblock_<id>` → `collection_<id>`): сохранённый уровень доступа должен остаться выбранным после перезагрузки формы и реально давать доступ. Тест `group_collection_access_roundtrip`, Задача 4.
3. Пользователь только с правом `collections.manage` (после миграции бывшее `iblocks.manage`) видит пункт меню «Коллекции» и открывает список. Тест `collections_manage_permission`, Задача 2.
4. Удаление записи по-прежнему чистит незаказанные позиции корзины (переименованный триггер). Тест `item_delete_cleans_cart`, Задача 2.
5. Импорт пишет в переименованные таблицы: `write_all` с минимальными `Data` создаёт коллекцию, поле, вариант, раздел, запись. Тест `import_writes_collections`, Задача 2.

---

### Task 1: Страховочные тесты до переименования

**Files:**
- Modify: `Cargo.toml` — `[dev-dependencies] tower = { version = "0.5", features = ["util"] }`
- Modify: `src/main.rs` — вынести сборку роутера из `serve` в `fn app(state: AppState) -> Router`
- Modify: `src/test_support.rs` — тестовое приложение и фикстура контента
- Create: `src/smoke_tests.rs` (`#[cfg(test)] mod smoke_tests;` в `main.rs`)

**Interfaces:**
- Produces: `main::app(state: AppState) -> Router` (без `.with_state` снаружи — возвращает готовый `Router` с состоянием); `test_support::test_state(db: PgPool) -> AppState` (шаблоны из `templates/admin`, `Config` с `upload_dir`/`mail_dir` во временном каталоге, проект по умолчанию); `test_support::admin_cookie(db: &PgPool) -> String` (создаёт администратора `smoke`, возвращает `"cms_session=<token>"`); `test_support::content_fixture(db: &PgPool) -> Content` с полями `collection_id`, `section_id`, `field_id`, `option_id`, `item_id`, `group_id`; `smoke_tests::get(app, path, cookie) -> (StatusCode, String)`, `smoke_tests::post_form(app, path, cookie, body) -> (StatusCode, String)` (с заголовком `Origin: http://127.0.0.1:3000`).
- Фикстура контента: коллекция `news` «Новости» (`api_enabled`), раздел «Раздел А» (`razdel-a`), поле `COLOR` «Цвет» вида `list` с вариантом «Красный» (`red`), поле `LINK` «Связь» вида `element` с привязкой к той же коллекции, запись «Первая новость» (`pervaya`, активна, опубликована, `COLOR` = вариант, раздел А); группа «Редакторы» с уровнем `write` на коллекцию.

- [ ] **Step 1: Снять «до» с работающего сервера**

Run: сохранить в `$SP/rename/before/` ответы `GET /api/v1/iblocks`, `GET /api/v1/iblocks/catalog`, `GET /api/v1/iblocks/catalog/elements?per_page=5`, одного `GET .../elements/{id}` и шести POST-запросов bxapi из `$SP/compat/requests.tsv` (`$SP` — scratchpad сессии).
Expected: 10 файлов, все HTTP 200.

- [ ] **Step 2: Написать тесты, закрепляющие текущее поведение**

```rust
#[sqlx::test]
async fn admin_pages_render(db: PgPool) {
    // GET → 200 и строки на странице:
    // /admin/iblocks                         → "Новости"
    // /admin/iblocks/{c}                     → "Цвет", "Связь"
    // /admin/properties/{field}              → "Красный"
    // /admin/iblocks/{c}/sections/new        → "Раздел А"
    // /admin/sections/{s}                    → "Раздел А"
    // /admin/iblocks/{c}/elements            → "Первая новость", "Раздел А"
    // /admin/iblocks/{c}/elements/new        → "Красный", "Первая новость" (вариант списка и запись для привязки)
    // /admin/elements/{i}                    → "Первая новость", "Красный"
    // /admin/groups/{g}                      → "Новости" и отмеченный write у коллекции
    // /admin/                                → пункт меню "Инфоблоки"
}

#[sqlx::test]
async fn own_api_unchanged(db: PgPool) {
    // /api/v1/iblocks, /api/v1/iblocks/news, /api/v1/iblocks/news/elements,
    // /api/v1/iblocks/news/elements/{i}: сравнить с литералом serde_json::json!({...})
    // (ключи iblock/elements/properties, значения фикстуры; даты — из фикстуры, заданные явно)
}

#[sqlx::test]
async fn bxapi_list_unchanged(db: PgPool) {
    // POST /api/v1/iblock/news/element/list {"select":["id","name","color","link.element.name"]}
    // → status success, data.items[0] == {"id": i, "name": "Первая новость", "color": ..., "link": ...} (литерал)
}
```

- [ ] **Step 3: Запустить — тесты должны пройти на текущем коде**

Run: `rtk proxy cargo test smoke_tests 2>&1 | tail -5`
Expected: `3 passed`. Закрепляем поведение до изменений, поэтому падение здесь — дефект теста, а не кода.

- [ ] **Step 4: Коммит**

```bash
git add Cargo.toml Cargo.lock src/main.rs src/test_support.rs src/smoke_tests.rs
git commit -m "Тесты: страницы админки и ответы API через роутер"
```

### Task 2: Миграция 0014 и SQL

**Files:**
- Create: `migrations/0014_collections.sql`
- Modify: все `.rs` с SQL по старым именам (`rg -n "iblocks|iblock_elements|iblock_sections|iblock_properties|iblock_property_enums|iblock_group_access|element_id|iblock_id|link_iblock_id" src projects/transopt.rs`; кроме запросов импорта к MySQL `b_*` и `property_id` свойств заказа)
- Modify: `src/access.rs` — `IBLOCKS_MANAGE` = `"collections.manage"` (имя константы — в Задаче 3)
- Modify: `src/test_support.rs`, тесты с SQL-фикстурами
- Modify: шаблоны, которые читают переименованные поля структур `FromRow`
- Test: `src/smoke_tests.rs`, `src/import.rs`

**Interfaces:**
- Produces: схема БД — таблицы `collections`, `collection_items`, `collection_sections`, `collection_fields`, `collection_field_options`, `collection_access`; колонки `collection_id` (вместо `iblock_id`), `item_id` (вместо `element_id` в `catalog_prices`, `catalog_products`, `catalog_store_amounts`, `cart_items`), `field_id` (в `collection_field_options`), `link_collection_id` (в `collection_fields`), `field_values` (вместо `properties` в `collection_items`); поля структур `FromRow` переименованы так же.
- Миграция переименовывает и: все индексы, ограничения и последовательности с префиксами `iblocks_`, `iblock_elements_`, `iblock_sections_`, `iblock_properties_`, `iblock_property_enums_`, `iblock_group_access_` → соответствующие `collections_`, `collection_items_`, `collection_sections_`, `collection_fields_`, `collection_field_options_`, `collection_access_`; `cart_items_element_idx` → `cart_items_item_idx`; `catalog_prices_element_idx` → `catalog_prices_item_idx`; ограничения `*_element_id_fkey` → `*_item_id_fkey`; триггер `iblock_elements_cart_cleanup` → `collection_items_cart_cleanup`; функцию `cart_items_drop_element()` → `cart_items_drop_item()` (её тело ссылается на колонку `element_id` — пересоздать через `CREATE OR REPLACE FUNCTION` с `item_id`); `UPDATE group_permissions SET permission = 'collections.manage' WHERE permission = 'iblocks.manage'`.
- Решение плана сверх перечня спецификации: колонка `collection_items.properties` → `field_values` — это тоже слово «свойства» из словаря; наружу (наш API, bxapi) ключ `properties` по-прежнему отдаётся.

- [ ] **Step 1: Написать тесты**

```rust
#[sqlx::test(migrations = false)]
async fn migration_0014_keeps_rows(db: PgPool) {
    // Migrator из sqlx::migrate!(); применить только версии <= 13
    // (клон Migrator с отфильтрованным migrations); наполнить старые таблицы:
    // инфоблок, раздел, свойство list + enum, элемент со значением, цена, остаток, catalog_products,
    // покупатель + позиция корзины, группа + iblock_group_access, право iblocks.manage.
    // Применить полный Migrator.
    // assert: по одной строке в collections, collection_sections, collection_fields,
    // collection_field_options, collection_items, catalog_prices, catalog_store_amounts,
    // catalog_products, cart_items, collection_access; collection_items.field_values не изменился;
    // group_permissions содержит 'collections.manage' и не содержит 'iblocks.manage';
    // INSERT INTO collections (code, name) VALUES ('new', 'Новая') проходит (последовательность жива).
}

#[sqlx::test]
async fn item_delete_cleans_cart(db: PgPool) {
    // order_fixture; DELETE FROM collection_items WHERE id = f.item_id_of_product
    // → в cart_items нет незаказанных позиций этого товара
}

#[sqlx::test]
async fn collections_manage_permission(db: PgPool) {
    // пользователь не-админ в группе с правами admin.access и collections.manage
    // GET /admin/iblocks → 200, "Новости"; /admin/ → пункт меню коллекций
}

#[sqlx::test]
async fn import_writes_collections(db: PgPool) {
    // Data::default() + один IblockRow, PropertyRow (L) с EnumRow, SectionRow, ElementRow;
    // write_all → collections/collection_fields/collection_field_options/collection_sections/collection_items по 1 строке,
    // у записи field_values содержит id варианта
}
```

- [ ] **Step 2: Запустить — должны упасть**

Run: `rtk proxy cargo test migration_0014 item_delete_cleans collections_manage import_writes 2>&1 | tail -8`
Expected: FAIL — нет таблицы `collections` / колонки `item_id`.

- [ ] **Step 3: Написать `migrations/0014_collections.sql` и переписать SQL в коде, тестах и фикстурах; поля `FromRow` и обращения к ним в шаблонах — по новым именам колонок**

- [ ] **Step 4: Прогнать всё**

Run: `rtk proxy cargo test > $WS/t2.log 2>&1; tail -5 $WS/t2.log`
Expected: все тесты зелёные, в том числе три страховочных из Задачи 1 (адреса админки ещё старые).

- [ ] **Step 5: Проверить, что старые имена остались только где разрешено**

Run: `rtk proxy grep -rnE "\b(iblocks|iblock_elements|iblock_sections|iblock_properties|iblock_property_enums|iblock_group_access|element_id|link_iblock_id)\b" src migrations/0014_collections.sql projects/transopt.rs | grep -v "src/import.rs.*b_\|IBLOCK"`
Expected: только строки `ALTER … RENAME` в `0014` и обращения импорта к MySQL.

- [ ] **Step 6: Применить миграцию к dev-базе и коммит**

Run: `sqlx migrate run` (или запуск сервера); затем `$SP/q "select count(*) from collection_items"` — то же число, что `iblock_elements` до миграции (записать до).

```bash
git add migrations/0014_collections.sql src templates
git commit -m "БД: коллекции вместо инфоблоков"
```

### Task 3: Имена в Rust

**Files:**
- Rename: `src/iblock/` → `src/collection/` (`mod.rs`, `props.rs` → `fields.rs`, `repo.rs`); `src/admin/iblocks.rs` → `src/admin/collections.rs`; `src/admin/elements.rs` → `src/admin/items.rs`
- Modify: все использования (компилятор подскажет), `src/access.rs`, `src/groups.rs`, `src/catalog/mod.rs`, `src/cart/*`, `src/sale/*`, `src/admin/*`, `src/test_support.rs`, `projects/transopt.rs`

**Interfaces:**
- Produces (переименования): `Iblock` → `Collection`, `IblockSummary` → `CollectionSummary` (поле `iblock` → `collection`, `element_count` → `item_count`), `IblockInput` → `CollectionInput`, `Property` → `Field`, `PropertyInput` → `FieldInput`, `PropertyEnum` → `FieldOption`, `EnumInput` → `OptionInput`, `Element` → `Item`, `ElementInput` → `ItemInput`, `PropertyKind` → `FieldKind`; функции `repo::*_iblock*` → `*_collection*`, `*_property*` → `*_field*`, `*_enums` → `*_options`, `list_iblock_enums` → `list_collection_options`, `*_element*` → `*_item*`, `element_names` → `item_names`; `access::IBLOCKS_MANAGE` → `COLLECTIONS_MANAGE`, `Access::iblock_level` → `collection_level`, `require_iblock` → `require_collection`, `can_see_iblocks` → `can_see_collections`; `groups::iblock_levels` → `collection_levels`; `catalog::load(db, item_ids)` и прочие параметры `element_id(s)` → `item_id(s)`; `test_support::Fixture.element_id` → `product_id`.
- В bxapi, `api.rs`, `import.rs` — только то, что требует компилятор.
- Шаблоны: обращения к переименованным полям (`can_see_iblocks`, `element_count`, `summary.iblock`) — по новым именам.

- [ ] **Step 1: Переименовать и собрать**

Run: `cargo build 2>&1 | tail -3`
Expected: сборка без ошибок.

- [ ] **Step 2: Прогнать всё**

Run: `rtk proxy cargo test > $WS/t3.log 2>&1; tail -5 $WS/t3.log; cargo clippy --all-targets 2>&1 | tail -2`
Expected: все зелёные (страховочные тесты ловят забытые поля в шаблонах), clippy без предупреждений.

- [ ] **Step 3: Коммит**

```bash
git add -A src templates
git commit -m "Код: коллекции, записи и поля вместо инфоблоков"
```

### Task 4: Адреса и тексты админки

**Files:**
- Modify: `src/admin/mod.rs` (маршруты), `src/admin/collections.rs`, `src/admin/items.rs`, `src/admin/sections.rs`, `src/admin/groups.rs` (поля формы `collection_<id>`)
- Rename: `templates/admin/iblocks.html` → `collections.html`, `iblock_form.html` → `collection_form.html`, `elements.html` → `items.html`, `element_form.html` → `item_form.html`, `property_form.html` → `field_form.html`
- Modify: `templates/admin/base.html` (меню, id пункта `collections`), `dashboard.html`, `section_form.html`, `group_form.html`, тексты во всех перечисленных
- Test: `src/smoke_tests.rs`

**Interfaces:**
- Маршруты: `/collections` (GET список, POST создать), `/collections/new`, `/collections/{id}` (GET/POST), `/collections/{id}/delete`, `/collections/{id}/fields` (POST), `/fields/{id}` (GET/POST), `/fields/{id}/options` (POST), `/fields/{id}/delete`, `/collections/{id}/sections` (POST), `/collections/{id}/sections/new`, `/sections/{id}` (GET/POST), `/sections/{id}/delete`, `/collections/{id}/items` (GET/POST), `/collections/{id}/items/new`, `/items/{id}` (GET/POST), `/items/{id}/delete`.
- Тексты по словарю во всех падежах: «Коллекции», «Новая коллекция», «Записи», «Добавить запись», «Поля», «Добавить поле», «Варианты», «Значение списка» → «Вариант». Вне словаря не переписывать.

- [ ] **Step 1: Перевести страховочные тесты на новые адреса и тексты и добавить тесты**

`admin_pages_render` — те же проверки по новым адресам; пункт меню «Коллекции»; ни одна страница не содержит «нфоблок» (без учёта регистра первой буквы).

```rust
#[sqlx::test]
async fn old_admin_urls_gone(db: PgPool) {
    // GET /admin/iblocks, /admin/elements/{i}, /admin/properties/{f} → 404
}

#[sqlx::test]
async fn group_collection_access_roundtrip(db: PgPool) {
    // POST /admin/groups/{g} c collection_{c}=read (+ обязательные поля формы группы)
    // → GET /admin/groups/{g}: у коллекции отмечен read;
    // пользователь группы (не админ, admin.access) открывает /admin/collections/{c}/items → 200,
    // POST /admin/collections/{c}/items → 403
}
```

- [ ] **Step 2: Запустить — должны упасть**

Run: `rtk proxy cargo test smoke_tests 2>&1 | tail -8`
Expected: FAIL — 404 на `/admin/collections`.

- [ ] **Step 3: Маршруты, редиректы и ссылки в обработчиках, имена шаблонов, тексты, поля формы группы**

- [ ] **Step 4: Прогнать всё**

Run: `rtk proxy cargo test > $WS/t4.log 2>&1; tail -5 $WS/t4.log`
Expected: все зелёные.

- [ ] **Step 5: Коммит**

```bash
git add -A src templates
git commit -m "Админка: коллекции, записи и поля"
```

### Task 5: Документация, проект и сквозная проверка

**Files:**
- Modify: `README.md` — раздел «Инфоблоки» → «Коллекции» со словарём и пометкой «аналог инфоблоков Битрикса»; остальные упоминания по словарю, кроме описаний bxapi, нашего API и импорта
- Modify: `projects/transopt.rs` (вне git) — если ещё остались старые имена

- [ ] **Step 1: README и проект**

- [ ] **Step 2: Аудит имён**

Run: `rtk proxy grep -rniE "iblock|инфоблок" src templates README.md | grep -vE "^src/(bxapi|import\.rs|api\.rs)" `
Expected: только README в описаниях bxapi, нашего API, импорта и в пометке «аналог инфоблоков Битрикса»; в `src` вне bxapi/import/api — ничего (кроме `access.rs`-комментариев, если описывают миграцию прав — переписать).

- [ ] **Step 3: Полный прогон с `projects/` и без**

Run: `rtk proxy cargo test > $WS/t5.log 2>&1; tail -3 $WS/t5.log; cargo clippy --all-targets 2>&1 | tail -1`; затем то же без `projects/`.
Expected: все зелёные оба раза, clippy без предупреждений.

- [ ] **Step 4: «После» с работающего сервера**

Run: пересобрать release, перезапустить сервер; снять те же 10 ответов в `$SP/rename/after/`; `diff -r $SP/rename/before $SP/rename/after`.
Expected: различий нет.

- [ ] **Step 5: Фронт трансопта и нагрузка**

Run: через фронт на :5010 — главная, каталог, карточка, корзина (`/cart`), оформление (`/order?type=pickup`), ЛК (`/profile`); затем `bench.sh` из замера 2026-10-09 (Rust, n=2000, c=16).
Expected: страницы работают; rps каждого запроса не ниже 90% от `docs/benchmark-bitrix-vs-rust.md`. Новый тестовый заказ (если оформлен) не удалять.

- [ ] **Step 6: Коммит**

```bash
git add README.md
git commit -m "Документация: коллекции"
```
