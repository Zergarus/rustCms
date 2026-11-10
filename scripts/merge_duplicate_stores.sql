-- Слияние складов с одинаковыми названиями (последствие импорта Битрикса).
-- Канонический склад — с минимальным id; строки остатков, штрихкодов и партий
-- переносятся, дубли удаляются. Скрипт идемпотентен: без дублей ничего не меняет.
BEGIN;

CREATE TEMP TABLE store_merge ON COMMIT DROP AS
SELECT s.id AS old_id, m.canonical_id
FROM catalog_stores s
JOIN (
    SELECT name, min(id) AS canonical_id
    FROM catalog_stores
    GROUP BY name
    HAVING count(*) > 1
) m ON m.name = s.name
WHERE s.id <> m.canonical_id;

UPDATE catalog_store_amounts a SET store_id = m.canonical_id
FROM store_merge m WHERE a.store_id = m.old_id;

UPDATE cart_items a SET store_id = m.canonical_id
FROM store_merge m WHERE a.store_id = m.old_id;

UPDATE basket_reservations a SET store_id = m.canonical_id
FROM store_merge m WHERE a.store_id = m.old_id;

UPDATE catalog_barcodes a SET store_id = m.canonical_id
FROM store_merge m WHERE a.store_id = m.old_id;

UPDATE catalog_store_batches a SET store_id = m.canonical_id
FROM store_merge m WHERE a.store_id = m.old_id;

-- У службы доставки могут быть привязаны оба дубля: добавляем канонический
-- и удаляем старые привязки, не нарушая первичный ключ.
INSERT INTO delivery_stores (delivery_id, store_id)
SELECT d.delivery_id, m.canonical_id
FROM delivery_stores d JOIN store_merge m ON m.old_id = d.store_id
ON CONFLICT DO NOTHING;
DELETE FROM delivery_stores d USING store_merge m WHERE d.store_id = m.old_id;

DELETE FROM catalog_stores s USING store_merge m WHERE s.id = m.old_id;

COMMIT;
