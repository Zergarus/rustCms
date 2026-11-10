-- Токены оформленных заказов: гость доказывает доступ к своему заказу
-- непрозрачным токеном, а не голым id. В БД только sha256.
CREATE TABLE checkout_tokens (
    token_hash TEXT PRIMARY KEY,
    order_id   BIGINT NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX checkout_tokens_order_idx ON checkout_tokens (order_id);
