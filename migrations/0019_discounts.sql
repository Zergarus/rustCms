-- Скидки: правила работы с корзиной (b_sale_discount), их группы, купоны, купоны корзины,
-- цены позиций и доставки до скидок, применённые к заказу правила

-- Дерево условий и действий — JSON по формату Битрикса: {"CLASS_ID", "DATA", "CHILDREN"}
CREATE TABLE discounts (
    id                  BIGSERIAL PRIMARY KEY,
    name                TEXT NOT NULL DEFAULT '',
    active              BOOLEAN NOT NULL DEFAULT TRUE,
    active_from         TIMESTAMPTZ,
    active_to           TIMESTAMPTZ,
    priority            INT NOT NULL DEFAULT 1,
    sort                INT NOT NULL DEFAULT 100,
    -- Прекратить дальнейшее применение правил
    last_discount       BOOLEAN NOT NULL DEFAULT FALSE,
    -- Не применять правила с меньшим приоритетом
    last_level_discount BOOLEAN NOT NULL DEFAULT FALSE,
    -- Только по купону
    use_coupons         BOOLEAN NOT NULL DEFAULT FALSE,
    conditions          JSONB NOT NULL
        DEFAULT '{"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": []}',
    actions             JSONB NOT NULL
        DEFAULT '{"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": []}',
    -- Классы дерева, которых движок не знает, через запятую; правило с ними не применяется
    unsupported         TEXT NOT NULL DEFAULT '',
    xml_id              TEXT NOT NULL DEFAULT '',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Группы пользователей правила; правило без групп — для всех
CREATE TABLE discount_groups (
    discount_id BIGINT NOT NULL REFERENCES discounts (id) ON DELETE CASCADE,
    group_id    BIGINT NOT NULL REFERENCES groups (id) ON DELETE CASCADE,
    PRIMARY KEY (discount_id, group_id)
);
CREATE INDEX discount_groups_group_idx ON discount_groups (group_id);

-- Купоны (b_sale_discount_coupon): basket — одноразовый на позицию (TYPE 1), order — одноразовый
-- на заказ (2), multi — многоразовый (4)
CREATE TABLE discount_coupons (
    id          BIGSERIAL PRIMARY KEY,
    discount_id BIGINT NOT NULL REFERENCES discounts (id) ON DELETE CASCADE,
    coupon      TEXT NOT NULL,
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    active_from TIMESTAMPTZ,
    active_to   TIMESTAMPTZ,
    kind        TEXT NOT NULL DEFAULT 'order' CHECK (kind IN ('basket', 'order', 'multi')),
    -- 0 — без лимита
    max_use     INT NOT NULL DEFAULT 0 CHECK (max_use >= 0),
    use_count   INT NOT NULL DEFAULT 0 CHECK (use_count >= 0),
    -- NULL — любому покупателю; купон пользователя исчезает вместе с ним
    user_id     BIGINT REFERENCES users (id) ON DELETE CASCADE,
    description TEXT NOT NULL DEFAULT '',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX discount_coupons_discount_idx ON discount_coupons (discount_id);
-- Код купона — без учёта регистра, как в Битриксе на MySQL
CREATE UNIQUE INDEX discount_coupons_coupon_idx ON discount_coupons (lower(coupon));

-- Купоны, введённые покупателем до оформления заказа
CREATE TABLE buyer_coupons (
    buyer_id  BIGINT NOT NULL REFERENCES buyers (id) ON DELETE CASCADE,
    coupon_id BIGINT NOT NULL REFERENCES discount_coupons (id) ON DELETE CASCADE,
    added_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (buyer_id, coupon_id)
);
CREATE INDEX buyer_coupons_coupon_idx ON buyer_coupons (coupon_id);

-- Цена до скидок и скидка на единицу (наценка — отрицательная); price — итоговая
ALTER TABLE cart_items
    ADD COLUMN base_price     NUMERIC(18, 2),
    ADD COLUMN discount_price NUMERIC(18, 2) NOT NULL DEFAULT 0;
-- Цена доставки до скидок
ALTER TABLE shipments
    ADD COLUMN base_price NUMERIC(18, 2) NOT NULL DEFAULT 0;

-- Существующие позиции и отгрузки — без скидок; заказы при этом изменёнными не считаются
ALTER TABLE cart_items DISABLE TRIGGER cart_items_touch_order;
UPDATE cart_items SET base_price = price;
ALTER TABLE cart_items ENABLE TRIGGER cart_items_touch_order;
ALTER TABLE shipments DISABLE TRIGGER shipments_touch_order;
UPDATE shipments SET base_price = price;
ALTER TABLE shipments ENABLE TRIGGER shipments_touch_order;

-- Правила, применённые к заказу: applied — позиции [{cart_item_id, value}] и доставка
CREATE TABLE order_discounts (
    id          BIGSERIAL PRIMARY KEY,
    order_id    BIGINT NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    -- NULL, если правило удалено
    discount_id BIGINT REFERENCES discounts (id) ON DELETE SET NULL,
    name        TEXT NOT NULL DEFAULT '',
    coupon      TEXT NOT NULL DEFAULT '',
    applied     JSONB NOT NULL DEFAULT '{}',
    sum         NUMERIC(18, 2) NOT NULL DEFAULT 0
);
CREATE INDEX order_discounts_order_idx ON order_discounts (order_id);
