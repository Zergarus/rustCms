# SEO Metadata Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans.

**Goal:** Редактируемые наследуемые метаданные с публичным API.
**Architecture:** Отдельная таблица и модуль src/seo.rs, форма src/admin/seo.rs, существующие права и маршрутизаторы.
**Tech Stack:** Rust, SQLx/PostgreSQL, Axum, MiniJinja.
**Spec:** docs/superpowers/specs/2026-11-09-seo-metadata-design.md

## Global Constraints

Без новых зависимостей. Не менять существующие bxapi-ответы. Пустое значение наследуется. Поиск вне объёма.

## Review Focus

- Закрытые коллекции и неактивные предки не раскрываются публично.
- Запрещённые изменения не проходят через прямой POST.
- Удаление/перенос объекта не оставляет неверное наследование.
- Пользовательские значения не становятся кодом шаблона или HTML.
- Очистка поля восстанавливает наследование без перезапуска.

### Task 1: Метаданные, форма и API

**Files:** migrations/0028_seo.sql; src/seo.rs; src/admin/seo.rs; templates/admin/seo_form.html; src/main.rs; src/api.rs; src/admin/mod.rs; карточки коллекции/раздела/записи; README.md.

**Interfaces:** `seo::load(db, kind, id) -> Target`; `seo::settings(db, target) -> Settings`; `seo::resolve(db, target) -> Metadata`; `seo::save(db, target, settings)`; `seo::public` — Axum handler. Settings содержит title/description/h1. Target включает владельца, коллекцию, контекст и цепочку разделов.

- [x] Добавить HTTP-тест на отсутствующий API: ожидается title имени объекта, вместо текущего 404. Run: cargo test seo_. Expected: FAIL (404 != 200).
- [x] Реализовать хранение, наследование, простые подстановки, API и форму с существующими правами.
- [x] Проверить реальное сохранение/очистку, цепочку наследования, экранирование, некорректные шаблоны, 403, 404 неактивного/будущего/закрытого контента и каскадное удаление.
- [x] Run: cargo test. Expected: все тесты проходят. Run: cargo clippy --all-targets -- -D warnings и cargo fmt --check. Expected: exit 0.
- [x] Обновить README, провести независимый review и исправить существенные замечания. Зафиксировать только файлы этапа.


## Результат 2026-11-09

Реализованы настройки и наследование Title/Description/H1, форма с правами и публичный API.
Проверки: `cargo test` — 1133 passed; `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `git diff --check` — успешно.
Независимое ревью выявило конфликт 422 с HTMX: исправлен, регрессионный тест прошёл RED→GREEN. HTML валидации возвращается с 200, как в других формах админки.
В Chrome на порту 3000 проверены открытие из карточки, ошибка переменной, сохранение/подстановки, очистка и восстановление значений. Ответ API проверен до и после очистки.

Ограничения: фронтенд Nuxt ещё не подключён, импорт SEO/canonical/robots/sitemap остаются следующими этапами. Глубокие деревья не профилировались (загрузка предков по одному запросу, защита от циклов). Дополнительные тесты сочетаний Read/Write своей/чужой коллекции и переносов подразделов отложены как замечание к покрытию; базовые права, перенос записи и каскадное удаление покрыты.
