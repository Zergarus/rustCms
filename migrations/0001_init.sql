-- Пользователи и сессии админки
CREATE TABLE users (
    id            BIGSERIAL PRIMARY KEY,
    login         TEXT NOT NULL UNIQUE,
    email         TEXT,
    password_hash TEXT NOT NULL,
    is_admin      BOOLEAN NOT NULL DEFAULT FALSE,
    active        BOOLEAN NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- В БД хранится только sha256 от токена, сам токен живёт в cookie
CREATE TABLE sessions (
    token_hash TEXT PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX sessions_user_id_idx ON sessions (user_id);

-- Инфоблоки: произвольные типы контента (новости, статьи, товары...)
CREATE TABLE iblocks (
    id          BIGSERIAL PRIMARY KEY,
    code        TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    api_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    sort        INT NOT NULL DEFAULT 500,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Схема свойств инфоблока
CREATE TABLE iblock_properties (
    id          BIGSERIAL PRIMARY KEY,
    iblock_id   BIGINT NOT NULL REFERENCES iblocks (id) ON DELETE CASCADE,
    code        TEXT NOT NULL,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN ('string', 'text', 'number', 'boolean', 'date')),
    is_required BOOLEAN NOT NULL DEFAULT FALSE,
    sort        INT NOT NULL DEFAULT 500,
    UNIQUE (iblock_id, code)
);

-- Элементы инфоблока; значения свойств лежат в JSONB по коду свойства
CREATE TABLE iblock_elements (
    id           BIGSERIAL PRIMARY KEY,
    iblock_id    BIGINT NOT NULL REFERENCES iblocks (id) ON DELETE CASCADE,
    code         TEXT NOT NULL,
    name         TEXT NOT NULL,
    active       BOOLEAN NOT NULL DEFAULT TRUE,
    sort         INT NOT NULL DEFAULT 500,
    preview_text TEXT NOT NULL DEFAULT '',
    detail_text  TEXT NOT NULL DEFAULT '',
    published_at TIMESTAMPTZ,
    properties   JSONB NOT NULL DEFAULT '{}',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (iblock_id, code)
);
CREATE INDEX iblock_elements_list_idx ON iblock_elements (iblock_id, active, sort, id);
CREATE INDEX iblock_elements_props_idx ON iblock_elements USING GIN (properties);
