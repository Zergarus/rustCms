# API склада

REST API для внешних систем (WMS, ТСД, складские программы): склады, остатки, поиск товара по штрихкоду, документы склада и поставщики. Правила те же, что в админке: проверки, штрихкоды, партии. Базовый адрес в примерах — `https://example.ru/api/stock/v1`.

## Доступ

**Выпуск ключа.** Админка → «Пользователи» → карточка пользователя → блок «API-ключи» → «Выпустить ключ» (нужно право `users.manage`; ключи суперадминистратора правит только суперадминистратор). Укажите название, отметьте scope, при желании задайте срок. Ключ показывается **один раз** — сохраните его сразу. Отозвать ключ можно там же; следующий запрос с ним получит 401.

**Формат ключа:** `cms_<prefix>_<secret>` (префикс — 8 символов, секрет — 32). В базе хранится только хэш секрета; в админке виден `cms_<prefix>_…`.

**Заголовок:**

```
Authorization: Bearer cms_xxxxxxxx_…
```

**Что разрешено.** Запрос выполняется от имени владельца ключа. Доступ = scope ключа ∩ права пользователя: нужны и scope метода, и право пользователя. Всем методам нужно право `shop.manage`; закупочная цена в `products/lookup` — ещё `catalog.purchase_info`. Отобрали право у пользователя или группы — ключ сразу получает 403. Автор и ответственный созданного документа — владелец ключа.

### Scope

| Scope | Что разрешает |
|---|---|
| `stock.read` | склады, остатки, поиск товара, чтение документов и поставщиков |
| `stock.write` | создание, замена и удаление черновиков документов, создание поставщиков |
| `stock.conduct` | проведение и отмена проведения |

## Общие правила

- Формат — JSON (`Content-Type: application/json` для тел запросов). Даты и время — ISO 8601; дата документа — `YYYY-MM-DD`. Количества и цены — числа.
- Списки постраничные: `?limit=50&offset=0` (`limit` от 1 до 500, по умолчанию 50; `offset` от 0). Ответ: `{"items": [...], "total": N}`, где `total` — число записей с учётом фильтров.
- Методы документов при выключенном складском учёте отвечают 409 «Складской учёт выключен». Склады, остатки, товары и поставщики от этой настройки не зависят.
- Типы документов: `A` приход, `S` оприходование, `M` перемещение, `R` возврат, `D` списание. Статус: `draft` (черновик) или `conducted` (проведён).

В примерах ниже:

```bash
BASE=https://example.ru/api/stock/v1
KEY=cms_xxxxxxxx_…
```

## Чтение (`stock.read`)

### GET /stores — склады

```bash
curl -H "Authorization: Bearer $KEY" "$BASE/stores"
```

```json
{"items":[{"id":1,"xml_id":"main","name":"Основной склад","address":"","active":true,"is_default":true}],"total":1}
```

### GET /rests — остатки

Фильтры (необязательные, списки через запятую): `item_id`, `store_id`. Без фильтров отдаются все строки остатков постранично, порядок `item_id, store_id`.

```bash
curl -H "Authorization: Bearer $KEY" "$BASE/rests?item_id=38391,38392&store_id=1"
```

```json
{"items":[{"item_id":38391,"store_id":1,"amount":10.0,"quantity_reserved":2.0}],"total":1}
```

### GET /products/lookup — поиск товара

Ровно один параметр: `barcode`, `xml_id` или `id`, иначе 400. Штрихкод ищется так же, как в админке (в том числе штрихкоды отдельных экземпляров). Не найден — 404.

```bash
curl -H "Authorization: Bearer $KEY" "$BASE/products/lookup?barcode=4600000000017"
```

```json
{"id":38391,"xml_id":"p-38391","name":"Болт М8","type":"simple","barcode_multi":false,
 "barcodes":["4600000000017"],
 "stores":[{"store_id":1,"amount":10.0,"quantity_reserved":2.0}],
 "purchasing_price":100.0,"purchasing_currency":"RUB"}
```

`type` — `simple` или `offer` (торговое предложение). `barcodes` — общие штрихкоды товара. `purchasing_price` и `purchasing_currency` есть в ответе только при праве `catalog.purchase_info`.

### GET /documents — список документов

Фильтры (необязательные): `type` (`A`/`S`/`M`/`R`/`D`), `status` (`draft`/`conducted`), `from`, `to` (даты документа `YYYY-MM-DD`, включительно), `contractor_id`, `xml_id`. Порядок — `id` по убыванию. Строки документов в списке не отдаются.

```bash
curl -H "Authorization: Bearer $KEY" "$BASE/documents?type=A&status=draft&limit=20"
```

```json
{"items":[{"id":17,"xml_id":"WMS-1042","type":"A","title":"","number":"WMS-1042","date":"2026-10-30",
  "status":"draft","contractor_id":5,"currency":"RUB","total":1000.0,"comment":"",
  "created_by":3,"created_at":"2026-10-30T09:15:00Z","updated_at":"2026-10-30T09:15:00Z"}],"total":1}
```

### GET /documents/{id} — документ со строками

```bash
curl -H "Authorization: Bearer $KEY" "$BASE/documents/17"
```

Те же поля плюс `lines`:

```json
{"id":17,"xml_id":"WMS-1042","type":"A","status":"draft","…":"…",
 "lines":[{"id":101,"item_id":38391,"store_from":null,"store_to":1,"amount":10.0,
           "purchasing_price":100.0,"barcodes":[]}]}
```

### GET /contractors, GET /contractors/{id} — поставщики

```bash
curl -H "Authorization: Bearer $KEY" "$BASE/contractors?limit=10"
```

```json
{"items":[{"id":5,"person_type":"C","person_name":"","company":"ООО «Поставщик»","inn":"7700000000",
  "kpp":"770001001","address":"","phone":"","email":"","post_index":"",
  "created_at":"2026-10-01T08:00:00Z","updated_at":"2026-10-01T08:00:00Z"}],"total":1}
```

`GET /contractors/{id}` возвращает один такой объект (без обёртки) или 404.

## Запись (`stock.write`)

### POST /documents — создать черновик

```bash
curl -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" "$BASE/documents" -d '{
  "type": "A",
  "xml_id": "WMS-1042",
  "number": "WMS-1042",
  "date": "2026-10-30",
  "contractor_id": 5,
  "currency": "RUB",
  "comment": "",
  "lines": [
    {"item_id": 38391, "store_to": 1, "amount": 10, "purchasing_price": 100, "barcodes": []}
  ]
}'
```

Поля документа: `type` (обязателен), `xml_id`, `number`, `date`, `contractor_id`, `currency`, `comment`, `items_order_date`, `items_received_date` (даты `YYYY-MM-DD`), `lines`.

Поля строки: товар — **ровно одно** из `item_id` и `item_xml_id` (внешний код товара); `store_from`, `store_to` (какие нужны, зависит от типа документа), `amount` (число, обязательно), `purchasing_price`, `barcodes` (массив строк — штрихкоды экземпляров для поштучных товаров, как в форме документа).

Ответ — 201 и документ в том же виде, что `GET /documents/{id}`. Правила склада проверяются при создании, как при сохранении формы в админке; отказ — 422 со списком текстов.

### PUT /documents/{id} — заменить черновик

Полная замена, как сохранение формы: тело в том же формате, что у `POST`; что не передано, становится пустым (строки заменяются целиком, `type` необязателен). Ответ — 200 и документ. Проведённый документ — 409.

```bash
curl -X PUT -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" "$BASE/documents/17" -d '{
  "xml_id": "WMS-1042", "number": "WMS-1042", "date": "2026-10-30", "currency": "RUB",
  "lines": [{"item_xml_id": "p-38391", "store_to": 1, "amount": 12, "purchasing_price": 100}]
}'
```

### DELETE /documents/{id} — удалить черновик

```bash
curl -X DELETE -H "Authorization: Bearer $KEY" "$BASE/documents/17"
```

Ответ 204 без тела. Проведённый документ — 409 (сначала отмените проведение).

### POST /contractors — создать поставщика

`person_type`: `I` (физлицо, нужно `person_name`) или `C` (юрлицо, нужна `company`). Остальные поля необязательны: `inn`, `kpp`, `address`, `phone`, `email`, `post_index`.

```bash
curl -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" "$BASE/contractors" \
  -d '{"person_type":"C","company":"ООО «Поставщик»","inn":"7700000000","kpp":"770001001"}'
```

Ответ — 201 и объект поставщика, как в `GET /contractors/{id}`.

## Проведение (`stock.conduct`)

```bash
curl -X POST -H "Authorization: Bearer $KEY" "$BASE/documents/17/conduct"
curl -X POST -H "Authorization: Bearer $KEY" "$BASE/documents/17/cancel"
```

`conduct` проводит документ (меняются остатки и партии, как при проведении из админки), `cancel` отменяет проведение. Ответ — 200 и документ с новым `status`. Отказ правил склада (например, не хватает остатка) — 422 со списком текстов.

## Внешний код документа `xml_id`

Необязательное поле документа (до 100 символов; длиннее — 400), уникальное среди непустых значений. Внешняя система кладёт в него свой идентификатор и тем самым делает создание идемпотентным: повторный `POST /documents` с тем же `xml_id` ничего не меняет и отвечает 409 с `id` уже созданного документа:

```json
{"error":"conflict","message":"Документ с внешним кодом WMS-1042 уже есть","id":17}
```

Найти документ по внешнему коду: `GET /documents?xml_id=WMS-1042`. В админке код виден в форме и списке документов («Внешний код»).

## Ошибки

Тело ошибки — JSON с полем `error`; у всех, кроме 422, есть `message`.

| Код | `error` | Когда |
|---|---|---|
| 400 | `bad_request` | неверный JSON, неизвестный тип документа, неверная дата или число, неверные `limit`/`offset`/фильтры, не тот набор параметров `lookup`, слишком длинный `xml_id`, неверные данные поставщика |
| 401 | `unauthorized` | нет заголовка, ключ неверный, отозван или просрочен, пользователь ключа неактивен (в ответе `WWW-Authenticate: Bearer`) |
| 403 | `forbidden` | у ключа нет нужного scope или у пользователя нет нужного права |
| 404 | `not_found` | нет документа, поставщика или товара (`lookup`) |
| 409 | `conflict` | правка или удаление проведённого документа; повторный `xml_id` (с `id`); складской учёт выключен |
| 422 | `rejected` | отказ правил склада или не найден товар/поставщик в теле запроса; `messages` — список текстов, те же, что в админке |
| 500 | `internal` | внутренняя ошибка |

Примеры:

```json
// 401
{"error":"unauthorized","message":"Неверный или отозванный API-ключ"}
// 403: нет scope (проверяется первым) или права
{"error":"forbidden","message":"Нужен scope stock.conduct"}
{"error":"forbidden","message":"Нужно право shop.manage"}
// 404
{"error":"not_found","message":"Не найдено"}
// 400
{"error":"bad_request","message":"Нужен ровно один параметр: barcode, xml_id или id"}
// 409
{"error":"conflict","message":"Проведённый документ нельзя изменить — сначала отмените проведение"}
{"error":"conflict","message":"Складской учёт выключен"}
// 422
{"error":"rejected","messages":["Не найден товар с внешним кодом p-00000"]}
```
