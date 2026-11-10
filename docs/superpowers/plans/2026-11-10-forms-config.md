# Формы как конфигурация — план

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans.

**Goal:** Форма описывается `config/forms.toml` и реестром, а не только кодом проекта; эндпоинт дополнительно принимает multipart с файлами.

**Architecture:** `Forms` (owned-конфиг) в `AppState`; сборка из `Project` + файла с оверрайдом по коду; `forms.rs` работает с этой моделью.

**Spec:** `docs/superpowers/specs/2026-11-10-forms-config-design.md`

## Global Constraints

- Контракт `POST /form/{code}` не меняется: `{id, data}`, те же коды ошибок.
- Правила — `required`, `email` (семантика модуля: `email` только если значение пришло).
- Неизвестное правило в файле — ошибка старта, не молчаливое пропускание.
- Без правок фронта; формы проекта в коде продолжают работать.

## Tasks

### Task 1: Модель и реестр форм

**Files:** Create `src/bxapi/forms_config.rs`; Modify `src/state.rs`, `src/main.rs`, `src/test_support.rs`, `src/bxapi/mod.rs`; add `toml` в `Cargo.toml`.

**Interfaces:**
- `pub struct FormSpec { code, rules: Vec<(String, Vec<Rule>)>, writer: Option<WriterSpec>, notifier: Option<NotifierSpec>, files: Vec<(String, String)> }`;
- `pub struct WriterSpec { collection: String, mapping: Vec<(String, String)> }`, `NotifierSpec { event: String, mapping: Vec<(String, String)> }`;
- `pub struct Forms { … }`: `from_project(&Project) -> Forms`, `load(&Project) -> anyhow::Result<Forms>` (файл `FORMS_CONFIG` или `config/forms.toml`), `get(&str) -> Option<&FormSpec>`, `insert(FormSpec)`;
- `AppState.forms: Arc<Forms>`.

- [ ] Тест RED: разбор TOML, оверрайд кодовой формы файлом, неизвестное правило — ошибка.
- [ ] Реализация → GREEN.

### Task 2: `forms.rs` на реестре

**Files:** Modify `src/bxapi/forms.rs`.

- [ ] Тесты RED: форма из файла пишет запись в коллекцию и шлёт письмо; неизвестная форма — 404; ошибки валидации прежние; JSON-контракт не изменился.
- [ ] Перевести `validate`/`write_element`/notifier на `FormSpec` → GREEN.

### Task 3: Multipart и файлы

**Files:** Modify `src/bxapi/forms.rs`.

- [ ] Тест RED: `multipart/form-data` с файлом создаёт запись, файл сохранён, id в поле коллекции; недопустимое расширение — ошибка.
- [ ] Разбор тела по `Content-Type`, сохранение через `files::save`, маппинг `files` → GREEN.

### Task 4: Образец конфига, README, проверки

- [ ] `config/forms.toml.example` с двумя формами (callback/brief), README-раздел.
- [ ] `cargo test`, clippy `-D warnings`, `cargo fmt --check`, `git diff --check`.

## Результат 2026-11-10

Реестр форм `src/bxapi/forms_config.rs` (TOML + оверрайд кодовых форм), `AppState.forms`,
`forms.rs` работает с `FormSpec` и принимает multipart с файлами (сохранение через
`files::save`, id в поле коллекции), образец `config/forms.toml.example`, раздел в
README. `cargo test` — 1160 passed; clippy, fmt, diff — чисто.
