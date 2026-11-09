-- Одноразовые токены посетителей: подтверждение регистрации и сброс пароля.
-- В БД только sha256, сам токен уходит письмом.
CREATE TABLE auth_tokens (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind       TEXT NOT NULL CHECK (kind IN ('confirm', 'reset')),
    token_hash TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Один активный токен на вид: новый вытесняет старый
CREATE UNIQUE INDEX auth_tokens_kind_user_idx ON auth_tokens (kind, user_id);
CREATE INDEX auth_tokens_hash_idx ON auth_tokens (token_hash);
