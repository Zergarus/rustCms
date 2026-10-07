-- Загруженные файлы. path — относительно каталога загрузок (UPLOAD_DIR),
-- наружу отдаётся как /upload/<path>
CREATE TABLE files (
    id            BIGSERIAL PRIMARY KEY,
    path          TEXT NOT NULL UNIQUE,
    original_name TEXT NOT NULL DEFAULT '',
    content_type  TEXT NOT NULL DEFAULT '',
    size          BIGINT NOT NULL DEFAULT 0,
    width         INT,
    height        INT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Разделы инфоблока: дерево через parent_id, depth_level поддерживается приложением
CREATE TABLE iblock_sections (
    id          BIGSERIAL PRIMARY KEY,
    iblock_id   BIGINT NOT NULL REFERENCES iblocks (id) ON DELETE CASCADE,
    parent_id   BIGINT REFERENCES iblock_sections (id) ON DELETE CASCADE,
    code        TEXT NOT NULL DEFAULT '',
    xml_id      TEXT NOT NULL DEFAULT '',
    name        TEXT NOT NULL,
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    sort        INT NOT NULL DEFAULT 500,
    depth_level INT NOT NULL DEFAULT 1,
    description TEXT NOT NULL DEFAULT '',
    picture_id  BIGINT REFERENCES files (id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX iblock_sections_tree_idx ON iblock_sections (iblock_id, parent_id, sort, id);
CREATE INDEX iblock_sections_code_idx ON iblock_sections (iblock_id, code) WHERE code <> '';

-- Элементы: раздел, внешний код, картинки анонса и детальная.
-- Символьный код необязателен (как в Битриксе), уникален только непустой.
ALTER TABLE iblock_elements
    ADD COLUMN section_id        BIGINT REFERENCES iblock_sections (id) ON DELETE SET NULL,
    ADD COLUMN xml_id            TEXT NOT NULL DEFAULT '',
    ADD COLUMN preview_picture_id BIGINT REFERENCES files (id) ON DELETE SET NULL,
    ADD COLUMN detail_picture_id  BIGINT REFERENCES files (id) ON DELETE SET NULL,
    DROP CONSTRAINT iblock_elements_iblock_id_code_key;
CREATE UNIQUE INDEX iblock_elements_code_key ON iblock_elements (iblock_id, code) WHERE code <> '';
CREATE INDEX iblock_elements_section_idx ON iblock_elements (section_id);
CREATE INDEX iblock_elements_xml_id_idx ON iblock_elements (iblock_id, xml_id) WHERE xml_id <> '';

-- Новые типы свойств: список, привязка к элементу, файл; множественные значения.
-- Значения в iblock_elements.properties: одиночное — скаляр или null,
-- множественное — массив. Список хранит id варианта, привязка — id элемента, файл — id файла.
ALTER TABLE iblock_properties
    DROP CONSTRAINT iblock_properties_kind_check,
    ADD CONSTRAINT iblock_properties_kind_check
        CHECK (kind IN ('string', 'text', 'number', 'boolean', 'date', 'list', 'element', 'file')),
    ADD COLUMN multiple       BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN link_iblock_id BIGINT REFERENCES iblocks (id) ON DELETE SET NULL;

-- Варианты значений свойства-списка
CREATE TABLE iblock_property_enums (
    id          BIGSERIAL PRIMARY KEY,
    property_id BIGINT NOT NULL REFERENCES iblock_properties (id) ON DELETE CASCADE,
    value       TEXT NOT NULL,
    xml_id      TEXT NOT NULL,
    sort        INT NOT NULL DEFAULT 500,
    is_default  BOOLEAN NOT NULL DEFAULT FALSE,
    UNIQUE (property_id, xml_id)
);
