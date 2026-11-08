-- Sitemap как в битриксовом модуле seo: строка-тип и связь с коллекциями.
-- Адреса берутся из шаблона типа, а если он пуст — из шаблонов коллекции
-- (detail_page_url для записей, section_page_url для разделов).
CREATE TABLE seo_sitemap (
    id            BIGSERIAL PRIMARY KEY,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    active        BOOLEAN NOT NULL DEFAULT TRUE,
    name          TEXT NOT NULL UNIQUE CHECK (name ~ '^[a-z0-9-]+$'),
    path_template TEXT NOT NULL DEFAULT '' CHECK (char_length(path_template) <= 500),
    include       TEXT NOT NULL DEFAULT 'items' CHECK (include IN ('items', 'sections')),
    sort          INT NOT NULL DEFAULT 500
);

CREATE TABLE seo_sitemap_collection (
    sitemap_id    BIGINT NOT NULL REFERENCES seo_sitemap (id) ON DELETE CASCADE,
    collection_id BIGINT NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    PRIMARY KEY (sitemap_id, collection_id)
);

CREATE INDEX seo_sitemap_collection_collection_idx ON seo_sitemap_collection (collection_id);
