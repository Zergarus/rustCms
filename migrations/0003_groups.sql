-- Группы пользователей
CREATE TABLE groups (
    id          BIGSERIAL PRIMARY KEY,
    code        TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    sort        INT NOT NULL DEFAULT 500,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE user_groups (
    user_id  BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    group_id BIGINT NOT NULL REFERENCES groups (id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, group_id)
);
CREATE INDEX user_groups_group_id_idx ON user_groups (group_id);

-- Права группы на разделы админки (коды — в src/access.rs)
CREATE TABLE group_permissions (
    group_id   BIGINT NOT NULL REFERENCES groups (id) ON DELETE CASCADE,
    permission TEXT NOT NULL,
    PRIMARY KEY (group_id, permission)
);

-- Доступ группы к элементам конкретного инфоблока
CREATE TABLE iblock_group_access (
    iblock_id BIGINT NOT NULL REFERENCES iblocks (id) ON DELETE CASCADE,
    group_id  BIGINT NOT NULL REFERENCES groups (id) ON DELETE CASCADE,
    level     TEXT NOT NULL CHECK (level IN ('read', 'write')),
    PRIMARY KEY (iblock_id, group_id)
);
CREATE INDEX iblock_group_access_group_id_idx ON iblock_group_access (group_id);

-- Стартовая группа для редакторов контента
INSERT INTO groups (code, name, description, sort)
VALUES ('content', 'Контент-менеджеры', 'Вход в админку и работа с элементами разрешённых инфоблоков', 100);
INSERT INTO group_permissions (group_id, permission)
SELECT id, 'admin.access' FROM groups WHERE code = 'content';
