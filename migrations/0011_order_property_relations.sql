-- Привязка свойств заказа к платёжкам и доставкам (b_sale_order_props_relation):
-- свойство с привязками участвует в заказе, только если выбрана одна из них
CREATE TABLE order_property_relations (
    property_id BIGINT NOT NULL REFERENCES order_properties (id) ON DELETE CASCADE,
    -- 'P' — платёжная система, 'D' — служба доставки
    entity_type CHAR(1) NOT NULL CHECK (entity_type IN ('P', 'D')),
    entity_id   BIGINT NOT NULL,
    PRIMARY KEY (property_id, entity_type, entity_id)
);
