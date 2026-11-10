-- Очередь писем: событие и поля, попытки и время следующей отправки.
CREATE TABLE mail_queue (
    id         BIGSERIAL PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    event      TEXT NOT NULL,
    fields     JSONB NOT NULL DEFAULT '{}',
    attempts   INTEGER NOT NULL DEFAULT 0,
    next_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    sent_at    TIMESTAMPTZ,
    error      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX mail_queue_pending_idx ON mail_queue (next_at) WHERE sent_at IS NULL;
