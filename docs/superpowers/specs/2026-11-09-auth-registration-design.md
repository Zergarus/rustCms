# Регистрация и пароли (bxapi)

Закрывает P1-3 из `docs/bitrix-parity.md`: сценарии, без которых витрину нельзя
переключить на CMS. Контракт снят с фронта (`server/api/auth/*`, `app/stores/authStore.ts`),
исходников модуля `mediagroup.bxapi` в checkout нет — поэтому контракт и есть спека.

## Контракт

| Маршрут | Тело | Ответ |
|---|---|---|
| `POST /api/v1/auth/register` | `name, email, phone, password, passwordConfirm, personalData, smartCaptchaToken?` | `user: null, csrfToken, pending: true` |
| `POST /api/v1/auth/confirm-registration` | `login, confirmCode` | `user, csrfToken` |
| `POST /api/v1/auth/forgot-password` | `login?` | `sent: true` |
| `POST /api/v1/auth/reset-password` | `login, checkword, password, passwordConfirm` | `user, csrfToken, changed: true, authorized: true` |

Обёртка и ошибки — как у остального bxapi: `status/data/errors`, прикладные
ошибки с HTTP 200 и `errors[0].code`.

## Решения

- **Логин = email** (в форме регистрации логина нет), в нижнем регистре; `users.login` уникален.
- **Подтверждение обязательно:** пользователь создаётся `active = false`, вход до
  подтверждения запрещён. `confirm-registration` активирует и сразу выдаёт сессию.
- **Токены** — таблица `auth_tokens(user_id, kind, token_hash, expires_at, created_at)`,
  `kind ∈ {confirm, reset}`; в БД только sha256. Confirm живёт 7 дней, reset — 2 часа,
  новый токен вида вытесняет старый.
- **Коды ошибок**, которые различает фронт: `captcha_failed`, `email_exists`,
  `invalid_confirm_code`, `invalid_checkword`, `password_mismatch`, `weak_password`
  (минимум 6 символов — как правило фронта), `personal_data_required`, `invalid_email`.
- **Капча не входит в этот блок** (решение пользователя): `smartCaptchaToken`
  принимаем и игнорируем, чтобы витрина не менялась; защита от роботов — отдельная
  проектная задача (TD-010).
- **Письма:** существующие шаблоны из импорта — `NEW_USER_CONFIRM`
  (`#CONFIRM_CODE#`, `#LOGIN#`, `#EMAIL#`, `#USER_ID#`) и `USER_PASS_REQUEST`
  (`#CHECKWORD#`, `#LOGIN#`). Нет активного шаблона — `warn`, сценарий не падает.
- **`forgot-password` всегда отвечает `sent: true`** — существование аккаунта не раскрываем.
- **Сброс пароля** меняет пароль, активирует пользователя, гасит остальные токены и
  все его сессии, выдаёт новую сессию (`authorized: true`).
- **Анти-брутфорс:** в памяти процесса — не больше 5 попыток на пару (IP, login) за
  15 минут. Ограничение однопроцессное; при нескольких инстансах понадобится общий
  счётчик (в техдолг).

## Файлы

- `migrations/0031_auth_tokens.sql` — таблица токенов.
- `src/bxapi/register.rs` — четыре хендлера, проверки, выдача сессии.
- `src/bxapi/mod.rs` — маршруты.
- `src/register_tests.rs` — HTTP-тесты на `sqlx::test`.
- `README.md` — раздел про регистрацию и пароли.

`src/mail.rs` не меняется: используется существующий `send_event`.

## Проверка

TDD по каждому шагу: регистрация создаёт неактивного пользователя и токен;
повторный email — `email_exists`; слабый пароль и несовпадение — ошибки;
подтверждение активирует и выдаёт сессию, неверный код — `invalid_confirm_code`;
`forgot-password` создаёт reset-токен и не раскрывает аккаунт; сброс меняет пароль,
гасит старые сессии и авторизует.

## Не входит

SMS-подтверждение, соцвход, 2FA, «запомнить меня» при регистрации, реальная
отправка почты в тестах (проверяем шаблоны и токены, а не SMTP).
