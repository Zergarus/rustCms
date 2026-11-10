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
При `order/submit` ядро выдаёт **непрозрачный токен** (32 байта, в БД только
sha256, таблица `checkout_tokens`, срок 30 дней) и кладёт до 10 последних токенов
в cookie `CMS_CHECKOUT`. Голый id заказа доступ не даёт: подделанная cookie не
открывает чужой заказ. Отсутствующий и чужой заказ неразличимы — `order_not_found`.

## Ядро

- Трейты в проекте (`src/bxapi/project/mod.rs`):

  ```rust
  pub struct PaymentStarted {
      pub url: Option<String>,
      pub data: serde_json::Value,
      pub external_id: Option<String>, // сохраняется в payments.external_id
  }
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
  вызывается `status`. Оплата фиксируется доменной функцией `sale::repo::sync_paid_in`
  (заказ оплачен, когда оплачены все платежи) с пересчётом резервов, в порядке
  блокировок «заказ → оплата»; история статусов пишется один раз. Ошибка
  `status` не ломает опрос — отдаётся последнее известное состояние.
  Если оплачено, а провайдер не дал `notice`, ядро синтезирует стандартное
  уведомление: фронт останавливает опрос только при `paid && notice`.
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

## Проверка (2026-11-10, после ревью)

Найдено ревью и исправлено: IDOR через cookie (теперь токены в БД), гонка `pay`
(блокировка строки платежа), дубли истории (доменная `sync_paid_in` и
`UPDATE … AND NOT paid`), отсутствие `notice`, утечка ошибок провайдера,
`external_id`. Тесты: `cargo test payment_tests` — 8 (включая конкурентные
`pay`/`channel`, чужой токен, ошибку провайдера, `notice`).

TDD: провайдер-заглушка в проекте — `pay` отдаёт данные и сохраняет их; повторный
`pay` переиспользует платёж; `payment-channel` после `paid` отмечает заказ
оплаченным; гость по cookie проходит, чужой — `order_not_found`; отменённый
заказ — `payment_forbidden`; без провайдера — `payment_provider_missing`;
`order/submit` ставит cookie (проверено юнит-тестом сборки cookie: новый заказ
первым, прежние сохраняются, дубли убираются; полного HTTP-теста успешного
`order/submit` в репозитории нет).
