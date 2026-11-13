# Замер CMS против Битрикса и презентация — план

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Получить воспроизводимый замер bxapi-ручек CMS и Битрикса в Docker и собрать md-презентацию с фичами админки и цифрами.

**Architecture:** Один Python-генератор с двумя режимами (latency c=1, throughput c=16) гоняет пять payload'ов фронта по HTTP через nginx к двум приложениям в Docker. Bitrix — отдельный bench-стек из того же образа, CMS — собранный `Dockerfile`; данные общие, только чтение. Результаты — сырые JSON + сводка; презентация — markdown со скриншотами.

**Tech Stack:** Python 3 stdlib (`http.client`, `multiprocessing`), Docker, nginx, `ab` не нужен; Bash; Markdown.

**Spec:** `docs/superpowers/specs/2026-11-13-benchmark-presentation-design.md`

## Global Constraints

- Рабочий `transopt-local` стек не изменять и не перезапускать; Bitrix bench — отдельные контейнеры.
- Оба приложения за nginx, HTTP, `Host: transopt.local`; Битриксу — `X-Forwarded-Proto: https`.
- CMS: `CSRF_ENFORCE=false`, `RATE_LIMIT_PER_MIN=0` только на время замера; в отчёте указать.
- Лимиты приложений: `--cpus=4 --memory=2g`; nginx без лимитов.
- Только чтение; никаких заказов, регистраций и записей.
- Один генератор и одни payload'ы для обеих систем.
- Три перемежающихся прогона A/B; в отчёте медиана и min–max.
- Дамп БД в git не коммитить; зафиксировать counts/дату/коммиты.
- Не выдавать throughput при 16 клиентах за предельную пропускную способность.

## Review Focus

- Медленный ответ при первом запросе (прогрев/OPcache) не должен попасть в замер.
- 308 от Bitrix-nginx не должен считаться ошибкой: проверять `X-Forwarded-Proto`.
- Разные данные/расхождение counts должны остановить замер, а не замаскироваться.
- Генератор не должен быть узким местом на быстрой стороне; для latency — c=1, для throughput — фиксированное время и оговорка.
- Память меряется у приложения, а не у БД/nginx.

---

### Task 1: Генератор нагрузки и payload'ы

**Files:**
- Create: `bench/load.py`
- Create: `bench/payloads/product.json`, `list.json`, `sections.json`, `search.json`, `schemes.json`
- Test: `python3 -m py_compile bench/load.py`

**Interfaces:**
- Consumes: URL, JSON-файл, режим, конкурентность, длительность/число запросов.
- Produces: JSON-результат с полями `mode,url,total,concurrency,elapsed_s,rps,errors,p50_ms,p95_ms,p99_ms,mean_ms`.

- [ ] **Step 1: Создать пять payload'ов из фронта** — те же тела, что в `server/api/...` TransOpt; товар 38382, список с `limit:20`, разделы с `hasElements`, поиск `U660`, схемы с `imageResize`.
- [ ] **Step 2: Написать `bench/load.py`** с `argparse`, keep-alive, прогревом 30, режимами `latency` (c=1, N запросов) и `throughput` (c=N, duration секунд), multiprocessing-воркерами, сбором latency и ошибок.
- [ ] **Step 3: Проверить синтаксис:** `python3 -m py_compile bench/load.py` — без вывода.
- [ ] **Step 4: Smoke на живом CMS:** `python3 bench/load.py --url http://127.0.0.1:3011 --payload bench/payloads/schemes.json --mode latency --requests 5 --concurrency 1` — `errors: 0`.
- [ ] **Step 5: Commit** `bench: генератор нагрузки и payload'ы bxapi`.

### Task 2: Отдельный Bitrix bench-стек и nginx для CMS

**Files:**
- Create: `bench/bitrix/php.ini`, `bench/bitrix/www.conf`, `bench/bitrix/nginx.conf`
- Create: `bench/setup.sh`
- Test: `bash bench/setup.sh --check`

**Interfaces:**
- Consumes: существующие образы/тома TransOpt, Docker-сеть `bitrix-common_bitrix`.
- Produces: `bench-bitrix-nginx`, `bench-cms-nginx`, `cms-bench`; оба nginx отвечают 200 на `GET /api/v1/iblock/list`.

- [ ] **Step 1: Написать `bench/bitrix/php.ini`** — Xdebug не загружается, `opcache.enable=1`, `opcache.jit=1255`, `opcache.validate_timestamps=0`.
- [ ] **Step 2: Написать `bench/bitrix/www.conf`** — `pm=static`, `pm.max_children=32`, `listen=9000`.
- [ ] **Step 3: Написать `bench/bitrix/nginx.conf`** — копия штатного конфига TransOpt с `fastcgi_pass bench-php-fpm:9000`, `client_max_body_size`, без TLS-редиректа при `X-Forwarded-Proto=https`.
- [ ] **Step 4: Написать `bench/setup.sh`** — `docker run` для `bench-php-fpm`, `bench-bitrix-nginx`, `bench-cms-nginx`, `cms-bench`; `--check` проверяет health/200 и печатает URL.
- [ ] **Step 5: Запустить `bash bench/setup.sh --check`** — оба `/api/health` и POST-ручка 200.
- [ ] **Step 6: Commit** `bench: отдельный стенд Битрикса и nginx для CMS`.

### Task 3: Снимок данных и проверка равенства

**Files:**
- Create: `bench/snapshot.sh`
- Create: `bench/snapshot.json`
- Test: `bash bench/snapshot.sh`

**Interfaces:**
- Produces: counts коллекций/разделов, наличие товара 38382, результаты поиска, дата, коммиты CMS; JSON для отчёта.

- [ ] **Step 1: Написать `bench/snapshot.sh`** — curl'ом получить counts/товар/поиск с обоих стендов, записать JSON; при несовпадении counts — exit 1.
- [ ] **Step 2: Запустить** — `bench/snapshot.json` создан, товар 38382 есть в обеих системах, counts совпадают.
- [ ] **Step 3: Commit** `bench: снимок данных для сравнения`.

### Task 4: Прогон замеров

**Files:**
- Create: `bench/run.sh`
- Create: `bench/results/*.json`
- Test: `bash bench/run.sh`

**Interfaces:**
- Consumes: URL'ы из Task 2, payload'ы Task 1, снимок Task 3.
- Produces: 5 ручек × 2 системы × 2 режима × 3 прогона; сырые JSON.

- [ ] **Step 1: Написать `bench/run.sh`** — три раунда, в каждом чередовать CMS/Bitrix по ручкам; latency и throughput; сохранять `bench/results/<endpoint>-<system>-<mode>-<round>.json`.
- [ ] **Step 2: Запустить** — все файлы созданы, `errors=0`; при ошибке остановиться и разобрать.
- [ ] **Step 3: Проверить разброс** — rps между раундами в пределах разумного; иначе повторить раунд и записать.
- [ ] **Step 4: Commit** `bench: результаты замера CMS и Битрикса`.

### Task 5: Отчёт о замере

**Files:**
- Create: `docs/benchmark-v2.md`
- Modify: `README.md` (ссылка на отчёт)
- Test: прочитать отчёт, сверить цифры с `bench/results/`

**Interfaces:**
- Consumes: `snapshot.json`, `results/*.json`.
- Produces: стенд, методика, таблицы latency/throughput/память, оговорки, команды.

- [ ] **Step 1: Свести медианы и min–max** из результатов.
- [ ] **Step 2: Написать `docs/benchmark-v2.md`** — дата, коммиты, железо, Docker, PHP-конфиг, лимиты, 5 ручек, таблицы, оговорки (TLS, MariaDB/Postgres, dev-стенд, live data).
- [ ] **Step 3: Добавить ссылку в README.**
- [ ] **Step 4: Commit** `docs: замер CMS против Битрикса v2`.

### Task 6: Презентация и скриншоты

**Files:**
- Create: `docs/presentation.md`
- Create: `docs/design/presentation-*.png` (3–4 свежих)
- Test: открыть markdown, проверить ссылки на изображения

**Interfaces:**
- Consumes: `benchmark-v2.md`, существующие `docs/design/*.png`.
- Produces: 10–15-минутный рассказ: проблема, архитектура, демо, API, цифры, ограничения, P0.

- [ ] **Step 1: Снять свежие скриншоты** — дашборд, коллекция/запись, заказ, инструменты; 1440×900, без личных данных.
- [ ] **Step 2: Написать `docs/presentation.md`** — 6–8 слайдов-разделов, таблицы, ссылки на скриншоты, честная рамка «Битрикс:Бизнес без CRM».
- [ ] **Step 3: Проверить ссылки/цифры** — все изображения существуют, цифры совпадают с отчётом.
- [ ] **Step 4: Commit** `docs: презентация CMS для тимлида`.

### Task 7: Финальная проверка

- [ ] `cargo test` (если код CMS не менялся — пропустить и записать это).
- [ ] `git status` — чисто, все результаты закоммичены.
- [ ] Повторный `bash bench/setup.sh --check` — стенд жив.
- [ ] Commit финальных правок, если были.
