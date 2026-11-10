# Поле коллекции «Блоки» — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Поле коллекции `blocks` хранит контент массивом JSON-блоков и отдаёт его фронтенду как JSON.

**Architecture:** Значение — массив объектов `{type, <payload>}` в `field_values` (JSONB), без новой таблицы. Валидация, нормализация и очистка HTML — в `src/collection/blocks.rs`; kind поля подключается в `fields.rs`; админка получает JS-редактор, картинки грузятся отдельной ручкой.

**Tech Stack:** Rust, Axum 0.8, SQLx, MiniJinja, vanильный JS, `ammonia` (очистка HTML).

**Spec:** `docs/superpowers/specs/2026-11-10-blocks-field-design.md`

## Global Constraints

- Блоки хранятся в `field_values.<code>` массивом; `multiple` у поля — `false`.
- Ключи латиницей, содержимое называется по типу блока (`text`, `heading`, `image`, …).
- Пустые блоки отбрасываются; неизвестный тип/лишний ключ/неверный тип значения — ошибка поля.
- HTML очищается `ammonia` на сохранении; ссылки — только `http(s)://`, `/…`, `#…`.
- Загрузка картинок: `POST /admin/blocks/upload?collection=ID`, только изображения, ≤ 10 МБ, право записи в коллекцию.
- Тесты: `DATABASE_URL=postgres://cms:cms@localhost:5433/cms cargo test`.

## Review Focus

- Битая JSON-строка при ручной правке — ошибка поля, значение в записи не меняется.
- `javascript:`-ссылка и `onclick` в HTML-блоке — вырезаются очисткой.
- Пустой блок в редакторе — не попадает в сохранённые данные.
- Не-картинка или файл > 10 МБ — отказ (422 / 413), файл не создаётся.
- `collection`, где нет права записи, — 403 и файл не создаётся.

---

### Task 1: Модель блоков и валидация

**Files:**
- Create: `src/collection/blocks.rs`
- Modify: `src/collection/mod.rs` (объявить `pub mod blocks;`)
- Modify: `Cargo.toml` (`ammonia`)
- Test: unit-тесты в `src/collection/blocks.rs`

**Interfaces:**
- Produces:
  - `pub fn from_form(raw: &str) -> Result<serde_json::Value, String>` — пустая строка → `Value::Null`, иначе JSON-массив после `normalize`.
  - `pub fn normalize(value: &Value) -> Result<Vec<serde_json::Value>, String>` — проверка, нормализация, очистка, отбрасывание пустых.

- [ ] **Step 1: Написать падающие тесты** (`#[cfg(test)] mod tests` в `blocks.rs`): заголовок с `level` по умолчанию `2`; текст с абзацами сохраняется; неизвестный `type` — `Err`; лишний ключ — `Err`; `image` с `javascript:` — `Err`; `html` с `<script>alert(1)</script><p onclick="x">ok</p>` → `<p>ok</p>`; пустые `text`/`heading`/`gallery`/`table` отбрасываются; `from_form("")` → `Null`.
- [ ] **Step 2: `cargo test blocks` — падает** (нет модуля).
- [ ] **Step 3: Реализовать `normalize`/`from_form`**: allowlist типов и ключей, проверки строк/чисел, `ammonia::Builder` с разрешёнными тегами и `class` у контейнеров, `url_ok` (`http(s)://`, `/`, `#`), `image` — `/upload/` или `http(s)://`.
- [ ] **Step 4: `cargo test blocks` — проходит.**
- [ ] **Step 5: Commit** — `git commit -m "Блоки: модель, валидация и очистка HTML"`.

### Task 2: Kind поля `blocks`

**Files:**
- Modify: `src/collection/fields.rs` (KINDS, `parse_value`, `to_form_values`)
- Test: тесты в `src/collection/fields.rs`

**Interfaces:**
- Consumes: `blocks::from_form`.
- Produces: `FieldKind { code: "blocks", name: "Блоки", multiple: false }`; `parse_value` для `blocks` разбирает первую raw-строку целиком (без разбиения по строкам); `to_form_values` отдаёт один компактный JSON (`"[]"` для пустого).

- [ ] **Step 1: Тесты**: `parse_value(&prop("blocks"), &["[{\"type\":\"text\",\"text\":\"a\"}]"], &[])` → массив; битый JSON → `Err` с именем поля; `to_form_values` → `["[{\"type\":\"text\",\"text\":\"a\"}]"]`; обязательное пустое поле → `Err`.
- [ ] **Step 2: `cargo test fields::` — падает.**
- [ ] **Step 3: Реализовать ветки** в `parse_value`/`to_form_values` и запись в `KINDS`.
- [ ] **Step 4: `cargo test fields::` — проходит.**
- [ ] **Step 5: Commit** — `git commit -m "Поля: kind «Блоки»"`.

### Task 3: Отдача в bxapi

**Files:**
- Test: `src/blocks_api_tests.rs` (новый, подключить в `main.rs`)

**Interfaces:**
- Consumes: значение поля из `field_values` (JSONB).
- Produces: в ответе `properties.<code>` — JSON-массив блоков как есть.

- [ ] **Step 1: Тест**: создать коллекцию с полем `blocks`, запись с двумя блоками; `POST /api/v1/iblock/{code}/element/list` (или `element/{id}`) → `data.item.properties.<code>` — массив с `type: "heading"` и `type: "text"`.
- [ ] **Step 2: `cargo test blocks_api` — падает** (поле неизвестно/сериализация строкой).
- [ ] **Step 3: Поправить сериализацию** в `src/bxapi/serialize.rs`/`registry.rs`, если массив не проходит как есть.
- [ ] **Step 4: Тест проходит.**
- [ ] **Step 5: Commit** — `git commit -m "bxapi: блоки отдаются JSON-массивом"`.

### Task 4: Сохранение из админки

**Files:**
- Modify: `templates/admin/item_form.html` (control для `p.kind == "blocks"`)
- Test: `src/blocks_admin_tests.rs`

**Interfaces:**
- Consumes: `fields::parse_value`.
- Produces: POST `/admin/items/{id}` с `prop_<code>` = JSON сохраняет нормализованный массив; ошибка — 200 с формой и текстом ошибки, значение в БД прежнее.

- [ ] **Step 1: Тесты**: multipart-сохранение с валидным JSON → в БД массив, пустой блок отброшен; с битым JSON → 200, в БД прежнее значение, в HTML есть сообщение об ошибке.
- [ ] **Step 2: `cargo test blocks_admin` — падает.**
- [ ] **Step 3: Добавить в шаблон ветку** `{% elif p.kind == "blocks" %}`: `<textarea name="{{ key }}" class="blocks-input" data-collection="{{ collection.id }}" rows="8">{{ form[key] }}</textarea>`.
- [ ] **Step 4: Тесты проходят.**
- [ ] **Step 5: Commit** — `git commit -m "Админка: поле «Блоки» в карточке записи"`.

### Task 5: Загрузка картинки блока

**Files:**
- Create: `src/admin/blocks.rs`
- Modify: `src/admin/mod.rs` (маршрут `POST /admin/blocks/upload`)
- Test: `src/blocks_upload_tests.rs`

**Interfaces:**
- Produces: `pub async fn upload(State, Extension<Access>, Query<UploadQuery>, Multipart) -> AppResult<Response>`; ответ `{ "status": "success", "data": { "path": "/upload/blocks/…", "id": N } }`.

- [ ] **Step 1: Тесты**: PNG → 200 и `path` начинается с `/upload/`; текстовый файл → 422; без `collection` → 400; `collection` без права записи → 403 и файла нет.
- [ ] **Step 2: `cargo test blocks_upload` — падает.**
- [ ] **Step 3: Реализовать**: `user.require_collection(query.collection, Level::Write)?`, лимит 10 МБ, `files::image_size` для проверки, `files::save(..., "blocks", ...)`.
- [ ] **Step 4: Тесты проходят.**
- [ ] **Step 5: Commit** — `git commit -m "Блоки: загрузка картинок"`.

### Task 6: Редактор блоков в админке

**Files:**
- Create: `static/admin/blocks.js`
- Modify: `templates/admin/base.html` (подключить скрипт)
- Modify: `templates/admin/blocks.js`-контракт: textarea `.blocks-input`

**Interfaces:**
- Consumes: `/admin/blocks/upload?collection=ID`.
- Produces: редактор пишет в textarea компактный JSON; при ошибке сервера форма возвращает текст в textarea как есть.

- [ ] **Step 1: Написать `blocks.js`**: парсинг textarea, карточки блоков, добавление (меню типов), вверх/вниз, дублировать, удалить, поля по типу, drag&drop (HTML5), загрузка картинки через fetch, сериализация в textarea на каждое изменение.
- [ ] **Step 2: Подключить скрипт и стили** (минимальные классы в `static/admin/admin.css`).
- [ ] **Step 3: Ручная проверка** на dev-сервере (`BIND_ADDR=127.0.0.1:3010`): добавить/переставить/удалить блок, сохранить, перезагрузить — блоки на месте; загрузить картинку — путь появился.
- [ ] **Step 4: Commit** — `git commit -m "Блоки: редактор в админке"`.

### Task 7: Документация и финальные проверки

**Files:**
- Modify: `README.md`, `docs/core-backlog.md` (пункт 1 → сделан), `project/README.md` (переопределение шаблонов не требуется — рендер на фронте)
- Modify: `docs/tech-debt.md` (TD: язык/поиск по HTML-блоку, ограничение 200 блоков)

- [ ] **Step 1: Обновить README**: формат блоков, поля, загрузка, ограничения.
- [ ] **Step 2: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`** — всё зелёное.
- [ ] **Step 3: Commit** — `git commit -m "Документация: поле «Блоки»"`.
