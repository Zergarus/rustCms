# Оплата заказа: ядро и провайдер проекта

Заказ сейчас создаётся, но оплатить его нельзя: фронт зовёт `POST /order/pay` и
`GET /order/{id}/payment-channel`, в CMS их нет. Провайдеров (ЮKassa, Альфа SBP,
QR) в Битриксе реализует проект, поэтому ядро даёт контракт и учёт платежей, а
провайдер подключается как расширение — тем же приёмом, что `FormExtension`.

## Контракт фронта

`POST /api/v1/order/pay` — тело `{ "id": <orderId> }`, ответ
`{ "success": true, "payment": { … } }`. `payment` — свободные данные обработчика
(`paymentUrl`, `qrPayload`, `qrImage`, `qrStatus`, `dateExpire`, …).

`GET /api/v1/order/{orderId}/payment-channel` — ответ
`{ "pull": null, "moduleId": "cms", "command": "paid", "state": { "orderId", "paymentId", "paid", "sum", "paymentType", "deliveryType", "notice" } }`.
`pull: null` фронт уже поддерживает — переходит на polling раз в 5 секунд.

## Доступ

Владелец заказа (`orders.user_id`) или гость, оформивший заказ в этой сессии.
При `order/submit` ядро ставит cookie `CMS_CHECKOUT` со списком последних id
заказов (не больше 10). Отсутствующий и чужой заказ неразличимы — `order_not_found`.

## Ядро

- Трейты в проекте (`src/bxapi/project/mod.rs`):

  ```rust
  pub struct PaymentStarted { pub url: Option<String>, pub data: serde_json::Value }
  pub struct PaymentState { pub paid: bool, pub notice: Option<serde_json::Value> }
  pub trait PaymentProvider: Send + Sync {
      fn start<'a>(&'a self, state: &'a AppState, payment: &'a PaymentRef)
          -> BoxFuture<'a, Result<PaymentStarted, BxError>>;
      fn status<'a>(&'a self, state: &'a AppState, payment: &'a PaymentRef)
          -> BoxFuture<'a, Result<PaymentState, BxError>>;
  }
  pub struct PaymentExtension { pub pay_system_id: i64, pub provider: Arc<dyn PaymentProvider> }
  ```

  В `Project` — `payment_extensions: Vec<PaymentExtension>`.

- `pay`: заказ не отменён и не оплачен; берём его неоплаченный платёж (создаётся
  при `order/submit`); провайдер ищется по `pay_system_id`; `start` вызывается
  один раз, повторный запрос переиспользует платёж и не создаёт новый; результат
  сохраняется в `payments.data`/`external_id`. Нет провайдера —
  прикладная ошибка `payment_provider_missing` (HTTP 200, как в bxapi).
- `payment-channel`: доступ тот же; если платёж не оплачен и провайдер есть —
  вызывается `status`; оплаченный платёж обновляет `payments.paid/paid_at`,
  `orders.paid/paid_at` и пишет историю; в ответе `state.paid` и `notice`.
- Идемпотентность: повторный `pay` для того же заказа возвращает те же данные
  без второго списания; оплаченный заказ отвечает `success: true` и
  `payment: null`.

## Не входит (в техдолг)

- PDF счёта и чека (`/bill`, `/receipt`) — следующий инкремент.
- Вебхуки провайдеров и фоновая сверка статусов: сейчас статус обновляется при
  опросе `payment-channel`.
- Возвраты, частичные оплаты, генерация QR (данные отдаёт провайдер).
- Bitrix Pull для мгновенных уведомлений — фронт уже умеет polling.

## Файлы

- `src/payment.rs` — работа с платежом и провайдером.
- `src/bxapi/project/mod.rs` — трейты и `PaymentExtension`.
- `src/bxapi/order.rs` — маршруты `pay` и `payment-channel`, cookie `CMS_CHECKOUT`.
- `src/bxapi/order.rs` (submit) — постановка cookie.
- `src/payment_tests.rs` — тесты.

## Проверка (2026-11-10)

TDD: провайдер-заглушка в проекте — `pay` отдаёт данные и сохраняет их; повторный
`pay` переиспользует платёж; `payment-channel` после `paid` отмечает заказ
оплаченным; гость по cookie проходит, чужой — `order_not_found`; отменённый
заказ — `payment_forbidden`; без провайдера — `payment_provider_missing`;
`order/submit` ставит cookie (проверено юнит-тестом сборки cookie: новый заказ
первым, прежние сохраняются, дубли убираются; полного HTTP-теста успешного
`order/submit` в репозитории нет).
