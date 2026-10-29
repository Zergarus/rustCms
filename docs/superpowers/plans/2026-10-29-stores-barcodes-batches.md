# Склады: штрихкоды и партии (5б-1) — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** штрихкоды товаров и поштучный учёт через документы и отгрузку, партии по FIFO или
средней с себестоимостью и маржой заказа, админка и перенос из Битрикса — паритет с `catalog`
Битрикса.

**Architecture:** три новых файла в `src/stock/`:
- `barcodes.rs` — штрихкоды: хранение, проверки, хуки проведения и отгрузки, поиск;
- `batches.rs` — партии: чистые правила (расход, средняя цена) плюс применение к БД из
  проведения и отгрузки;
- `cost.rs` — себестоимость позиций, маржа заказа, стоимость остатков;
- `rebuild.rs` — фоновая перестройка партий по истории при выборе метода.

Хуки вызываются из существующих `documents::run` и `ship::ship`/`unship` в той же транзакции,
после проверок 5а и до записи статуса. Без поштучных товаров и без выбранного метода хуки
ничего не делают — так держится регрессия.

**Tech Stack:** Rust 2024, Axum 0.8, SQLx 0.9 (`#[sqlx::test]`), PostgreSQL 17, MiniJinja, HTMX,
vanilla JS.

**Spec:** `docs/superpowers/specs/2026-10-29-stores-barcodes-batches-design.md`

## Global Constraints

- Новая миграция `0025_barcodes_batches.sql`; `0001`–`0024` применены — не менять.
- Ветка `feature/stores-barcodes` (от `feature/admin-shell`).
- Опция метода: `options (module='catalog', name='cost_price_calculation_method')`, значения
  `''` (нет), `fifo`, `average`. Выбирается один раз; при включённом складском учёте.
- Право: `catalog.purchase_info`, название «Видеть закупочные цены и себестоимость».
- Тексты Битрикса дословно (`#…#` подставляется):
  - «В документе складского учета число штрихкодов товара ##ID# (#NAME#) не совпадает с
    количеством самого товара»;
  - «Не совпадает количество товара ##ID# (#NAME#) и число штрихкодов в строке документа
    складского учета»;
  - «Штрихкод #BARCODE# товара ##ID# (#NAME#) привязан к товару #OTHER_ID#»;
  - «На складе #STORE# нет товара #PRODUCT# со штрихкодом #BARCODE#»;
  - «Штрихкод #BARCODE#, введенный для товара #PRODUCT#, уже существует.»;
  - «У товара "#PRODUCT_NAME#" не указан штрихкод.»;
  - «Для товара #PRODUCT_NAME# (##PRODUCT_ID#) не найден штрихкод "#BARCODE#".».
- Наши тексты (из спеки): «У товара есть штрихкоды единиц на складах»; «Метод выбирается один
  раз, сменить его потом будет нельзя»; «Партии перестраиваются: N из M»; «партии покрывают N из
  M»; «оценка»; «по закупочной цене»; «Маржа заказа».
- Округление средней цены партии — до 2 знаков.
- Себестоимость и маржа переводятся в валюту заказа (`crate::currency::Converter`), стоимость
  остатков — в валюту сайта.
- Блокировки строк товаров — только через `catalog::load_locked`, как в 5а.
- Регрессия: без поштучных товаров и без метода ответы bxapi и `/api/v1` байт в байт как до
  подпроекта; эталоны `tests/fixtures/**` не меняются; существующие тесты 5а проходят без правок.
- Тексты интерфейса и комментарии — по-русски, как в окружающем коде.
- Сообщения коммитов — по-русски, без упоминаний ИИ/Claude и строк `Co-Authored-By`.
- В локальный Битрикс — только SELECT; `docs/benchmark-bitrix-vs-rust.md` не коммитится.
- Проверка после задачи: `cargo test` (вся сборка, `DATABASE_URL` из `.env`),
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.

## Review Focus

1. **Повторный скан одного штрихкода** в документе или отгрузке. Ожидание: дубль в форме не
   сохраняется молча — проведение даёт «Штрихкод X, введенный для товара Y, уже существует.».
   Тест в Task 3 `duplicate_barcode_in_document_rejected`.
2. **Флажок «поштучный» включён у товара, по которому уже были приходы без штрихкодов.**
   Ожидание: старые остатки не ломаются, требования действуют на новые проведения; списание
   существующего остатка без штрихкодов отклоняется с понятной ошибкой. Тест в Task 3
   `multi_enabled_after_stock_requires_barcodes`.
3. **Отмена прихода FIFO, когда из его партии уже продали.** Ожидание: себестоимость отгрузки
   переезжает на другие партии склада, а не теряется. Тест в Task 6
   `fifo_cancel_arrival_rebinds_shipment`.
4. **Документ в долларах при средней партии в рублях.** Ожидание: цена переводится по текущему
   курсу, средняя считается в валюте партии. Тест в Task 6
   `average_merges_foreign_currency`.
5. **Сервер перезапустился посреди перестройки.** Ожидание: перестройка продолжается со
   следующего товара, обработанные товары не дублируются. Тест в Task 7
   `rebuild_resumes_after_restart`.

---

### Task 1: Миграция и право `catalog.purchase_info`

**Files:**
- Create: `migrations/0025_barcodes_batches.sql`
- Modify: `src/access.rs` (константа и `PERMISSIONS`)
- Test: `src/access.rs` (`mod tests`), `src/stock/mod.rs` (`mod tests`)

**Interfaces:**
- Produces: таблицы ниже; `access::PURCHASE_INFO: &str = "catalog.purchase_info"`.

Схема (`0025_barcodes_batches.sql`):
- `ALTER TABLE catalog_products ADD COLUMN barcode_multi BOOLEAN NOT NULL DEFAULT FALSE;`
- `catalog_barcodes (id BIGSERIAL PK, item_id BIGINT NOT NULL REFERENCES collection_items ON
  DELETE CASCADE, barcode TEXT NOT NULL UNIQUE, store_id BIGINT REFERENCES catalog_stores ON
  DELETE SET NULL, order_id BIGINT REFERENCES orders ON DELETE SET NULL, created_by, modified_by
  BIGINT, created_at, updated_at TIMESTAMPTZ NOT NULL DEFAULT now())`; индекс `(item_id, store_id)`.
- `store_document_barcodes (id BIGSERIAL PK, doc_item_id BIGINT NOT NULL REFERENCES
  store_document_items ON DELETE CASCADE, barcode TEXT NOT NULL)`; индекс `(doc_item_id)`.
- `ALTER TABLE shipment_item_stores ADD COLUMN id BIGSERIAL; ` + `UNIQUE (id)` (составной PK
  остаётся).
- `shipment_item_barcodes (id BIGSERIAL PK, shipment_id BIGINT NOT NULL REFERENCES shipments ON
  DELETE CASCADE, cart_item_id BIGINT NOT NULL REFERENCES cart_items ON DELETE CASCADE, store_id
  BIGINT NOT NULL, barcode TEXT NOT NULL, UNIQUE (shipment_id, barcode))`.
- `catalog_store_batches (id BIGSERIAL PK, item_id BIGINT NOT NULL REFERENCES collection_items ON
  DELETE CASCADE, store_id BIGINT NOT NULL REFERENCES catalog_stores ON DELETE CASCADE,
  available NUMERIC(18,3) NOT NULL DEFAULT 0, price NUMERIC(18,2) NOT NULL DEFAULT 0, currency
  TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now())`; индекс `(item_id, store_id, id)`.
- `store_batch_moves (id BIGSERIAL PK, batch_id BIGINT NOT NULL REFERENCES catalog_store_batches
  ON DELETE CASCADE, doc_item_id BIGINT REFERENCES store_document_items ON DELETE CASCADE,
  shipment_store_id BIGINT REFERENCES shipment_item_stores (id) ON DELETE CASCADE, amount
  NUMERIC(18,3) NOT NULL, price NUMERIC(18,2) NOT NULL, currency TEXT NOT NULL, CHECK
  (num_nonnulls(doc_item_id, shipment_store_id) = 1))`; индексы по `batch_id`, `doc_item_id`,
  `shipment_store_id`.
- `INSERT INTO group_permissions (group_id, permission) SELECT DISTINCT group_id,
  'catalog.purchase_info' FROM group_permissions WHERE permission IN ('collections.manage',
  'shop.manage') ON CONFLICT DO NOTHING;`

- [ ] **Step 1: Тесты.** `purchase_info_listed` в `src/access.rs`: `PERMISSIONS` содержит
  `PURCHASE_INFO` с названием «Видеть закупочные цены и себестоимость». `#[sqlx::test]
  migration_grants_purchase_info` в `src/stock/mod.rs`: каждая группа, у которой после
  миграций есть `collections.manage` или `shop.manage` (группы из `0003_groups.sql`), имеет и
  `catalog.purchase_info`; суррогатный `shipment_item_stores.id` заполняется при вставке.
- [ ] **Step 2:** `cargo test purchase_info migration_grants` — FAIL.
- [ ] **Step 3:** Миграция и константа.
- [ ] **Step 4:** Эти тесты — PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Склады: таблицы штрихкодов и партий, право на закупочные цены».

### Task 2: Штрихкоды товара — хранение и поиск

**Files:**
- Create: `src/stock/barcodes.rs`
- Modify: `src/stock/mod.rs` (`pub mod barcodes;`)

**Interfaces:**
- Produces:
  - `pub async fn set_common(tx: &mut PgConnection, item_id: i64, codes: &[String], user:
    Option<i64>) -> Result<(), StockError>` — заменяет строки товара с `store_id IS NULL AND
    order_id IS NULL`; пустые и повторы внутри списка отбрасываются (trim); код у другого
    товара — `Rejected` с «Штрихкод #BARCODE#, введенный для товара #PRODUCT#, уже
    существует.» (`#PRODUCT#` — название товара, для которого вводили).
  - `pub async fn set_multi(tx, item_id: i64, on: bool) -> Result<(), StockError>` — снятие
    флажка при строках с `store_id IS NOT NULL` → `Rejected("У товара есть штрихкоды единиц на
    складах")`.
  - `#[derive(Debug, Clone, PartialEq, sqlx::FromRow)] pub struct BarcodeRow { pub barcode:
    String, pub store_id: Option<i64>, pub order_id: Option<i64> }`;
    `pub async fn list(tx, item_id: i64) -> sqlx::Result<Vec<BarcodeRow>>` (по `barcode`).
  - `pub async fn find_item(db: &mut PgConnection, code: &str) -> sqlx::Result<Option<i64>>` —
    точное совпадение (trim) в `catalog_barcodes`, затем в значении поля с кодом
    `cml2_bar_code` в `collection_items.field_values` (формат хранения — как пишет
    `assign_system` в `src/exchange/apply.rs`); при нескольких записях — наименьший id.
  - `pub fn looks_like_barcode(q: &str) -> bool` — 8–14 цифр после trim (EAN-8…GTIN-14).

- [ ] **Step 1: Тесты** (`#[sqlx::test]` в `barcodes.rs`, фикстура — `order_fixture`):
  `set_common_replaces` (`["111","222"]` → `["222","333"]` → `list` = 222, 333);
  `set_common_rejects_foreign` (код второго товара → текст «уже существует.» с названием);
  `set_multi_blocked_by_store_barcodes`; `find_item_by_barcode_then_field` (код только в
  `cml2_bar_code` находится; код в обоих местах у разных товаров → из `catalog_barcodes`);
  `#[test] looks_like_barcode_cases` (`"4601234567890"` да, `"1044"` нет, `"12345678"` да,
  `"abc12345678"` нет).
- [ ] **Step 2:** `cargo test stock::barcodes` — FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Склады: штрихкоды товара».

### Task 3: Штрихкоды в документах склада

**Files:**
- Modify: `src/stock/documents.rs` (`Line`, `insert_lines`, загрузка строк в `run`, вызов хука)
- Modify: `src/stock/barcodes.rs` (хук)
- Modify: все места, где строится `Line` (`src/admin/shop/store_docs.rs::to_input`, тесты) —
  `barcodes: Vec::new()` до Task 9

**Interfaces:**
- Consumes: Task 2.
- Produces:
  - `Line` получает поле `pub barcodes: Vec<String>`; `insert_lines` пишет его в
    `store_document_barcodes`; `run` загружает их в строки.
  - `pub(super) async fn apply_document(tx: &mut PgConnection, doc_type: DocType, lines:
    &[Line], names: &HashMap<i64, String>, stores: &HashMap<i64, (String, bool)>, cancel: bool,
    user: Option<i64>) -> Result<(), StockError>` — вызывается в `run` после проверки минуса и до
    записи остатков; ошибки собираются списком.

Правила `apply_document` (спека §1):
- Флаг `barcode_multi` читается для товаров строк одним запросом.
- Обычный товар, проведение: каждый код строки — если нет в `catalog_barcodes`, вставить с
  `store_id NULL`; если есть у этого товара — пропустить; у другого — «Штрихкод #BARCODE#
  товара ##ID# (#NAME#) привязан к товару #OTHER_ID#». Отмена — ничего.
- Поштучный товар: сначала Σ кодов по товару ≠ Σ количеств → «В документе … ##ID# (#NAME#) не
  совпадает с количеством самого товара»; иначе по строке ≠ → «Не совпадает количество товара
  ##ID# (#NAME#) …». Повтор кода в документе → «Штрихкод #BARCODE#, введенный для товара
  #PRODUCT#, уже существует.». Код у другого товара → «…привязан к товару #OTHER_ID#».
- Таблица движений поштучного — как в спеке §1: приход/оприходование/возврат — upsert на
  `store_to` (код уже на складе у этого товара → «уже существует.»), отмена — delete;
  перемещение — код должен быть на `store_from` («На складе #STORE# нет товара #PRODUCT# со
  штрихкодом #BARCODE#», `#STORE#` и `#PRODUCT#` — названия), затем `store_id = store_to`, отмена —
  наоборот (проверка на `store_to`); списание — проверка на `store_from`, затем delete, отмена —
  insert на `store_from`.

- [ ] **Step 1: Тесты** (`#[sqlx::test]` в `documents.rs`, рядом с существующими):
  `ordinary_line_barcodes_become_common`; `ordinary_foreign_barcode_rejected`;
  `multi_count_mismatch_rejected` (оба текста: Σ и по строке);
  `multi_arrival_binds_and_cancel_removes`; `multi_moving_moves_and_back`;
  `multi_moving_wrong_store_rejected`; `multi_deduct_removes_and_cancel_restores`;
  `duplicate_barcode_in_document_rejected`; `multi_enabled_after_stock_requires_barcodes`
  (приход без кодов при выключенном флаге, затем флаг, списание 1 шт. с кодом, которого нет
  на складе → «На складе … нет товара … со штрихкодом …»); `barcodes_ignored_without_multi_flag`
  (остатки как в 5а).
- [ ] **Step 2:** `cargo test stock::documents` — новые FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Склады: штрихкоды в документах».

### Task 4: Штрихкоды в отгрузке

**Files:**
- Modify: `src/stock/ship.rs` (`set_deducted`, `ship`, `unship`)
- Modify: `src/stock/barcodes.rs`
- Modify: `src/admin/shop/orders.rs` (вызов `set_deducted` — передать пустой список до Task 10)

**Interfaces:**
- Consumes: Task 1.
- Produces:
  - `pub async fn choose_barcodes(tx, shipment_id: i64, codes: &[(i64, i64, Vec<String>)]) ->
    sqlx::Result<()>` в `barcodes.rs` — `(позиция, склад, коды)`; заменяет строки
    `shipment_item_barcodes` позиции; у отгруженной отгрузки не меняет; чужие позиции
    пропускаются.
  - `pub(super) async fn ship_out(tx, shipment_id: i64, lines: &[(i64, i64, String, i64)]) ->
    Result<(), StockError>` — `(позиция, товар, название, склад)` поштучных позиций; вызывается
    в `ship` до записи движений. Ошибки: кодов меньше количества (или ноль) → «У товара
    "#PRODUCT_NAME#" не указан штрихкод.»; кода нет на складе у товара → «Для товара
    #PRODUCT_NAME# (##PRODUCT_ID#) не найден штрихкод "#BARCODE#".». Успех: `store_id = NULL,
    order_id = <заказ>`.
  - `pub(super) async fn ship_back(tx, shipment_id: i64) -> sqlx::Result<()>` — коды отгрузки
    возвращаются: `store_id` из `shipment_item_barcodes`, `order_id = NULL`; вызывается в
    `unship`.
  - Без складского учёта оба хука не вызываются.

- [ ] **Step 1: Тесты** (`#[sqlx::test]` в `ship.rs`): `multi_ship_requires_barcodes`;
  `multi_ship_unknown_barcode_rejected`; `multi_ship_binds_order_and_unship_returns`;
  `ordinary_ship_unchanged` (существующий путь — остатки как в 5а);
  `barcode_error_cancels_whole_shipment` (две позиции, ошибка во второй — первая не списана).
- [ ] **Step 2:** `cargo test stock::ship` — новые FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Склады: штрихкоды в отгрузке».

### Task 5: Партии — чистые правила

**Files:**
- Create: `src/stock/batches.rs`
- Modify: `src/stock/mod.rs` (`pub mod batches;`)

**Interfaces:**
- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Method { Fifo, Average }`,
    `Method::from_code(&str) -> Option<Method>` (`"fifo"`, `"average"`), `code()`.
  - `#[derive(Debug, Clone, PartialEq)] pub struct Batch { pub id: i64, pub available: f64,
    pub price: f64, pub currency: String }`.
  - `#[derive(Debug, Clone, PartialEq)] pub struct Part { pub batch_id: i64, pub amount: f64 }`.
  - `pub fn write_off(batches: &[Batch], amount: f64) -> (Vec<Part>, f64)` — расход от
    меньшего `id` к большему, только `available > 0`; второе — непокрытое количество.
  - `pub fn average_add(old_amount: f64, old_price: f64, amount: f64, price: f64) -> f64` —
    средневзвешенная, округление до 2 знаков; `old_amount <= 0` → `price`.
  - `pub fn average_remove(old_amount: f64, old_price: f64, amount: f64, price: f64) -> f64` —
    обратная операция; остаток после вычитания `<= 0` → `old_price`.

- [ ] **Step 1: Тесты** (`#[test]`): `write_off_oldest_first` (партии 10@100 и 10@150, расход 12
  → `[(1,10),(2,2)]`, непокрыто 0); `write_off_shortage` (5 шт., расход 8 → непокрыто 3);
  `write_off_skips_empty`; `average_add_example` (10@100 + 10@150 → 125.0);
  `average_rounds_to_two` (3@10 + 1@11 → 10.25; 3@10 + 4@11 → 10.57);
  `average_remove_inverse` (20@125 − 10@150 → 100.0); `average_remove_to_zero_keeps_price`;
  `method_codes`.
- [ ] **Step 2:** `cargo test stock::batches` — FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Коммит «Склады: правила партий FIFO и средней».

### Task 6: Партии в проведении и отгрузке

**Files:**
- Modify: `src/stock/batches.rs` (слой БД)
- Modify: `src/stock/documents.rs` (`run` — вызов после записи остатков)
- Modify: `src/stock/ship.rs` (`ship` — `RETURNING id` у вставки `shipment_item_stores`;
  `unship`)
- Modify: `src/stock/mod.rs` (`Settings` получает `pub method: Option<batches::Method>`)

**Interfaces:**
- Consumes: Task 5; `crate::currency::Converter` (`new`, `convert`).
- Produces:
  - `pub async fn method(db: &mut PgConnection) -> sqlx::Result<Option<Method>>`.
  - `pub async fn set_method(tx, method: Method) -> Result<bool, StockError>` — `true`, если
    выбран сейчас; уже выбран (любой) → `Rejected("Метод расчёта себестоимости уже выбран")`;
    складской учёт выключен → `Rejected("Складской учёт выключен")`. Перестройку не запускает
    (Task 7).
  - `pub(super) async fn apply_document(tx, method: Method, doc_type: DocType, lines: &[Line],
    currency: &str, cancel: bool) -> Result<(), StockError>`.
  - `pub(super) async fn ship_out(tx, method: Method, shipment_store_id: i64, item_id: i64,
    store_id: i64, amount: f64) -> sqlx::Result<()>`.
  - `pub(super) async fn ship_back(tx, method: Method, shipment_id: i64) -> sqlx::Result<()>` —
    вызывается в `unship`; строки `shipment_item_stores` остаются (выбор склада), их движения
    удаляются.
  - Метод `None` — ни одна функция не вызывается.

Правила — таблица «Правила» спеки §2, плюс решения:
- Партии склада читаются `FOR UPDATE` в порядке `id`.
- Цена прихода — `purchasing_price` строки (нет — 0) в валюте документа. FIFO: новая партия
  в валюте документа. Средняя: партия `(товар, склад)` одна; новая — в валюте документа,
  существующая — цена строки переводится в её валюту `Converter` на сегодня, затем
  `average_add`.
- Каждое движение пишет `store_batch_moves` с ценой и валютой партии на момент движения.
- Отмена прихода, оприходования, возврата и получателя перемещения: FIFO — у партий этой строки
  расходы (минусовые движения других строк и отгрузок) перепривязываются через `write_off` по
  остальным партиям склада, непокрытое остаётся без движения, затем партии удаляются (каскад
  удаляет их движения). Средняя — `available -= amount`, цена через `average_remove`,
  движения строки удаляются.
- Отмена списания, источника перемещения, снятие отгрузки: количество каждого минусового
  движения возвращается в его партию (средняя — `average_add` с ценой движения), движения
  удаляются.
- Получатель перемещения: FIFO — на каждую часть расхода источника новая партия с ценой и
  валютой исходной партии; средняя — вливание в партию получателя через `average_add`.
- Документы `R` (возврат) — как приход.

- [ ] **Step 1: Тесты** (`#[sqlx::test]` в `batches.rs`, документы через
  `documents::create`/`conduct`, отгрузка через `ship::set_deducted`):
  `no_method_no_batches` (5а-поток, таблицы пусты); `set_method_once`;
  `fifo_arrival_creates_batches`; `fifo_ship_takes_oldest` (10@100, 10@150, отгрузка 12 →
  движения −10@100 и −2@150); `fifo_unship_returns`; `fifo_moving_carries_prices`;
  `fifo_cancel_arrival_rebinds_shipment` (приход 1 10@100, приход 2 10@150, отгрузка 5, отмена
  прихода 1 → движение отгрузки −5@150 на партию прихода 2);
  `average_merges_and_ships` (10@100 + 10@150 → одна партия 20@125; отгрузка 5 → −5@125);
  `average_merges_foreign_currency` (`seed_usd` с курсом 90; партия в RUB 10@100, приход в USD
  10@2 → цена (1000+1800)/20 = 140.00); `shortage_leaves_uncovered` (1С подняла остаток мимо
  документов: партии 5, отгрузка 8 → движения на 5).
- [ ] **Step 2:** `cargo test stock::batches` — новые FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Склады: партии в документах и отгрузке».

### Task 7: Перестройка партий по истории

**Files:**
- Create: `src/stock/rebuild.rs`
- Modify: `src/stock/mod.rs` (`pub mod rebuild;`), `src/main.rs` (`stock::rebuild::spawn`)

**Interfaces:**
- Consumes: Task 6.
- Produces:
  - Опции прогресса: `catalog.batch_rebuild_last` (последний обработанный `item_id`, пусто —
    задачи нет), `catalog.batch_rebuild_total`, `catalog.batch_rebuild_done`.
  - `pub async fn start(tx) -> sqlx::Result<bool>` — если в `catalog_store_batches` нет строк и
    метод выбран: записывает `last = 0`, `total` = число товаров с проведёнными строками
    документов или отгрузками, `done = 0`; иначе `false`.
  - `pub async fn step(db: &PgPool, limit: i64) -> anyhow::Result<bool>` — следующие `limit`
    товаров с `item_id > last` по возрастанию; каждый товар — своя транзакция: блокировка
    `catalog::load_locked`, удаление партий товара, проигрыш событий в порядке времени
    (проведённые документы по `date_status`, затем `id`; отгрузки основной отгрузки по
    `deducted_at`) через функции Task 6, затем `last`, `done`. `false` — товары кончились,
    опции `last` очищается.
  - `pub async fn progress(db: &mut PgConnection) -> sqlx::Result<Option<(i64, i64)>>` —
    `(done, total)` пока задача есть.
  - `pub fn spawn(state: AppState)` — цикл раз в 5 с: пока `step(…, 100)` возвращает `true`,
    шаги идут подряд; ошибки — в лог.

- [ ] **Step 1: Тесты** (`#[sqlx::test]`): `rebuild_replays_history` (без метода: два прихода,
  отгрузка, списание; выбор FIFO, `start`, `step` до конца → партии и движения как при живом
  методе — сравнить с тем же сценарием `fifo_ship_takes_oldest`); `rebuild_resumes_after_restart`
  (три товара, `step(…, 1)` дважды, затем `step` до конца → у каждого товара партии ровно по
  разу, `done = total = 3`); `start_skips_when_batches_exist`; `progress_reports`.
- [ ] **Step 2:** `cargo test stock::rebuild` — FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Склады: перестройка партий по истории».

### Task 8: Себестоимость, маржа, стоимость остатков

**Files:**
- Create: `src/stock/cost.rs`
- Modify: `src/stock/mod.rs` (`pub mod cost;`)

**Interfaces:**
- Consumes: Task 5–6.
- Produces:
  - `#[derive(Debug, Clone, PartialEq, Serialize)] pub enum CostKind { Actual, Estimate,
    ByPurchasePrice }`.
  - `#[derive(Debug, Clone, Serialize)] pub struct LineCost { pub cart_item_id: i64, pub
    unit_cost: Option<f64>, pub kind: CostKind }` (`None` — ни партий, ни закупочной цены).
  - `pub async fn order_costs(db: &mut PgConnection, order_id: i64) -> sqlx::Result<Vec<LineCost>>`:
    - основная отгрузка отгружена и метод выбран → `Actual` по формуле спеки §2 (движения
      `shipment_store_id` позиции + непокрытое × `purchasing_price`);
    - метод выбран, не отгружено → `Estimate`: склад позиции (`shipment_item_stores`, иначе
      `cart_items.store_id`), FIFO — `write_off` по партиям склада без записи, средняя — цена
      партии; непокрытое × закупочная;
    - метод не выбран → `ByPurchasePrice`.
    - всё — в валюте заказа.
  - `pub struct Margin { pub sum: f64, pub percent: Option<f64> }`;
    `pub fn margin(lines: &[(f64 /*qty*/, f64 /*price*/, Option<f64> /*unit_cost*/)]) ->
    Option<Margin>` — `None`, если ни у одной позиции нет себестоимости; позиции без
    себестоимости не входят; процент — от выручки позиций с себестоимостью; доставка не входит.
  - `pub async fn store_values(db, item_ids: &[i64]) -> sqlx::Result<HashMap<(i64, i64),
    (f64 /*value*/, f64 /*covered*/)>>` — Σ `available × price` в валюте сайта и Σ `available`.

- [ ] **Step 1: Тесты:** `#[test] margin_example` (2 × 200 при себестоимости 125 и 1 × 50 без
  себестоимости → сумма 150, процент 37.5); `margin_none_without_costs`. `#[sqlx::test]`:
  `actual_cost_fifo` (отгрузка 12 из 10@100 + 10@150 → 108.33 за единицу);
  `actual_cost_with_uncovered` (партии 5@100, отгрузка 8, закупочная 120 → (500+360)/8 = 107.5);
  `estimate_before_ship`; `by_purchase_price_without_method`; `cost_in_order_currency`
  (партия в USD); `store_values_sum`.
- [ ] **Step 2:** `cargo test stock::cost` — FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Склады: себестоимость и маржа».

### Task 9: Админка — карточка товара, документ склада, настройки, палитра

**Files:**
- Modify: `src/admin/items.rs`, `templates/admin/item_form.html` (блок «Торговый каталог»)
- Modify: `src/admin/shop/store_docs.rs`, `templates/admin/shop/store_doc_form.html`
- Modify: `src/admin/shop/settings.rs`, `templates/admin/shop/settings.html`
- Modify: `src/admin/palette.rs`
- Modify: `static/admin/admin.js` (сканирование, счётчики штрихкодов; уведомления — существующие тосты)

**Interfaces:**
- Consumes: Task 2, 3, 6, 7; `access::PURCHASE_INFO`.
- Produces:
  - Форма товара: `barcode_multi` (чекбокс), `barcodes` (textarea, по одному в строке) →
    `barcodes::set_multi`, `set_common` в той же транзакции, что и вкладка каталога; ошибки —
    в общий список ошибок формы. У поштучного — таблица «склад → число кодов» и ссылка на
    `/admin/shop/barcodes?item=<id>` (список `barcodes::list`, новый маршрут, `shop.manage`).
  - Закупочная цена в карточке показывается и сохраняется только с `PURCHASE_INFO`; без права
    поле не выводится и не перезаписывается.
  - Документ: параллельный массив `line_barcodes[]` (textarea, коды через перевод строки) →
    `Line.barcodes`; кнопка «Штрихкоды N/M» раскрывает textarea, счётчик красный у поштучного
    при N ≠ M; поле «Сканировать штрихкод» шлёт `GET /admin/shop/store-docs/scan?code=` →
    JSON `{item_id, name, multi}` или 404; JS добавляет строку / +1 / код, 404 — красное
    уведомление «Штрихкод не найден». У проведённого — только чтение.
  - Настройки: `<select name="cost_method">` «не выбран / FIFO / Средняя», при выбранном методе —
    текст метода без поля; `confirm` не используем — подтверждение отдельным шагом формы
    (второй POST с `confirm=1`, на первом — предупреждение «Метод выбирается один раз, сменить
    его потом будет нельзя» и кнопка «Выбрать»). Успех — `set_method`, затем `rebuild::start`.
    Статус «Партии перестраиваются: N из M» из `rebuild::progress`.
  - Палитра: при `looks_like_barcode(q)` и найденном `find_item` товар ставится первым в группе
    «Записи».

- [ ] **Step 1: Тесты** (`#[sqlx::test]`, HTTP через `test_support`): `item_saves_barcodes`;
  `item_barcode_conflict_shows_error`; `item_purchase_price_hidden_without_right`
  (`permitted_cookie` с `collections.manage` без `catalog.purchase_info` → в HTML нет
  `purchasing_price`, сохранение не стирает цену); `doc_form_saves_line_barcodes`;
  `doc_scan_finds_item` (200 с JSON и 404); `settings_choose_method_needs_confirm`;
  `settings_method_readonly_after_choice` (повторный POST с другим методом → ошибка, опция не
  изменилась); `palette_barcode_first`.
- [ ] **Step 2:** `cargo test admin::` — новые FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Админка: штрихкоды товара и документа, метод себестоимости».

### Task 10: Админка — заказ и остатки

**Files:**
- Modify: `src/admin/shop/orders.rs`, `templates/admin/shop/order/_shipment.html`,
  `templates/admin/shop/order/_items.html`, `templates/admin/shop/order/_aside.html`
- Modify: `src/admin/shop/rests.rs`, `templates/admin/shop/rests.html`
- Create: `templates/admin/shop/batches.html`; маршрут `/admin/shop/rests/{item_id}/batches`
  в `src/admin/mod.rs`

**Interfaces:**
- Consumes: Task 4, 8.
- Produces:
  - Вкладка «Отгрузка»: у поштучных позиций поле `barcodes_<позиция>_<склад>` (коды через
    перевод строки) со счётчиком «N/M» и полем сканирования; `save_shipment` вызывает
    `barcodes::choose_barcodes` до `set_deducted`.
  - Вкладка «Состав» с `PURCHASE_INFO`: колонки «Себестоимость» и «Маржа» из
    `cost::order_costs`; подпись «оценка» для `Estimate`, «по закупочной цене» для
    `ByPurchasePrice`. Боковая колонка: «Маржа заказа: <сумма> (<процент>%)» из `cost::margin`.
    Без права — ничего из этого.
  - «Остатки» с `PURCHASE_INFO` и выбранным методом: колонка «Стоимость» (`store_values`) и
    итог по фильтру; ссылка на партии. Если `covered < amount` — «партии покрывают N из M».
  - Страница партий: таблица «Склад · Дата · Документ-источник · Было · Осталось · Цена», под
    каждой партией — её движения со ссылками на документ (`/admin/shop/store-docs/{id}`) и заказ
    (`/admin/shop/orders/{id}`). «Было» — сумма плюсовых движений партии. Требует
    `PURCHASE_INFO`, иначе 403.

- [ ] **Step 1: Тесты** (`#[sqlx::test]`): `shipment_saves_barcodes_and_ships`;
  `shipment_barcode_error_shown`; `order_items_show_cost_and_margin`;
  `order_cost_hidden_without_right`; `rests_value_column`; `rests_covered_note`;
  `batches_page_lists_moves`; `batches_page_forbidden_without_right`.
- [ ] **Step 2:** `cargo test admin::shop` — новые FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Админка: штрихкоды отгрузки, себестоимость и партии».

### Task 11: Перенос из Битрикса

**Files:**
- Modify: `src/import.rs` (`Data`, чтение, запись, тесты)

**Interfaces:**
- Consumes: таблицы Task 1.
- Produces: в `Data` поля `barcodes: Vec<BarcodeRow>`, `doc_barcodes: Vec<(i64 /*DOC_ELEMENT_ID*/,
  String)>`, `batches: Vec<BatchRow>`, `batch_moves: Vec<BatchMoveRow>`; `catalog` получает
  `barcode_multi` у товара; опция `catalog.cost_price_calculation_method` идёт общим путём
  `put_option`. Чтение — `read_store_docs` (таблица может отсутствовать — проверка
  `mysql_table_exists`, как у поставщиков). Запись — новая `write_barcodes_batches(tx, data)`
  после `write_store_docs`, возвращает предупреждения.

Правила (спека §4):
- `b_catalog_store_barcode`: `PRODUCT_ID`, `BARCODE`, `STORE_ID` (0 → NULL); `ORDER_ID` не
  переносится; несуществующий товар или склад — пропуск с предупреждением; дубль кода — первый
  по `ID`, остальные с предупреждением.
- `b_catalog_docs_barcode`: только к перенесённым строкам документов (`DOC_ELEMENT_ID`).
- `b_catalog_store_batch` с сохранением `ID`; `b_catalog_store_batch_docs_element`: строки с
  `DOCUMENT_ELEMENT_ID` → `doc_item_id`; с `SHIPMENT_ITEM_STORE_ID` — пропуск с одним
  предупреждением с количеством; `AMOUNT`, `BATCH_PRICE`, `BATCH_CURRENCY` как есть.
- Повторный перенос: `DELETE` штрихкодов, партий (каскад движений) перед вставкой; последовательности
  `setval` после вставки с явными id.
- Метод выбран и партий нет → `rebuild::start` после записи.

- [ ] **Step 1: Тесты** (`#[sqlx::test]`, на собранном вручную `Data`, как существующие тесты
  переноса): `import_barcodes_and_multi`; `import_barcode_duplicate_warns`;
  `import_doc_barcodes_linked`; `import_batches_skip_shipment_moves`;
  `import_method_starts_rebuild_without_batches`; `import_twice_replaces`.
- [ ] **Step 2:** `cargo test import::` — новые FAIL.
- [ ] **Step 3:** Реализация.
- [ ] **Step 4:** PASS; `cargo test` целиком — PASS.
- [ ] **Step 5:** Коммит «Перенос из Битрикса: штрихкоды и партии».

### Task 12: README и проверки по критериям

**Files:**
- Modify: `README.md` (раздел «Склады»: штрихкоды, поштучный учёт, метод себестоимости и что
  он выбирается один раз, право `catalog.purchase_info`, перестройка партий)

- [ ] **Step 1:** README.
- [ ] **Step 2: Регрессия.** Собрать `feature/admin-shell` и эту ветку, на копии dev-базы
  (метод не выбран, поштучных нет) снять ответы bxapi и `/api/v1` (каталог, поиск, корзина,
  оформление — тот же набор, что в 5а) → байт в байт.
- [ ] **Step 3: Сквозная проверка** на копии с переносом трансопта по списку «Готово, когда»
  спеки (шаги 1–6), в браузере.
- [ ] **Step 4:** Копии баз и временные пользователи удалены; `cargo test`, clippy, fmt — PASS.
- [ ] **Step 5:** Коммит «README: штрихкоды и партии».
