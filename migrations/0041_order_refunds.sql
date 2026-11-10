-- Возвраты по заказу: полные и частичные.
CREATE TABLE order_refunds (
    id         BIGSERIAL PRIMARY KEY,
    order_id   BIGINT NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    user_id    BIGINT,
    user_login TEXT NOT NULL DEFAULT '',
    amount     NUMERIC(14,2) NOT NULL CHECK (amount > 0),
    reason     TEXT NOT NULL DEFAULT ''
);
CREATE INDEX order_refunds_order_idx ON order_refunds (order_id, id);
ALTER TABLE orders ADD COLUMN refunded_amount NUMERIC(14,2) NOT NULL DEFAULT 0;
