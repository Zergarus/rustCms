-- Версии записи: снимок строки после каждого сохранения или восстановления.
CREATE TABLE item_revisions (
    id            BIGSERIAL PRIMARY KEY,
    item_id       BIGINT NOT NULL REFERENCES collection_items (id) ON DELETE CASCADE,
    collection_id BIGINT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    user_id       BIGINT,
    user_login    TEXT NOT NULL DEFAULT '',
    action        TEXT NOT NULL CHECK (action IN ('create', 'update', 'restore')),
    data          JSONB NOT NULL
);
CREATE INDEX item_revisions_item_idx ON item_revisions (item_id, id DESC);
