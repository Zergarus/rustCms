-- Настройки модулей (аналог b_option): email отправителя, имя сайта и т.п.
CREATE TABLE options (
    module TEXT NOT NULL,
    name   TEXT NOT NULL,
    value  TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (module, name)
);

-- Почтовые шаблоны (аналог b_event_message): письмо на событие с подстановкой #ПОЛЕЙ#
CREATE TABLE mail_templates (
    id         BIGSERIAL PRIMARY KEY,
    event_name TEXT NOT NULL,
    active     BOOLEAN NOT NULL DEFAULT TRUE,
    email_from TEXT NOT NULL DEFAULT '#DEFAULT_EMAIL_FROM#',
    email_to   TEXT NOT NULL DEFAULT '',
    bcc        TEXT NOT NULL DEFAULT '',
    subject    TEXT NOT NULL DEFAULT '',
    body       TEXT NOT NULL DEFAULT '',
    -- text | html
    body_type  TEXT NOT NULL DEFAULT 'text',
    external_id TEXT
);
CREATE INDEX mail_templates_event_idx ON mail_templates (event_name) WHERE active;
CREATE UNIQUE INDEX mail_templates_external_id_key ON mail_templates (external_id) WHERE external_id IS NOT NULL;

-- Реквизиты складов (как в b_catalog_store)
ALTER TABLE catalog_stores
    ADD COLUMN address  TEXT NOT NULL DEFAULT '',
    ADD COLUMN phone    TEXT NOT NULL DEFAULT '',
    ADD COLUMN email    TEXT NOT NULL DEFAULT '',
    ADD COLUMN schedule TEXT NOT NULL DEFAULT '';
