-- Покупатель (как b_sale_fuser): пользователь или гость с токеном из cookie
CREATE TABLE buyers (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT UNIQUE REFERENCES users (id) ON DELETE CASCADE,
    -- sha256 токена из cookie гостя
    token_hash TEXT UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (user_id IS NOT NULL OR token_hash IS NOT NULL)
);

-- Позиции корзины (как b_sale_basket); при оформлении заказа получают order_id
CREATE TABLE cart_items (
    id         BIGSERIAL PRIMARY KEY,
    buyer_id   BIGINT NOT NULL REFERENCES buyers (id) ON DELETE CASCADE,
    element_id BIGINT NOT NULL REFERENCES iblock_elements (id) ON DELETE CASCADE,
    store_id   BIGINT REFERENCES catalog_stores (id) ON DELETE SET NULL,
    quantity   NUMERIC(18, 3) NOT NULL CHECK (quantity > 0),
    name       TEXT NOT NULL DEFAULT '',
    price      NUMERIC(18, 2),
    currency   TEXT NOT NULL DEFAULT '',
    order_id   BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- Один товар на одном складе — одна позиция корзины (склад NULL — тоже значение)
CREATE UNIQUE INDEX cart_items_unique ON cart_items (buyer_id, element_id, store_id)
    NULLS NOT DISTINCT WHERE order_id IS NULL;
CREATE INDEX cart_items_buyer_idx ON cart_items (buyer_id) WHERE order_id IS NULL;

-- Флаги товара (как b_catalog_product); NULL — «по умолчанию» из настроек каталога
ALTER TABLE catalog_products
    ADD COLUMN available      BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN quantity_trace BOOLEAN,
    ADD COLUMN can_buy_zero   BOOLEAN;

-- UF-поля складов (b_uts_cat_store), ключи в snake_case: uf_city_id
ALTER TABLE catalog_stores
    ADD COLUMN extra JSONB NOT NULL DEFAULT '{}';
