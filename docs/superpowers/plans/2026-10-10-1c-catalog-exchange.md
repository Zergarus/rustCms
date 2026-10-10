# Обмен с 1С: каталог — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 1С выгружает каталог в CMS по протоколу `1c_exchange.php` (CommerceML 3.1 пакетами и классический 2.x) так же, как в Битрикс.

**Architecture:** Новый модуль `src/exchange/`: `settings` (настройки в `options`), `cml` (потоковый разбор XML в типизированные структуры, без БД), `apply` (запись разобранного в коллекции/каталог в одной транзакции), `session` (сессии и файлы обмена), `http` (роутер протокола). Админка — страница настроек и журнала. Всё сопоставляется по `xml_id`.

**Tech Stack:** Rust 2024, Axum 0.8, SQLx 0.9 (PostgreSQL 17), quick-xml (с `encoding`), zip, MiniJinja.

**Spec:** `docs/superpowers/specs/2026-10-10-1c-catalog-exchange-design.md`

## Global Constraints

- Адрес протокола: `/bitrix/admin/1c_exchange.php` и `/api/1c/exchange`; ответы `text/plain; charset=utf-8`, строки через `\n`; первая строка `success` | `progress` | `failure`.
- Cookie сессии обмена — `CMS_1C_SID`; `checkauth` отвечает `success\nCMS_1C_SID\n<токен>`; `init` — `zip=yes|no\nfile_limit=<байты>\nsessid=<токен>\nversion=3.1`.
- Право `exchange.1c`, название «Обмен с 1С».
- Значения действий как в Битриксе: `N` ничего, `A` деактивировать, `D` удалить.
- Настройки по умолчанию: интервал 30 с; `file_limit` 209715200 (200 МБ на запрос); zip — да; транслит при добавлении — да, при обновлении — нет; предложения — нет; деактивировать без цены — нет; действия разделов и записей — `N`.
- Лимиты архива: распакованный размер ≤ 2147483648 байт, записей ≤ 100000; пути только внутри каталога сессии.
- Сессия живёт 24 часа с последней активности; каталог сессии — `<UPLOAD_DIR>/1c_exchange/<id сессии>/`.
- Системные поля: `xml_id`/код/вид — `CML2_ARTICLE`/`cml2_article`/string, `CML2_BAR_CODE`/`cml2_bar_code`/string, `CML2_MANUFACTURER`/`cml2_manufacturer`/list, `CML2_TRAITS`/`cml2_traits`/string множественное, `CML2_TAXES`/`cml2_taxes`/string множественное, `CML2_BASE_UNIT`/`cml2_base_unit`/string, `CML2_PICTURES`/`cml2_pictures`/file множественное, `CML2_FILES`/`cml2_files`/file множественное. Поле коллекции с таким `xml_id` используется вместо создания.
- Реальные выгрузки трансопта в репозиторий не кладутся; фикстуры — рукописные в `tests/fixtures/1c/`.
- Тесты: `rtk proxy cargo test`; `cargo fmt`; `cargo clippy --all-targets` без предупреждений; то же без `projects/`.
- Коммиты на русском, без упоминаний Claude/ИИ. В локальный Битрикс — только чтение. Сквозной прогон реальных пакетов — только на копии dev-базы.

## Review Focus

1. Тот же пакет приходит повторно (1С повторяет обмен после сбоя). Ожидается: ничего не меняется — нет новых записей, файлов, вариантов, `updated_at` не трогается у неизменённых записей. Тест `repeat_package_is_noop`, Задача 4.
2. Пакет цен/остатков приходит раньше пакета товаров (1С шлёт файлы в своём порядке) или ссылается на неизвестный товар/склад/тип цены. Ожидается: неизвестные предложения пропускаются со счётчиком «пропущено» в журнале, импорт не падает; новый склад или тип цены из пакета цен/остатков создаётся. Тест `offers_for_unknown_goods_are_skipped`, Задача 5.
3. Обрыв: `import` вызвали до того, как файл догружен, или архив повреждён. Ожидается: `failure` с понятным текстом, база не тронута, повтор после догрузки проходит. Тест `import_of_broken_zip_fails_cleanly`, Задача 6.
4. Значение справочного свойства, которого нет в `ВариантыЗначений`, и свойство, удалённое в 1С (`ПометкаУдаления`). Ожидается: недостающий вариант добавляется; удалённое свойство не удаляет поле и данные (только перестаёт обновляться). Тест `unknown_directory_value_adds_option`, Задача 3.
5. Две сессии обмена одновременно (две базы 1С или повторный запуск). Ожидается: файлы и множества «пришедших» не смешиваются, у каждой свой каталог; `deactivate` одной сессии не учитывает записи другой. Тест `sessions_are_isolated`, Задача 6.

---

### Task 1: Данные, право, настройки, перенос из Битрикса

**Files:**
- Create: `migrations/0017_exchange.sql`, `src/exchange/mod.rs`, `src/exchange/settings.rs`
- Modify: `src/main.rs` (`mod exchange;`), `src/access.rs` (право), `src/files.rs` (sha256), `src/import.rs` (XML_ID и опции), структуры `Collection`/`Field` и запросы, читающие их (`xml_id`)
- Test: `src/exchange/settings.rs`, `src/import.rs`, `src/files.rs`

**Interfaces:**
- Produces: колонки `xml_id TEXT NOT NULL DEFAULT ''` у `collections`, `collection_fields`, `catalog_stores`, `catalog_price_types` с уникальными индексами по непустому значению (`collection_fields` — `(collection_id, xml_id)`); `files.sha256 TEXT NOT NULL DEFAULT ''` + индекс; таблицы `exchange_sessions (id BIGSERIAL, token_hash TEXT UNIQUE, user_id, created_at, touched_at, state JSONB NOT NULL DEFAULT '{}')`, `exchange_seen (session_id REFERENCES exchange_sessions ON DELETE CASCADE, kind TEXT, xml_id TEXT, PRIMARY KEY (session_id, kind, xml_id))`, `exchange_log (id, created_at, user_id, mode, filename, result, stats JSONB, error TEXT, duration_ms INT)`.
- `access::EXCHANGE_1C = "exchange.1c"` в `PERMISSIONS` («Обмен с 1С», описание «Приём каталога из 1С по протоколу обмена»).
- `exchange::settings::{Action, Settings}`; `Action::{None, Deactivate, Delete}` ↔ `"N"|"A"|"D"`; `Settings { default_collection_id: Option<i64>, section_action: Action, element_action: Action, translit_on_add: bool, translit_on_update: bool, use_offers: bool, deactivate_without_price: bool, interval_secs: u32, file_limit: u64, use_zip: bool }`; `Settings::load(db: &PgPool) -> sqlx::Result<Settings>`, `Settings::save(&self, db: &PgPool) -> sqlx::Result<()>`; хранение в `options` модуля `exchange`, имена `default_collection`, `section_action`, `element_action`, `translit_on_add`, `translit_on_update`, `use_offers`, `deactivate_without_price`, `interval`, `file_limit`, `use_zip` (булевы — `Y`/`N`).
- `files::save_in(conn: &mut PgConnection, upload_dir, subdir, original_name, content_type, data) -> Result<FileRecord, SaveError>` — как `save`, но в переданном соединении; оба пишут `sha256` (hex). `FileRecord.sha256: String`.
- Импорт из Битрикса: `b_iblock.XML_ID` → `collections.xml_id`, `b_iblock_property.XML_ID` → `collection_fields.xml_id`, `b_catalog_store.XML_ID`, `b_catalog_group.XML_ID`; опции `catalog`: `1C_SECTION_ACTION`, `1C_ELEMENT_ACTION`, `1C_TRANSLIT_ON_ADD`, `1C_TRANSLIT_ON_UPDATE`, `1C_USE_OFFERS`, `1C_INTERVAL`, `1C_FILE_SIZE_LIMIT`, `1C_USE_ZIP`, `deactivate_1c_no_price` → настройки обмена.

- [ ] **Step 1: Тесты**

```rust
#[sqlx::test] async fn settings_defaults_and_roundtrip(db: PgPool) {
    // load на пустой базе = значения по умолчанию из Global Constraints; save(изменённые) → load == изменённые
}
#[test] fn action_codes() { /* "N"/"A"/"D" ↔ Action, неизвестное → None */ }
#[sqlx::test] async fn import_carries_xml_ids_and_exchange_options(db: PgPool) {
    // Data с IblockRow/PropertyRow/StoreRow/price type с XML_ID и опциями 1C_* → write_all →
    // xml_id в collections/collection_fields/catalog_stores/catalog_price_types; Settings::load отражает опции
}
#[sqlx::test] async fn saved_file_has_sha256(db: PgPool) { /* save(...) → sha256 = hex sha256 данных */ }
```

- [ ] **Step 2: Запустить — падают** (нет колонок/модуля).
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Весь набор зелёный.**
- [ ] **Step 5: Коммит** — `git commit -m "Обмен с 1С: данные и настройки"`

### Task 2: Разбор CommerceML

**Files:**
- Modify: `Cargo.toml` — `quick-xml = { version = "0.37", features = ["encoding"] }`
- Create: `src/exchange/cml.rs`, `tests/fixtures/1c/` (рукописные: `groups.xml`, `properties.xml`, `units.xml`, `storages.xml`, `price_lists.xml`, `goods.xml`, `prices.xml`, `rests.xml` в 3.1; `import.xml`, `offers.xml` в 2.x; `goods_cp1251.xml`)
- Test: `src/exchange/cml.rs`

**Interfaces:**
- Produces (`exchange::cml`), всё `Debug + Clone + PartialEq`:

```rust
pub struct Parsed { pub classifier: Option<Classifier>, pub catalog: Option<Catalog>, pub offers: Option<OfferPackage> }
pub struct Classifier { pub id: String, pub name: String, pub only_changes: bool, pub groups: Vec<Group>,
    pub properties: Vec<Property>, pub units: Vec<Unit>, pub stores: Vec<Store>, pub price_types: Vec<PriceType> }
pub struct Group { pub id: String, pub parent: Option<String>, pub name: String, pub deleted: bool }  // дерево развёрнуто, порядок пакета
pub enum PropKind { Directory, String, Number, Date }
pub struct Property { pub id: String, pub name: String, pub deleted: bool, pub kind: PropKind, pub multiple: bool, pub options: Vec<(String, String)> }
pub struct Unit { pub code: String, pub name: String }
pub struct Store { pub id: String, pub name: String, pub deleted: bool }
pub struct PriceType { pub id: String, pub name: String, pub currency: String, pub deleted: bool }
pub struct Catalog { pub id: String, pub classifier_id: String, pub name: String, pub only_changes: bool, pub goods: Vec<Goods> }
pub struct Goods { pub id: String, pub deleted: bool, pub name: String, pub article: String, pub barcode: String,
    pub description: String, pub groups: Vec<String>, pub pictures: Vec<String>, pub manufacturer: Option<(String, String)>,
    pub properties: Vec<(String, Vec<String>)>, pub traits: Vec<(String, String)>, pub taxes: Vec<(String, String)>,
    pub base_unit: String, pub weight_grams: Option<f64> }
pub struct OfferPackage { pub catalog_id: String, pub classifier_id: String, pub only_changes: bool, pub offers: Vec<Offer> }
pub struct Price { pub type_id: String, pub value: f64, pub currency: String }
pub struct Offer { pub id: String, pub deleted: bool, pub name: String, pub prices: Option<Vec<Price>>,
    pub rests: Option<Vec<(String, f64)>>, pub quantity: Option<f64>, pub characteristics: Vec<(String, String)> }
pub fn parse(data: &[u8]) -> Result<Parsed, CmlError>   // CmlError: thiserror, текст для ответа failure
```

- Правила: `СодержитТолькоИзменения` (нет атрибута → `false`); `ПометкаУдаления` → `deleted`; вес — реквизит «Вес» (значение в кг, если реквизит «ЕдиницаВеса»/текст содержит «кг», иначе граммы — как в Битриксе: «Вес» в кг → ×1000); пустые теги → `""`; пробелы в `БазоваяЕдиница` обрезаются; кодировка по объявлению XML (UTF-8 с BOM/без, windows-1251); `Цены` отсутствует → `prices: None`, пустой `<Цены/>` → `Some(vec![])`; то же для `Остатки`; 2.x `Количество` предложения → `quantity`.

- [ ] **Step 1: Тесты** — по одному на каждую фикстуру с проверкой конкретных значений (`classifier.groups[1].parent == Some(<ид родителя>)`, `properties[0].options[0] == (ид, значение)`, `goods[0].pictures.len() == 2`, `offers[0].prices.unwrap()[0].value == 6540.66`, `offers[0].rests.unwrap()` по складам, `offer.id` с `#` и `characteristics`), а также `detects_combined_import_xml` (2.x: `classifier` и `catalog` вместе), `cp1251_decoded`, `bom_ok`, `garbage_is_error`.
- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация** — `quick_xml::Reader` с собственным разбором событий (без построения DOM).
- [ ] **Step 4: Зелёные.**
- [ ] **Step 5: Коммит** — `git commit -m "Обмен с 1С: разбор CommerceML"`

### Task 3: Запись классификатора

**Files:**
- Create: `src/exchange/apply.rs`
- Test: `src/exchange/apply.rs`

**Interfaces:**
- Consumes: Task 1 (колонки `xml_id`, `Settings`), Task 2 (`cml::*`).
- Produces:

```rust
pub struct Ctx<'a> { pub settings: &'a Settings, pub session_id: Option<i64>, pub session_dir: &'a Path, pub upload_dir: &'a Path }
#[derive(Default, Serialize)] pub struct Stats { pub created: BTreeMap<&'static str, u32>, pub updated: BTreeMap<&'static str, u32>,
    pub deactivated: BTreeMap<&'static str, u32>, pub deleted: BTreeMap<&'static str, u32>, pub skipped: BTreeMap<&'static str, u32> }
pub async fn apply(conn: &mut PgConnection, ctx: &Ctx<'_>, parsed: &Parsed) -> Result<Stats, ApplyError>
pub async fn resolve_collection(conn, settings, classifier_or_catalog_id: &str, name: &str) -> Result<Collection, ApplyError>
```
  Ключи статистики: `"sections" | "fields" | "options" | "stores" | "price_types" | "items" | "offers" | "prices" | "files"`.
- В этой задаче `apply` обрабатывает `classifier` (разделы, поля и варианты, склады, типы цен) — по правилам раздела «Сопоставление» спецификации; товары и предложения — Задачи 4–5 (до них — `Ok` без изменений).
- При `ctx.session_id` и полной выгрузке (`only_changes == false`) пришедшие `xml_id` пишутся в `exchange_seen` (kind `"section"`, `"item"`).
- Код нового поля/коллекции — `collection::slugify(name)`, при занятости — суффикс `_2`, `_3`…

- [ ] **Step 1: Тесты**

```rust
#[sqlx::test] async fn classifier_creates_and_updates(db: PgPool) {
    // fixtures groups+properties+storages+price_lists → коллекция по xml_id классификатора (новая, каталог);
    // разделы деревом с parent; поля list/string с вариантами; склады; типы цен (валюта, не базовый);
    // повтор с изменённым названием группы → обновление, не дубль
}
#[sqlx::test] async fn default_collection_is_used(db: PgPool) { /* Settings.default_collection_id = существующая → ей присвоен xml_id */ }
#[sqlx::test] async fn deleted_group_follows_section_action(db: PgPool) { /* N — без изменений; A — active=false; D — удалена */ }
#[sqlx::test] async fn unknown_directory_value_adds_option(db: PgPool) { /* Review Focus 4: вариант добавлен; удалённое свойство не удаляет поле */ }
```

- [ ] **Step 2–4: RED → реализация → весь набор зелёный.**
- [ ] **Step 5: Коммит** — `git commit -m "Обмен с 1С: классификатор"`

### Task 4: Запись товаров и файлов

**Files:**
- Modify: `src/exchange/apply.rs`, `src/bxapi/project/mod.rs` (хук)
- Test: `src/exchange/apply.rs`

**Interfaces:**
- Consumes: Task 3 (`apply`, `Ctx`, `Stats`, `resolve_collection`), Task 1 (`files::save_in`, `sha256`).
- Produces: обработка `catalog` в `apply`: записи по `xml_id` (поля и системные поля из Global Constraints; первая картинка → детальная и превью; все → `cml2_pictures`; вес → `catalog_products.weight`; раздел — первая группа; код — транслит по настройкам; `ПометкаУдаления` → `element_action`); `pub async fn store_file(conn, ctx, rel_path: &str, existing: &[i64]) -> Result<Option<i64>, ApplyError>` — файл из `session_dir`, sha256 совпал с одним из `existing` → его id, нет файла в пакете → `None`; хук `Project::after_import_item: Option<Box<dyn Fn…>>` — в виде трейта `ImportHook: Send + Sync { async fn after_item(&self, conn: &mut PgConnection, item_id: i64, goods: &cml::Goods) -> anyhow::Result<()> }` по образцу существующих трейтов `Decorator`/`FormGuard` (поле `Project.import_hook: Option<Arc<dyn ImportHook>>`, по умолчанию `None`).
- Неизменённая запись не обновляется (сравнение значимых полей до `UPDATE`).

- [ ] **Step 1: Тесты**

```rust
#[sqlx::test] async fn goods_create_update_with_fields(db: PgPool) { /* значения полей, системных, раздел, код транслитом, вес */ }
#[sqlx::test] async fn pictures_are_deduplicated_by_sha256(db: PgPool) {
    // две картинки → 2 файла, детальная = первая; повтор с теми же байтами под другим именем → те же file_id, files не растут
}
#[sqlx::test] async fn deleted_goods_follow_element_action(db: PgPool) {}
#[sqlx::test] async fn repeat_package_is_noop(db: PgPool) { /* Review Focus 1: updated_at, count(files), count(options) не меняются */ }
#[sqlx::test] async fn existing_field_with_cml2_xml_id_is_reused(db: PgPool) { /* поле more_photo с xml_id CML2_PICTURES получает картинки, cml2_pictures не создаётся */ }
```

- [ ] **Step 2–4: RED → реализация → весь набор зелёный.**
- [ ] **Step 5: Коммит** — `git commit -m "Обмен с 1С: товары и файлы"`

### Task 5: Предложения: цены, остатки, SKU

**Files:**
- Modify: `src/exchange/apply.rs`
- Test: `src/exchange/apply.rs`

**Interfaces:**
- Consumes: Tasks 3–4; SKU: `repo::create_offer_collection`, `Item.product_id`, `catalog::save_purchase_in` / прямые запросы к `catalog_prices`, `catalog_store_amounts`, `catalog_products`.
- Produces: обработка `offers` в `apply`: `Ид` без `#` → цены/остатки записи; `товар#характеристика` → предложение (только при `use_offers`; коллекция предложений создаётся через `create_offer_collection`, поля-строки характеристик с `offer_tree = true`, `xml_id` = полный `Ид`, `product_id` = товар); `Цены` → замена цен этих типов у записи; `Остатки` → суммы по складам и `catalog_products.quantity`; 2.x `Количество` без складов → только `quantity`; новый склад/тип цены, встреченный в пакете, создаётся; неизвестный товар → `skipped["offers"] += 1`; `deactivate_without_price` → запись без цен после пакета цен неактивна.

- [ ] **Step 1: Тесты**

```rust
#[sqlx::test] async fn prices_and_rests_applied(db: PgPool) { /* значения по типам цен и складам из фикстур, quantity = сумма */ }
#[sqlx::test] async fn characteristic_offers_become_sku(db: PgPool) { /* use_offers=true: коллекция предложений, product_id, поле характеристики offer_tree, товар type 3 */ }
#[sqlx::test] async fn characteristic_offers_skipped_without_use_offers(db: PgPool) {}
#[sqlx::test] async fn offers_for_unknown_goods_are_skipped(db: PgPool) { /* Review Focus 2 */ }
#[sqlx::test] async fn goods_without_price_deactivated_when_enabled(db: PgPool) {}
```

- [ ] **Step 2–4: RED → реализация → весь набор зелёный.**
- [ ] **Step 5: Коммит** — `git commit -m "Обмен с 1С: цены, остатки и предложения"`

### Task 6: Протокол и сессии

**Files:**
- Modify: `Cargo.toml` — `zip = { version = "2", default-features = false, features = ["deflate"] }`
- Create: `src/exchange/session.rs`, `src/exchange/http.rs`
- Modify: `src/main.rs` (`app()` — маршруты протокола)
- Test: `src/exchange/http.rs`, `src/exchange/session.rs`

**Interfaces:**
- Consumes: Tasks 1–5 (`Settings`, `cml::parse`, `apply::apply`, `exchange_*` таблицы).
- Produces: `exchange::http::router() -> Router<AppState>` на оба адреса; `session::{Session, start(db, user_id) -> (Session, token), find(db, token) -> Option<Session>, dir(upload_dir, &Session) -> PathBuf, append(dir, filename, bytes) -> Result<(), SessionError>, unpack_zips(dir, limits) -> Result<(), SessionError>, cleanup_expired(db, upload_dir)}`; `pub async fn deactivate_missing(conn, settings, session_id) -> Result<Stats, ApplyError>` в `apply.rs` (действия по `exchange_seen` для коллекций, затронутых сессией); запись в `exchange_log` на каждый `import`, `deactivate`, `complete`.
- Шаг `import`: один файл — одна транзакция `parse` + `apply`; при превышении `interval_secs` файл импортируется до конца всё равно (Rust укладывается), ответ `success\n<итог>`; состояние «файл импортирован» в `state` сессии — повторный `import` того же файла в той же сессии отвечает `success` без повторной записи.
- `checkauth` без/с неверным Basic или без права → `failure\nДоступ запрещён`; суперадмин проходит.

- [ ] **Step 1: Тесты**

```rust
#[sqlx::test] async fn full_exchange_flow(db: PgPool) {
    // checkauth (пользователь с exchange.1c) → init (zip=yes, file_limit=209715200, version=3.1) →
    // file: zip с groups/goods/prices частями по 1 КБ → import каждого → success → complete → данные в БД, запись в exchange_log
}
#[sqlx::test] async fn checkauth_requires_permission(db: PgPool) { /* без права, неверный пароль, без Basic → failure */ }
#[sqlx::test] async fn path_traversal_rejected(db: PgPool) { /* filename=../x и zip с ../x → failure, файл вне каталога не создан */ }
#[sqlx::test] async fn zip_limits_enforced(db: PgPool) { /* архив, распакованный размер > лимита (лимит параметром) → failure */ }
#[sqlx::test] async fn import_of_broken_zip_fails_cleanly(db: PgPool) { /* Review Focus 3 */ }
#[sqlx::test] async fn sessions_are_isolated(db: PgPool) { /* Review Focus 5 */ }
#[sqlx::test] async fn full_export_deactivates_missing(db: PgPool) { /* only_changes=false пакет без одной записи, element_action=A → deactivate → запись неактивна */ }
#[sqlx::test] async fn expired_session_rejected(db: PgPool) {}
```

- [ ] **Step 2–4: RED → реализация → весь набор зелёный.**
- [ ] **Step 5: Коммит** — `git commit -m "Обмен с 1С: протокол"`

### Task 7: Админка

**Files:**
- Create: `src/admin/exchange.rs`, `templates/admin/exchange.html`
- Modify: `src/admin/mod.rs` (маршруты `/admin/exchange` GET/POST), `templates/admin/base.html` (пункт меню «Обмен с 1С», виден с правом `exchange.1c`)
- Test: `src/smoke_tests.rs`

**Interfaces:**
- Consumes: `Settings::{load, save}`, `exchange_log`.
- Страница: форма настроек (коллекция по умолчанию — выбор из коллекций с каталогом; действия N/A/D с подписями «ничего / деактивировать / удалить»; флажки; интервал; лимит в МБ) и таблица 50 последних записей журнала (время, режим, файл, итог, счётчики, ошибка).

- [ ] **Step 1: Тесты**

```rust
#[sqlx::test] async fn exchange_settings_page(db: PgPool) {
    // GET /admin/exchange → 200, «Обмен с 1С»; POST изменённых настроек → 303 → Settings::load отражает;
    // запись exchange_log видна на странице; пользователь без права → 403; пункт меню виден только с правом
}
```

- [ ] **Step 2–4: RED → реализация → весь набор зелёный.**
- [ ] **Step 5: Коммит** — `git commit -m "Админка: обмен с 1С"`

### Task 8: Документация и сквозная проверка на реальных пакетах

**Files:**
- Modify: `README.md` — раздел «Обмен с 1С» (адрес, право, форматы, сопоставление, настройки, журнал)

- [ ] **Step 1: README; полный набор с `projects/` и без; clippy.**
- [ ] **Step 2: Копия dev-базы** — `CREATE DATABASE cms_1c TEMPLATE cms` (сервер на копии — отдельный процесс на порту 3001 с `DATABASE_URL` копии и отдельным `UPLOAD_DIR`).
- [ ] **Step 3: Прогон «Песочницы»** — скрипт в scratchpad имитирует 1С: `checkauth` (пользователь с правом), `init`, для каждого файла `Exchange_Песочница_2026-08-20/files/**` в порядке лога 1С (priceLists, storages, propertiesGoods, units, groups, goods, prices, rests) — `file` (архив пакета с `import_files`, частями по `file_limit`) и `import`, затем `complete`. Expected: все ответы `success`; журнал без ошибок; время — в отчёт.
- [ ] **Step 4: Сверка с Битриксом** — 50 случайных товаров из пакета: название, раздел, цены по типам (`b_catalog_price` по `CATALOG_GROUP_ID` ↔ тип цены по `xml_id`), остатки по складам (`b_catalog_store_product`), значения 5 полей — с `transopt` MySQL (только SELECT). Expected: расхождения перечислены и объяснены (например, данные Битрикса новее пакета).
- [ ] **Step 5: Повтор пакета** — Expected: `files`, `collection_items`, `collection_field_options` не растут; `updated_at` неизменённых записей прежний.
- [ ] **Step 6: Пакет «Номенклатура 2026-09-29»** — то же; Expected: `success`.
- [ ] **Step 7: Регрессия на основной dev-базе** — ответы bxapi байт в байт (снимки `scratchpad/rename/before`), фронт трансопта (каталог, карточка, корзина, ЛК). Копию базы и её `UPLOAD_DIR` после проверки удалить.
- [ ] **Step 8: Коммит** — `git commit -m "Документация: обмен с 1С"`
