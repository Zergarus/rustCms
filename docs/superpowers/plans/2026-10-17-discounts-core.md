# Скидки: ядро — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** правила работы с корзиной как в Битриксе: скидки и наценки на товары и доставку по дереву условий, приоритеты, купоны; цены со скидкой в корзине, итогах, заказе, каталоге, письмах, истории, админке и выгрузке в 1С; перенос из Битрикса.

**Architecture:** новый модуль `src/discount/`: `model` (правило, купон, дерево; проверка дерева), `cond` (проверка условий), `engine` (чистый `calculate`), `repo` (загрузка правил, купоны покупателя, запись в заказ), `context` (сборка контекста из корзины/заказа). Корзина и оформление зовут движок; админка — `src/admin/discounts.rs`; перенос — `src/import.rs` + разбор PHP-serialize `src/import/php.rs` (или функция в `import.rs`, по стилю файла).

**Tech Stack:** Rust 2024, Axum 0.8, SQLx 0.9 (PostgreSQL 17), MiniJinja, serde_json.

**Spec:** `docs/superpowers/specs/2026-10-17-discounts-core-design.md`

**Эталон поведения** (только чтение): `sites/transopt.local/www/bitrix/modules/sale/` — `lib/discountbase.php` (порядок, приоритеты, `LAST_DISCOUNT`/`LAST_LEVEL_DISCOUNT`, купоны), `lib/discount/actions.php` (применение процента/суммы/распределения, `Max`, округление), `general/sale_cond.php`, `general/sale_act.php` (классы и `DATA`), `lib/discountcouponsmanagerbase.php` + `lang/ru/lib/discountcouponsmanagerbase.php` (статусы и тексты купонов); `sites/transopt.local/www/bitrix/modules/catalog/general/catalog_cond.php` (условия товара: `CondIB*`, `CondCat*`).

## Global Constraints

- Формат дерева — как в Битриксе: `{ "CLASS_ID": String, "DATA": Object, "CHILDREN": Array }`; ключи `DATA` и их значения — дословно Битрикса (`All` `AND`/`OR`, `True` `True`/`False`, `logic` `Equal`/`Not`/`Great`/`Less`/`EqGr`/`EqLs`/`Contain`/`NotCont`, `value`; действия — `Type` `Discount`/`Extra`/`Closeout`, `Unit` `Perc`/`CurEach`/`CurAll`, `Max`; доставка — `Type` `Discount`/`DiscountZero`/`Extra`, `Unit` `Perc`/`Cur`).
- Поддерживаемые классы — ровно таблица спецификации плюс `ActSaleBsktGrp`, `ActSaleSubGrp`, `ActSaleDelivery`; прочие — в `discounts.unsupported`, правило не применяется.
- Порядок: `priority` DESC, `sort` ASC, `id` ASC; каждое правило от текущей цены; `last_discount` — стоп всего; `last_level_discount` — стоп правил с меньшим `priority`; цена ≥ 0; `custom_price` не участвует; округление до копеек на каждом шаге.
- Без активных правил и без купонов ответы bxapi и `/api/v1` байт в байт как до скидок (поле `coupons` в корзине — только если у покупателя есть купоны; `price_discount` в `/api/v1` — только у товаров коллекций с каталогом, у которых есть применимое правило).
- Права: `discounts.manage` «Скидки».
- Купоны: `kind` `basket` | `order` | `multi` ↔ `TYPE` Битрикса 1 | 2 | 4.
- Тесты: `rtk proxy cargo test`; `cargo fmt`; `cargo clippy --all-targets` без предупреждений; то же без `projects/` (временно убрать `projects/transopt.rs` и вернуть).
- Коммиты на русском, без упоминаний Claude/ИИ. В локальный Битрикс — только чтение. Тестовые заказы dev-базы не удалять; сквозной прогон — на копии базы.

## Review Focus

1. Покупатель меняет состав корзины после ввода купона так, что условие правила купона перестаёт выполняться, или купон истекает между корзиной и оформлением. Ожидается: скидка исчезает из корзины и итогов, купон остаётся в корзине со статусом «не применён», оформление проходит по ценам без скидки (без ошибки), купон не помечается использованным. Тест `coupon_without_effect_is_not_consumed`, Задача 6.
2. Два покупателя одновременно оформляют заказ с одним одноразовым купоном (или многоразовым на последнее использование). Ожидается: скидку получает ровно один заказ, второй оформляется по ценам без купона с сообщением «купон уже использован» (как Битрикс). Тест `one_time_coupon_race`, Задача 6.
3. Цена позиции после скидок уходит в ноль или ниже (скидка 100 %+ или сумма больше цены), наценка на позицию с ценой 0. Ожидается: цена 0, не отрицательная; сумма скидки по позиции = базовая цена; заказ оформляется. Тест `price_never_negative`, Задача 4.
4. `CurAll` (фиксированная сумма на группу позиций) при копейках, не делящихся нацело. Ожидается: сумма скидок по позициям равна заданной до копейки, остаток округления — на последней позиции (как `Actions::applyToBasket` Битрикса). Тест `cur_all_distribution_is_exact`, Задача 4.
5. Товар удалён или деактивирован, а правило ссылается на него (`CondIBElement`, `CondIBProp` с вариантом). Ожидается: правило продолжает работать для остальных, не падает; админка показывает ссылку как недействительную. Тест `rule_with_dangling_reference`, Задача 3.

---

### Task 1: Данные, право, модель правил и проверка дерева

**Files:**
- Create: `migrations/0019_discounts.sql`, `src/discount/mod.rs`, `src/discount/model.rs`
- Modify: `src/main.rs` (`mod discount;`), `src/access.rs`
- Test: `src/discount/model.rs`

**Interfaces:**
- Produces: таблицы и колонки раздела «Данные» спецификации (`discounts`, `discount_groups`, `discount_coupons`, `buyer_coupons`, `cart_items.base_price`/`discount_price`, `shipments.base_price`, `order_discounts`); `access::DISCOUNTS_MANAGE = "discounts.manage"` («Скидки», «Правила работы с корзиной и купоны»).
- `discount::model`:

```rust
pub struct Node { pub class_id: String, pub data: serde_json::Map<String, Value>, pub children: Vec<Node> } // serde: CLASS_ID/DATA/CHILDREN
pub struct Rule { pub id: i64, pub name: String, pub active: bool, pub active_from: Option<DateTime<Utc>>, pub active_to: Option<DateTime<Utc>>,
    pub priority: i32, pub sort: i32, pub last_discount: bool, pub last_level_discount: bool, pub use_coupons: bool,
    pub conditions: Node, pub actions: Node, pub group_ids: Vec<i64>, pub unsupported: String, pub xml_id: String }
pub enum CouponKind { Basket, Order, Multi }  // "basket"/"order"/"multi"
pub struct Coupon { pub id: i64, pub discount_id: i64, pub coupon: String, pub active: bool, pub active_from: Option<DateTime<Utc>>,
    pub active_to: Option<DateTime<Utc>>, pub kind: CouponKind, pub max_use: i32, pub use_count: i32, pub user_id: Option<i64> }
pub const CONDITION_CLASSES: &[&str]; pub const ACTION_CLASSES: &[&str]; // CondIBProp:<коллекция>:<поле> — по префиксу
pub fn unsupported_classes(conditions: &Node, actions: &Node) -> Vec<String>; // уникальные, в порядке обхода
pub fn validate(node: &Node, refs: &Refs) -> Vec<(String /* путь "0.1.2" */, String /* ошибка */)>;
pub struct Refs { pub collections: HashSet<i64>, pub fields: HashMap<i64, (i64 /*коллекция*/, String /*kind*/)>, pub options: HashSet<i64>,
    pub items: HashSet<i64>, pub sections: HashSet<i64>, pub pay_systems: HashSet<i64>, pub deliveries: HashSet<i64>, pub person_types: HashSet<i64> }
```

- [ ] **Step 1: Тесты** — `node_roundtrip_bitrix_json` (JSON правила трансопта №24 из спецификации, переведённый в JSON, ↔ `Node`); `unsupported_lists_unknown_classes` (`CondCumulativeGroup`, `ActSaleGift` → в списке; `CondIBProp:4:55` — поддержан); `validate_reports_path_and_reason` (неизвестный элемент, поле не той коллекции, неверный `logic`, `Unit` не из списка); `rule_with_dangling_reference` — `validate` возвращает ошибку по ссылке на удалённую запись, но `Node` остаётся загружаемым.
- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Весь набор зелёный.**
- [ ] **Step 5: Коммит** — `git commit -m "Скидки: данные и модель правил"`

### Task 2: Перенос из Битрикса

**Files:**
- Modify: `src/import.rs` (чтение `b_sale_discount`, `b_sale_discount_group`, `b_sale_discount_coupon`, предупреждение `b_catalog_discount`; разбор PHP-serialize — отдельной функцией `php_unserialize(&str) -> Result<serde_json::Value, String>` рядом с прочими помощниками импорта)
- Test: `src/import.rs`

**Interfaces:**
- Consumes: Задача 1 (`Node`, `unsupported_classes`).
- Produces: импорт правил: `CONDITIONS`/`ACTIONS` → `Node` (PHP-массивы с ключами 0..n → массивы JSON; `d:` → число; `b:` → bool), замена ссылок: id коллекций, полей, вариантов, записей, разделов, платёжек, доставок, типов плательщика переносятся как есть (импорт сохраняет id); группы — по `groups.external_id = 'bitrix:<ID>'`; `CondIBProp:<IBLOCK_ID>:<PROPERTY_ID>` — те же id; `unsupported` заполнен; `discount_groups`; купоны (`TYPE` 1/2/4 → `kind`, `MAX_USE`, `USE_COUNT`, `USER_ID` по карте пользователей, `ACTIVE_FROM/TO`); `b_catalog_discount` — `warn` с id и названиями, без переноса.

- [ ] **Step 1: Тесты** — `php_unserialize_bitrix_rule` (строка `ACTIONS` правила №24 трансопта из спецификации → ожидаемый JSON, включая `d:10`, вложенные `a:`); `php_unserialize_rejects_garbage`; `import_carries_discounts` (правило с группой 16 → `discount_groups` с нашим id группы `bitrix:16`; купоны трёх типов; правило с `CondCumulativeGroup` → `unsupported`, `active` сохранён; `b_catalog_discount` с одной строкой → предупреждение, правил не прибавилось).
- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Весь набор зелёный.**
- [ ] **Step 5: Коммит** — `git commit -m "Скидки: перенос правил и купонов из Битрикса"`

### Task 3: Проверка условий

**Files:**
- Create: `src/discount/cond.rs`, `src/discount/context.rs`
- Modify: `src/bxapi/project/mod.rs` (`DiscountConditionHook`)
- Test: `src/discount/cond.rs`

**Interfaces:**
- Consumes: Задача 1.
- Produces:

```rust
// context.rs — данные для условий, без БД
pub struct CtxItem { pub key: i64 /* id позиции */, pub product_id: i64, pub parent_id: Option<i64>, pub collection_id: i64,
    pub section_ids: Vec<i64> /* разделы записи и всех их предков; у предложения — и родителя */, pub xml_id: String, pub name: String,
    pub code: String, pub fields: serde_json::Map<String, Value> /* field_values записи; у предложения — объединение с родителем, как Битрикс */,
    pub field_ids: HashMap<i64, String> /* id поля → код */, pub quantity: f64, pub base_price: f64, pub custom_price: bool,
    pub weight: f64, pub stock_quantity: f64 }
pub struct OrderCtx { pub person_type_id: Option<i64>, pub pay_system_id: Option<i64>, pub delivery_id: Option<i64>, pub delivery_price: Option<f64> }
pub struct Context { pub items: Vec<CtxItem>, pub user_id: Option<i64>, pub group_ids: Vec<i64>, pub coupons: Vec<Coupon>,
    pub order: OrderCtx, pub now: DateTime<Utc> /* день недели — по МСК, как bxapi */ }
// cond.rs
pub struct State<'a> { pub ctx: &'a Context, pub prices: &'a HashMap<i64, f64> /* текущие цены */, pub applied: &'a HashMap<i64, Vec<i64>> /* позиция → правила */ }
pub fn check_order(node: &Node, st: &State, hook: Option<&dyn DiscountConditionHook>) -> bool;          // conditions правила
pub fn check_item(node: &Node, item: &CtxItem, st: &State, hook: Option<&dyn DiscountConditionHook>) -> bool; // фильтр позиций в действиях и подгруппах
pub trait DiscountConditionHook: Send + Sync { fn class_ids(&self) -> &[&'static str];
    fn check(&self, class_id: &str, data: &serde_json::Map<String, Value>, ctx: &Context, item: Option<&CtxItem>) -> Option<bool>; }
// Project::discount_condition_hook: Option<Arc<dyn DiscountConditionHook>>, по умолчанию None
```

- Семантика каждого класса и сравнения (`logic`, строки — без учёта регистра для `Contain`, списки значений — «любое из») — по `sale_cond.php` / `catalog_cond.php`; пустая группа — `True`; `True=False` инвертирует результат группы.

- [ ] **Step 1: Тесты** — по тесту на каждый класс таблицы спецификации (истина и ложь), `and_or_false_groups`, `basket_subgroups_count_amount_base_amount`, `offer_matches_parent_section_and_props`, `applied_discount_condition`, `hook_class_is_checked`, `rule_with_dangling_reference` (условие на несуществующую запись — просто ложь), `transopt_property_condition` (`CondIBProp:4:55` `Equal` 32 на `fields` с вариантом 32 и без).
- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Тесты зелёные.**
- [ ] **Step 5: Коммит** — `git commit -m "Скидки: проверка условий"`

### Task 4: Движок применения

**Files:**
- Create: `src/discount/engine.rs`
- Test: `src/discount/engine.rs`

**Interfaces:**
- Consumes: Задачи 1, 3.
- Produces:

```rust
pub struct ItemResult { pub price: f64, pub discount: f64 /* base_price - price, на единицу */, pub rules: Vec<i64> }
pub struct Applied { pub discount_id: i64, pub name: String, pub coupon: Option<String>, pub items: Vec<(i64, f64)> /* позиция → сумма по позиции */,
    pub delivery: f64, pub sum: f64 }
pub struct DiscountResult { pub items: HashMap<i64, ItemResult>, pub delivery_price: Option<f64>, pub applied: Vec<Applied>,
    pub coupons: Vec<(String, CouponStatus)> }
pub enum CouponStatus { Applied, NotApplied /* условия не выполнены */, NotFound, Inactive, Expired, Used, Foreign }
pub fn calculate(rules: &[Rule], ctx: &Context, hook: Option<&dyn DiscountConditionHook>) -> DiscountResult;
pub fn product_rules(rules: &[Rule]) -> Vec<&Rule>; // для цены в каталоге: условия без CondBskt*/CondSale*, без купонов
```

- Порядок и правила — Global Constraints; `CurAll` — пропорционально стоимости позиций, остаток копеек — на последней позиции; `Max` — ограничение суммы скидки правила; `Closeout` — установить цену (не выше текущей для `Discount`-смысла — как Битрикс); `DiscountZero` — доставка бесплатно.

- [ ] **Step 1: Тесты** — `percent_discount_and_extra`, `cur_each_and_closeout`, `cur_all_distribution_is_exact` (300 ₽ на позиции 100 / 100 / 100.01 → сумма скидок ровно 300.00), `max_limits_percent`, `delivery_actions`, `priority_and_sort_order`, `last_discount_stops_all`, `last_level_discount_stops_lower_priority`, `coupon_rule_needs_active_coupon` (статусы `Applied`/`NotApplied`/`Expired`/`Used`/`Foreign`), `groups_and_dates_filter`, `custom_price_untouched`, `price_never_negative`, `sequential_application_uses_current_price`, `transopt_rules` (правила №24–27 из спецификации: пользователь группы «+10%» на товар со свойством 55 = 32 → +10 %, на прочий — без изменений; группы «-5%» → −5 %; без групп — без изменений), `product_rules_excludes_basket_conditions`.
- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Тесты зелёные.**
- [ ] **Step 5: Коммит** — `git commit -m "Скидки: движок применения"`

### Task 5: Корзина, итоги формы, купоны в bxapi

**Files:**
- Create: `src/discount/repo.rs` (загрузка правил и купонов, купоны покупателя), сборка `Context` из корзины в `src/discount/context.rs`
- Modify: `src/bxapi/cart.rs` (`cart_state_for` → расчёт скидок, снимок с ценами со скидкой, `coupons`), `src/cart/snapshot.rs` (цена позиции из результата скидок), `src/bxapi/order.rs` (`summary`), `src/sale/form.rs` (`summary` со строкой «Скидка»), роуты bxapi `basket/coupon/add`, `basket/coupon/remove` (рядом с остальными роутами корзины)
- Test: `src/bxapi/cart.rs`, `src/sale/form.rs`, `src/discount/repo.rs`

**Interfaces:**
- Consumes: Задачи 1, 3, 4.
- Produces:
  - `repo::active_rules(db: &PgPool, now: DateTime<Utc>) -> sqlx::Result<Vec<Rule>>`; `repo::buyer_coupons(db, buyer_id) -> sqlx::Result<Vec<Coupon>>`; `repo::add_buyer_coupon(db, buyer_id, code, user_id) -> Result<Coupon, CouponStatus>`; `repo::remove_buyer_coupon(db, buyer_id, code) -> sqlx::Result<bool>`.
  - `context::for_cart(db: &PgPool, items: &[CartItem], info: &HashMap<i64, PurchaseInfo>, user_id: Option<i64>, coupons: Vec<Coupon>, order: OrderCtx) -> sqlx::Result<Context>`.
  - `CartState.discounts: DiscountResult`; снимок: `price` позиции — цена со скидкой, `sum`/`totalPrice` — по ней (как `getPrice`/`getFinalPrice` Битрикса), `coupons: [{coupon, status, text}]` — только при наличии купонов; позиции корзины получают `base_price`/`discount_price`/`price` при пересчёте.
  - `form::summary(goods_base: f64, discount: f64, delivery: f64, payment_name: Option<&str>) -> Vec<Value>` — «Стоимость товаров» (базовая), «Скидка» `-discount` (только > 0), «Доставка» (со скидкой доставки), «Итого».
  - Тексты статусов купонов — из `lang/ru/lib/discountcouponsmanagerbase.php` Битрикса.

- [ ] **Step 1: Тесты** — `cart_prices_with_rule` (правило −10 % → `price`, `sum`, `totalPrice`, хеш снимка изменился), `cart_without_rules_is_byte_identical` (снимок без правил и купонов равен снимку до изменений: сравнить JSON с эталоном, записанным из текущего кода до правки), `summary_has_discount_line`, `summary_without_discount_unchanged`, `coupon_add_remove_via_bxapi` (верный, неизвестный, истёкший, чужой, неактивное правило — статус и текст; повторное добавление — без дубля), `coupon_rule_applies_only_with_coupon`, `delivery_discount_in_summary`.
- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Весь набор зелёный.**
- [ ] **Step 5: Коммит** — `git commit -m "Скидки: корзина, итоги и купоны"`

### Task 6: Оформление и заказ

**Files:**
- Modify: `src/bxapi/order.rs` (`submit`), `src/sale/repo.rs` (`NewOrder` с базовой ценой и скидкой позиций, доставки; запись `order_discounts`; `recalculate_discounts`), `src/sale/mail.rs` (суммы скидки), `src/bxapi/profile_orders.rs` (`basePrice` — `cart_items.base_price`), `src/admin/shop/orders.rs` + `templates/admin/shop/order.html` (блок «Скидки», кнопка «Пересчитать скидки»)
- Test: `src/bxapi/order.rs`, `src/sale/repo.rs`, `src/admin/shop/orders.rs`

**Interfaces:**
- Consumes: Задачи 4, 5 (`CartState.discounts`, `repo::*`).
- Produces: при оформлении — пересчёт скидок в транзакции заказа с блокировкой купонов (`SELECT … FOR UPDATE` строк `discount_coupons`), позиции получают `base_price`/`discount_price`/`price`, отгрузка — `base_price`/`price`; `order_discounts` по `DiscountResult.applied`; купоны: `basket`/`order` → `active = FALSE`, `use_count += 1`; `multi` → `use_count += 1` (при `max_use > 0` и `use_count >= max_use` — `active = FALSE`); `buyer_coupons` покупателя очищаются; купон, ставший недоступным к моменту оформления, — заказ по ценам без него и сообщение в `messages` ответа (текст Битрикса «использован»/«не активен»).
  - `sale::repo::recalculate_discounts(db: &PgPool, order_id: i64) -> anyhow::Result<()>` — текущие правила к позициям заказа (купоны заказа из `order_discounts`), пересчёт сумм заказа, оплаты (если одна и не оплачена) и `order_discounts`; оплаченный заказ — ошибка «заказ оплачен».
  - Письма: поля `ORDER_DISCOUNT`/`DISCOUNT_SUM` — как в `sale/mail.rs` для прочих сумм (имя поля — как у шаблонов Битрикса `SALE_NEW_ORDER`; если в Битриксе такого поля нет — сумма в строке состава).

- [ ] **Step 1: Тесты** — `order_fixes_discounted_prices` (позиции, отгрузка, `order_discounts`, суммы заказа и оплаты), `coupon_consumed_on_order` (по типам), `coupon_without_effect_is_not_consumed`, `one_time_coupon_race` (две транзакции оформления с одним купоном: одна получает скидку, вторая — без скидки с сообщением), `cancel_does_not_return_coupon`, `profile_history_base_price`, `recalculate_discounts_admin` (новое правило → пересчёт; оплаченный — ошибка), `mail_has_discount`.
- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Весь набор зелёный.**
- [ ] **Step 5: Коммит** — `git commit -m "Скидки: оформление и заказ"`

### Task 7: Цена со скидкой в каталоге и выгрузка в 1С

**Files:**
- Modify: `src/api.rs` (`price_discount`), `src/bxapi/project/mod.rs` (флаг `catalog_price_discount: bool`, по умолчанию `false`), сериализация цен bxapi каталога (`src/bxapi/serialize.rs` — там, где выводятся цены, только при флаге), `src/exchange/sale/select.rs` + `src/exchange/sale/document.rs` (`Скидки` позиции и заказа)
- Test: `src/api.rs`, `src/bxapi/serialize.rs`, `src/exchange/sale/document.rs`, `src/exchange/sale/select.rs`

**Interfaces:**
- Consumes: Задача 4 (`product_rules`, `calculate`), Задача 6 (`cart_items.base_price`/`discount_price`).
- Produces: `discount::catalog_price(rules: &[&Rule], item: &CtxItem, group_ids: &[i64], now) -> Option<f64>` — цена одной единицы после `product_rules` (None — скидки нет); `/api/v1` — `price_discount` у записей коллекций с каталогом, когда скидка есть (гость — группы гостя); bxapi — та же цена в выводе цен каталога только при `Project::catalog_price_discount`; `ExportItem.discount: f64` (сумма скидки позиции) и `ExportOrder.discount: f64` → `<Скидки><Скидка><Наименование>Скидка на товар</Наименование><Сумма>…</Сумма><УчтеноВСумме>true</УчтеноВСумме></Скидка></Скидки>` у позиции (`getXmlBasketItems`) и скидка на заказ (`getXmlOrderDiscount`) — элементы и порядок по `general/export.php`.

- [ ] **Step 1: Тесты** — `api_price_discount_only_with_rule`, `api_without_rules_unchanged`, `bxapi_catalog_price_discount_behind_flag`, `export_item_discount_element` (эталон-фикстура `tests/fixtures/1c/sale/export_discount.xml`), `select_fills_item_discount`.
- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Весь набор зелёный.**
- [ ] **Step 5: Коммит** — `git commit -m "Скидки: цена в каталоге и выгрузка в 1С"`

### Task 8: Админка правил и купонов

**Files:**
- Create: `src/admin/discounts.rs`, `templates/admin/discounts.html`, `templates/admin/discount_form.html`, `templates/admin/discount_coupons.html`, `static/admin/discount-tree.js` (или в каталоге статики админки, по месту существующих скриптов)
- Modify: `src/admin/mod.rs` (маршруты, меню), `templates/admin/base.html` (пункт «Скидки»)
- Test: `src/admin/discounts.rs`

**Interfaces:**
- Consumes: Задачи 1, 4 (`validate`, `Refs`, `unsupported_classes`).
- Produces: `/admin/discounts` (список, переключение активности POST `/admin/discounts/{id}/active`), `/admin/discounts/new`, `/admin/discounts/{id}` (форма: поля, группы, дерево `conditions`/`actions` скрытыми JSON-полями, которые заполняет `discount-tree.js`; сервер — `serde` → `Node`, `validate`; ошибки — по пути узла, введённое сохраняется), `/admin/discounts/{id}/coupons` (список, добавить, сгенерировать: количество 1–10000, префикс, тип, лимит, срок; коды — 12 символов `A-Z0-9` после префикса, уникальные; выключить; удалить только при `use_count = 0`); всё под `discounts.manage`. Конструктор: список классов с подписями из `lang/ru` Битрикса (`sale_cond.php`/`catalog_cond.php`), выбор коллекции/поля/записи/раздела — выпадающие списки или поле id с подсказкой названия.

- [ ] **Step 1: Тесты** — права (без права — 403), создание и правка правила с деревом, ошибка в узле сохраняет введённое, `unsupported` показывается предупреждением, генерация купонов (число, префикс, уникальность), удаление использованного купона запрещено, переключение активности, экранирование названий (`<script>` в имени правила).
- [ ] **Step 2: Запустить — падают.**
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Весь набор зелёный.**
- [ ] **Step 5: Коммит** — `git commit -m "Админка скидок и купонов"`

### Task 9: Документация и сквозная проверка

**Files:**
- Modify: `README.md` (раздел «Скидки»: правила, классы, порядок, купоны, каталог, 1С, перенос, модуль проекта)
- Scratchpad (не в репозиторий): скрипты проверки

- [ ] **Step 1: README.**
- [ ] **Step 2: Копия dev-базы** (`pg_dump | psql` в новую базу), миграция, сервер на свободном порту с отдельным `UPLOAD_DIR`; перенос правил трансопта из локального Битрикса — только чтение (`import-bitrix` в копию с теми же параметрами, что для основной базы; при необходимости — только скидки).
- [ ] **Step 3: Проверка**: пользователь группы «+10%» — корзина с товаром со свойством 55 = 32 и с прочим товаром: цены и итоги формы; пользователь без групп — без изменений; правило с купоном (создать в копии) — купон через bxapi, итоги, оформление, заказ в истории (`basePrice`), XML заказа для 1С со `Скидками`; повторное использование одноразового купона — отказ.
- [ ] **Step 4: Регрессия на основной базе** (правила не переносятся): ответы bxapi и `/api/v1` байт в байт со сборкой до скидок; затем перезапуск сервера :3000 на новой сборке после `cms migrate`. Тестовые заказы не удалять.
- [ ] **Step 5: Уборка** — копия базы, сервер, каталоги.
- [ ] **Step 6: Коммит** — `git commit -m "Скидки: документация"`
