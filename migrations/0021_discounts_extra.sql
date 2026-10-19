-- Правила округления цен (аналог b_catalog_round) и индекс для накопительных скидок.
CREATE TABLE catalog_rounding (
    id            BIGSERIAL PRIMARY KEY,
    price_type_id BIGINT NOT NULL REFERENCES catalog_price_types (id) ON DELETE CASCADE,
    price         NUMERIC(18, 2) NOT NULL DEFAULT 0,
    round_type    TEXT NOT NULL CHECK (round_type IN ('math', 'up', 'down')),
    precision     NUMERIC(18, 4) NOT NULL CHECK (precision > 0),
    created_by    BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by    BIGINT REFERENCES users (id) ON DELETE SET NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (price_type_id, price)
);

CREATE INDEX orders_cumulative_idx ON orders (user_id, created_at) WHERE paid AND NOT canceled;
