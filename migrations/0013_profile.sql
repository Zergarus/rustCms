-- Личный кабинет: множественные свойства заказа (файлы), трек-номер и разрешение доставки

-- b_sale_order_props.MULTIPLE
ALTER TABLE order_properties ADD COLUMN multiple BOOLEAN NOT NULL DEFAULT FALSE;

-- b_sale_order_delivery.TRACKING_NUMBER и ALLOW_DELIVERY
ALTER TABLE shipments
    ADD COLUMN tracking_number TEXT NOT NULL DEFAULT '',
    ADD COLUMN allow_delivery  BOOLEAN NOT NULL DEFAULT FALSE;
