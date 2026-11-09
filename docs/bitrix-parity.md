# Паритет с Битриксом

Что из бизнес-логики Битрикса ещё не перенесено в CMS. Аудит 2026-11-09: фронт
(Nuxt BFF) → upstream `/api/v1/**` → реализация в CMS, плюс кастомный код
`www/local/**`.

Как пользоваться: закрывая пункт, ставим ✅ и дату с коммитом. Новый пункт
появляется здесь, а не в переписке. Оценка размера — грубая, по числу
недостающих частей, а не по часам.

Обозначения: ✅ есть, 🟡 частично, ⛔ нет.

## P1 — блокирует переключение витрины на CMS

| # | Сценарий | Битрикс | CMS | Статус |
|---|---|---|---|---|
| 1 | Оплата заказа целиком: инициация (`/order/pay`), канал оплаты (`/order/{id}/payment-channel`), счёт (`/bill/{order}/{payment}`), чек (`/receipt/{order}/{payment}`), шлюзы ЮKassa / Альфа SBP / QR, чеки ОФД | `order/pay`, `PaymentChannelController`, `ReceiptDownloadController`, `transopt/payment/**` | только `order/submit`, платёжных клиентов нет | ⛔ (большой) |
| 2 | Сотрудники и отзывы: `/employees`, `/employees/{id}`, `/review/employees/{id}` (список и создание) | `userStoreEmployeeSource.php`, `routes/review_mediagroup_bxapi.php` | нет | ⛔ (средний) |
| 3 | Регистрация и пароли: `/auth/register`, `/auth/forgot-password`, `/auth/reset-password`, `/auth/confirm-registration` | штатный `main.auth` + шаблоны писем | ✅ 2026-11-09, `4692d71` (капча — TD-010) | ✅ |
| 4 | Индивидуальная скидка: правило корзины по `UF_INDIVIDUAL_DISCOUNT`, `cashDiscount`, приём изменений из 1С | `transopt/sale/IndividualDiscount*` | в обмене есть только домен `Individual`, правила скидки нет | ⛔ (средний) |

## P2 — нужно для полноты

| # | Сценарий | Битрикс | CMS | Статус |
|---|---|---|---|---|
| 5 | Повторить заказ: `/profile/orders/{id}/repeat` | штатный sale | нет | ⛔ |
| 6 | Кастомный `/1c/api`: семейства, маркировки, пользователи/группы/скидка, склад, массовая правка цен | `routes/akpp_1c_project.php`, `users_1c_project.php`, `api_project.php` | только CommerceML и `sale.export.1c` | ⛔ |
| 7 | Sitemap: типы `catalog` (семейства) и `contacts` (сотрудники) | `transopt/seo/Sitemap.php` | типы настраиваются, но slug семейств и сотрудники не портированы | 🟡 (см. TD-008) |
| 8 | Легаси-редиректы `/catalog/…/{id}`, `/news/{id}` | правила проекта | механизм `nav/legacy-redirect` есть, `legacy_rules` пуст | 🟡 |
| 9 | Письмо админам о новом заказе с CITY / ADDRESS / FIO / PHONE и названием склада | `saleEventer.php`, `OrderMailFields.php`, `OrderStoreTitle.php` | только `SALE_NEW_ORDER` покупателю | 🟡 |

## P3 — можно позже

| # | Сценарий | Битрикс | Статус |
|---|---|---|---|
| 10 | Водяной знак на фото и оригиналы без знака | `transopt/iblock/Watermark.php`, `OriginalPhoto.php` | ⛔ |
| 11 | Наценка PRICELIST: базовая цена × (1 + наценка) | `transopt.pricemarkup` | ⛔ |
| 12 | Отчёты для 1С | `transopt/report/ReportAgent.php` | ⛔ |
| 13 | Уведомления о низком остатке | `transopt.stockcontrol` | ⛔ |
| 14 | Подсветка складов в списке заказов админки | `OrderStoreHighlight.php` | ⛔ |
| 15 | Аналитика доход/расход/прибыль и массовая правка цен | `transopt/api/controllers/analytics/*`, `PriceController` | ⛔ |

## Есть в CMS, но не подключено или не проверено

- SEO-метаданные `/api/v1/seo/{kind}/{id}` — витрина их не запрашивает (фронт не трогаем).
- Собственный публичный API `/api/v1/iblocks*` — витрина ходит только через bxapi-совместимый слой.
- Купоны `/cart/coupon/*` и подарки `/catalog/gifts` — фронт не использует.
- Складской учёт: документы, резервы, партии, штрихкоды, себестоимость, `/api/stock/v1` — шире Битрикса, витрина читает только остатки.
- Обмен заказами с 1С (`src/exchange/sale/**`) — на живой 1С не проверялся.
- Валюты — витрина курсы не запрашивает.
- Модуль проекта (`project/mod.rs`): декораторы, поиск, форма, капча — проверены тестами CMS, но не на данных ТрансОпта.
