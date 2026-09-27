ALTER TABLE users
    ADD COLUMN name          TEXT NOT NULL DEFAULT '',
    ADD COLUMN last_login_at TIMESTAMPTZ;
