-- Справочники оформления заказа (модуль sale)

-- Статусы заказа (b_sale_status с TYPE = 'O' + b_sale_status_lang)
CREATE TABLE order_statuses (
    code        TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    sort        INT NOT NULL DEFAULT 100,
    description TEXT NOT NULL DEFAULT '',
    -- Слать письмо SALE_STATUS_CHANGED_<код> при смене статуса
    notify      BOOLEAN NOT NULL DEFAULT FALSE
);

-- Типы плательщика (b_sale_person_type)
CREATE TABLE person_types (
    id     BIGSERIAL PRIMARY KEY,
    code   TEXT NOT NULL DEFAULT '',
    name   TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT TRUE,
    sort   INT NOT NULL DEFAULT 100
);

-- Группы свойств заказа (b_sale_order_props_group); block_code — блок формы для фронта
CREATE TABLE order_property_groups (
    id             BIGSERIAL PRIMARY KEY,
    person_type_id BIGINT NOT NULL REFERENCES person_types (id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    sort           INT NOT NULL DEFAULT 100,
    block_code     TEXT NOT NULL DEFAULT ''
);

-- Свойства заказа (b_sale_order_props)
CREATE TABLE order_properties (
    id              BIGSERIAL PRIMARY KEY,
    person_type_id  BIGINT NOT NULL REFERENCES person_types (id) ON DELETE CASCADE,
    group_id        BIGINT REFERENCES order_property_groups (id) ON DELETE SET NULL,
    code            TEXT NOT NULL DEFAULT '',
    name            TEXT NOT NULL,
    -- text | textarea | number | select | checkbox | date | file | location | address
    kind            TEXT NOT NULL DEFAULT 'text',
    required        BOOLEAN NOT NULL DEFAULT FALSE,
    -- Служебное: покупателю не показывается
    util            BOOLEAN NOT NULL DEFAULT FALSE,
    is_email        BOOLEAN NOT NULL DEFAULT FALSE,
    is_phone        BOOLEAN NOT NULL DEFAULT FALSE,
    is_payer        BOOLEAN NOT NULL DEFAULT FALSE,
    is_profile_name BOOLEAN NOT NULL DEFAULT FALSE,
    is_location     BOOLEAN NOT NULL DEFAULT FALSE,
    is_address      BOOLEAN NOT NULL DEFAULT FALSE,
    is_zip          BOOLEAN NOT NULL DEFAULT FALSE,
    default_value   TEXT NOT NULL DEFAULT '',
    description     TEXT NOT NULL DEFAULT '',
    sort            INT NOT NULL DEFAULT 100,
    active          BOOLEAN NOT NULL DEFAULT TRUE
);
CREATE INDEX order_properties_person_type_idx ON order_properties (person_type_id);

-- Варианты свойств-списков (b_sale_order_props_variant)
CREATE TABLE order_property_variants (
    id          BIGSERIAL PRIMARY KEY,
    property_id BIGINT NOT NULL REFERENCES order_properties (id) ON DELETE CASCADE,
    value       TEXT NOT NULL,
    name        TEXT NOT NULL,
    sort        INT NOT NULL DEFAULT 100
);
CREATE INDEX order_property_variants_property_idx ON order_property_variants (property_id);

-- Службы доставки (b_sale_delivery_srv); public = FALSE — не показывать покупателю
CREATE TABLE deliveries (
    id          BIGSERIAL PRIMARY KEY,
    code        TEXT NOT NULL DEFAULT '',
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    public      BOOLEAN NOT NULL DEFAULT TRUE,
    sort        INT NOT NULL DEFAULT 100,
    price       NUMERIC(18, 2) NOT NULL DEFAULT 0,
    currency    TEXT NOT NULL DEFAULT 'RUB'
);

-- Склады самовывоза службы (ExtraServices\Store)
CREATE TABLE delivery_stores (
    delivery_id BIGINT NOT NULL REFERENCES deliveries (id) ON DELETE CASCADE,
    store_id    BIGINT NOT NULL REFERENCES catalog_stores (id) ON DELETE CASCADE,
    PRIMARY KEY (delivery_id, store_id)
);

-- Платёжные системы (b_sale_pay_system_action)
CREATE TABLE pay_systems (
    id          BIGSERIAL PRIMARY KEY,
    code        TEXT NOT NULL DEFAULT '',
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    sort        INT NOT NULL DEFAULT 100,
    -- Обработчик онлайн-оплаты: '' — нет, 'yookassa'
    handler     TEXT NOT NULL DEFAULT '',
    -- Тип для API: cashless | cash | document | redirect | qr | other
    api_type    TEXT NOT NULL DEFAULT 'other',
    -- Кому доступна; пусто — всем
    group_ids   BIGINT[] NOT NULL DEFAULT '{}'
);

-- Заказ (b_sale_order)
CREATE TABLE orders (
    id              BIGSERIAL PRIMARY KEY,
    account_number  TEXT UNIQUE,
    user_id         BIGINT REFERENCES users (id) ON DELETE SET NULL,
    person_type_id  BIGINT NOT NULL REFERENCES person_types (id),
    status          TEXT NOT NULL REFERENCES order_statuses (code) ON UPDATE CASCADE,
    goods_price     NUMERIC(18, 2) NOT NULL DEFAULT 0,
    delivery_price  NUMERIC(18, 2) NOT NULL DEFAULT 0,
    price           NUMERIC(18, 2) NOT NULL DEFAULT 0,
    currency        TEXT NOT NULL DEFAULT 'RUB',
    user_comment    TEXT NOT NULL DEFAULT '',
    manager_comment TEXT NOT NULL DEFAULT '',
    canceled        BOOLEAN NOT NULL DEFAULT FALSE,
    canceled_at     TIMESTAMPTZ,
    cancel_reason   TEXT NOT NULL DEFAULT '',
    paid            BOOLEAN NOT NULL DEFAULT FALSE,
    paid_at         TIMESTAMPTZ,
    -- Остатки по позициям заказа списаны (снимается при отмене)
    stock_deducted  BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX orders_user_idx ON orders (user_id);
CREATE INDEX orders_created_idx ON orders (created_at DESC);
CREATE INDEX orders_status_idx ON orders (status);

-- Значения свойств заказа; код и название копируются на момент оформления
CREATE TABLE order_property_values (
    order_id    BIGINT NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    property_id BIGINT NOT NULL,
    code        TEXT NOT NULL DEFAULT '',
    name        TEXT NOT NULL DEFAULT '',
    value       TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (order_id, property_id)
);

-- Отгрузка: служба, стоимость, склад самовывоза (копии — справочники могут меняться)
CREATE TABLE shipments (
    id            BIGSERIAL PRIMARY KEY,
    order_id      BIGINT NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    delivery_id   BIGINT,
    delivery_name TEXT NOT NULL DEFAULT '',
    price         NUMERIC(18, 2) NOT NULL DEFAULT 0,
    store_id      BIGINT
);
CREATE INDEX shipments_order_idx ON shipments (order_id);

-- Оплата заказа
CREATE TABLE payments (
    id            BIGSERIAL PRIMARY KEY,
    order_id      BIGINT NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    pay_system_id BIGINT,
    name          TEXT NOT NULL DEFAULT '',
    sum           NUMERIC(18, 2) NOT NULL DEFAULT 0,
    currency      TEXT NOT NULL DEFAULT 'RUB',
    paid          BOOLEAN NOT NULL DEFAULT FALSE,
    paid_at       TIMESTAMPTZ,
    external_id   TEXT NOT NULL DEFAULT '',
    data          JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX payments_order_idx ON payments (order_id);

-- История статусов; user_id NULL — система
CREATE TABLE order_status_history (
    id         BIGSERIAL PRIMARY KEY,
    order_id   BIGINT NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    status     TEXT NOT NULL,
    user_id    BIGINT REFERENCES users (id) ON DELETE SET NULL,
    comment    TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX order_status_history_order_idx ON order_status_history (order_id);

-- Позиции заказа — строки cart_items с order_id (как b_sale_basket)
ALTER TABLE cart_items
    ADD CONSTRAINT cart_items_order_fk FOREIGN KEY (order_id) REFERENCES orders (id) ON DELETE CASCADE,
    ADD COLUMN custom_price BOOLEAN NOT NULL DEFAULT FALSE;
CREATE INDEX cart_items_order_idx ON cart_items (order_id) WHERE order_id IS NOT NULL;

-- Позиции заказа переживают удаление товара (как в Битриксе): каскад только для корзин
ALTER TABLE cart_items DROP CONSTRAINT cart_items_element_id_fkey;
CREATE INDEX cart_items_element_idx ON cart_items (element_id) WHERE order_id IS NULL;
CREATE FUNCTION cart_items_drop_element() RETURNS trigger AS $$
BEGIN
    DELETE FROM cart_items WHERE element_id = OLD.id AND order_id IS NULL;
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER iblock_elements_cart_cleanup
    BEFORE DELETE ON iblock_elements
    FOR EACH ROW EXECUTE FUNCTION cart_items_drop_element();

-- Картинка склада (b_catalog_store.IMAGE_ID) — для пунктов выдачи
ALTER TABLE catalog_stores
    ADD COLUMN image_id BIGINT REFERENCES files (id) ON DELETE SET NULL;
