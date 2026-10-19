# Скидки 3б: накопительные, подарки, округление — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** накопительные скидки, подарки и правила округления цен с паритетом Битрикса — в
движке скидок, корзине, оформлении, каталоге, админке, переносе и 1С.

**Architecture:** движок `discount::calculate` остаётся чистой функцией: новые данные
(накопленные суммы, правила округления, тип цены позиции) кладутся в `Context` функцией
`context::load_extras` перед расчётом во всех четырёх местах вызова. Округление — отдельный
модуль `catalog::rounding` (чистый алгоритм + загрузка), используемый движком и выводом каталога.
Подарки — новое действие движка и список доступных подарков в `DiscountResult`.

**Tech Stack:** Rust 2024, Axum 0.8, SQLx 0.9 (`#[sqlx::test]`), PostgreSQL 17, MiniJinja, JS без
библиотек в админке.

**Spec:** `docs/superpowers/specs/2026-10-19-discounts-cumulative-gifts-rounding-design.md`
(строится на `docs/superpowers/specs/2026-10-17-discounts-core-design.md`).

## Global Constraints

- Миграция `0021_discounts_extra.sql`; `0019`/`0020` уже применены — не менять.
- Без накопительных правил, правил с подарками и правил округления ответы bxapi и `/api/v1`
  байт в байт как до 3б (существующий `cart_without_rules_is_byte_identical` и тесты `/api/v1`
  не меняются и проходят).
- Деньги — `round2` ядра (`src/discount/model.rs`, PHP-совместимое) на каждом шаге.
- Константы Битрикса как есть: `CondCumulativeGroup`, `Period`, `PeriodRelative`
  (`ValueStart`, `ValueEnd`, `Value`, `TypeRelativePeriod` `D`/`M`/`Y`), `CumulativeAction`
  (`ranges[{sum,value,type}]`, `type` `P`/`F`, `type_sum_period` `all_time`/`interval`/
  `relative`, `sum_period_data.discount_sum_order_start`/`discount_sum_order_end`/
  `discount_sum_period_value`/`discount_sum_period_type`, `apply_if_more_profitable` `Y`/`N`,
  `All`, `True`), `GiftCondGroup`, `GifterCondIBElement`, `GifterCondIBSection` (`Type`
  `one`/`all`, `Value`).
- Округление: `VALUE_EPS = 1e-6`, типы Битрикса `ROUND_MATH = 1`, `ROUND_UP = 2`,
  `ROUND_DOWN = 4`; у нас `math`/`up`/`down`. Пресеты точности: 0.0001, 0.001, 0.005, 0.01,
  0.02, 0.05, 0.1, 0.2, 0.5, 1, 2, 5, 10, 20, 50, 100, 200, 500, 1000, 5000.
- Тексты интерфейса и комментарии — по-русски, как в окружающем коде.
- Сообщения коммитов — по-русски, без упоминаний ИИ/Claude и строк `Co-Authored-By`.
- В локальный Битрикс запросы с побочными эффектами не отправляются; реальные пакеты 1С и
  `docs/benchmark-bitrix-vs-rust.md` не коммитятся.
- Проверка после задачи: `cargo test` (вся сборка, `DATABASE_URL` из `.env`), `cargo clippy
  --all-targets -- -D warnings`, `cargo fmt --check`.

## Review Focus

1. Покупатель с оплаченным, а затем отменённым заказом — сумма отменённого не учитывается
   (тест в Task 4 `cumulative_sums_skip_canceled_and_unpaid`).
2. Подарок количеством 2 в корзине — не бесплатный, и в списке `gifts` товар остаётся
   доступным (тест в Task 5 `gift_requires_quantity_one`).
3. Правило округления вверх без скидки — цена выше базовой, `discount_price` отрицательный,
   а «Скидка» в итогах формы не появляется и не уходит в минус (тест в Task 2
   `rounding_up_without_discount_has_no_discount_line`).
4. Предложение, у которого подарком указан родитель — бесплатное; в `gifts` после этого правило
   «один из» исчезает (тест в Task 5 `gift_by_parent_of_offer`).
5. Накопительная скидка «только если выгоднее» при уже большей скидке позиции — позиция не
   меняется, правило в записях заказа без этой позиции (тест в Task 3
   `cumulative_more_profitable_keeps_better_discount`).

---

### Task 1: Правила округления — алгоритм и хранение

**Files:**
- Create: `migrations/0021_discounts_extra.sql`
- Create: `src/catalog/rounding.rs`
- Modify: `src/catalog/mod.rs` (`pub mod rounding;`)

**Interfaces:**
- Produces:
  - `pub enum RoundType { Math, Up, Down }` — `Debug, Clone, Copy, PartialEq, Eq`;
    `fn as_str(self) -> &'static str` (`math`/`up`/`down`), `fn parse(&str) -> Option<Self>`,
    `fn from_bitrix(i64) -> Option<Self>` (1/2/4).
  - `pub struct RoundRule { pub price_type_id: i64, pub price: f64, pub round_type: RoundType, pub precision: f64 }` — `Debug, Clone, PartialEq`.
  - `pub fn round_value(value: f64, precision: f64, t: RoundType) -> f64` (`roundValue`).
  - `pub struct Rounding` (`Debug, Clone, Default, PartialEq`): `pub fn new(rules: Vec<RoundRule>) -> Self`
    (сортирует по типу, порог по убыванию), `pub fn rule(&self, price_type_id: i64, price: f64) -> Option<&RoundRule>`
    (`searchRoundRule`: первое с `порог < цена`), `pub fn round(&self, price_type_id: Option<i64>, price: f64) -> f64`
    (цена ≤ 0, тип `None` или нет правила — как есть; иначе `round2(round_value(...))`).
  - `pub async fn load(db: &mut PgConnection) -> sqlx::Result<Option<Rounding>>` — `None` при пустой таблице.
  - `pub const PRESET_PRECISIONS: &[f64]` — пресеты из Global Constraints.

- [ ] **Step 1: Миграция.** `catalog_rounding (id BIGSERIAL PK, price_type_id BIGINT NOT NULL REFERENCES catalog_price_types(id) ON DELETE CASCADE, price NUMERIC(18,2) NOT NULL DEFAULT 0, round_type TEXT NOT NULL CHECK (round_type IN ('math','up','down')), precision NUMERIC(18,4) NOT NULL CHECK (precision > 0), created_by BIGINT REFERENCES users(id) ON DELETE SET NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now(), updated_by BIGINT REFERENCES users(id) ON DELETE SET NULL, updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), UNIQUE (price_type_id, price))`;
  `CREATE INDEX orders_cumulative_idx ON orders (user_id, created_at) WHERE paid AND NOT canceled;`

- [ ] **Step 2: Падающие тесты** в `src/catalog/rounding.rs`:

```rust
#[test]
fn round_value_matches_bitrix() {
    use RoundType::*;
    // (значение, точность, math, up, down) — посчитано кодом Битрикса (PHP)
    let table = [
        (1234.56, 10.0, 1230.0, 1240.0, 1230.0),
        (1235.0, 10.0, 1240.0, 1240.0, 1230.0),
        (1234.99, 1.0, 1235.0, 1235.0, 1234.0),
        (99.5, 1.0, 100.0, 100.0, 99.0),
        (0.05, 0.1, 0.1, 0.1, 0.0),
        (1234.56, 0.5, 1234.5, 1235.0, 1234.5),
        (1234.25, 0.5, 1234.5, 1234.5, 1234.0),
        (1234.75, 0.5, 1235.0, 1235.0, 1234.5),
        (1999.99, 100.0, 2000.0, 2000.0, 1900.0),
        (1950.0, 100.0, 2000.0, 2000.0, 1900.0),
        (1949.99, 100.0, 1900.0, 2000.0, 1900.0),
        (10.005, 0.01, 10.01, 10.01, 10.0),
        (0.3, 0.1, 0.3, 0.3, 0.3),
        (4455.0, 50.0, 4450.0, 4500.0, 4450.0),
        (4475.0, 50.0, 4500.0, 4500.0, 4450.0),
        (123.456, 0.05, 123.45, 123.5, 123.45),
        (100.0, 10.0, 100.0, 100.0, 100.0),
        (7.2, 5.0, 5.0, 10.0, 5.0),
        (1234.56, 0.99, 1234.99, 1234.99, 1234.0),
    ];
    for (v, p, m, u, d) in table {
        assert_eq!(round2(round_value(v, p, Math)), m, "{v} {p} math");
        assert_eq!(round2(round_value(v, p, Up)), u, "{v} {p} up");
        assert_eq!(round2(round_value(v, p, Down)), d, "{v} {p} down");
    }
}

#[test]
fn rule_search_is_strictly_greater_than_threshold() {
    let r = Rounding::new(vec![
        RoundRule { price_type_id: 1, price: 0.0, round_type: RoundType::Math, precision: 10.0 },
        RoundRule { price_type_id: 1, price: 1000.0, round_type: RoundType::Up, precision: 100.0 },
    ]);
    assert_eq!(r.round(Some(1), 1001.0), 1100.0);
    assert_eq!(r.round(Some(1), 1000.0), 1000.0); // порог 0, math до 10
    assert_eq!(r.round(Some(1), 994.0), 990.0);
    assert_eq!(r.round(Some(1), 0.0), 0.0);
    assert_eq!(r.round(Some(2), 994.0), 994.0); // у типа 2 правил нет
    assert_eq!(r.round(None, 994.0), 994.0);
}
```

  и `#[sqlx::test] async fn load_none_when_empty_and_sorted(db)`: пустая таблица → `None`;
  после вставки двух правил типа 1 (пороги 0 и 1000) — `Some`, `rule(1, 1500.0)` — порог 1000.
  (`round2` — `crate::discount::model::round2`, сейчас `pub(super)` — сделать `pub(crate)`.)

- [ ] **Step 3: Запуск** `cargo test rounding` → FAIL (нет модуля/функций).
- [ ] **Step 4: Реализация** по `Catalog\Product\Price::roundValue`/`roundWhole`/`roundFraction`
  (точность ≤ 0 → 0; ≥ 1 → целая часть точности; |значение| ≤ EPS → 0) и `searchRoundRule`.
- [ ] **Step 5: Запуск** `cargo test rounding` → PASS; затем вся проверка (Global Constraints).
- [ ] **Step 6: Коммит** `Округление цен: правила и алгоритм Битрикса`.

### Task 2: Округление в корзине, оформлении, заказе и 1С

**Files:**
- Modify: `src/discount/context.rs` (поле `price_type_id`, `Context.rounding`, `Line.price_type_id`, `load_extras`)
- Modify: `src/discount/engine.rs` (округление в конце `calculate_refs`, `catalog_price_with`)
- Modify: `src/bxapi/cart.rs` (`priced`), `src/bxapi/order.rs` (расчёт при оформлении), `src/sale/repo.rs` (`recalculate_discounts_with`), `src/admin/shop/carts.rs`
- Test: те же файлы, `src/exchange/sale/select.rs`

**Interfaces:**
- Consumes: `catalog::rounding::{Rounding, load}` (Task 1).
- Produces:
  - `CtxItem.price_type_id: Option<i64>` — тип цены, по которой взята `base_price` (`main_price(..).type_id`; ручная цена и пересчёт удалённого товара — `None`).
  - `Context.rounding: Option<Rounding>`; `Line.price_type_id: Option<i64>`.
  - `pub async fn load_extras(db: &mut PgConnection, ctx: &mut Context, rules: &[Rule]) -> sqlx::Result<()>` — грузит округление (если у позиций есть тип цены); Task 4 добавит в неё накопленные суммы. Вызывается после `for_cart`/`for_lines` во всех четырёх местах.
  - `ItemResult.price` — округлённая итоговая цена; `ItemResult.discount = round2(base_price − price)`.

- [ ] **Step 1: Падающие тесты.**
  - `engine.rs`: `rounding_applies_to_final_price` — правило 10 % скидки, позиция 1234.56 тип 1, правило округления `up` 10 для типа 1 → `price == 1120.0`, `discount == 114.56`; сумма `applied[0].sum` — только скидка правила (`123.46` × количество), без округления. `rounding_skips_custom_price_and_zero` — позиция с `custom_price` 1234.56 и позиция с ценой 0 остаются как есть.
  - `bxapi/cart.rs`: `rounding_up_without_discount_has_no_discount_line` — без правил скидок, правило `up` 100 для типа 1, товар фикстуры 1000→ корзина: цена позиции 1000 (порог 0 < 1000, кратно 100 — без изменений), затем цена товара 1001 → в `basket` цена 1100, `cart_items.discount_price = -99`, в итогах формы строки «Скидка» нет, «Стоимость товаров» = 2002 (по базовой), «Итого» = 2200 + доставка.
  - `bxapi/order.rs`: `order_stores_rounded_prices` — правило `math` 10, цена 1004 → в заказе `cart_items.price = 1000`, `base_price = 1004`, `discount_price = 4`.
  - `sale/repo.rs`: `recalculate_rounds_prices` — после добавления правила округления «Пересчитать скидки» записывает округлённую цену.
  - `exchange/sale/select.rs`: `select_uses_rounded_price` — у заказа из `order_stores_rounded_prices` (или через `UPDATE` как в `select_fills_item_discount`) `ЦенаЗаЕдиницу` = 1000.
- [ ] **Step 2: Запуск** этих тестов → FAIL.
- [ ] **Step 3: Реализация.** В `calculate_refs` после цикла правил: для позиций без `custom_price` и с ценой > 0 — `ctx.rounding.round(item.price_type_id, price)`. В `catalog_price_with` — то же (каталог видит округлённую цену со скидкой; если после округления цена совпала с округлённой базовой — `None`). Условия по цене видят неокруглённые цены — округление только в конце.
- [ ] **Step 4: Запуск** → PASS; вся проверка; `cart_without_rules_is_byte_identical` проходит без изменений.
- [ ] **Step 5: Коммит** `Округление цен в корзине, заказе и 1С`.

### Task 3: Накопительные скидки — движок

**Files:**
- Create: `src/discount/cumulative.rs` (периоды)
- Modify: `src/discount/mod.rs`, `src/discount/model.rs` (классы, вложенность, проверка), `src/discount/cond.rs` (условие), `src/discount/engine.rs` (действие, `product_rules`), `src/discount/context.rs` (`Context.cumulative`)

**Interfaces:**
- Produces:
  - `pub enum CumulativePeriod { AllTime, Interval { start: Option<i64>, end: Option<i64> }, Relative { value: i64, unit: char } }` — `Debug, Clone, PartialEq, Eq, Hash`; unix-секунды; неизвестный `unit` → `AllTime`.
  - `pub fn from_condition(node: &Node) -> CumulativePeriod` (дети `CondCumulativeGroup`), `pub fn from_action(data: &Map<String, Value>) -> CumulativePeriod`.
  - `pub fn periods(rules: &[Rule]) -> Vec<CumulativePeriod>` — уникальные периоды из условий и действий правил (в порядке обхода).
  - `pub fn start(&self, now: DateTime<Utc>) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>)` — границы `[start, end)` (относительный — `now − N D/M/Y` через `chrono::Months`/`Duration`).
  - `Context.cumulative: HashMap<CumulativePeriod, f64>`; нет ключа — 0 (гость и правило без загрузки).
  - `model::CONDITION_CLASSES` += `CondCumulativeGroup`, `Period`, `PeriodRelative`; `ACTION_CLASSES` += `CumulativeAction`.

- [ ] **Step 1: Падающие тесты** (`engine.rs`, с хелперами существующих тестов):
  - `cumulative_condition_logic` — накоплено 5000: `Great 4000` → применяется, `Less 4000` → нет, `EqGr 5000` → да, `Equal 5000` → да, `Not 5000` → нет; гость (нет ключа) с `Less 1000` → да, с `EqGr 1000` → нет.
  - `cumulative_condition_uses_its_period` — суммы `AllTime=9000`, `Relative{1,'M'}=100`; условие `Great 1000` с `PeriodRelative {Value:1, TypeRelativePeriod:"M"}` → не применяется.
  - `cumulative_action_picks_highest_reached_range` — `ranges [{sum:1000,value:3,type:P},{sum:5000,value:7,type:P},{sum:20000,value:10,type:P}]` в обратном порядке; накоплено 6000, позиция 1000 → 930; накоплено 999 → без изменений; `type:F value:50` → 950.
  - `cumulative_action_child_filter` — `CHILDREN [CondBsktAppliedDiscount {logic:Equal, value:N}]`: позиция со скидкой предыдущего правила не меняется, без скидки — получает.
  - `cumulative_more_profitable_keeps_better_discount` — позиция 1000 со скидкой 20 % предыдущего правила (800), накопительная 10 % с `apply_if_more_profitable Y` → 800 (не применяется); при скидке 5 % (950) → 900 (от базовой), запись правила с суммой `50 × qty`.
  - `cumulative_rules_excluded_from_catalog` — `product_rules` не включает правила с `CondCumulativeGroup` в условиях или `CumulativeAction` в действиях.
  - `model.rs`: `validate_cumulative_nodes` — `CondCumulativeGroup` допустим в корне условий, `Period`/`PeriodRelative` — только в нём и не более одного; `CumulativeAction` — только в корне действий, дети — условия строки; неверный `type` порога (`X`) и `TypeRelativePeriod` (`W`) — ошибки у узла; `unsupported_classes` таких правил пуст.
- [ ] **Step 2: Запуск** → FAIL.
- [ ] **Step 3: Реализация.**
  - Условие: значение `ctx.cumulative.get(&from_condition(node)).unwrap_or(0.0)`, сравнение `L_NUM` с `Value`.
  - Действие (`applyCumulativeToBasket`): пороги по `sum`, последний с `накоплено >= sum`; `P` → процент от текущей цены (`-value`), `F` → `-value` на единицу; позиции — `!custom_price` и дети по `All`/`True` как `buildSubsCode` (`ActSaleBsktGrp`-группа ядра); при `apply_if_more_profitable == "Y"`: цена считается от `base_price` и применяется, только если меньше текущей (фильтр детей проверяется всегда — отклонение от `wrapFilterToFindMoreProfitableForCumulative`, где позиция без скидки проходит без фильтра; записать в ledger как Ruling); результат < 0 — не применяется. Ведёт себя как остальные действия: изменённые позиции → `Applied`.
- [ ] **Step 4: Запуск** → PASS; вся проверка.
- [ ] **Step 5: Коммит** `Накопительные скидки: условие и действие в движке`.

### Task 4: Накопленная сумма покупателя

**Files:**
- Modify: `src/discount/cumulative.rs` (запрос), `src/discount/context.rs` (`load_extras`)

**Interfaces:**
- Consumes: `CumulativePeriod`, `periods`, `start` (Task 3); `load_extras` (Task 2).
- Produces: `pub async fn sums(db: &mut PgConnection, user_id: i64, periods: &[CumulativePeriod], now: DateTime<Utc>) -> sqlx::Result<HashMap<CumulativePeriod, f64>>` — одним запросом (`SUM(...) FILTER (WHERE ...)` по периодам или `unnest` границ); `load_extras` вызывает её, только если есть `user_id` и `periods(rules)` не пуст.

- [ ] **Step 1: Падающие тесты** (`#[sqlx::test]` в `cumulative.rs`, заказы через `place_order` из `test_support` и `UPDATE orders`):
  - `cumulative_sums_skip_canceled_and_unpaid` — у пользователя оплаченный 3000, оплаченный и отменённый 5000, неоплаченный 7000, чужой оплаченный 9000 → `AllTime = 3000`.
  - `cumulative_sums_by_period` — оплаченные заказы `created_at` 40 дней назад (1000) и 5 дней назад (2000): `Relative{1,'M'} = 2000`, `Relative{60,'D'} = 3000`, `Interval{start: 10 дней назад, end: None} = 2000`, `Interval{start: None, end: 10 дней назад} = 1000`.
  - `cumulative_sums_first_currency_only` — первый (по id) оплаченный в `RUB` 1000, второй в `USD` 50 → 1000.
  - `load_extras_skips_query_without_cumulative_rules` — без таких правил `ctx.cumulative` пуст (и для гостя с правилами — пуст).
  - `bxapi/cart.rs`: `cumulative_discount_in_cart_for_user` — правило-пресет 10 % от 2000, пользователь с оплаченным заказом 2500 → цена позиции в `basket` со скидкой 10 %; гость — без скидки.
- [ ] **Step 2: Запуск** → FAIL.
- [ ] **Step 3: Реализация** запроса (`orders.user_id = $1 AND paid AND NOT canceled`, `currency` первого по id заказа периода) и вызова из `load_extras`.
- [ ] **Step 4: Запуск** → PASS; вся проверка.
- [ ] **Step 5: Коммит** `Накопительные скидки: накопленная сумма покупателя`.

### Task 5: Подарки — движок

**Files:**
- Modify: `src/discount/model.rs`, `src/discount/engine.rs`

**Interfaces:**
- Produces:
  - `model::ACTION_CLASSES` += `GiftCondGroup`, `GifterCondIBElement`, `GifterCondIBSection`.
  - `pub enum GiftType { One, All }` (`one`/`all`), `pub struct GiftOffer { pub rule_id: i64, pub kind: GiftType, pub product_ids: Vec<i64>, pub section_id: Option<i64> }` (`Debug, Clone, PartialEq, Serialize`; `product_ids` — для `GifterCondIBElement`, `section_id` — для раздела, раскрывает Task 6).
  - `DiscountResult.gifts: Vec<GiftOffer>` — доступные подарки: правила с `GiftCondGroup`, дошедшие в цикле до проверки и с выполненными `conditions`, за вычетом подаренных (`one` с подаренным товаром — убирается; у `all` убираются подаренные `product_ids`, пустой — убирается; у раздела — убирается при `one`, при `all` остаётся).

- [ ] **Step 1: Падающие тесты** (`engine.rs`):
  - `gift_one_makes_cheapest_free` — подарок `GifterCondIBElement {Type:one, Value:[10,11]}`, в корзине 10 (300, 1 шт.) и 11 (200, 1 шт.) → 11 бесплатна (0, скидка 200), 10 — 300; `applied[0].items == [(key11, 200.0)]`; `gifts` пуст.
  - `gift_all_makes_every_match_free` — `Type:all` → обе 0; `gifts` пуст. Только 10 в корзине → 10 бесплатна, `gifts == [GiftOffer{kind: All, product_ids: [11], ..}]`.
  - `gift_requires_quantity_one` — 10 количеством 2 → платная, в `gifts` правило `one` со списком `[10, 11]`.
  - `gift_by_parent_of_offer` — `Value:[parent]`, в корзине предложение родителя → бесплатное; `gifts` пуст.
  - `gift_by_section` — `GifterCondIBSection {Type:one, Value:5}`, позиция в разделе 5 (`section_ids` включает 5) → бесплатна.
  - `gift_offered_only_when_conditions_pass` — условие `CondBsktAmtGroup ≥ 5000` при корзине 3000 → `gifts` пуст, при 6000 → правило в `gifts`.
  - `gift_skips_custom_price_and_zero` — позиция с ручной ценой и позиция с ценой 0 не становятся подарком.
  - `gift_rules_excluded_from_catalog` — `product_rules` не включает правила с `GiftCondGroup`.
  - `model.rs`: `validate_gift_nodes` — `GiftCondGroup` только в корне действий, ровно один ребёнок-`Gifter*`; `Type` не `one`/`all` — ошибка; `Value` раздела — существующий раздел, товаров — существующие записи (через `Refs`).
- [ ] **Step 2: Запуск** → FAIL.
- [ ] **Step 3: Реализация** (`applySimpleGift` + `GenerateApplyCallableFilter`): позиции по возрастанию текущей цены (стабильно по ключу), подходящие — товар в `Value` или `parent.id` в `Value` (элемент) / `section_ids` содержит раздел; `quantity == 1.0`, текущая цена > 0, `!custom_price`; `one` — первая. Цена → 0.
- [ ] **Step 4: Запуск** → PASS; вся проверка.
- [ ] **Step 5: Коммит** `Подарки: действие в движке и доступные подарки`.

### Task 6: Подарки в bxapi

**Files:**
- Modify: `src/bxapi/cart.rs` (`gifts` в снимке корзины), `src/bxapi/mod.rs` (маршрут), `src/discount/repo.rs` (активные товары раздела)
- Create: `src/bxapi/gifts.rs` (`GET /catalog/gifts`)

**Interfaces:**
- Consumes: `DiscountResult.gifts`, `GiftOffer` (Task 5).
- Produces:
  - `pub async fn section_products(db: &PgPool, section_ids: &[i64]) -> sqlx::Result<HashMap<i64, Vec<i64>>>` — активные записи раздела и его потомков, по id.
  - В `basket` поле `gifts: [{"ruleId": i64, "type": "one"|"all", "productIds": [i64]}]` — только если среди активных правил есть `GiftCondGroup`.
  - `GET /catalog/gifts?productId=<id>` → `success([...])` того же формата: подарки, которые появятся при добавлении товара количеством 1 (разница по `(ruleId, productId)` между расчётом корзины с товаром и без); нет правил с подарками — `[]` без расчёта; товара нет/не продаётся — `[]`.

- [ ] **Step 1: Падающие тесты** (`bxapi/cart.rs`, `bxapi/gifts.rs`):
  - `basket_lists_available_gifts` — правило подарка (товар-предложение фикстуры, `one`) с условием «сумма ≥ 1000», корзина фикстуры 2000 → `gifts == [{"ruleId":…, "type":"one", "productIds":[offer]}]`; положили предложение → оно с ценой 0, `gifts` пуст.
  - `basket_gifts_absent_without_gift_rules` — правило только со скидкой → ключа `gifts` нет.
  - `catalog_gifts_for_product` — условие «в корзине есть товар X»; пустая корзина гостя, `GET /catalog/gifts?productId=X` → правило с подарком; для другого товара → `[]`.
  - `section_gift_lists_section_products` — подарок `GifterCondIBSection` раздела фикстуры → в `productIds` товар раздела.
- [ ] **Step 2: Запуск** → FAIL.
- [ ] **Step 3: Реализация.** `gifts` строится в `priced` из `discounts.gifts` (раздел — через `section_products`); `/catalog/gifts` — `CartBase` текущего покупателя (или пустая) + позиция товара с временным ключом `-1`, два расчёта движком.
- [ ] **Step 4: Запуск** → PASS; вся проверка; `cart_without_rules_is_byte_identical` без изменений.
- [ ] **Step 5: Коммит** `Подарки в корзине и карточке товара bxapi`.

### Task 7: Округление в каталоге

**Files:**
- Modify: `src/bxapi/serialize.rs` (`catalogPrice`), `src/api.rs` (`price_discount`), `src/discount/catalog.rs`, `src/discount/context.rs` (`for_products` — тип цены)

**Interfaces:**
- Consumes: `Rounding` (Task 1), округление в `catalog_price_with` (Task 2).
- Produces: `for_products(db, products: &[(i64, f64, Option<i64>)])` — третий элемент тип цены; `CatalogRules` хранит `rounding: Option<Rounding>`.

- [ ] **Step 1: Падающие тесты.**
  - `bxapi/serialize.rs`: `catalog_price_is_rounded` — правило `up` 100 для типа 1, цена 1001 → `catalogPrice.value == 1100`, `formatted` по 1100, `byType[0].value == 1100`; тип без правил — как есть. С флагом `catalog_price_discount` и 10 % скидкой → `discountValue == 1000` (900.9 → вверх до 1000).
  - `api.rs`: `api_price_discount_is_rounded` — 10 % скидки и `math` 10, цена 1004 → `price_discount == 900` (903.6 → 900).
  - Без правил округления — существующие тесты `catalogPrice` и `/api/v1` проходят без изменений.
- [ ] **Step 2: Запуск** → FAIL.
- [ ] **Step 3: Реализация.** bxapi: `rounding::load` один раз при `select` с `catalogPrice`, округление `value`/`byType`/`formatted`/`discountValue`. `/api/v1`: округление `price_discount` (тип — основной цены).
- [ ] **Step 4: Запуск** → PASS; вся проверка.
- [ ] **Step 5: Коммит** `Округление цен в каталоге`.

### Task 8: Админка: новые узлы дерева и пресет накопительной скидки

**Files:**
- Modify: `src/admin/discounts.rs` (`class_defs`, маршруты пресета), `static/admin/discount-tree.js` (виды полей `ranges`, `datetime`), `src/admin/mod.rs`
- Create: `templates/admin/discount_cumulative.html`

**Interfaces:**
- Consumes: классы и проверки `model.rs` (Tasks 3, 5).
- Produces:
  - `FieldDef` виды `ranges` (таблица `sum`/`value`/`type` `P`/`F`, добавление/удаление строк, значение — массив объектов) и `datetime` (ввод даты-времени по МСК, хранение — unix-секунды; пусто — ключа нет); `FieldDef::ids_at(key, lookup)` для `Value` подарков.
  - Классы конструктора: «Накопительная сумма заказов» (`CondCumulativeGroup`: логика, порог), «Период: с … по …» (`Period`), «Период: последние N …» (`PeriodRelative`, единица Дней/Месяцев/Лет), «Накопительная скидка» (`CumulativeAction`: пороги, период полями `type_sum_period`/`sum_period_data`, «только если выгоднее»), «Подарок» (`GiftCondGroup`), «Товары» (`GifterCondIBElement`: «один из»/«все», товары), «Раздел» (`GifterCondIBSection`).
  - `GET/POST /admin/discounts/cumulative` — форма пресета: название, активность, группы, пороги, период, «только если выгоднее», «пропускать позиции со скидкой»; сохраняет правило: `conditions` — пустой `CondGroup`, `actions` — `CondGroup` с одним `CumulativeAction` (+ ребёнок `CondBsktAppliedDiscount {logic: Equal, value: N}` при «пропускать»), как `Cumulative::generateDiscount`; кнопка «Накопительная скидка» в списке правил.

- [ ] **Step 1: Падающие тесты** (`#[sqlx::test]` в `admin/discounts.rs`, через `admin_cookie`/`permitted_cookie` с `discounts.manage`):
  - `cumulative_preset_creates_rule` — POST пресета (пороги 1000→3 %, 5000→7 %, относительный период 12 месяцев, «пропускать» = да) → правило с деревом из пресета Битрикса (сверка JSON), `unsupported == ""`.
  - `cumulative_preset_validates_ranges` — пустые пороги и процент > 100 → форма с ошибкой, введённое сохранено.
  - `discount_form_saves_gift_rule` — POST формы правила с `GiftCondGroup`/`GifterCondIBElement` → сохранено; с несуществующим товаром — ошибка у узла `0.0.0`.
  - `tree_data_lists_new_classes` — данные конструктора (`#discount-tree-data`) содержат классы выше.
- [ ] **Step 2: Запуск** → FAIL.
- [ ] **Step 3: Реализация** (JS — по образцу видов `choice`/`ids`; сервер проверяет дерево как в ядре).
- [ ] **Step 4: Запуск** → PASS; вся проверка; ручная проверка формы в браузере не требуется — тесты сервера.
- [ ] **Step 5: Коммит** `Админка скидок: накопительные и подарки`.

### Task 9: Админка правил округления

**Files:**
- Create: `src/admin/shop/rounding.rs`, `templates/admin/shop/rounding.html`, `templates/admin/shop/rounding_form.html`
- Modify: `src/admin/shop/mod.rs`, `src/admin/mod.rs` (маршруты `/shop/rounding`, `/shop/rounding/new`, `/shop/rounding/{id}`, `/shop/rounding/{id}/delete`), `templates/admin/shop/_nav.html` («Правила округления»)

**Interfaces:**
- Consumes: `RoundType`, `PRESET_PRECISIONS` (Task 1).
- Produces: CRUD по образцу `price_types.rs`, право `require_shop`; список сгруппирован по типам цен (порог по убыванию); форма: тип цены, «для цен больше», способ (Математически / Вверх / Вниз), точность (выбор пресета или своё значение > 0); `created_by`/`updated_by` — текущий пользователь.

- [ ] **Step 1: Падающие тесты** (`#[sqlx::test]` в `rounding.rs`):
  - `rounding_crud` — создание, правка, удаление; список показывает «Розница», «> 1000», «Вверх», «100».
  - `rounding_rejects_bad_input` — точность 0, отрицательная, нечисловая, неизвестный тип цены → ошибка в форме, введённое сохранено; дубликат порога у типа → «Правило с таким порогом уже есть».
  - `rounding_requires_shop_permission` — без права `shop.manage` → 403.
- [ ] **Step 2: Запуск** → FAIL.
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: Запуск** → PASS; вся проверка.
- [ ] **Step 5: Коммит** `Админка правил округления`.

### Task 10: Перенос из Битрикса и документация

**Files:**
- Modify: `src/import.rs` (чтение `b_catalog_rounding`, `b_catalog_disc_save_group`; нормализация дат накопления; запись), `README.md`

**Interfaces:**
- Consumes: `RoundType::from_bitrix` (Task 1), классы моделей (Tasks 3, 5).
- Produces:
  - `b_catalog_rounding` → `catalog_rounding` (`CATALOG_GROUP_ID` → `price_type_id`, id типов цен сохраняются; правило с неизвестным типом цены или `ROUND_TYPE` — предупреждение, не переносится); повторный перенос заменяет правила округления целиком.
  - `b_catalog_disc_save_group` (если таблица есть и не пуста) — предупреждение «накопительные программы каталога (b_catalog_disc_save_*) не переносятся: <id «название»…>».
  - В деревьях правил: `Period.ValueStart`/`ValueEnd` и `sum_period_data.discount_sum_order_start`/`_end` — число или строка даты Битрикса `DD.MM.YYYY[ HH:MI[:SS]]` (МСК) → unix-секунды; пустое — ключ удаляется.

- [ ] **Step 1: Падающие тесты** (по образцу `import_carries_discounts`):
  - `import_carries_rounding` — две строки `b_catalog_rounding` (тип 1: 0/1/10 и 1000/2/100) → два правила `math`/`up`; строка с `ROUND_TYPE = 3` → предупреждение.
  - `import_warns_disc_save` — строка в `b_catalog_disc_save_group` → предупреждение со списком.
  - `import_carries_cumulative_and_gift_rules` — правило с `CumulativeAction` (`discount_sum_order_start = "01.01.2026 00:00:00"`) и правило с `GiftCondGroup` → `unsupported == ""`, дата → `1767214800`.
  - `discount_tree_normalizes_cumulative_dates` (чистый тест `discount_tree`) — число остаётся числом, пустая строка — ключ удалён.
- [ ] **Step 2: Запуск** → FAIL.
- [ ] **Step 3: Реализация.**
- [ ] **Step 4: README.** Раздел «Скидки»: накопительные (условие, действие, пресет, накопленная сумма — оплаченные неотменённые, в валюте первого заказа, гость — 0), подарки (`gifts` в `basket`, `GET /catalog/gifts`), округление (где применяется, админка); «Не входит» → `b_catalog_disc_save_*`, старые скидки каталога, перенос исторических заказов, автодобавление подарков. Раздел переноса — `b_catalog_rounding` и предупреждение `disc_save`.
- [ ] **Step 5: Запуск** → PASS; вся проверка.
- [ ] **Step 6: Коммит** `Перенос округления и накопительных скидок из Битрикса, документация`.

### Task 11: Сквозная проверка и регрессия (контролёр)

Не реализация — проверка, выполняет контролёр после финального ревью.

- [ ] Копия dev-базы (`createdb -T cms cms_3b`), сервер на копии: синтетические правила — пресет накопительной скидки 5 % от 1000 у пользователя с оплаченным заказом, подарок (товар каталога) за сумму корзины ≥ 3000, округление `up` 10 для основного типа цены. Через bxapi: корзина, `gifts`, добавление подарка (цена 0), итоги формы, оформление, история заказа, XML для 1С — суммы сходятся. Копия удаляется.
- [ ] Регрессия на основной базе (без новых правил): ответы bxapi и `/api/v1` из набора 3а байт в байт со сборкой до 3б.
- [ ] Основная база мигрирована до 0021, сервер :3000 на новой сборке.
