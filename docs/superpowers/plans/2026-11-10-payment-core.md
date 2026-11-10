# Платёжное ядро — план

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans.

**Goal:** `/order/pay` и `/order/{id}/payment-channel` работают с провайдером, который подключает проект.

**Architecture:** `src/payment.rs` — доступ к заказу, поиск платежа, вызов провайдера, отметка оплаты. Трейты и `PaymentExtension` — в `project/mod.rs`. Cookie `CMS_CHECKOUT` ставится при `order/submit`.

**Spec:** `docs/superpowers/specs/2026-11-10-payment-core-design.md`

## Global Constraints

- Контракт ответов: `{success, payment}` у `pay`, `{pull: null, moduleId, command, state}` у `payment-channel`.
- Чужой и несуществующий заказ неразличимы — `order_not_found`.
- Повторный `pay` не создаёт второй платёж.
- Без провайдера — прикладная ошибка `payment_provider_missing` (HTTP 200).

## Tasks

### Task 1: Трейты и расширение проекта

**Files:** Modify `src/bxapi/project/mod.rs`.

- `PaymentRef { order_id, payment_id, pay_system_id, sum, currency, account_number, data }`, `PaymentStarted { url, data }`, `PaymentState { paid, notice }`;
- `trait PaymentProvider { start, status }`, `struct PaymentExtension { pay_system_id, provider }`, `Project.payment_extensions`.

- [ ] Тест RED: проект с расширением виден в `Project::default()`-совместимой сборке (юнит на доступ к полю).
- [ ] Реализация → GREEN.

### Task 2: Ядро платежа

**Files:** Create `src/payment.rs`.

**Interfaces:**
- `pub const CHECKOUT_COOKIE: &str = "CMS_CHECKOUT"`;
- `pub async fn start(state, jar, order_id) -> BxResult<Value>`;
- `pub async fn channel(state, jar, order_id) -> BxResult<Value>`;
- `pub fn checkout_cookie(state, order_id) -> Cookie<'static>`;
- доступ: владелец `orders.user_id` или id в cookie.

- [ ] Тесты RED: провайдер-заглушка отдаёт данные; повторный `pay` возвращает то же без второго `start`; `channel` после `paid` ставит `orders.paid`; чужой заказ — `order_not_found`; отменённый — `payment_forbidden`; без провайдера — `payment_provider_missing`.
- [ ] Реализация → GREEN.

### Task 3: Маршруты и cookie

**Files:** Modify `src/bxapi/order.rs`, `src/bxapi/mod.rs`.

- [ ] Тесты RED: `POST /order/pay` и `GET /order/{id}/payment-channel` через роутер; `order/submit` ставит `CMS_CHECKOUT`; гость по cookie проходит, без cookie — `order_not_found`.
- [ ] Реализация → GREEN.

### Task 4: Документация и проверки

- [ ] README (раздел про оплату и расширение провайдера), `project/README.md` (пример `PaymentProvider`).
- [ ] `cargo test`, clippy `-D warnings`, fmt, `git diff --check`.

## Результат 2026-11-10

`src/payment.rs` (доступ, провайдер, отметка оплаты), трейты `PaymentProvider` и
`PaymentExtension` в проекте, маршруты `/order/pay` и `/order/{id}/payment-channel`,
cookie `CMS_CHECKOUT` при submit. `cargo test` — все тесты зелёные.
