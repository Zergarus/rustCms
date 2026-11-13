-- Параметры обработчика платёжной системы (ID магазина, секретные ключи).
ALTER TABLE pay_systems ADD COLUMN settings JSONB NOT NULL DEFAULT '{}';
