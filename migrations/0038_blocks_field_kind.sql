-- Новый тип поля коллекции: блочный контент (аналог sprint.editor).
ALTER TABLE collection_fields
    DROP CONSTRAINT collection_fields_kind_check,
    ADD CONSTRAINT collection_fields_kind_check
        CHECK (kind IN ('string', 'text', 'number', 'boolean', 'date', 'list', 'element', 'file', 'blocks'));
