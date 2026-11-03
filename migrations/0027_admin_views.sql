-- Сохранённые виды списков админки: личные и общие, закрепление в левом меню.
CREATE TABLE admin_views (
    id         BIGSERIAL PRIMARY KEY,
    list_key   TEXT NOT NULL,
    owner_id   BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    query      TEXT NOT NULL,          -- нормализованная строка запроса без page
    shared     BOOLEAN NOT NULL DEFAULT FALSE,
    sort       INT NOT NULL DEFAULT 500,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX admin_views_list_idx ON admin_views (list_key);

CREATE TABLE admin_view_pins (
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    view_id BIGINT NOT NULL REFERENCES admin_views(id) ON DELETE CASCADE,
    sort    INT NOT NULL DEFAULT 500,
    PRIMARY KEY (user_id, view_id)
);
