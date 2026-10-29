-- Складской учёт: штрихкоды единиц товара, партии и право на закупочные цены

-- Товар с поштучным учётом штрихкодов (несколько штрихкодов на единицы складов)
ALTER TABLE catalog_products
    ADD COLUMN barcode_multi BOOLEAN NOT NULL DEFAULT FALSE;

-- Штрихкоды товара (b_catalog_barcode): уникальны во всей системе; склад и заказ — где единица
CREATE TABLE catalog_barcodes (
    id         BIGSERIAL PRIMARY KEY,
    item_id    BIGINT NOT NULL REFERENCES collection_items (id) ON DELETE CASCADE,
    barcode    TEXT NOT NULL UNIQUE,
    store_id   BIGINT REFERENCES catalog_stores (id) ON DELETE SET NULL,
    order_id   BIGINT REFERENCES orders (id) ON DELETE SET NULL,
    created_by BIGINT REFERENCES users (id) ON DELETE SET NULL,
    modified_by BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX catalog_barcodes_item_store_idx ON catalog_barcodes (item_id, store_id);

-- Штрихкоды строк документа складского учёта (b_catalog_store_barcode)
CREATE TABLE store_document_barcodes (
    id          BIGSERIAL PRIMARY KEY,
    doc_item_id BIGINT NOT NULL REFERENCES store_document_items (id) ON DELETE CASCADE,
    barcode     TEXT NOT NULL
);
CREATE INDEX store_document_barcodes_doc_item_idx ON store_document_barcodes (doc_item_id);

-- Суррогатный id для ссылок из партий на отгрузку; составной PK остаётся
ALTER TABLE shipment_item_stores
    ADD COLUMN id BIGSERIAL;
ALTER TABLE shipment_item_stores
    ADD CONSTRAINT shipment_item_stores_id_key UNIQUE (id);

-- Штрихкоды, отгруженные в составе отгрузки (b_sale_store_barcode)
CREATE TABLE shipment_item_barcodes (
    id           BIGSERIAL PRIMARY KEY,
    shipment_id  BIGINT NOT NULL REFERENCES shipments (id) ON DELETE CASCADE,
    cart_item_id BIGINT NOT NULL REFERENCES cart_items (id) ON DELETE CASCADE,
    store_id     BIGINT NOT NULL,
    barcode      TEXT NOT NULL,
    UNIQUE (shipment_id, barcode)
);

-- Партии товара на складе (b_catalog_store_product_batch — по поступлениям)
CREATE TABLE catalog_store_batches (
    id         BIGSERIAL PRIMARY KEY,
    item_id    BIGINT NOT NULL REFERENCES collection_items (id) ON DELETE CASCADE,
    store_id   BIGINT NOT NULL REFERENCES catalog_stores (id) ON DELETE CASCADE,
    available  NUMERIC(18, 3) NOT NULL DEFAULT 0,
    price      NUMERIC(18, 2) NOT NULL DEFAULT 0,
    currency   TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX catalog_store_batches_item_store_idx ON catalog_store_batches (item_id, store_id, id);

-- Движения партий: приход/расход по строке документа или по отгрузке (b_catalog_store_batch_move)
CREATE TABLE store_batch_moves (
    id                BIGSERIAL PRIMARY KEY,
    batch_id          BIGINT NOT NULL REFERENCES catalog_store_batches (id) ON DELETE CASCADE,
    doc_item_id       BIGINT REFERENCES store_document_items (id) ON DELETE CASCADE,
    shipment_store_id BIGINT REFERENCES shipment_item_stores (id) ON DELETE CASCADE,
    amount            NUMERIC(18, 3) NOT NULL,
    price             NUMERIC(18, 2) NOT NULL,
    currency          TEXT NOT NULL,
    CHECK (num_nonnulls(doc_item_id, shipment_store_id) = 1)
);
CREATE INDEX store_batch_moves_batch_idx ON store_batch_moves (batch_id);
CREATE INDEX store_batch_moves_doc_item_idx ON store_batch_moves (doc_item_id);
CREATE INDEX store_batch_moves_shipment_store_idx ON store_batch_moves (shipment_store_id);

-- Право на закупочные цены и себестоимость — у групп, которые управляют каталогом и магазином
INSERT INTO group_permissions (group_id, permission)
SELECT DISTINCT group_id, 'catalog.purchase_info' FROM group_permissions
WHERE permission IN ('collections.manage', 'shop.manage')
ON CONFLICT DO NOTHING;
