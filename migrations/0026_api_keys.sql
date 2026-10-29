-- API-ключи для внешних систем (склад и будущие API).
CREATE TABLE api_keys (
    id           BIGSERIAL PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name         TEXT NOT NULL CHECK (name <> ''),
    prefix       TEXT NOT NULL UNIQUE,
    secret_hash  TEXT NOT NULL,
    scopes       TEXT[] NOT NULL CHECK (
        cardinality(scopes) > 0
        AND scopes <@ ARRAY['stock.read', 'stock.write', 'stock.conduct']
    ),
    expires_at   TIMESTAMPTZ,
    created_by   BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ,
    last_used_ip TEXT,
    revoked_at   TIMESTAMPTZ
);
CREATE INDEX api_keys_user_idx ON api_keys (user_id);

-- Внешний код документа склада (идемпотентность загрузки из внешних систем).
ALTER TABLE store_documents ADD COLUMN xml_id TEXT;
CREATE UNIQUE INDEX store_documents_xml_id_uq ON store_documents (xml_id) WHERE xml_id <> '';
