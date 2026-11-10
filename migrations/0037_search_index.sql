-- Полнотекстовый поиск: словоформы (russian) и подстроки/опечатки (pg_trgm).
CREATE EXTENSION IF NOT EXISTS pg_trgm;

ALTER TABLE collection_items ADD COLUMN search_vector tsvector GENERATED ALWAYS AS (
    to_tsvector(
        'russian',
        coalesce(name, '') || ' ' || coalesce(code, '') || ' ' ||
        coalesce(preview_text, '') || ' ' || coalesce(detail_text, '') || ' ' ||
        coalesce(field_values::text, '')
    )
) STORED;

CREATE INDEX collection_items_search_vector_idx
    ON collection_items USING gin (search_vector);
CREATE INDEX collection_items_name_trgm_idx
    ON collection_items USING gin (name gin_trgm_ops);
CREATE INDEX collection_items_code_trgm_idx
    ON collection_items USING gin (code gin_trgm_ops);
