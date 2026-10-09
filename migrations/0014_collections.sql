-- Коллекции вместо инфоблоков: только переименования, данные остаются на месте.
-- Словарь: инфоблок → коллекция, элемент → запись, свойство → поле, значение списка → вариант.

-- Таблицы
ALTER TABLE iblocks RENAME TO collections;
ALTER TABLE iblock_elements RENAME TO collection_items;
ALTER TABLE iblock_sections RENAME TO collection_sections;
ALTER TABLE iblock_properties RENAME TO collection_fields;
ALTER TABLE iblock_property_enums RENAME TO collection_field_options;
ALTER TABLE iblock_group_access RENAME TO collection_access;

-- Колонки
ALTER TABLE collection_items RENAME COLUMN iblock_id TO collection_id;
ALTER TABLE collection_items RENAME COLUMN properties TO field_values;
ALTER TABLE collection_sections RENAME COLUMN iblock_id TO collection_id;
ALTER TABLE collection_fields RENAME COLUMN iblock_id TO collection_id;
ALTER TABLE collection_fields RENAME COLUMN link_iblock_id TO link_collection_id;
ALTER TABLE collection_field_options RENAME COLUMN property_id TO field_id;
ALTER TABLE collection_access RENAME COLUMN iblock_id TO collection_id;
ALTER TABLE catalog_prices RENAME COLUMN element_id TO item_id;
ALTER TABLE catalog_store_amounts RENAME COLUMN element_id TO item_id;
ALTER TABLE catalog_products RENAME COLUMN element_id TO item_id;
ALTER TABLE cart_items RENAME COLUMN element_id TO item_id;

-- Ограничения (у первичных и уникальных ключей вместе с ними переименовываются индексы)
ALTER TABLE collections RENAME CONSTRAINT iblocks_pkey TO collections_pkey;
ALTER TABLE collections RENAME CONSTRAINT iblocks_code_key TO collections_code_key;

ALTER TABLE collection_items RENAME CONSTRAINT iblock_elements_pkey TO collection_items_pkey;
ALTER TABLE collection_items RENAME CONSTRAINT iblock_elements_iblock_id_fkey TO collection_items_collection_id_fkey;
ALTER TABLE collection_items RENAME CONSTRAINT iblock_elements_section_id_fkey TO collection_items_section_id_fkey;
ALTER TABLE collection_items RENAME CONSTRAINT iblock_elements_preview_picture_id_fkey TO collection_items_preview_picture_id_fkey;
ALTER TABLE collection_items RENAME CONSTRAINT iblock_elements_detail_picture_id_fkey TO collection_items_detail_picture_id_fkey;

ALTER TABLE collection_sections RENAME CONSTRAINT iblock_sections_pkey TO collection_sections_pkey;
ALTER TABLE collection_sections RENAME CONSTRAINT iblock_sections_iblock_id_fkey TO collection_sections_collection_id_fkey;
ALTER TABLE collection_sections RENAME CONSTRAINT iblock_sections_parent_id_fkey TO collection_sections_parent_id_fkey;
ALTER TABLE collection_sections RENAME CONSTRAINT iblock_sections_picture_id_fkey TO collection_sections_picture_id_fkey;

ALTER TABLE collection_fields RENAME CONSTRAINT iblock_properties_pkey TO collection_fields_pkey;
ALTER TABLE collection_fields RENAME CONSTRAINT iblock_properties_iblock_id_fkey TO collection_fields_collection_id_fkey;
ALTER TABLE collection_fields RENAME CONSTRAINT iblock_properties_iblock_id_code_key TO collection_fields_collection_id_code_key;
ALTER TABLE collection_fields RENAME CONSTRAINT iblock_properties_kind_check TO collection_fields_kind_check;
ALTER TABLE collection_fields RENAME CONSTRAINT iblock_properties_link_iblock_id_fkey TO collection_fields_link_collection_id_fkey;

ALTER TABLE collection_field_options RENAME CONSTRAINT iblock_property_enums_pkey TO collection_field_options_pkey;
ALTER TABLE collection_field_options RENAME CONSTRAINT iblock_property_enums_property_id_fkey TO collection_field_options_field_id_fkey;
ALTER TABLE collection_field_options RENAME CONSTRAINT iblock_property_enums_property_id_xml_id_key TO collection_field_options_field_id_xml_id_key;

ALTER TABLE collection_access RENAME CONSTRAINT iblock_group_access_pkey TO collection_access_pkey;
ALTER TABLE collection_access RENAME CONSTRAINT iblock_group_access_iblock_id_fkey TO collection_access_collection_id_fkey;
ALTER TABLE collection_access RENAME CONSTRAINT iblock_group_access_group_id_fkey TO collection_access_group_id_fkey;
ALTER TABLE collection_access RENAME CONSTRAINT iblock_group_access_level_check TO collection_access_level_check;

ALTER TABLE catalog_prices RENAME CONSTRAINT catalog_prices_element_id_fkey TO catalog_prices_item_id_fkey;
ALTER TABLE catalog_store_amounts RENAME CONSTRAINT catalog_store_amounts_element_id_fkey TO catalog_store_amounts_item_id_fkey;
ALTER TABLE catalog_products RENAME CONSTRAINT catalog_products_element_id_fkey TO catalog_products_item_id_fkey;

-- Индексы
ALTER INDEX iblock_elements_code_key RENAME TO collection_items_code_key;
ALTER INDEX iblock_elements_list_idx RENAME TO collection_items_list_idx;
ALTER INDEX iblock_elements_props_idx RENAME TO collection_items_values_idx;
ALTER INDEX iblock_elements_section_idx RENAME TO collection_items_section_idx;
ALTER INDEX iblock_elements_xml_id_idx RENAME TO collection_items_xml_id_idx;
ALTER INDEX iblock_sections_code_idx RENAME TO collection_sections_code_idx;
ALTER INDEX iblock_sections_tree_idx RENAME TO collection_sections_tree_idx;
ALTER INDEX iblock_group_access_group_id_idx RENAME TO collection_access_group_id_idx;
ALTER INDEX cart_items_element_idx RENAME TO cart_items_item_idx;
ALTER INDEX catalog_prices_element_idx RENAME TO catalog_prices_item_idx;

-- Последовательности
ALTER SEQUENCE iblocks_id_seq RENAME TO collections_id_seq;
ALTER SEQUENCE iblock_elements_id_seq RENAME TO collection_items_id_seq;
ALTER SEQUENCE iblock_sections_id_seq RENAME TO collection_sections_id_seq;
ALTER SEQUENCE iblock_properties_id_seq RENAME TO collection_fields_id_seq;
ALTER SEQUENCE iblock_property_enums_id_seq RENAME TO collection_field_options_id_seq;

-- Удаление записи чистит незаказанные позиции корзины (тело ссылалось на element_id)
CREATE FUNCTION cart_items_drop_item() RETURNS trigger AS $$
BEGIN
    DELETE FROM cart_items WHERE item_id = OLD.id AND order_id IS NULL;
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;
DROP TRIGGER iblock_elements_cart_cleanup ON collection_items;
CREATE TRIGGER collection_items_cart_cleanup
    BEFORE DELETE ON collection_items
    FOR EACH ROW EXECUTE FUNCTION cart_items_drop_item();
DROP FUNCTION cart_items_drop_element();

-- Право на управление
UPDATE group_permissions SET permission = 'collections.manage' WHERE permission = 'iblocks.manage';
