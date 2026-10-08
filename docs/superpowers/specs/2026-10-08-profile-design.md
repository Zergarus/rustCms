# Личный кабинет — дизайн

Дата: 2026-10-08. Подпроект 3 из 4 этапа «Коммерция» (корзина → оформление заказа →
**личный кабинет** → онлайн-оплата). Оформление — в `2026-10-08-order-design.md`.

## Цель и критерии успеха

Вошедший покупатель на неизменённом фронте трансопта видит и правит свой профиль
(поля, аватар, пароль), видит свои заказы со статусом, оплатой, доставкой, позициями и
документами (накладная, счёт, УПД), а страница «Заказ оформлен» показывает только что
оформленные заказы. Менеджер загружает документы к заказу и ведёт трек-номер в
админке.

Готово, когда:

- фронт работает через раздел «7. Личный кабинет» `FRONTEND_API.md` без правок:
  `GET/POST /profile`, `GET /profile/orders`, `GET /profile/orders/{id}`, коды ошибок;
- в шапке фронта — число заказов (`ordersCount`);
- файлы, загруженные в карточке заказа в админке, приходят в `properties` заказа в
  формате Битрикса и открываются на фронте;
- сквозная проверка через фронт пройдена; тестовые заказы остаются в базе.

Не входит: `payments[].receiptUrl` и кнопка «Оплатить» (подпроект 4, ЮKassa); разбор
местоположения пользователя в `ufCity` (фронт не использует, у трансопта и в Битриксе
не приходит); проектные декораторы заказа (`OrderDecoratorInterface`) — фронт трансопта
сам вычисляет статусы и действия из сырых полей.

## Настройки проекта

`ProfileConfig` в `Project` (аналог `bxapi.profile`), значения по умолчанию — как в
документации контракта:

- `fields` — поля профиля в UPPER_SNAKE: `NAME`, `LAST_NAME`, `SECOND_NAME`, `EMAIL`,
  `PERSONAL_PHONE`, `PERSONAL_PHOTO`;
- `editable_fields` — по умолчанию те же;
- `include_orders_count` — `true`;
- `orders_limit` — 50 (максимум запроса — 200);
- `order_created_at_format` — `d.m.Y H:i:s` (токены PHP `date()`: `d m Y H i s`).

Режим `itemsCount` — из `CartConfig.items_count_positions` (тот же, что у корзины).

Поля Битрикса → колонки `users`: `NAME`→`name`, `LAST_NAME`→`last_name`,
`SECOND_NAME`→`second_name`, `EMAIL`→`email`, `PERSONAL_PHONE`→`phone`,
`PERSONAL_CITY`→`city`, `WORK_POSITION`→`work_position`, `PERSONAL_PHOTO`→`photo_id`,
`UF_*` → ключ в `extra` (snake_case). Неизвестное поле в настройке пропускается.
Ключ в JSON — camelCase (`PERSONAL_PHONE` → `personalPhone`).

## API (`src/bxapi/profile.rs`)

Все эндпоинты — только с сессией сайта; без неё HTTP 401 `unauthorized`. POST — с
CSRF, как остальные изменяющие эндпоинты.

### `GET /profile`

`{id, login, email, <fields>, editable, ordersCount}`. `PERSONAL_PHOTO` — путь
`/upload/...` или `null`; пустые строки отдаются как есть. `editable` — camelCase
имён из `editable_fields`. `ordersCount` — число заказов с `user_id` = текущий; нет
поля при `include_orders_count = false`.

### `POST /profile`

JSON или `multipart/form-data`. Имя поля — camelCase или UPPER_SNAKE. Принимаются
только поля из `editable_fields`, остальные молча игнорируются; строки обрезаются.

- `personalPhoto` (multipart) — картинка (jpg, jpeg, png, gif, webp), сохраняется через
  `files::save` в раздел `main`; старый файл отвязывается. `personalPhotoDelete=Y`
  (`1`, `y`, `yes`, `true`, `on`) без файла — удалить аватар.
- `password` + `passwordRepeat` — смена пароля: не совпали →
  `passwordRepeat: password_confirm_mismatch`; короче 6 символов (политика паролей
  Битрикса по умолчанию) → `password: "Пароль должен быть не менее 6 символов."`;
  хеш — `auth::hash_password` (argon2). Остальные сессии пользователя не сбрасываются.
- `EMAIL`: некорректный или занятый другим пользователем — ошибка поля текстом.
- Ошибки: HTTP 400 `profile_update_failed`, `customData.fields` — словарь
  `camelCase-поле → код или текст` (`upload_failed` — файл не картинка или не
  сохранился).
- Успех — профиль в формате `GET /profile`. Ничего не передано — тоже успех.

### `GET /profile/orders`

Query `limit` (1..200, по умолчанию `orders_limit`; некорректный — по умолчанию),
`offset` (≥ 0). Заказы текущего пользователя, `created_at DESC, id DESC`.
`data: {orders: [...]}`, заказ:

- `id`, `accountNumber` (пусто → id строкой), `status: {id, name}`, `createdAt`
  (ISO 8601 с часовым поясом сервера), `createdAtLabel`, `totalPrice`, `currency`,
  `canceled`, `paid`, `itemsCount`, `itemsCountLabel` («1 товар», «4 товара»,
  «11 товаров»), `personTypeId`, `comment` (комментарий покупателя);
- `shipments[]`: `id`, `deliveryId`, `deliveryCode`, `deliveryName` (копия в отгрузке,
  иначе текущее название службы), `price`, `currency`, `allowDelivery`, `deducted`
  (= `orders.stock_deducted`), `trackingNumber`, `stores` — склады самовывоза службы
  `{id, name, address, phone, image, ...UF склада из CartConfig.store_user_fields}`;
  у службы без складов поля нет;
- `payments[]`: `id`, `paySystemId`, `paySystemCode`, `paySystemName`, `sum`,
  `currency`, `paid`, `datePaid` (ISO или `null`);
- `items[]`: `id`, `productId`, `name`, `quantity`, `price`, `basePrice`, `currency`,
  `weight` (вес товара в каталоге, нет — 0), `detailPageUrl` (текущий адрес товара,
  нет — пусто), `properties` (`[{code: "STORE_ID", name: "Склад", value: "<id>"}]`
  при выбранном складе, иначе `[]`), `store: {id, name, xmlId, address}` (при
  складе), `image` (по правилам картинок корзины; нет картинки или товара — поля нет);
- `properties` — объект: все активные свойства типа плательщика заказа плюс свойства
  со значениями; ключ — camelCase кода (`SDEK_TRACKING_URL` → `sdekTrackingUrl`), без
  кода — `prop<ID>`; значение `{id, code: <ключ>, name, value}`. Значение файлового
  свойства — объект `{ID, SRC, ORIGINAL_NAME, FILE_NAME, CONTENT_TYPE, FILE_SIZE}`
  (`SRC` — `/upload/...`, `FILE_NAME` — имя файла на диске), у множественного — массив
  таких объектов; пустое файловое — `""` у одиночного, `[]` у множественного. Остальные
  — строка.

Списки, склады, файлы, картинки — пакетными запросами на страницу, не на заказ.

### `GET /profile/orders/{id}`

`{order}` в том же формате. Нечисловой id — 400 `order_id_invalid`; нет заказа или он
чужой — 404 `order_not_found`.

## Данные (миграция 0013)

- `order_properties.multiple BOOLEAN NOT NULL DEFAULT FALSE` — импорт из `MULTIPLE`;
  форма оформления не меняется (файловые свойства трансопта служебные).
- `shipments.tracking_number TEXT NOT NULL DEFAULT ''`,
  `shipments.allow_delivery BOOLEAN NOT NULL DEFAULT FALSE`.
- Значение файлового свойства в `order_property_values.value` — id файлов из `files`
  через запятую (одиночное — один id). Файлы — в разделе `sale` хранилища, отдаются по
  `/upload/...` как в Битриксе. Несуществующие id при выдаче пропускаются.

## Админка

Под правом «Работа с заказами»:

- **Свойства заказа в карточке**: у файлового свойства — список загруженных файлов
  (ссылка, отметка «удалить») и поле загрузки (у множественного — несколько файлов,
  добавляются к имеющимся; у одиночного — заменяет). Форма свойств — multipart;
  проверка остальных свойств — как сейчас.
- **Отгрузка в карточке**: «Трек-номер» и «Доставка разрешена», сохранение отдельной
  кнопкой.

## Проверка

- Модульные тесты (TDD): склонение `itemsCountLabel`, camelCase-ключи и `prop<ID>`,
  форматирование `createdAtLabel`, значение файлового свойства (одиночное,
  множественное, пустое, пропавший файл), `itemsCount` в режимах units/positions,
  разбор `limit`/`offset`, выбор принимаемых полей профиля из JSON/multipart.
- `sqlx::test`: список с пагинацией и порядком, чужой заказ → 404, `ordersCount`,
  `POST /profile` (только `editable`, несовпадение паролей, смена пароля, занятый email,
  загрузка и удаление аватара), файлы в карточке заказа (загрузка, добавление к
  множественному, удаление), трек-номер в ответе.
- Сквозной сценарий через фронт: оформить заказы; «Мои заказы» и «Заказ оформлен»;
  счётчик в шапке; правка профиля, аватар, смена пароля; загрузка накладной и счёта в
  админке → ссылки в ЛК. Тестовые заказы не удаляются.
- Нагрузка: `ab` на `GET /profile/orders` с сессией.
- Запросы с побочными эффектами в локальный Битрикс не отправляются.
