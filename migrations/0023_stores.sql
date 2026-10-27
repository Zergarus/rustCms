-- Складской учёт: признаки складов, резервы, документы складского учёта

-- Склад по умолчанию (не больше одного), пункт выдачи, отгрузочный склад
ALTER TABLE catalog_stores
    ADD COLUMN is_default      BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN issuing_center  BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN shipping_center BOOLEAN NOT NULL DEFAULT FALSE;
CREATE UNIQUE INDEX catalog_stores_default_key ON catalog_stores (is_default) WHERE is_default;

-- Резерв на складе (b_catalog_store_product.QUANTITY_RESERVED)
ALTER TABLE catalog_store_amounts
    ADD COLUMN quantity_reserved NUMERIC(18, 2) NOT NULL DEFAULT 0;

-- Резерв товара и закупочная цена (b_catalog_product)
ALTER TABLE catalog_products
    ADD COLUMN quantity_reserved   NUMERIC(18, 2) NOT NULL DEFAULT 0,
    ADD COLUMN purchasing_price    NUMERIC(18, 2),
    ADD COLUMN purchasing_currency TEXT;

-- Резервы позиций заказа (b_sale_basket_reservation); store_id NULL — без склада.
-- item_id — товар позиции: счётчики пересчитываются и при каскадном удалении позиции.
CREATE TABLE basket_reservations (
    id               BIGSERIAL PRIMARY KEY,
    cart_item_id     BIGINT NOT NULL REFERENCES cart_items (id) ON DELETE CASCADE,
    item_id          BIGINT NOT NULL,
    store_id         BIGINT REFERENCES catalog_stores (id) ON DELETE SET NULL,
    quantity         NUMERIC(18, 3) NOT NULL CHECK (quantity >= 0),
    date_reserve     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- NULL — не снимать
    date_reserve_end TIMESTAMPTZ,
    -- NULL — система
    reserved_by      BIGINT REFERENCES users (id) ON DELETE SET NULL
);
CREATE INDEX basket_reservations_cart_item_idx ON basket_reservations (cart_item_id);

-- История резервов (b_sale_basket_reservation_history)
CREATE TABLE basket_reservation_history (
    id             BIGSERIAL PRIMARY KEY,
    reservation_id BIGINT NOT NULL REFERENCES basket_reservations (id) ON DELETE CASCADE,
    date_reserve   TIMESTAMPTZ NOT NULL DEFAULT now(),
    quantity       NUMERIC(18, 3) NOT NULL
);
CREATE INDEX basket_reservation_history_reservation_idx ON basket_reservation_history (reservation_id);

-- Склады отгрузки позиций (b_sale_store_barcode без штрихкода)
CREATE TABLE shipment_item_stores (
    shipment_id  BIGINT NOT NULL REFERENCES shipments (id) ON DELETE CASCADE,
    cart_item_id BIGINT NOT NULL REFERENCES cart_items (id) ON DELETE CASCADE,
    store_id     BIGINT NOT NULL,
    quantity     NUMERIC(18, 3) NOT NULL,
    PRIMARY KEY (shipment_id, cart_item_id, store_id)
);
CREATE INDEX shipment_item_stores_cart_item_idx ON shipment_item_stores (cart_item_id);

-- Поставщики (b_catalog_contractor): I — физлицо, C — юрлицо
CREATE TABLE contractors (
    id          BIGSERIAL PRIMARY KEY,
    person_type CHAR(1) NOT NULL DEFAULT 'C' CHECK (person_type IN ('I', 'C')),
    person_name TEXT NOT NULL DEFAULT '',
    company     TEXT NOT NULL DEFAULT '',
    inn         TEXT NOT NULL DEFAULT '',
    kpp         TEXT NOT NULL DEFAULT '',
    address     TEXT NOT NULL DEFAULT '',
    phone       TEXT NOT NULL DEFAULT '',
    email       TEXT NOT NULL DEFAULT '',
    post_index  TEXT NOT NULL DEFAULT '',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Документы складского учёта (b_catalog_store_docs): A — приход, S — оприходование,
-- M — перемещение, R — возврат, D — списание; статус N — черновик, Y — проведён
CREATE TABLE store_documents (
    id                  BIGSERIAL PRIMARY KEY,
    doc_type            CHAR(1) NOT NULL CHECK (doc_type IN ('A', 'S', 'M', 'R', 'D')),
    title               TEXT NOT NULL DEFAULT '',
    doc_number          TEXT NOT NULL DEFAULT '',
    date_document       TIMESTAMPTZ NOT NULL DEFAULT now(),
    contractor_id       BIGINT REFERENCES contractors (id) ON DELETE RESTRICT,
    currency            TEXT NOT NULL DEFAULT 'RUB',
    total               NUMERIC(18, 2) NOT NULL DEFAULT 0,
    items_order_date    TIMESTAMPTZ,
    items_received_date TIMESTAMPTZ,
    responsible_id      BIGINT REFERENCES users (id) ON DELETE SET NULL,
    commentary          TEXT NOT NULL DEFAULT '',
    status              CHAR(1) NOT NULL DEFAULT 'N' CHECK (status IN ('N', 'Y')),
    was_cancelled       BOOLEAN NOT NULL DEFAULT FALSE,
    date_status         TIMESTAMPTZ,
    status_by           BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_by          BIGINT REFERENCES users (id) ON DELETE SET NULL,
    modified_by         BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX store_documents_type_idx ON store_documents (doc_type, date_document DESC);

-- Строки документов (b_catalog_docs_element); склады проверяются при проведении
CREATE TABLE store_document_items (
    id               BIGSERIAL PRIMARY KEY,
    doc_id           BIGINT NOT NULL REFERENCES store_documents (id) ON DELETE CASCADE,
    store_from       BIGINT,
    store_to         BIGINT,
    item_id          BIGINT REFERENCES collection_items (id) ON DELETE SET NULL,
    amount           NUMERIC(18, 3) NOT NULL,
    purchasing_price NUMERIC(18, 2),
    base_price       NUMERIC(18, 2),
    comment          TEXT NOT NULL DEFAULT ''
);
CREATE INDEX store_document_items_doc_idx ON store_document_items (doc_id);

-- Счётчики резервов: товар (доступно −, резерв +) и склад резерва (резерв +)
CREATE FUNCTION basket_reservation_apply(item BIGINT, store BIGINT, delta NUMERIC) RETURNS void AS $$
BEGIN
    IF delta = 0 THEN
        RETURN;
    END IF;
    UPDATE catalog_products
    SET quantity_reserved = quantity_reserved + delta, quantity = quantity - delta
    WHERE item_id = item;
    IF store IS NOT NULL THEN
        UPDATE catalog_store_amounts SET quantity_reserved = quantity_reserved + delta
        WHERE item_id = item AND store_id = store;
        -- Строки остатка нет — создаётся при постановке резерва (товар и склад существуют)
        IF NOT FOUND AND delta > 0 THEN
            INSERT INTO catalog_store_amounts (item_id, store_id, quantity_reserved)
            SELECT item, store, delta
            WHERE EXISTS (SELECT 1 FROM collection_items WHERE id = item)
              AND EXISTS (SELECT 1 FROM catalog_stores WHERE id = store);
        END IF;
    END IF;
END;
$$ LANGUAGE plpgsql;

CREATE FUNCTION basket_reservations_counters_trg() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.item_id = OLD.item_id
       AND NEW.store_id IS NOT DISTINCT FROM OLD.store_id THEN
        PERFORM basket_reservation_apply(NEW.item_id, NEW.store_id, NEW.quantity - OLD.quantity);
        RETURN NULL;
    END IF;
    -- Смена склада или товара: снять со старого, поставить на новый
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        PERFORM basket_reservation_apply(OLD.item_id, OLD.store_id, -OLD.quantity);
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        PERFORM basket_reservation_apply(NEW.item_id, NEW.store_id, NEW.quantity);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER basket_reservations_counters
    AFTER INSERT OR UPDATE OR DELETE ON basket_reservations
    FOR EACH ROW EXECUTE FUNCTION basket_reservations_counters_trg();

-- Перевод заказов со списанными остатками в резервы: неотменённые, без отгруженной основной
-- отгрузки (min(id)), позиции товаров с учётом количества. Списание вернуть (доступно и
-- остаток склада +), затем поставить резерв без срока — триггер снова уменьшит доступно.
CREATE TEMP TABLE stock_convert_lines ON COMMIT DROP AS
SELECT ci.id, ci.item_id, ci.store_id, ci.quantity
FROM cart_items ci
JOIN orders o ON o.id = ci.order_id
LEFT JOIN catalog_products p ON p.item_id = ci.item_id
WHERE NOT o.canceled
  AND o.stock_deducted
  AND NOT EXISTS (
      SELECT 1 FROM shipments s
      WHERE s.id = (SELECT min(id) FROM shipments WHERE order_id = o.id) AND s.deducted
  )
  AND COALESCE(
      p.quantity_trace,
      (SELECT value = 'Y' FROM options WHERE module = 'catalog' AND name = 'default_quantity_trace'),
      TRUE
  );

UPDATE catalog_products p SET quantity = p.quantity + l.total
FROM (SELECT item_id, sum(quantity) AS total FROM stock_convert_lines GROUP BY item_id) l
WHERE p.item_id = l.item_id;

UPDATE catalog_store_amounts a SET amount = a.amount + l.total
FROM (
    SELECT item_id, store_id, sum(quantity) AS total FROM stock_convert_lines
    WHERE store_id IS NOT NULL GROUP BY item_id, store_id
) l
WHERE a.item_id = l.item_id AND a.store_id = l.store_id;

INSERT INTO basket_reservations (cart_item_id, item_id, store_id, quantity, date_reserve_end)
SELECT id, item_id, store_id, quantity, NULL FROM stock_convert_lines ORDER BY id;

DROP TABLE stock_convert_lines;
