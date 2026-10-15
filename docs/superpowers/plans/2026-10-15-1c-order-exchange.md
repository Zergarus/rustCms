# Обмен с 1С: заказы — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 1С обменивается заказами с CMS по протоколу `type=sale` так же, как с модулем `sale` Битрикса: забирает новые и изменённые заказы и присылает изменения заказов, оплат и отгрузок.

**Architecture:** Новый подмодуль `src/exchange/sale/`: `settings` (настройки `sale_*` и соответствие реквизитов), `document` (модель документа и запись XML без БД), `select` (сборка документов из БД и отбор к выгрузке), `parse` (разбор документов 1С без БД), `apply` (запись принятого в заказы). Протокол — ветка `type=sale` в существующем `src/exchange/http.rs`. Изменения заказов на сайте сбрасывают `updated_1c` триггером, изменения обмена ставят его через `SET LOCAL cms.exchange_1c = 'on'`.

**Tech Stack:** Rust 2024, Axum 0.8, SQLx 0.9 (PostgreSQL 17), quick-xml (уже есть, с `encoding`), encoding_rs (через quick-xml), MiniJinja.

**Spec:** `docs/superpowers/specs/2026-10-15-1c-order-exchange-design.md`

**Эталон поведения** — исходники Битрикса, только чтение: `sites/transopt.local/www/bitrix/modules/sale/` — `install/components/bitrix/sale.export.1c/component.php` (протокол), `general/export.php` (`CSaleExport`: документ, контрагент, `$systemCodes` — коды соответствия), `general/order_loader.php` (`CSaleOrderLoader::nodeHandlerDefaultModuleOneC` и вызываемые им методы — приём), `lang/ru/general/export.php` и `install/components/bitrix/sale.export.1c/lang/ru/component.php` (имена тегов и реквизитов).

## Global Constraints

- Адрес — `/api/1c/exchange`, `type=sale`; первая строка ответа `success` | `progress` | `failure` (у `success` без выгрузки — `error`); `query` и `info` — `application/xml; charset=windows-1251`, остальное — `text/plain; charset=utf-8`.
- `init` с `version`: `zip=yes|no\nfile_limit=<байты>\nsessid=<токен>\nversion=2.09`; без `version` — только первые две строки.
- Версия схемы — число из `version` (запятая = точка, пробелы убираются), без `version` — 2.05. Пороги: `> 2.05` — НомерВерсии, Номер1С, `Единица` с кодом (796 по умолчанию), типы контактов «Телефон рабочий» / «Электронная почта» (иначе «ТелефонРабочий» / «Почта»); `>= 2.10` — документы оплат, отгрузок и удалённых отгрузок; `>= 3` — документы заказа в `<Контейнер>`. `cmlVersion >= 2.10` — `failure` «Схема обмена cmlVersion ≥ 2.10 не поддерживается».
- Право `exchange.1c.sale`, название «Обмен заказами с 1С»; `exchange.1c` его не даёт и наоборот.
- Метка изменения из обмена: `SET LOCAL cms.exchange_1c = 'on'` в транзакции обмена; любое изменение строки `orders` без неё даёт `updated_1c = FALSE`.
- Настройки по умолчанию — как в Битриксе: выгрузка только оплаченных — нет, только с разрешённой доставкой — нет, начиная со статуса — не задан, замена валюты и префикс — пусто, менять статус из 1С — нет, статус при отгрузке — не задан, обновлять количество в корзине — да, создавать заказы / оплаты / отгрузки / отгрузку для нового заказа — да, платёжные системы и служба по умолчанию — не заданы.
- Ид контрагента без `users.xml_id`: первые 40 символов `<user_id>#<логин>#<фамилия> <имя> <отчество>`, сохраняется в `users.xml_id` при выгрузке (не при «Выгрузить XML» из админки); гость — `<order_id>#guest`.
- Коды соответствия — как у Битрикса: `BUYER_PERSON_*`, `BUYER_COMPANY_*` (список и поле контрагента — `$systemCodes` в `general/export.php`), произвольные реквизиты `1C_REKV_<n>`.
- Тексты реквизитов и тегов — дословно из языковых файлов Битрикса (см. «Эталон поведения»).
- Фикстуры — рукописные, `tests/fixtures/1c/sale/`; реальные заказы и выгрузки в репозиторий не кладутся.
- Тесты: `rtk proxy cargo test`; `cargo fmt`; `cargo clippy --all-targets` без предупреждений; то же без `projects/` (переименовать `projects/transopt.rs` на время прогона и вернуть).
- Коммиты на русском, без упоминаний Claude/ИИ. В локальный Битрикс — только чтение. Тестовые заказы dev-базы не удалять; сквозной прогон — на копии базы.

## Review Focus

1. Покупатель меняет заказ на сайте (или менеджер в админке) между `query` и `success` 1С. Ожидается: изменённый заказ уйдёт в следующем обмене, даже если `success` подтвердит метку, — метка сессии берётся по `updated_at` выгруженных строк, а изменённый после выгрузки заказ получает больший `updated_at`. Тест `order_changed_between_query_and_success_is_exported_again`, Задача 3.
2. 1С присылает тот же файл повторно (повтор после сбоя). Ожидается: ни новых заказов, оплат, отгрузок и пользователей, ни повторного письма о статусе, ни двойного возврата остатков. Тест `repeat_file_is_noop`, Задача 6.
3. В документе 1С товар, которого нет на сайте (неизвестный `xml_id`), при правке состава или создании заказа. Ожидается: ошибка этого документа с Ид товара в тексте, заказ не тронут, остальные документы файла применены. Тест `unknown_product_fails_only_its_document`, Задача 5.
4. Заказ с символами вне windows-1251 (эмодзи в комментарии, «№», `&`, `<`). Ожидается: валидный XML, спецсимволы экранированы, непредставимые — `?`, остальной текст цел. Тест `cp1251_escaping_and_unmappable`, Задача 2.
5. Две сессии `type=sale` одновременно или `type=catalog` и `type=sale` в одной сессии. Ожидается: метка выгрузки и состояние `query` у каждой сессии свои; `success` одной не подтверждает выгрузку другой; каталоговая сессия не даёт `query`. Тест `sale_sessions_are_isolated`, Задача 7.

---

### Task 1: Данные, право, настройки, перенос, транзакционные действия заказа

**Files:**
- Create: `migrations/0018_order_exchange.sql`, `src/exchange/sale/mod.rs`, `src/exchange/sale/settings.rs`
- Modify: `src/exchange/mod.rs` (`pub mod sale;`), `src/access.rs`, `src/sale/repo.rs` (`set_status`, `set_paid`, `set_canceled` + `_in`-варианты), `src/import.rs` (опции, соответствия, домен, `XML_ID` пользователей, метка)
- Test: `src/exchange/sale/settings.rs`, `src/sale/repo.rs`, `src/import.rs`

**Interfaces:**
- Produces: колонки и таблицы из раздела «Данные» спецификации (`shipments.tracking_number` и `allow_delivery` уже есть с 0013 — не добавлять); таблица `shipments_removed (order_id BIGINT REFERENCES orders ON DELETE CASCADE, shipment_id BIGINT, id_1c TEXT, removed_at TIMESTAMPTZ DEFAULT now())`, заполняемая AFTER DELETE на `shipments` (источник `ExportOrder.removed_shipments`, как `SHIPMENT_REMOVED` у Битрикса); триггер `orders_mark_1c` (BEFORE UPDATE на `orders`: при `NEW IS DISTINCT FROM OLD` — `NEW.updated_1c := current_setting('cms.exchange_1c', true) = 'on'` с COALESCE, `NEW.updated_at := now()`); триггеры AFTER INSERT/UPDATE/DELETE на `payments`, `shipments`, `order_property_values` и `cart_items` (только строки с `order_id`) — `UPDATE orders SET updated_at = now() WHERE id = <order_id>`.
- `exchange::sale::mark_exchange(conn: &mut PgConnection) -> sqlx::Result<()>` — `SET LOCAL cms.exchange_1c = 'on'`.
- `access::EXCHANGE_1C_SALE = "exchange.1c.sale"` в `PERMISSIONS` («Обмен заказами с 1С», «Выгрузка заказов в 1С и приём изменений по протоколу обмена»).
- `exchange::sale::settings::SaleSettings { export_paid_only: bool, export_allow_delivery_only: bool, export_from_status: Option<String>, replace_currency: String, number_prefix: String, change_status_from_1c: bool, final_status_on_delivery: Option<String>, update_basket_quantity: bool, import_new_orders: bool, import_new_payments: bool, import_new_shipments: bool, new_order_new_shipment: bool, default_ps_cash: Option<i64>, default_ps_cashless: Option<i64>, default_ps_acquiring: Option<i64>, default_ps_order_paid: Option<i64>, default_delivery: Option<i64>, last_export_committed: Option<DateTime<Utc>> }`; `load(db)`, `save(&self, db)`, `commit_export(db, at: DateTime<Utc>)`. Имена в `options` модуля `exchange`: `sale_export_paid`, `sale_export_allow_delivery`, `sale_export_from_status`, `sale_replace_currency`, `sale_number_prefix`, `sale_change_status`, `sale_final_status_on_delivery`, `sale_update_basket_quantity`, `sale_import_new_orders`, `sale_import_new_payments`, `sale_import_new_shipments`, `sale_new_order_new_shipment`, `sale_default_ps_cash`, `sale_default_ps_cashless`, `sale_default_ps_acquiring`, `sale_default_ps_order_paid`, `sale_default_delivery`, `sale_last_export_committed` (RFC 3339).
- `exchange::sale::settings::{MapSource, MapEntry, load_map, save_map}`: `MapSource::{Property, User, Order, Value}` ↔ `"property"|"user"|"order"|"value"`; `MapEntry { person_type_id: i64, code: String, name: String, source: MapSource, value: String }`; `load_map(db) -> sqlx::Result<Vec<MapEntry>>`; `save_map(db, person_type_id, &[MapEntry])` (замена строк типа плательщика). Поля пользователя: `email`, `login`, `name`, `last_name`, `second_name`, `phone`; поля заказа: `id`, `account_number`, `user_comment`, `price`.
- `exchange::sale::settings::CONTRAGENT_CODES: &[(&str, &str, Domain)]` — (код, ключ поля контрагента, домен) из `$systemCodes`; `Domain::{Individual, Entity}` ↔ `'I'|'E'`.
- `sale::repo::{set_status_in, set_paid_in, set_canceled_in}(conn: &mut PgConnection, …)` — тела нынешних функций; пул-варианты открывают транзакцию и вызывают их.
- Перенос: `sale/1C_*` → `sale_*` по таблице спецификации (`1C_FILE_SIZE_LIMIT`, `1C_INTERVAL`, `1C_SALE_USE_ZIP` модуля `sale` не переносятся — эти настройки общие с каталогом); `last_export_time_committed_*` (unix-время) → `sale_last_export_committed`; `b_sale_bizval` с `CONSUMER_KEY = '1C'` поверх `COMMON` (тип плательщика 0 — для всех), `PROVIDER_KEY` `PROPERTY|USER|ORDER|VALUE|INPUT` → `property|user|order|value|value`, id свойств — через карту id импорта; имена `1C_REKV_<n>` — из `b_sale_bizval_code_1c`; `b_sale_bizval_persondomain` → `person_types.domain`; `b_user.XML_ID` → `users.xml_id`.

- [ ] **Step 1: Тесты**

```rust
#[sqlx::test] async fn site_change_resets_updated_1c(db: PgPool) {
    // заказ с updated_1c = TRUE; UPDATE orders SET manager_comment = 'x' → updated_1c FALSE, updated_at вырос;
    // в транзакции с mark_exchange тот же UPDATE → TRUE; INSERT в payments заказа без метки → FALSE
}
#[sqlx::test] async fn sale_settings_defaults_and_roundtrip(db: PgPool) { /* умолчания из Global Constraints; save → load */ }
#[sqlx::test] async fn order_map_roundtrip(db: PgPool) { /* save_map двух типов, замена одного не трогает другой */ }
#[sqlx::test] async fn repo_actions_run_inside_caller_transaction(db: PgPool) { /* set_status_in + rollback → статус прежний */ }
#[sqlx::test] async fn import_carries_sale_exchange_options(db: PgPool) {
    // опции 1C_CHANGE_STATUS_FROM_1C=Y, 1C_FINAL_STATUS_ON_DELIVERY=F, 1C_IMPORT_NEW_ORDERS=N, 1C_REPLACE_CURRENCY=руб.,
    // last_export_time_committed_/x=1778677162; bizval 1C + COMMON; persondomain E; b_user.XML_ID → settings, map, domain, users.xml_id
}
```

- [ ] **Step 2: Запустить — падают** (нет колонок, модуля, функций).
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Весь набор зелёный**, clippy чистый.
- [ ] **Step 5: Коммит** — `git commit -m "Обмен заказами с 1С: данные, настройки, перенос"`

### Task 2: Документ заказа и XML

**Files:**
- Create: `src/exchange/sale/document.rs`, `tests/fixtures/1c/sale/export_*.xml` (эталоны: `export_205_person.xml`, `export_208_company.xml`, `export_210_payment_shipment.xml`, `export_31_container.xml`, `export_canceled_guest.xml`)
- Test: `src/exchange/sale/document.rs`

**Interfaces:**
- Produces (`exchange::sale::document`), всё `Debug + Clone + PartialEq`:

```rust
pub struct SchemaVersion(pub f64);            // parse("2,08") == 2.08; default 2.05
impl SchemaVersion { pub fn parse(raw: Option<&str>) -> SchemaVersion; pub fn new_contacts(&self) -> bool /* > 2.05 */;
                     pub fn documents(&self) -> bool /* >= 2.10 */; pub fn container(&self) -> bool /* >= 3 */ }
pub struct ExportOrder { pub id: i64, pub number: String, pub created_at: DateTime<Utc>, pub currency: String,
    pub sum: f64, pub comment: String, pub id_1c: String, pub version_1c: String,
    pub agent: Agent, pub items: Vec<ExportItem>, pub delivery: Option<ExportDelivery>, pub store_xml_id: Option<String>,
    pub traits: Vec<(String, String)>, pub payments: Vec<ExportPayment>, pub shipments: Vec<ExportShipment>,
    pub removed_shipments: Vec<RemovedShipment> }
pub struct RemovedShipment { pub id: String /* id_1c или наш id */, pub date: DateTime<Utc> }
pub struct Agent { pub id: String, pub domain: Domain, pub fields: BTreeMap<String, String> /* ключи $systemCodes: FULL_NAME, INN, PHONE… */ }
pub struct ExportItem { pub xml_id: String, pub catalog_xml_id: String, pub name: String, pub unit_code: String,
    pub unit_name: String, pub price: f64, pub quantity: f64, pub sum: f64 }
pub struct ExportDelivery { pub price: f64 }
pub struct ExportPayment { pub id: String, pub number: String, pub date: DateTime<Utc>, pub operation: PayOperation,
    pub sum: f64, pub version_1c: String, pub traits: Vec<(String, String)> }
pub enum PayOperation { Cashless, Cash, Acquiring }   // «Выплата безналичных денег» / «… наличных …» / «Эквайринговая операция»
pub struct ExportShipment { pub id: String, pub number: String, pub date: DateTime<Utc>, pub sum: f64,
    pub version_1c: String, pub items: Vec<ExportItem>, pub delivery: Option<ExportDelivery>, pub traits: Vec<(String, String)> }
pub fn render(orders: &[ExportOrder], version: SchemaVersion, generated_at: DateTime<Utc>) -> Vec<u8>; // windows-1251
```

- Состав и порядок элементов — `CSaleExport::OutputXmlDocument`, `ExportContragents`, `getXmlBasketItems`, `getOrderDeliveryItem`, `OutputXmlDocumentsByType`, `OutputXmlDocumentRemove`; корень `КоммерческаяИнформация ВерсияСхемы="<версия как пришла, по умолчанию 2.05>" ДатаФормирования="<YYYY-MM-DDTHH:MM:SS>"`; `<?xml version="1.0" encoding="windows-1251"?>`. Реквизиты заказа (`traits`) собирает Задача 3, `render` пишет их как есть.

- [ ] **Step 1: Тесты**

```rust
#[test] fn version_thresholds() { /* parse(None)=2.05; "2,10".documents(); "3.1".container(); "2.05".new_contacts()==false */ }
#[test] fn renders_reference_documents() { /* для каждого эталона: собранный вручную ExportOrder → render → сравнение
   разобранных деревьев (теги, атрибуты, текст; пробелы между тегами не важны) с fixtures/1c/sale/export_*.xml */ }
#[test] fn cp1251_escaping_and_unmappable() { /* комментарий "№ 5 & <b> 😀" → в байтах cp1251 "№ 5 &amp; &lt;b&gt; ?" */ }
#[test] fn delivery_line_and_units_by_version() { /* 2.05: ORDER_DELIVERY без Единица; 2.08: Единица 796 */ }
```

- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация** (запись через `quick_xml::Writer` в UTF-8, затем `encoding_rs::WINDOWS_1251.encode` — его замена непредставимых на `&#…;` заменяется на `?`).
- [ ] **Step 4: Тесты зелёные.**
- [ ] **Step 5: Коммит** — `git commit -m "Обмен заказами с 1С: документ заказа в CommerceML"`

### Task 3: Сборка из БД и отбор к выгрузке

**Files:**
- Create: `src/exchange/sale/select.rs`
- Modify: `src/bxapi/project/mod.rs` (`OrderExportHook`), `src/exchange/sale/mod.rs`
- Test: `src/exchange/sale/select.rs`

**Interfaces:**
- Consumes: Задачи 1–2.
- Produces:
  - `select::build(conn: &mut PgConnection, order_id: i64, s: &SaleSettings, map: &[MapEntry], hook: Option<&dyn OrderExportHook>, persist_xml_id: bool) -> anyhow::Result<ExportOrder>` — заказ, позиции (`xml_id` записи и коллекции, `cml2_base_unit`), отгрузки, оплаты, контрагент по соответствию (без строки соответствия — умолчания: почта `users.email`, `FULL_NAME` — свойство `is_payer`, телефон `is_phone`, адрес `is_address`, город `is_location`), реквизиты заказа (список из спецификации, дословные названия), `1C_REKV_*`, реквизиты хука; `persist_xml_id` — сохранять ли сгенерированный Ид контрагента.
  - `select::query(db: &PgPool, s: &SaleSettings, interval: std::time::Duration /* Settings.interval_secs каталога */, hook) -> anyhow::Result<Batch>`; `Batch { orders: Vec<ExportOrder>, mark: Option<DateTime<Utc>> }` — отбор и порядок из спецификации, остановка по интервалу (0 — без остановки), `mark` — максимальный `updated_at` выгруженных.
  - `pub trait OrderExportHook: Send + Sync { fn traits<'a>(&'a self, conn: &'a mut PgConnection, order_id: i64) -> BoxFuture<'a, anyhow::Result<Vec<(String, String)>>>; }`; `Project::order_export_hook: Option<Arc<dyn OrderExportHook>>`, по умолчанию `None`.

- [ ] **Step 1: Тесты** (`order_fixture`/`place_order` из `test_support`)

```rust
#[sqlx::test] async fn new_order_is_selected_then_not_after_commit(db: PgPool) { /* query → 1 заказ; commit_export(mark) → query пуст */ }
#[sqlx::test] async fn order_changed_between_query_and_success_is_exported_again(db: PgPool) { /* query; правка; commit_export(mark) → query = этот заказ */ }
#[sqlx::test] async fn change_from_exchange_is_not_selected(db: PgPool) { /* UPDATE в транзакции с mark_exchange → пусто */ }
#[sqlx::test] async fn filters_paid_delivery_status_and_external(db: PgPool) { /* каждый фильтр отсекает; external не уходит никогда */ }
#[sqlx::test] async fn agent_from_map_and_defaults(db: PgPool) { /* соответствие свойства, поля пользователя, константы; без соответствия — умолчания; юрлицо по domain */ }
#[sqlx::test] async fn agent_xml_id_generated_once(db: PgPool) { /* persist=true → users.xml_id = обрезка до 40; повтор — то же; persist=false не пишет */ }
#[sqlx::test] async fn hook_traits_are_appended(db: PgPool) { /* хук-заглушка → реквизит в traits */ }
#[sqlx::test] async fn interval_limits_batch(db: PgPool) { /* interval ~0 → хотя бы один заказ за вызов */ }
```

- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Тесты зелёные.**
- [ ] **Step 5: Коммит** — `git commit -m "Обмен заказами с 1С: сборка и отбор заказов"`

### Task 4: Разбор документов из 1С

**Files:**
- Create: `src/exchange/sale/parse.rs`, `tests/fixtures/1c/sale/import_*.xml` (`import_order_status.xml`, `import_order_basket.xml`, `import_new_order_agent.xml`, `import_payment.xml`, `import_shipment.xml`, `import_removed.xml`, `import_container.xml`, `import_cp1251.xml`)
- Test: `src/exchange/sale/parse.rs`

**Interfaces:**
- Produces:

```rust
pub struct SaleFile { pub agents: Vec<AgentIn>, pub groups: Vec<Vec<DocumentIn>> /* документ вне контейнера — группа из одного */ }
pub struct AgentIn { pub id: String, pub domain: Domain, pub fields: BTreeMap<String, String> /* те же ключи, что Agent */ }
pub enum Operation { Order, Payment(PayOperation), Shipment, Unknown(String) }
pub struct DocumentIn { pub id: String, pub number: String, pub id_1c: String, pub version: String, pub operation: Operation,
    pub base: String /* Основание */, pub deleted: bool, pub sum: Option<f64>, pub agent_id: Option<String>,
    pub items: Vec<ItemIn>, pub traits: BTreeMap<String, String> /* ЗначенияРеквизитов: Наименование → Значение */ }
pub struct ItemIn { pub xml_id: String, pub name: String, pub price: f64, pub quantity: f64, pub sum: f64, pub is_delivery: bool }
pub fn parse(data: &[u8]) -> Result<SaleFile, CmlError>; // CmlError — из exchange::cml
```

- Кодировки и чисел — как `exchange::cml` (BOM, windows-1251, пробелы и запятые в числах). `Номер1С` = `Номер` документа 1С, если отдельного тега нет (как `CSaleOrderLoader` берёт `XML_1C_DOCUMENT_ID`) — эталон берёт из `order_loader.php`.

- [ ] **Step 1: Тесты** — на каждую фикстуру ожидаемый `SaleFile`; `Unknown("Возврат товаров")` для неизвестной операции; контейнер даёт группу из трёх документов; cp1251 читается.
- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Тесты зелёные.**
- [ ] **Step 5: Коммит** — `git commit -m "Обмен заказами с 1С: разбор документов 1С"`

### Task 5: Приём документа заказа

**Files:**
- Create: `src/exchange/sale/apply.rs`
- Modify: `src/bxapi/project/mod.rs` (`OrderImportHook`), `src/sale/repo.rs` (если нужна общая функция пересчёта сумм)
- Test: `src/exchange/sale/apply.rs`

**Interfaces:**
- Consumes: Задачи 1, 4; `sale::repo::*_in`, `sale::stock::{plan, apply}`, `sale::mail::notify`.
- Produces:
  - `apply::SaleStats { orders_created, orders_updated, payments_created, payments_updated, shipments_created, shipments_updated, skipped: u32, errors: Vec<String> }` (`Serialize`, `Default`).
  - `apply::apply_file(db: &PgPool, s: &SaleSettings, map: &[MapEntry], file: &SaleFile, hook: Option<&dyn OrderImportHook>) -> SaleStats` — каждая группа в своей транзакции с `mark_exchange`; ошибка группы — откат группы и строка в `errors` вида «Документ <Ид>: <текст>»; письма о смене статуса — после коммита.
  - `apply::find_order(conn, doc_id_or_number: &str, prefix: &str) -> sqlx::Result<Option<i64>>` — по id, затем `id_1c`, затем `account_number` без префикса.
  - `pub trait OrderImportHook: Send + Sync { fn after_document<'a>(&'a self, conn: &'a mut PgConnection, order_id: i64, doc: &'a DocumentIn) -> BoxFuture<'a, anyhow::Result<()>>; }`; `Project::order_import_hook`.
- В этой задаче — только `Operation::Order`; остальные операции — ошибка «не реализовано» до Задачи 6.
- Тексты ошибок: «заказ не найден», «заказ в финальном состоянии, состав не изменён», «товар <xml_id> не найден», «тип плательщика не найден».

- [ ] **Step 1: Тесты**

```rust
#[sqlx::test] async fn status_changes_only_with_option(db: PgPool) { /* выкл — статус прежний, id_1c записан; вкл — статус, история «1С», updated_1c TRUE */ }
#[sqlx::test] async fn status_change_sends_mail_once(db: PgPool) { /* статус с notify → одно письмо; повтор файла → ни одного */ }
#[sqlx::test] async fn cancel_and_uncancel_restore_stock(db: PgPool) { /* Отменен=true → остатки вернулись; false → списаны снова */ }
#[sqlx::test] async fn deletion_mark_cancels(db: PgPool) {}
#[sqlx::test] async fn older_version_is_skipped(db: PgPool) { /* version_1c "5", документ "4" → без изменений, skipped=1 */ }
#[sqlx::test] async fn basket_update_with_option(db: PgPool) { /* количество, цена, удалённая и новая строка → позиции, суммы, остатки */ }
#[sqlx::test] async fn basket_kept_without_option_and_for_final_order(db: PgPool) { /* выкл → без правки; оплаченный → ошибка финального состояния */ }
#[sqlx::test] async fn new_order_with_new_and_existing_agent(db: PgPool) { /* external, покупатель по xml_id или новый пользователь без пароля; тип по домену; отгрузка при опции */ }
#[sqlx::test] async fn new_order_refused_when_disabled(db: PgPool) { /* ошибка «заказ не найден», заказов не прибавилось */ }
#[sqlx::test] async fn unknown_product_fails_only_its_document(db: PgPool) { /* два документа, второй с чужим товаром → первый применён, errors=1 */ }
#[sqlx::test] async fn import_hook_runs_in_transaction(db: PgPool) { /* хук с ошибкой → документ откатился */ }
```

- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Тесты зелёные.**
- [ ] **Step 5: Коммит** — `git commit -m "Обмен заказами с 1С: приём документа заказа"`

### Task 6: Приём оплат и отгрузок

**Files:**
- Modify: `src/exchange/sale/apply.rs`
- Test: `src/exchange/sale/apply.rs`

**Interfaces:**
- Consumes: Задача 5 (`apply_file`, `find_order`, `SaleStats`).
- Produces: обработка `Operation::Payment(_)` и `Operation::Shipment` по спецификации; реквизиты 1С: «Оплачен», «Дата оплаты по 1С», «Номер оплаты по 1С», «ПризнакВозврата», «Причина к Возврату», «Дата отгрузки по 1С», «Номер отгрузки по 1С», «Идентификатор отправления», «Доставка разрешена»; `Unknown(_)` — ошибка «неизвестная хозяйственная операция <имя>».
- Тексты ошибок: «оплата не найдена», «отгрузка не найдена», «оплата проведена, удаление невозможно», «отгрузка проведена, удаление невозможно», «количество в отгрузке не совпадает с заказом».

- [ ] **Step 1: Тесты**

```rust
#[sqlx::test] async fn payment_paid_with_voucher(db: PgPool) { /* найдена по id → paid, номер и дата, orders.paid при всех оплаченных */ }
#[sqlx::test] async fn payment_return_unpays(db: PgPool) {}
#[sqlx::test] async fn new_payment_uses_default_pay_system_by_operation(db: PgPool) { /* наличные/безнал/эквайринг; оплачен целиком → default_ps_order_paid */ }
#[sqlx::test] async fn payment_deletion_rules(db: PgPool) { /* неоплаченная удалена; оплаченная — ошибка */ }
#[sqlx::test] async fn shipment_deducted_tracking_and_final_status(db: PgPool) { /* отгружена, номер, трек; статус → final_status_on_delivery; остатки не тронуты */ }
#[sqlx::test] async fn shipment_quantity_mismatch(db: PgPool) { /* с опцией — правка позиций; без — ошибка */ }
#[sqlx::test] async fn new_shipment_and_disabled_creation(db: PgPool) {}
#[sqlx::test] async fn container_is_one_transaction(db: PgPool) { /* контейнер: заказ ок, оплата с ошибкой → весь контейнер откатился */ }
#[sqlx::test] async fn repeat_file_is_noop(db: PgPool) { /* файл со всеми видами документов дважды → счётчики второго прогона нулевые, строк не прибавилось */ }
```

- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Тесты зелёные.**
- [ ] **Step 5: Коммит** — `git commit -m "Обмен заказами с 1С: приём оплат и отгрузок"`

### Task 7: Протокол `type=sale`

**Files:**
- Create: `src/exchange/sale/http.rs`
- Modify: `src/exchange/http.rs` (ветвление по `type`, `Outcome` с байтами и типом содержимого, журнал с `kind`), `src/exchange/session.rs` (тип сессии в `state`)
- Test: `src/exchange/sale/http.rs`, `src/exchange/http.rs`

**Interfaces:**
- Consumes: Задачи 1–6.
- Produces: `Outcome { body: Vec<u8>, content_type: &'static str, stats: Value }` (`Outcome::text` — как сейчас, `Outcome::xml(bytes)`); `checkauth` проверяет право по `type` (`catalog` → `exchange.1c`, `sale` → `exchange.1c.sale`) и пишет `state.type`; запрос с другим `type`, чем у сессии, — `failure` «Сессия открыта для другого типа обмена»; режимы `sale`: `init` (версия и `cmlVersion` в `state`), `query` (`select::query`, `state.mark`, `persist_xml_id = true`), `success` (`commit_export(state.mark)`, `state.mark` очищается; без `mark` — `error`), `file` (как каталог), `import` (распаковка как каталог, затем `parse` + `apply_file` целиком за вызов; `success\n<итог>` или `success` с ошибками документов построчно; файл в `state.imported` не обрабатывается повторно), `info` (справочники); журнал: `kind = 'sale'` для `query`, `success`, `import` со счётчиками `SaleStats` или числом выгруженных заказов.

- [ ] **Step 1: Тесты** (клиент из `exchange::http::tests`, вынесенный в `pub(crate)`-помощник)

```rust
#[sqlx::test] async fn sale_permission_is_separate(db: PgPool) { /* exchange.1c → failure на type=sale; exchange.1c.sale → failure на type=catalog */ }
#[sqlx::test] async fn init_with_and_without_version(db: PgPool) { /* строки ответа из Global Constraints */ }
#[sqlx::test] async fn query_success_cycle(db: PgPool) { /* query: application/xml; charset=windows-1251, документ заказа; success → повторный query без документов */ }
#[sqlx::test] async fn success_without_query_is_error(db: PgPool) {}
#[sqlx::test] async fn file_and_import_plain_and_zip(db: PgPool) { /* файл частями и в zip → статус заказа изменён, журнал kind=sale */ }
#[sqlx::test] async fn info_lists_statuses_pay_systems_deliveries(db: PgPool) {}
#[sqlx::test] async fn cml_version_210_fails(db: PgPool) {}
#[sqlx::test] async fn sale_sessions_are_isolated(db: PgPool) { /* две сессии: query A, query B, success A → заказ ещё выгружается B; каталоговая сессия на query → failure */ }
```

- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Весь набор зелёный** (каталоговые тесты протокола не меняются).
- [ ] **Step 5: Коммит** — `git commit -m "Обмен заказами с 1С: протокол type=sale"`

### Task 8: Админка

**Files:**
- Modify: `src/admin/exchange.rs`, `templates/admin/exchange.html`, `src/admin/shop/orders.rs`, `templates/admin/shop/order.html`, `src/admin/mod.rs` (маршруты)
- Test: `src/admin/exchange.rs`, `src/admin/shop/orders.rs`

**Interfaces:**
- Consumes: Задачи 1, 3.
- Produces: `/admin/exchange?tab=catalog|sale` (вкладка видна по своему праву; без обоих — 403); форма настроек `SaleSettings` (выпадающие списки статусов, платёжных систем, служб); `/admin/exchange/map/{person_type_id}` — домен и строки соответствия (код из `CONTRAGENT_CODES` по домену или `1C_REKV_<n>` с названием; источник и значение), сохранение `save_map`; журнал — колонка «Тип»; карточка заказа — блок «1С» (Номер1С, версия, «изменён из 1С», «создан в 1С»; у оплат и отгрузок — номера, даты, трек) и `GET /admin/shop/orders/{id}/xml1c` — `select::build(…, persist_xml_id = false)` + `render` с версией 2.08, `Content-Disposition: attachment; filename="order_<id>_1c.xml"` (право `orders.manage`).

- [ ] **Step 1: Тесты** — сохранение и показ каждой настройки; ошибка формы сохраняет введённое; соответствие сохраняется и отображается; право на вкладки; XML из карточки — windows-1251, документ заказа, `users.xml_id` не изменился, метка выгрузки не сдвинулась.
- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Тесты зелёные.**
- [ ] **Step 5: Коммит** — `git commit -m "Админка обмена заказами с 1С"`

### Task 9: Документация и сквозная проверка

**Files:**
- Modify: `README.md` (раздел «Обмен с 1С»: заказы, настройки, соответствия, перенос, точки расширения)
- Scratchpad (не в репозиторий): эмулятор 1С `emu1c_sale.py`

**Interfaces:**
- Consumes: всё выше.

- [ ] **Step 1: README.**
- [ ] **Step 2: Копия dev-базы** (`CREATE DATABASE cms_sale TEMPLATE …`), сервер на свободном порту с этой базой, пользователь с правом `exchange.1c.sale` в копии.
- [ ] **Step 3: Эмулятор**: `checkauth` → `init version=2.10` → `query` (сохранить XML, проверить валидность и число документов) → `success` → повторный `query` пуст → `file`+`import` ответного файла (статус, оплата, отгрузка с треком, новый заказ с контрагентом) → проверка в копии → повтор файла без новых строк → `info`. Время каждого шага — в отчёт.
- [ ] **Step 4: Регрессия на основной базе**: ответы bxapi и API байт в байт со сборкой до 2б (снимок из `rename/snap.sh`); оформление заказа с фронта → `updated_1c = FALSE`. Тестовые заказы не удалять.
- [ ] **Step 5: Уборка** — копия базы, сервер, каталоги сессий.
- [ ] **Step 6: Коммит** — `git commit -m "Обмен заказами с 1С: документация"`
