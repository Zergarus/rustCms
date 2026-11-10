# Модуль проекта

Код конкретного сайта, который не нужен остальным: настройки API bxapi (ширины
картинок, характеристики, источник картинки товара, алиасы, правила хлебных
крошек и редиректов), декораторы ответов и группы поиска — аналог `bxapi.*`
в `/bitrix/.settings_extra.php` и регистрации классов в `init.php`.

Одна установка CMS — один проект. Файлы этого каталога, кроме README, в репозиторий
CMS не попадают — храните их в репозитории проекта.

## Как подключить

1. Положите `project/mod.rs` с функцией:

   ```rust
   use crate::bxapi::project::Project;

   pub fn project() -> Project {
       Project {
           image_widths: vec![120, 320, 800],
           ..Project::default()
       }
   }
   ```

   Код можно разбить на файлы: `mod decorators;` в `mod.rs` подключит
   `project/decorators.rs`.

2. Соберите CMS — `build.rs` подключит модуль автоматически. Без `project/mod.rs`
   действуют настройки по умолчанию.

Декоратор — тип с `impl crate::bxapi::project::Decorator`, группа поиска —
`impl crate::bxapi::project::SearchGroup`; регистрируются в полях
`detail_decorators`, `list_decorators`, `search_groups` структуры `Project`.
Корзина и оформление заказа (аналог `bxapi.basket` и `bxapi.order`) — поля `cart`
(`CartConfig`) и `order` (`OrderConfig`; ограничение доставок по корзине — `impl
crate::bxapi::project::DeliveryFilter` в `order.delivery_filter`).

## Формы: шаги кодом проекта

Аналог `FormRegistry::register()` в модуле `mediagroup.bxapi`. В `Project`
добавьте `form_extensions`:

```rust
use crate::bxapi::project::{FormExtension, FormValidator, FormWriter, FormNotifier};

project.form_extensions.push(FormExtension {
    code: "review",
    validator: Some(Arc::new(ReviewValidator)), // заменяет правила формы
    writer: Some(Arc::new(ReviewWriter)),       // заменяет запись в коллекцию
    notifier: Some(Arc::new(ReviewNotifier)),   // заменяет письмо
});
```

Каждый шаг необязателен: `None` означает «взять из `config/forms.toml` или из
описания формы в коде». Валидатор возвращает прошедшие проверку поля (они уйдут
в writer/notifier и в ответ `data`), writer — id созданной записи или `None`,
notifier просто выполняет отправку. Сигнатуры — в
`src/bxapi/project/mod.rs` (`FormValidator`, `FormWriter`, `FormNotifier`).

## Оплата: провайдер проекта

```rust
use crate::bxapi::project::{PaymentExtension, PaymentProvider, PaymentRef, PaymentStarted, PaymentState, BoxFuture};

struct YooKassa;

impl PaymentProvider for YooKassa {
    fn start<'a>(&'a self, _state: &'a AppState, payment: &'a PaymentRef)
        -> BoxFuture<'a, Result<PaymentStarted, BxError>>
    {
        Box::pin(async move {
            // запрос к API провайдера, создание платежа
            Ok(PaymentStarted {
                url: Some("https://…".into()),
                data: json!({ "qrPayload": "…" }),
                external_id: Some("провайдер-id".into()),
            })
        })
    }

    fn status<'a>(&'a self, _state: &'a AppState, payment: &'a PaymentRef)
        -> BoxFuture<'a, Result<PaymentState, BxError>>
    {
        Box::pin(async move { Ok(PaymentState { paid: false, amount: None, notice: None }) })
    }

    fn webhook<'a>(&'a self, _state: &'a AppState, payload: &'a Value, headers: &'a HeaderMap)
        -> BoxFuture<'a, Result<PaymentWebhook, BxError>>
    {
        Box::pin(async move {
            // проверка подписи/секрета; вернуть id нашего платежа
            Ok(PaymentWebhook { payment_id: payment_id_from(payload)? })
        })
    }
}

project.payment_extensions.push(PaymentExtension {
    pay_system_id: 1,
    provider: Arc::new(YooKassa),
});
```

`pay_system_id` — id платёжной системы из админки магазина. Возвращённые
`data` сохраняются в `payments.data.started` и переиспользуются при повторном
`pay`; в `status` провайдер получает ровно эти данные (`PaymentRef.data`).
`external_id` сохраняется в `payments.external_id` и годится для запроса статуса.
Ошибку из `start`/`status` ядро логирует и наружу отдаёт безопасный код
`payment_provider_failed`; сбой `status` не ломает опрос.

`webhook` вызывается публичным `POST /payment/webhook/{код системы}` и получает
только тело и заголовки — `PaymentRef` там нет. Задача метода: проверить
подпись/секрет и вернуть **наш** `payments.id`. Соответствие «платёж провайдера →
наш платёж» проект ищет сам по `payments.external_id` (туда ядро пишет
`PaymentStarted.external_id`) — доверять идентификатору из непроверенного тела
нельзя.

Факт оплаты ядро всё равно перепроверяет через `status` и сверяет сумму с
`payments.sum`; данные, которые `start` вернул в `data`, приходят в
`PaymentRef.data` в `status`.

## Свои HTTP-маршруты

Проекту не нужно форкать CMS ради собственного API (`/1c/api/*`, документы,
вебхуки внешних сервисов). В `Project` есть `routes`:

```rust
use axum::{routing::{get, post}, Router, Json, extract::State, http::StatusCode};

async fn users_discount(State(state): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    // State<AppState> даёт БД, настройки и реестры
    Ok(Json(serde_json::json!({ "ok": true })))
}

let routes = Router::new()
    .route("/1c/api/users/discount", post(users_discount));

project.routes = Some(routes);
```

Маршруты монтируются в общий роутер до слоёв логирования. CORS, авторизация и
rate-limit — на совести проекта: общие middleware админки и bxapi к ним не
применяются.
