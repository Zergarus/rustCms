-- Пользователи сайта живут в той же таблице, что и админы (как b_user в Битриксе):
-- в админку пускают только права групп.
ALTER TABLE users
    ADD COLUMN last_name   TEXT NOT NULL DEFAULT '',
    -- Откуда пользователь: 'bitrix:<ID>' — перенесён из Битрикса
    ADD COLUMN external_id TEXT;
CREATE UNIQUE INDEX users_external_id_key ON users (external_id) WHERE external_id IS NOT NULL;
CREATE INDEX users_email_lower_idx ON users (lower(email));

-- Сессии посетителей сайта (API bxapi); в БД — только sha256 от токена из cookie
CREATE TABLE site_sessions (
    token_hash TEXT PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX site_sessions_user_id_idx ON site_sessions (user_id);
