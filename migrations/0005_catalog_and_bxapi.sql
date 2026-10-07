-- Шаблоны URL как в Битриксе (#SECTION_CODE_PATH#, #ELEMENT_ID#, #ELEMENT_CODE#...)
-- и признак торгового каталога (цены, склады)
ALTER TABLE iblocks
    ADD COLUMN detail_page_url  TEXT NOT NULL DEFAULT '',
    ADD COLUMN section_page_url TEXT NOT NULL DEFAULT '',
    ADD COLUMN list_page_url    TEXT NOT NULL DEFAULT '',
    ADD COLUMN is_catalog       BOOLEAN NOT NULL DEFAULT FALSE;

-- Подтип свойства: 'directory' — привязка к элементу по его внешнему коду
-- (бывший справочник на HL-блоке: в API значение — XML_ID, поля — .item.ufXxx)
ALTER TABLE iblock_properties
    ADD COLUMN user_type TEXT NOT NULL DEFAULT '';

-- Автор элемента (id пользователя сайта; пользователи пока не переносятся — без FK)
ALTER TABLE iblock_elements
    ADD COLUMN created_by BIGINT;

-- Откуда файл: '' — загружен в CMS, 'bitrix' — перенесён (путь как в upload Битрикса)
ALTER TABLE files
    ADD COLUMN source TEXT NOT NULL DEFAULT '';

-- Торговый каталог
CREATE TABLE catalog_price_types (
    id      BIGSERIAL PRIMARY KEY,
    code    TEXT NOT NULL UNIQUE,
    name    TEXT NOT NULL,
    is_base BOOLEAN NOT NULL DEFAULT FALSE,
    sort    INT NOT NULL DEFAULT 500
);

CREATE TABLE catalog_prices (
    id             BIGSERIAL PRIMARY KEY,
    element_id     BIGINT NOT NULL REFERENCES iblock_elements (id) ON DELETE CASCADE,
    price_type_id  BIGINT NOT NULL REFERENCES catalog_price_types (id) ON DELETE CASCADE,
    price          NUMERIC(18, 2) NOT NULL,
    currency       TEXT NOT NULL,
    quantity_from  INT,
    quantity_to    INT
);
CREATE INDEX catalog_prices_element_idx ON catalog_prices (element_id);

CREATE TABLE catalog_stores (
    id     BIGSERIAL PRIMARY KEY,
    name   TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT TRUE,
    sort   INT NOT NULL DEFAULT 500
);

CREATE TABLE catalog_store_amounts (
    element_id BIGINT NOT NULL REFERENCES iblock_elements (id) ON DELETE CASCADE,
    store_id   BIGINT NOT NULL REFERENCES catalog_stores (id) ON DELETE CASCADE,
    amount     NUMERIC(18, 2) NOT NULL DEFAULT 0,
    PRIMARY KEY (element_id, store_id)
);

-- Товарные данные элемента каталога (общий остаток)
CREATE TABLE catalog_products (
    element_id BIGINT PRIMARY KEY REFERENCES iblock_elements (id) ON DELETE CASCADE,
    quantity   NUMERIC(18, 2) NOT NULL DEFAULT 0
);

-- Формат вывода цены по валюте (как CCurrencyLang в Битриксе): '# &#8381;'
CREATE TABLE currencies (
    code          TEXT PRIMARY KEY,
    format_string TEXT NOT NULL DEFAULT '#',
    dec_point     TEXT NOT NULL DEFAULT '.',
    thousands_sep TEXT NOT NULL DEFAULT ' ',
    decimals      INT NOT NULL DEFAULT 2,
    hide_zero     BOOLEAN NOT NULL DEFAULT TRUE
);
