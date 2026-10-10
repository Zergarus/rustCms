-- Обмен с 1С: внешние коды (XML_ID) справочников каталога, хеш файлов, сессии и журнал обмена

ALTER TABLE collections
    ADD COLUMN xml_id TEXT NOT NULL DEFAULT '';
CREATE UNIQUE INDEX collections_xml_id_key ON collections (xml_id) WHERE xml_id <> '';

ALTER TABLE collection_fields
    ADD COLUMN xml_id TEXT NOT NULL DEFAULT '';
CREATE UNIQUE INDEX collection_fields_xml_id_key ON collection_fields (collection_id, xml_id)
    WHERE xml_id <> '';

ALTER TABLE catalog_stores
    ADD COLUMN xml_id TEXT NOT NULL DEFAULT '';
CREATE UNIQUE INDEX catalog_stores_xml_id_key ON catalog_stores (xml_id) WHERE xml_id <> '';

ALTER TABLE catalog_price_types
    ADD COLUMN xml_id TEXT NOT NULL DEFAULT '';
CREATE UNIQUE INDEX catalog_price_types_xml_id_key ON catalog_price_types (xml_id)
    WHERE xml_id <> '';

-- Хеш содержимого файла (hex sha256): по нему обмен узнаёт уже загруженные картинки.
-- У существующих файлов пусто, заполняется по мере надобности.
ALTER TABLE files
    ADD COLUMN sha256 TEXT NOT NULL DEFAULT '';
CREATE INDEX files_sha256_idx ON files (sha256) WHERE sha256 <> '';

CREATE TABLE exchange_sessions (
    id         BIGSERIAL PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    user_id    BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    touched_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    state      JSONB NOT NULL DEFAULT '{}'
);

-- Внешние коды, встреченные в полной выгрузке сессии: остальное считается пропавшим из 1С
CREATE TABLE exchange_seen (
    session_id BIGINT NOT NULL REFERENCES exchange_sessions (id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,
    xml_id     TEXT NOT NULL,
    PRIMARY KEY (session_id, kind, xml_id)
);

CREATE TABLE exchange_log (
    id          BIGSERIAL PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    user_id     BIGINT REFERENCES users (id) ON DELETE SET NULL,
    mode        TEXT NOT NULL DEFAULT '',
    filename    TEXT NOT NULL DEFAULT '',
    result      TEXT NOT NULL DEFAULT '',
    stats       JSONB NOT NULL DEFAULT '{}',
    error       TEXT NOT NULL DEFAULT '',
    duration_ms INT NOT NULL DEFAULT 0
);
CREATE INDEX exchange_log_created_idx ON exchange_log (created_at DESC);
