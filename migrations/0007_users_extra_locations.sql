-- Поля профиля пользователя (как в b_user) и пользовательские UF-поля
ALTER TABLE users
    ADD COLUMN second_name   TEXT NOT NULL DEFAULT '',
    ADD COLUMN phone         TEXT NOT NULL DEFAULT '',
    ADD COLUMN city          TEXT NOT NULL DEFAULT '',
    ADD COLUMN work_position TEXT NOT NULL DEFAULT '',
    ADD COLUMN photo_id      BIGINT REFERENCES files (id) ON DELETE SET NULL,
    -- UF-поля: ключ — код поля в snake_case (uf_phone_list), множественное — массив
    ADD COLUMN extra         JSONB NOT NULL DEFAULT '{}';

-- Группы, перенесённые из Битрикса (права в админке у них пустые)
ALTER TABLE groups
    ADD COLUMN external_id TEXT;
CREATE UNIQUE INDEX groups_external_id_key ON groups (external_id) WHERE external_id IS NOT NULL;

-- Местоположения (модуль sale): дерево страна → округ → регион → район → город
CREATE TABLE locations (
    id          BIGINT PRIMARY KEY,
    code        TEXT NOT NULL UNIQUE,
    parent_id   BIGINT REFERENCES locations (id) ON DELETE CASCADE,
    type_code   TEXT NOT NULL,
    name        TEXT NOT NULL,
    sort        INT NOT NULL DEFAULT 100,
    depth_level INT NOT NULL DEFAULT 1
);
CREATE INDEX locations_parent_idx ON locations (parent_id);
CREATE INDEX locations_type_idx ON locations (type_code);
