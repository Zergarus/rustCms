# Коллекции вместо инфоблоков — дизайн

Дата: 2026-10-09. Подпроект 0 серии «до функционала Битрикса редакции „Бизнес“»
(переименование → SKU → обмен с 1С → скидки → остальные модули редакции).

## Цель и критерии успеха

В CMS не остаётся битриксового словаря «инфоблок / элемент / свойство»: админка, наш
API, код Rust и схема БД говорят «коллекция / запись / поле». Поведение не меняется.

Готово, когда:

- админка, наш API `/api/v1/...`, модули Rust, таблицы и колонки БД, README используют
  словарь ниже;
- слово `iblock` осталось только там, где оно нужно по сути (раздел «Что не меняется»);
- все тесты зелёные, clippy чистый, в том числе без `projects/`;
- фронт трансопта работает без правок: главная, каталог, карточка, корзина, оформление
  заказа, личный кабинет; нагрузка на каталог не просела.

## Словарь

| Битрикс | Интерфейс | Код / БД | Наш API |
|---|---|---|---|
| Инфоблок | Коллекция | `collection`, `collections` | `/api/v1/collections/{code}` |
| Элемент | Запись | `item`, `collection_items` | `.../items[/{id}]` |
| Раздел | Раздел | `section`, `collection_sections` | `.../sections` |
| Свойство | Поле | `field`, `collection_fields` | — |
| Значение списка | Вариант | `option`, `collection_field_options` | — |

## БД — миграция `0014_collections.sql`

Только `ALTER … RENAME`, данные не переливаются:

- таблицы: `iblocks` → `collections`, `iblock_elements` → `collection_items`,
  `iblock_sections` → `collection_sections`, `iblock_properties` → `collection_fields`,
  `iblock_property_enums` → `collection_field_options`, `iblock_group_access` →
  `collection_access`;
- колонки во всех таблицах, включая `catalog_*`, `cart_items`, `order_items` и
  `order_property_values` (где ссылаются на записи): `iblock_id` → `collection_id`,
  `element_id` → `item_id`, `property_id` → `field_id` (только ссылки на поля коллекций;
  `property_id` свойств заказа не трогается);
- индексы, ограничения, последовательности с префиксами `iblock_*` → `collection_*`;
  триггер и функция `iblock_elements_cart_cleanup` → `collection_items_cart_cleanup`;
- право `iblocks.manage` → `collections.manage` в `group_permissions`.

Миграции 0001–0013 не меняются (sqlx проверяет контрольные суммы).

## Код

- `src/iblock/` → `src/collection/`; `admin/iblocks.rs` → `admin/collections.rs`,
  `admin/elements.rs` → `admin/items.rs`; типы и функции по словарю (`Element` → `Item`,
  `Property` → `Field`, `PropertyEnum` → `FieldOption`, `require_iblock` →
  `require_collection`, `can_see_iblocks` → `can_see_collections`, `IBLOCKS_MANAGE` →
  `COLLECTIONS_MANAGE`).
- Админка: пути `/admin/collections/...`, `/admin/items/...`, `/admin/fields/...`;
  шаблоны `collection_form.html`, `collections.html`, `item_form.html`, `items.html`,
  `field_form.html`; меню и тексты по словарю.
- Наш API: `/api/v1/collections`, `/api/v1/collections/{code}`,
  `/api/v1/collections/{code}/items`, `/api/v1/collections/{code}/items/{id}`; ключи JSON
  по словарю (`collection`, `items`, `fields`).
- README: раздел «Коллекции» со словарём и пометкой «аналог инфоблоков Битрикса».
- `projects/transopt.rs` (вне git) — обновить под новые имена.

Старые адреса `/admin/iblocks...` и `/api/v1/iblocks...` не сохраняются: внешних
потребителей у нашего API нет.

## Что не меняется

- bxapi: пути `/api/v1/iblock/...`, имена полей ответа (`iblockId` и т. п.) и настройки
  контракта — внутри меняются только SQL и имена Rust;
- импорт из Битрикса: запросы к `b_iblock*` и имена полей источника; пишет в коллекции;
- миграции 0001–0013.

## Проверка

- Весь набор тестов и clippy — с `projects/` и без.
- `sqlx::test`: на схеме до 0014 с данными (коллекция, раздел, запись, поле с вариантом,
  цена, остаток, позиция корзины, заказ) после 0014 число строк и связи не изменились.
- Поиск: `rg -i iblock src templates` находит только bxapi, импорт и пути контракта.
- Сквозной прогон фронта трансопта и `ab` по запросам каталога из замера 2026-10-09.
