-- Ровно один владелец; удаление контента удаляет его SEO-настройки.
CREATE TABLE seo_settings (
    id BIGSERIAL PRIMARY KEY,
    collection_id BIGINT UNIQUE REFERENCES collections(id) ON DELETE CASCADE,
    section_id BIGINT UNIQUE REFERENCES collection_sections(id) ON DELETE CASCADE,
    item_id BIGINT UNIQUE REFERENCES collection_items(id) ON DELETE CASCADE,
    title TEXT NOT NULL DEFAULT '' CHECK (char_length(title) <= 2000),
    description TEXT NOT NULL DEFAULT '' CHECK (char_length(description) <= 2000),
    h1 TEXT NOT NULL DEFAULT '' CHECK (char_length(h1) <= 2000),
    CHECK (num_nonnulls(collection_id, section_id, item_id) = 1)
);
