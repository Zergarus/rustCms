# Установка и эксплуатация

## Что нужно

- Rust 1.95 или новее (сборка из исходников) либо Docker;
- PostgreSQL 17;
- 256 МБ памяти на процесс CMS (плюс база), места — по объёму файлов и писем;
- Node.js не нужен: админка серверная, фронтенд — отдельное приложение.

## Быстрый старт для разработки

```bash
docker compose up -d          # PostgreSQL на localhost:5433
cp .env.example .env
cargo run -- migrate          # применяет миграции
cargo run -- create-admin admin
cargo run                     # http://127.0.0.1:3000/admin
```

Миграции применяются и при старте `serve`, отдельная команда нужна для
подготовки базы до первого запуска. Проверка живости — `GET /api/health`:
`{"status":"ok"}` и успешный `SELECT 1`.

## Переменные окружения

| Переменная | Назначение |
|---|---|
| `DATABASE_URL` | строка подключения к PostgreSQL, обязательна |
| `BIND_ADDR` | адрес и порт, по умолчанию `127.0.0.1:3000` |
| `CORS_ORIGINS` | домены витрины через запятую, `*` — любой (только для разработки) |
| `TEMPLATES_DIR`, `STATIC_DIR` | каталоги шаблонов и статики админки |
| `UPLOAD_DIR` | загруженные файлы; том, который нужно бэкапить |
| `UPLOAD_ORIGIN_URL` | откуда докачивать старые `/upload/*` (например, с Битрикса) |
| `MAIL_SMTP_URL` | SMTP; важнее настроек в админке, пусто — письма в `MAIL_DIR` |
| `MAIL_DIR` | каталог `.eml`, если SMTP не настроен |
| `COOKIE_SECURE` | `true` за HTTPS |
| `CURRENCY_BANK_URL` | подмена адреса банка курсов (тесты, прокси) |
| `FORMS_CONFIG` | путь к `forms.toml`, по умолчанию `config/forms.toml` |
| `RUST_LOG` | например `cms=info,tower_http=warn` |

## Продакшен

### Docker

```bash
cp .env.example .env            # заполнить POSTGRES_PASSWORD, CORS_ORIGINS, MAIL_SMTP_URL
docker compose -f docker-compose.prod.yml up -d --build
```

Образ — `Dockerfile`: сборка в `rust:1.95`, рантайм `debian-slim` с
`HEALTHCHECK` на `/api/health`. Тома: база, `upload`, `mail`. CMS слушает
`127.0.0.1:3000`, наружу — через nginx.

### systemd без Docker

Сборка `cargo build --release`, раскладка в `/opt/cms` (бинарник, `templates`,
`static`, `migrations`), окружение — `/etc/cms/cms.env`. Пример юнита —
`deploy/cms.service.example`, конфиг nginx — `deploy/nginx.conf.example`
(учитывает лимит формы 20 МБ, проксирует `/upload/` с кэшем).

### Обязательно за HTTPS

`COOKIE_SECURE=true`, `CORS_ORIGINS` — только домен витрины. Письма о
регистрации и сбросе пароля содержат токены, по HTTPS они не текут в открытом
виде.

## Обновление

1. Бэкап: `pg_dump` базы и архив `UPLOAD_DIR`/`MAIL_DIR`.
2. Новая версия: `git pull` и `cargo build --release` либо
   `docker compose -f docker-compose.prod.yml up -d --build`.
3. Перезапуск: миграции применяются при старте `serve`.
4. Проверка: `/api/health`, вход в админку, один публичный запрос витрины.

Откат: вернуть предыдущий бинарник/образ и восстановить бэкап базы — миграции
вперёд несовместимы с откатом кода, поэтому бэкап перед обновлением
обязателен.

## Бэкапы

- база: `pg_dump -Fc` по расписанию, хранить вне сервера;
- `UPLOAD_DIR` — файлы контента, без них база ссылается в пустоту;
- `MAIL_DIR` — только если SMTP не настроен (иначе писем на диске нет).

## Что дальше

Новый проект: [new-project.md](new-project.md). Возможности CMS — в
[README](../README.md), расширения проекта — в
[project/README.md](../project/README.md).
