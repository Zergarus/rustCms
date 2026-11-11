-- Rust-скрипты супер-администратора: сохранённый код и история запусков.
CREATE TABLE admin_rust_scripts (
    id         BIGSERIAL PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    code       TEXT NOT NULL DEFAULT '',
    created_by BIGINT,
    updated_by BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE admin_rust_runs (
    id          BIGSERIAL PRIMARY KEY,
    script_id   BIGINT REFERENCES admin_rust_scripts (id) ON DELETE SET NULL,
    user_id     BIGINT,
    user_login  TEXT,
    started_at  TIMESTAMPTZ,
    duration_ms INT,
    ok          BOOL,
    output      TEXT,
    error       TEXT
);

CREATE INDEX admin_rust_runs_script_idx ON admin_rust_runs (script_id, started_at DESC);
