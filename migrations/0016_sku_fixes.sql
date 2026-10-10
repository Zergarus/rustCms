-- Торговые предложения: исправления после 0015

-- Пересчёт сначала блокирует строку товара (параллельные пересчёты одного товара идут
-- по очереди и не записывают устаревшую доступность); настройки каталога читаются
-- только когда товар есть
CREATE OR REPLACE FUNCTION refresh_sku_product(p BIGINT) RETURNS VOID AS $$
DECLARE
    trace_default BOOLEAN;
    zero_default BOOLEAN;
    has_offers BOOLEAN;
    has_available BOOLEAN;
BEGIN
    IF p IS NULL THEN
        RETURN;
    END IF;
    PERFORM 1 FROM collection_items WHERE id = p FOR NO KEY UPDATE;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    SELECT EXISTS (SELECT 1 FROM collection_items WHERE product_id = p) INTO has_offers;
    IF has_offers THEN
        trace_default := COALESCE(
            (SELECT value = 'Y' FROM options WHERE module = 'catalog' AND name = 'default_quantity_trace'), TRUE);
        zero_default := COALESCE(
            (SELECT value = 'Y' FROM options WHERE module = 'catalog' AND name = 'default_can_buy_zero'), FALSE);
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

-- Изменение каталога простого товара (без родителя) пересчёта не требует
CREATE OR REPLACE FUNCTION catalog_products_sku_trg() RETURNS TRIGGER AS $$
DECLARE
    parent BIGINT;
BEGIN
    SELECT product_id INTO parent FROM collection_items
    WHERE id = CASE WHEN TG_OP = 'DELETE' THEN OLD.item_id ELSE NEW.item_id END;
    IF parent IS NULL THEN
        RETURN NULL;
    END IF;
    PERFORM refresh_sku_product(parent);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
