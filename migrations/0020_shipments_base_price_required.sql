-- Цена доставки до скидок — без значения по умолчанию: запись отгрузки без неё — ошибка,
-- а не тихий 0
ALTER TABLE shipments ALTER COLUMN base_price DROP DEFAULT;
