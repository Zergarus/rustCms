-- Торговые предложения (SKU)

-- Коллекция предложений для коллекции товаров (не больше одной на коллекцию товаров)
-- и системное поле связи с товаром (CML2_LINK)
ALTER TABLE collections
    ADD COLUMN product_collection_id BIGINT UNIQUE REFERENCES collections (id) ON DELETE SET NULL,
    ADD COLUMN sku_field_id          BIGINT REFERENCES collection_fields (id) ON DELETE SET NULL;

-- Родительский товар предложения (у остальных записей NULL)
ALTER TABLE collection_items
    ADD COLUMN product_id BIGINT REFERENCES collection_items (id) ON DELETE CASCADE;
CREATE INDEX collection_items_product_idx ON collection_items (product_id);

-- Тип товара (коды Битрикса: 1 простой, 3 с предложениями, 4 предложение,
-- 6 с предложениями без предложений) и вес в граммах
ALTER TABLE catalog_products
    ADD COLUMN type   SMALLINT NOT NULL DEFAULT 1,
    ADD COLUMN weight NUMERIC(18, 3) NOT NULL DEFAULT 0;

ALTER TABLE collection_fields
    ADD COLUMN in_basket  BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN offer_tree BOOLEAN NOT NULL DEFAULT FALSE;

-- Свойства позиции корзины [{code, name, value}]
ALTER TABLE cart_items
    ADD COLUMN props JSONB NOT NULL DEFAULT '[]';

-- Пересчёт типа и доступности товара по его предложениям
CREATE FUNCTION refresh_sku_product(p BIGINT) RETURNS VOID AS $$
DECLARE
    trace_default BOOLEAN := COALESCE(
        (SELECT value = 'Y' FROM options WHERE module = 'catalog' AND name = 'default_quantity_trace'), TRUE);
    zero_default BOOLEAN := COALESCE(
        (SELECT value = 'Y' FROM options WHERE module = 'catalog' AND name = 'default_can_buy_zero'), FALSE);
    has_offers BOOLEAN;
    has_available BOOLEAN;
BEGIN
    IF p IS NULL OR NOT EXISTS (SELECT 1 FROM collection_items WHERE id = p) THEN
        RETURN;
    END IF;
    SELECT EXISTS (SELECT 1 FROM collection_items WHERE product_id = p) INTO has_offers;
    IF has_offers THEN
        SELECT EXISTS (
            SELECT 1 FROM collection_items o
            JOIN catalog_products c ON c.item_id = o.id
            WHERE o.product_id = p AND o.active AND c.available
              AND (c.quantity > 0
                   OR NOT COALESCE(c.quantity_trace, trace_default)
                   OR COALESCE(c.can_buy_zero, zero_default))
        ) INTO has_available;
        INSERT INTO catalog_products (item_id, type, available) VALUES (p, 3, has_available)
        ON CONFLICT (item_id) DO UPDATE SET type = 3, available = EXCLUDED.available;
    ELSE
        UPDATE catalog_products SET type = 6, available = FALSE
        WHERE item_id = p AND type IN (3, 6);
    END IF;
END;
$$ LANGUAGE plpgsql;

CREATE FUNCTION collection_items_sku_trg() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        PERFORM refresh_sku_product(OLD.product_id);
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        PERFORM refresh_sku_product(NEW.product_id);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER collection_items_sku
    AFTER INSERT OR DELETE OR UPDATE OF product_id, active ON collection_items
    FOR EACH ROW EXECUTE FUNCTION collection_items_sku_trg();

-- Предложение (запись с родителем) всегда типа 4
CREATE FUNCTION catalog_products_offer_type_trg() RETURNS TRIGGER AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM collection_items WHERE id = NEW.item_id AND product_id IS NOT NULL) THEN
        NEW.type := 4;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER catalog_products_offer_type
    BEFORE INSERT OR UPDATE ON catalog_products
    FOR EACH ROW EXECUTE FUNCTION catalog_products_offer_type_trg();

CREATE FUNCTION catalog_products_sku_trg() RETURNS TRIGGER AS $$
DECLARE
    parent BIGINT;
BEGIN
    SELECT product_id INTO parent FROM collection_items
    WHERE id = CASE WHEN TG_OP = 'DELETE' THEN OLD.item_id ELSE NEW.item_id END;
    PERFORM refresh_sku_product(parent);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER catalog_products_sku
    AFTER INSERT OR DELETE OR UPDATE OF available, quantity, quantity_trace, can_buy_zero ON catalog_products
    FOR EACH ROW EXECUTE FUNCTION catalog_products_sku_trg();
