-- Журнал действий в админке: кто, что и когда изменил.
CREATE TABLE audit_log (
    id         BIGSERIAL PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    user_id    BIGINT,
    user_login TEXT NOT NULL DEFAULT '',
    action     TEXT NOT NULL,
    entity     TEXT NOT NULL,
    entity_id  TEXT NOT NULL DEFAULT '',
    summary    TEXT NOT NULL DEFAULT '',
    data       JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX audit_log_created_idx ON audit_log (created_at DESC);
CREATE INDEX audit_log_entity_idx ON audit_log (entity, entity_id);
