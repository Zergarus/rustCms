# Sitemap API Implementation Plan (ревизия 2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (исполнение выбрано ранее).

**Goal:** Типы sitemap настраиваются в админке и хранятся в БД по образцу `b_seo_sitemap`, API отдаёт их в контракте Битрикса.

**Architecture:** Миграция `0030` с таблицами `seo_sitemap` и `seo_sitemap_collection`; модель и рендер путей — в `src/sitemap.rs`; CRUD-страница в `src/admin/seo_sitemap.rs`; API читает активные типы из БД.

**Spec:** `docs/superpowers/specs/2026-11-09-sitemap-api-design.md`

## Global Constraints

- Контракт ответов, порядок полей и `lastmod` RFC3339 `+00:00` не меняются.
- Никаких упоминаний конкретного сайта в коде CMS: коды коллекций и шаблоны путей — только данные.
- Существующие ответы (`/api/v1/seo/{kind}/{id}`, bxapi) не меняются.
- Видимость: `collections.api_enabled`, `collection_items.active` + наступившая публикация, разделы `active`.
- Без новых зависимостей.

## Review Focus

- Проект без настроенных типов — индекс 200 с пустым `items`, а не 404 и не 500.
- Тип с `active = false` — 404 по его URL и отсутствие в индексе.
- Удаление коллекции — связи типа чистятся каскадом, тип остаётся с нулём записей.
- Шаблон с `#ELEMENT_CODE_OR_ID#` при пустом коде даёт id, путь без `//` и с одним слэшем на конце.
- Тип из двух коллекций — счётчик сумма, `lastmod` максимум.

---

### Task 1: Миграция, модель и рендер путей

**Files:** Create `migrations/0030_seo_sitemap.sql`; Modify `src/sitemap.rs`.

**Interfaces:**
- `pub struct Entry { id, name, active, path_template, include, sort, collections: Vec<Source> }`, `pub struct Source { collection_id, code, detail_page_url, section_page_url }`;
- `pub async fn list(db: &PgPool, active_only: bool) -> AppResult<Vec<Entry>>`;
- `pub fn render(template: &str, ctx: &Ctx) -> String`, `pub fn normalize(path: &str) -> String`;
- `struct Ctx { section_path, section_code, element_id, element_code }` + `#SITE_DIR#`, `#ELEMENT_CODE_OR_ID#`.

- [ ] Тест RED: `list` возвращает типы по `sort,id` со связями; `render` подставляет `#ELEMENT_CODE_OR_ID#`, `#SECTION_CODE_PATH#`; `normalize` делает нижний регистр и слэш.
- [ ] Миграция + модель + рендер → тест GREEN.

### Task 2: API на данных из БД

**Files:** Modify `src/sitemap.rs` (`index`, `kind`), `src/sitemap_tests.rs`.

- [ ] Тесты RED: существующие два теста переписать на сиды `seo_sitemap`; добавить: пустая конфигурация → пустой индекс; выключенный тип → 404; тип из двух коллекций → сумма/максимум; `include = sections`.
- [ ] Реализация: агрегаты и записи по коллекциям типа, рекурсивный CTE для `#SECTION_CODE_PATH#`, сортировка коллекций по id → GREEN.

### Task 3: Админка «SEO → Sitemap»

**Files:** Create `src/admin/seo_sitemap.rs`, `templates/admin/sitemap_list.html`, `templates/admin/sitemap_form.html`; Modify `src/admin/mod.rs`, `src/admin/nav.rs`, `src/main.rs`.

- [ ] Тесты RED: список 200 для `collections.manage`, 403 без права; создание/правка/удаление через форму; невалидное имя типа → сообщение об ошибке и данные не сохранены.
- [ ] Реализация CRUD + шаблоны + пункт меню → GREEN.

### Task 4: README, сверка с Битриксом, финальные проверки

- [ ] README: как настроить типы, что путь берётся из шаблона коллекции или типа, чего нет (файлы, changefreq).
- [ ] `cargo test`, `clippy -D warnings`, `fmt --check`, `git diff --check`.
- [ ] Локальная база ТрансОпта: завести 5 типов (products/news/schemes/contacts + catalog пустой), сверить счётчики и пути с Битриксом на копии CMS.

## Результат 2026-11-09

Типы sitemap живут в `seo_sitemap` + `seo_sitemap_collection` (миграция `0030`),
API читает их из БД, страница «SEO → Sitemap» — в `src/admin/seo_sitemap.rs`.
`cargo test` — 1140 passed; clippy/fmt/diff — чисто. Сверка с Битриксом:
4330 / 0 / 143 / 421 / 0, пути `products` и `news` совпадают.
