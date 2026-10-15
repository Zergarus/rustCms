-- Обмен заказами с 1С: номера и версии документов 1С, метка «изменено из 1С», Ид контрагента,
-- домен типа плательщика, соответствие реквизитов, журнал удалённых отгрузок

ALTER TABLE orders
    ADD COLUMN id_1c      TEXT NOT NULL DEFAULT '',
    ADD COLUMN version_1c TEXT NOT NULL DEFAULT '',
    -- Последнее изменение пришло из обмена; любое изменение на сайте сбрасывает
    ADD COLUMN updated_1c BOOLEAN NOT NULL DEFAULT FALSE,
    -- Заказ создан из 1С и обратно не выгружается
    ADD COLUMN external   BOOLEAN NOT NULL DEFAULT FALSE;
CREATE INDEX orders_id_1c_idx ON orders (id_1c) WHERE id_1c <> '';
CREATE INDEX orders_updated_idx ON orders (updated_at);

ALTER TABLE payments
    ADD COLUMN id_1c            TEXT NOT NULL DEFAULT '',
    ADD COLUMN version_1c       TEXT NOT NULL DEFAULT '',
    ADD COLUMN pay_voucher_num  TEXT NOT NULL DEFAULT '',
    ADD COLUMN pay_voucher_date DATE,
    ADD COLUMN is_return        BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN return_comment   TEXT NOT NULL DEFAULT '',
    ADD COLUMN updated_1c       BOOLEAN NOT NULL DEFAULT FALSE;

-- tracking_number и allow_delivery — с 0013
ALTER TABLE shipments
    ADD COLUMN id_1c             TEXT NOT NULL DEFAULT '',
    ADD COLUMN version_1c        TEXT NOT NULL DEFAULT '',
    ADD COLUMN allow_delivery_at TIMESTAMPTZ,
    -- Отгружена (b_sale_order_delivery.DEDUCTED)
    ADD COLUMN deducted          BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN deducted_at       TIMESTAMPTZ,
    ADD COLUMN delivery_doc_num  TEXT NOT NULL DEFAULT '',
    ADD COLUMN delivery_doc_date DATE,
    ADD COLUMN updated_1c        BOOLEAN NOT NULL DEFAULT FALSE;

-- Удалённые отгрузки — для документов удалённых отгрузок (SHIPMENT_REMOVED у Битрикса)
CREATE TABLE shipments_removed (
    order_id    BIGINT REFERENCES orders (id) ON DELETE CASCADE,
    shipment_id BIGINT,
    id_1c       TEXT,
    removed_at  TIMESTAMPTZ DEFAULT now()
);
CREATE INDEX shipments_removed_order_idx ON shipments_removed (order_id);

CREATE FUNCTION shipments_log_removed() RETURNS trigger AS $$
BEGIN
    -- При удалении заказа целиком (каскад) заказа уже нет — журнал не нужен
    INSERT INTO shipments_removed (order_id, shipment_id, id_1c)
    SELECT OLD.order_id, OLD.id, OLD.id_1c
    WHERE EXISTS (SELECT 1 FROM orders WHERE id = OLD.order_id);
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER shipments_removed_log
    AFTER DELETE ON shipments
    FOR EACH ROW EXECUTE FUNCTION shipments_log_removed();

-- Ид контрагента для 1С (b_user.XML_ID)
ALTER TABLE users
    ADD COLUMN xml_id TEXT NOT NULL DEFAULT '';
CREATE UNIQUE INDEX users_xml_id_key ON users (xml_id) WHERE xml_id <> '';

-- Домен типа плательщика (b_sale_bizval_persondomain): I — физлицо, E — юрлицо
ALTER TABLE person_types
    ADD COLUMN domain CHAR(1) NOT NULL DEFAULT 'I' CHECK (domain IN ('I', 'E'));

-- Соответствие реквизитов для 1С (b_sale_bizval с потребителем «1C»); тип плательщика 0 — для всех.
-- code — код Битрикса: BUYER_PERSON_* / BUYER_COMPANY_* или 1C_REKV_<n> (произвольный реквизит, name — его название);
-- value — id свойства заказа, поле пользователя или заказа, константа
CREATE TABLE exchange_order_map (
    person_type_id BIGINT NOT NULL,
    code           TEXT NOT NULL,
    name           TEXT NOT NULL DEFAULT '',
    source         TEXT NOT NULL CHECK (source IN ('property', 'user', 'order', 'value')),
    value          TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (person_type_id, code)
);

CREATE FUNCTION exchange_order_map_drop_person_type() RETURNS trigger AS $$
BEGIN
    DELETE FROM exchange_order_map WHERE person_type_id = OLD.id;
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER person_types_order_map_cleanup
    AFTER DELETE ON person_types
    FOR EACH ROW EXECUTE FUNCTION exchange_order_map_drop_person_type();

ALTER TABLE exchange_log
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'catalog';

-- Метка изменения заказа: обмен ставит SET LOCAL cms.exchange_1c = 'on' в своей транзакции,
-- любое другое изменение строки заказа сбрасывает updated_1c
CREATE FUNCTION orders_mark_1c() RETURNS trigger AS $$
BEGIN
    IF NEW IS DISTINCT FROM OLD THEN
        NEW.updated_1c := COALESCE(current_setting('cms.exchange_1c', true) = 'on', FALSE);
        NEW.updated_at := now();
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER orders_mark_1c
    BEFORE UPDATE ON orders
    FOR EACH ROW EXECUTE FUNCTION orders_mark_1c();

-- Изменение оплаты, отгрузки, свойства или позиции — изменение заказа
CREATE FUNCTION orders_touch(order_id BIGINT) RETURNS void AS $$
BEGIN
    IF order_id IS NOT NULL THEN
        UPDATE orders SET updated_at = now() WHERE id = order_id;
    END IF;
END;
$$ LANGUAGE plpgsql;

CREATE FUNCTION orders_touch_child() RETURNS trigger AS $$
BEGIN
    IF TG_OP <> 'DELETE' THEN
        PERFORM orders_touch(NEW.order_id);
    END IF;
    IF TG_OP <> 'INSERT' AND (TG_OP = 'DELETE' OR OLD.order_id IS DISTINCT FROM NEW.order_id) THEN
        PERFORM orders_touch(OLD.order_id);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER payments_touch_order
    AFTER INSERT OR UPDATE OR DELETE ON payments
    FOR EACH ROW EXECUTE FUNCTION orders_touch_child();
CREATE TRIGGER shipments_touch_order
    AFTER INSERT OR UPDATE OR DELETE ON shipments
    FOR EACH ROW EXECUTE FUNCTION orders_touch_child();
CREATE TRIGGER order_property_values_touch_order
    AFTER INSERT OR UPDATE OR DELETE ON order_property_values
    FOR EACH ROW EXECUTE FUNCTION orders_touch_child();
-- Позиции открытых корзин (order_id IS NULL) заказ не трогают
CREATE TRIGGER cart_items_touch_order
    AFTER INSERT OR UPDATE OR DELETE ON cart_items
    FOR EACH ROW EXECUTE FUNCTION orders_touch_child();
