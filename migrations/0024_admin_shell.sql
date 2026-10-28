-- Оболочка админки: открытые вкладки пользователя и его настройки интерфейса
CREATE TABLE admin_tabs (
    id        BIGSERIAL PRIMARY KEY,
    user_id   BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key       TEXT NOT NULL,
    url       TEXT NOT NULL,
    title     TEXT NOT NULL,
    kind      TEXT NOT NULL CHECK (kind IN ('content', 'shop', 'system')),
    opened_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, key)
);
ALTER TABLE users ADD COLUMN admin_prefs JSONB NOT NULL DEFAULT '{}';
