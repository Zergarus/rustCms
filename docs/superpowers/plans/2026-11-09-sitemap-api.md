# Sitemap API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Отдать фронту два sitemap-эндпоинта в контракте Битрикса, чтобы переключение `API_BASE_URL` на CMS не ломало индексацию.

**Architecture:** Новый модуль `src/sitemap.rs` с таблицей из пяти типов и двумя Axum-хендлерами; данные берутся из `collection_items`/`collections` теми же правилами видимости, что у публичного API. Ответы — в битриксовой обёртке `status/data/errors` через `bxapi::success`.

**Tech Stack:** Rust, Axum 0.8, SQLx/PostgreSQL, serde_json, chrono.

**Spec:** `docs/superpowers/specs/2026-11-09-sitemap-api-design.md`

## Global Constraints

- robots.txt не трогаем — он остаётся статическим файлом на фронте.
- Существующие ответы (`/api/v1/seo/{kind}/{id}`, bxapi) не меняются.
- Пять типов и их порядок фиксированы: products, catalog, news, schemes, contacts.
- `catalog` всегда отдаёт `count: 0` и пустой `items` — до отдельной задачи про slug семейств.
- Видимость: `collections.api_enabled`, `collection_items.active`, `published_at IS NULL OR published_at <= now()`; разделы не фильтруются.
- `lastmod` — RFC3339 с `+00:00` (`2026-08-26T11:19:28+00:00`), `null` в индексе при нуле записей.
- Без новых зависимостей.

## Review Focus

- Фронт запрашивает `catalog` и `contacts` — ожидает 200 с пустым списком, а не 404 и не ошибку.
- Коллекция с `api_enabled = false` — ожидается 0 записей, а не все её записи.
- Запись с будущей `published_at` — не попадает в sitemap.
- Товар с пустым `code` — путь по числовому id, без `//` в адресе.
- Неизвестный тип в URL — 404 в обычном формате ошибок API, а не 500 и не пустой список.

---

### Task 1: Модуль sitemap и индекс-эндпоинт

**Files:**
- Create: `src/sitemap.rs`
- Create: `src/sitemap_tests.rs`
- Modify: `src/main.rs` (рядом с `mod seo_tests;` — добавить `mod sitemap;` и `mod sitemap_tests;`)
- Modify: `src/api.rs:20-31` (маршрут в `router()`)

**Interfaces:**
- Consumes: `crate::state::AppState`, `crate::error::{AppError, AppResult}`, `crate::bxapi::success(serde_json::Value) -> axum::response::Response`, `crate::test_support::{get_raw, test_state}`.
- Produces:
  - `pub enum Kind { Products, Catalog, News, Schemes, Contacts }` с `fn name(self) -> &'static str`, `fn collection(self) -> Option<&'static str>` (`Products => "catalog"`, `News => "info"`, `Schemes => "schemes"`, `Contacts => "contacts"`, `Catalog => None`);
  - `pub async fn index(State<AppState>) -> AppResult<Response>`;
  - приватный `async fn count(db: &PgPool, collection: &str) -> AppResult<(i64, Option<DateTime<Utc>>)>`.

- [ ] **Step 1: Write the failing test**

В `src/sitemap_tests.rs`:

```rust
use crate::test_support::{get_raw, test_state};
use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;

/// Фикстура: catalog (3 активных — из них один с пустым кодом и один с заглавными
/// в коде, 1 неактивная, 1 с будущей публикацией), info (2), schemes (1).
/// Коллекция `contacts` отсутствует вовсе.
async fn fixture(db: &PgPool) -> PgPool {
    for q in [
        "INSERT INTO collections(code,name,api_enabled) VALUES ('catalog','Каталог',TRUE)",
        "INSERT INTO collections(code,name,api_enabled) VALUES ('info','Статьи',TRUE)",
        "INSERT INTO collections(code,name,api_enabled) VALUES ('schemes','Схемы',TRUE)",
    ] {
        sqlx::query(q).execute(db).await.unwrap();
    }
    let item = |collection: &str, code: &str, active: bool, published: Option<&str>| {
        format!(
            "INSERT INTO collection_items(collection_id,code,name,active,published_at)
             SELECT id,'{code}','Запись {code}',{active},{published} FROM collections WHERE code='{collection}'"
        )
    };
    for (collection, code, active, published) in [
        ("catalog", "catalog1", true, None),
        ("catalog", "CATALOG2", true, None),   // путь должен прийти в нижнем регистре
        ("catalog", "", true, None),           // пустой код — путь по id
        ("catalog", "catalog4", false, None),  // неактивная
        ("catalog", "catalog5", true, Some("'2999-01-01'")), // будущая публикация
        ("info", "info1", true, None),
        ("info", "info2", true, None),
        ("schemes", "schemes1", true, None),
    ] {
        sqlx::query(&item(collection, code, active, published)).execute(db).await.unwrap();
    }
    db.clone()
}

#[sqlx::test]
async fn sitemap_index_counts_and_order(db: PgPool) {
    let db = fixture(&db).await;
    let app = crate::app(test_state(db));
    let (status, _, body) = get_raw(&app, "/api/v1/seo/sitemap", "").await;

    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "success");
    assert_eq!(json["errors"], serde_json::json!([]));

    let items = json["data"]["items"].as_array().unwrap();
    let types: Vec<&str> = items.iter().map(|i| i["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["products", "catalog", "news", "schemes", "contacts"]);

    let count = |t: &str| items.iter().find(|i| i["type"] == t).unwrap()["count"].as_i64().unwrap();
    assert_eq!(count("products"), 3);
    assert_eq!(count("catalog"), 0);
    assert_eq!(count("news"), 2);
    assert_eq!(count("schemes"), 1);
    assert_eq!(count("contacts"), 0);

    for t in ["catalog", "contacts"] {
        assert!(items.iter().find(|i| i["type"] == t).unwrap()["lastmod"].is_null(), "{t}");
    }
    let lastmod = items[0]["lastmod"].as_str().unwrap();
    assert!(lastmod.ends_with("+00:00"), "{lastmod}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test sitemap_index_counts_and_order`
Expected: FAIL — нет модуля `sitemap`, `/api/v1/seo/sitemap` не маршрутизируется (404).

- [ ] **Step 3: Implement**

`src/sitemap.rs`:

```rust
pub enum Kind { Products, Catalog, News, Schemes, Contacts }
impl Kind {
    pub const ALL: [Kind; 5] = [Kind::Products, Kind::Catalog, Kind::News, Kind::Schemes, Kind::Contacts];
    pub fn name(self) -> &'static str { /* products/catalog/news/schemes/contacts */ }
    fn collection(self) -> Option<&'static str> { /* по таблице из спеки, Catalog => None */ }
}
pub async fn index(State(state): State<AppState>) -> AppResult<Response>
async fn count(db: &PgPool, collection: &str) -> AppResult<(i64, Option<DateTime<Utc>>)>
```

SQL подсчёта: `SELECT count(*), max(i.updated_at) FROM collection_items i JOIN collections c ON c.id = i.collection_id WHERE c.code = $1 AND c.api_enabled AND i.active AND (i.published_at IS NULL OR i.published_at <= now())`; для отсутствующей коллекции функция возвращает `(0, None)`.
`lastmod` форматируется `to_rfc3339_opts(SecondsFormat::Secs, false)` в строку, `None` — `Value::Null`.
Ответ: `crate::bxapi::success(json!({ "items": items }))`, `items` — массив `{"type","count","lastmod"}` в порядке `Kind::ALL`.

В `src/api.rs` перед маршрутом `/v1/seo/{kind}/{id}`:
```rust
.route("/v1/seo/sitemap", get(crate::sitemap::index))
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test sitemap_index_counts_and_order`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/sitemap.rs src/sitemap_tests.rs src/main.rs src/api.rs
git commit -m "SEO: sitemap — индекс типов и счётчики"
```

---

### Task 2: Эндпоинт по типу, пути и 404

**Files:**
- Modify: `src/sitemap.rs` (публичный `kind`, построение путей)
- Modify: `src/sitemap_tests.rs` (тест ниже)
- Modify: `src/api.rs` (маршрут типа)

**Interfaces:**
- Consumes: `Kind`, `count` из Task 1.
- Produces: `pub async fn kind(State<AppState>, Path<String>) -> AppResult<Response>`; `fn Kind::parse(raw: &str) -> Option<Kind>`; `fn Kind::path(self, id: i64, code: &str) -> Option<String>` — `Products` даёт `/product/{code.to_lowercase()}/`, при пустом коде `/product/{id}/`; `News` — `/news/{code}/`; `Schemes` — `/schemes/{code}/`; `Contacts` — `/contacts/{id}/`; `Catalog` — `None`.

- [ ] **Step 1: Write the failing test**

```rust
#[sqlx::test]
async fn sitemap_items_paths_and_unknown_type(db: PgPool) {
    let db = fixture(&db).await;
    let app = crate::app(test_state(db.clone()));

    let (status, _, body) = get_raw(&app, "/api/v1/seo/sitemap/products", "").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["data"]["type"], "products");
    let paths: Vec<&str> = json["data"]["items"].as_array().unwrap().iter().map(|i| i["path"].as_str().unwrap()).collect();
    // catalog1, catalog2 — по коду; запись без кода — по id
    assert!(paths.contains(&"/product/catalog1/"), "{paths:?}");
    assert!(paths.contains(&"/product/catalog2/"), "{paths:?}");
    assert!(paths.iter().any(|p| p.starts_with("/product/") && p.trim_end_matches('/').rsplit('/').next().unwrap().parse::<i64>().is_ok()), "{paths:?}");
    assert_eq!(paths.len(), 3);
    assert!(!paths.iter().any(|p| p.contains("//")), "{paths:?}");
    let lastmod = json["data"]["items"][0]["lastmod"].as_str().unwrap();
    assert!(lastmod.ends_with("+00:00"), "{lastmod}");

    let (_, _, body) = get_raw(&app, "/api/v1/seo/sitemap/news", "").await;
    let json: Value = serde_json::from_slice(&body).unwrap();
    let paths: Vec<&str> = json["data"]["items"].as_array().unwrap().iter().map(|i| i["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["/news/info1/", "/news/info2/"]);

    let (_, _, body) = get_raw(&app, "/api/v1/seo/sitemap/schemes", "").await;
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["data"]["items"][0]["path"], "/schemes/schemes1/");

    for t in ["catalog", "contacts"] {
        let (status, _, body) = get_raw(&app, &format!("/api/v1/seo/sitemap/{t}"), "").await;
        assert_eq!(status, StatusCode::OK);
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["data"]["items"].as_array().unwrap().len(), 0, "{t}");
        assert_eq!(json["data"]["type"], t);
    }

    let (status, _, body) = get_raw(&app, "/api/v1/seo/sitemap/nope", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "not_found");

    // Выключенная из API коллекция не отдаёт записи, хотя они активны и опубликованы.
    sqlx::query("UPDATE collections SET api_enabled = FALSE WHERE code = 'info'")
        .execute(&db)
        .await
        .unwrap();
    let (status, _, body) = get_raw(&app, "/api/v1/seo/sitemap/news", "").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["data"]["items"].as_array().unwrap().len(), 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test sitemap_items_paths_and_unknown_type`
Expected: FAIL — 404 на `/api/v1/seo/sitemap/products`.

- [ ] **Step 3: Implement**

`kind` разрешает тип через `Kind::parse` (иначе `AppError::NotFound`), повторяет SQL из `count`, но выбирает `i.id, i.code, i.updated_at` (тот же фильтр видимости, `ORDER BY i.id`), строит путь через `Kind::path` и отдаёт `crate::bxapi::success(json!({ "type": kind.name(), "items": items }))`. Для `Catalog` и отсутствующей коллекции — пустой `items` без запроса.

В `src/api.rs` сразу после индекса:
```rust
.route("/v1/seo/sitemap/{kind}", get(crate::sitemap::kind))
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test sitemap_items_paths_and_unknown_type`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/sitemap.rs src/sitemap_tests.rs src/api.rs
git commit -m "SEO: sitemap — записи по типам и 404 для неизвестного"
```

---

### Task 3: README, сверка с живым API и финальные проверки

**Files:**
- Modify: `README.md` (раздел «SEO: метаданные», после абзаца про API)

**Interfaces:**
- Consumes: оба эндпоинта из Task 1–2.
- Produces: ничего для кода.

- [ ] **Step 1: Дополнить README**

Абзац о sitemap: два маршрута, что отдают, что `catalog` пока пустой (slug семейств — отдельная задача), что `robots.txt` остаётся на фронте, что ответы в контракте Битрикса с `lastmod` в RFC3339. Одна-две фразы, стиль соседних абзацев.

- [ ] **Step 2: Прогнать все проверки**

Run: `cargo test`
Expected: все тесты проходят (было 1134 + 2 новых).

Run: `cargo clippy --all-targets -- -D warnings` и `cargo fmt --check` и `git diff --check`
Expected: exit 0 у каждой.

- [ ] **Step 3: Сверить с живым Битриксом на запущенной CMS**

Run: `cargo run` (или уже собранный бинарь), затем
`curl -s http://127.0.0.1:3000/api/v1/seo/sitemap`.
Expected: пять типов; `products` = 4330, `news` = 143, `schemes` = 421, `catalog` = 0, `contacts` = 0 — как у `https://transopt.local/api/v1/seo/sitemap`.
Дополнительно: `curl -s http://127.0.0.1:3000/api/v1/seo/sitemap/products | head -c 300` — первый путь совпадает по форме с битриксовым (`/product/…/`).

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "SEO: sitemap в README"
```
