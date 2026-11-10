-- Ротация фоновой сверки платежей: давно не проверенные идут первыми.
ALTER TABLE payments ADD COLUMN reconcile_at TIMESTAMPTZ;
