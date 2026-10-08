-- Позиции заказов не удаляются вместе с покупателем (удаление пользователя, объединение
-- корзины гостя при входе): у них buyer_id обнуляется, открытые позиции корзины удаляет триггер
ALTER TABLE cart_items ALTER COLUMN buyer_id DROP NOT NULL;
ALTER TABLE cart_items DROP CONSTRAINT cart_items_buyer_id_fkey;
ALTER TABLE cart_items
    ADD CONSTRAINT cart_items_buyer_id_fkey FOREIGN KEY (buyer_id) REFERENCES buyers (id) ON DELETE SET NULL,
    ADD CONSTRAINT cart_items_owner_check CHECK (buyer_id IS NOT NULL OR order_id IS NOT NULL);

CREATE FUNCTION cart_items_drop_buyer() RETURNS trigger AS $$
BEGIN
    DELETE FROM cart_items WHERE buyer_id = OLD.id AND order_id IS NULL;
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER buyers_cart_cleanup
    BEFORE DELETE ON buyers
    FOR EACH ROW EXECUTE FUNCTION cart_items_drop_buyer();
