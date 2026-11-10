-- НДС в заказе: сумма налога, рассчитанная на момент оформления.
ALTER TABLE orders ADD COLUMN tax_amount NUMERIC(14,2) NOT NULL DEFAULT 0;
