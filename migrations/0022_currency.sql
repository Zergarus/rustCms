-- Валюты и курсы (модуль currency Битрикса): номинал, сортировка, базовая валюта, курсы по датам.
ALTER TABLE currencies
    ADD COLUMN amount_cnt        INT NOT NULL DEFAULT 1 CHECK (amount_cnt > 0),
    ADD COLUMN amount            NUMERIC(18, 4) NOT NULL DEFAULT 1 CHECK (amount > 0),
    ADD COLUMN sort              INT NOT NULL DEFAULT 100,
    ADD COLUMN base              BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN numcode           TEXT NOT NULL DEFAULT '',
    ADD COLUMN full_name         TEXT NOT NULL DEFAULT '',
    -- Курс к текущей базовой валюте, пересчитывается при смене курсов и базовой
    ADD COLUMN current_base_rate NUMERIC(26, 12) NOT NULL DEFAULT 1,
    ADD COLUMN updated_at        TIMESTAMPTZ NOT NULL DEFAULT now();

-- Базовая валюта одна
CREATE UNIQUE INDEX currencies_base_key ON currencies (base) WHERE base;

-- Без базовой по умолчанию рубль, если он есть
UPDATE currencies SET base = TRUE
WHERE code = 'RUB' AND NOT EXISTS (SELECT 1 FROM currencies WHERE base);

CREATE TABLE currency_rates (
    id            BIGSERIAL PRIMARY KEY,
    currency      TEXT NOT NULL REFERENCES currencies (code) ON DELETE CASCADE ON UPDATE CASCADE,
    base_currency TEXT NOT NULL REFERENCES currencies (code) ON DELETE CASCADE ON UPDATE CASCADE,
    date_rate     DATE NOT NULL,
    rate_cnt      INT NOT NULL CHECK (rate_cnt > 0),
    rate          NUMERIC(18, 4) NOT NULL CHECK (rate > 0),
    created_by    BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by    BIGINT REFERENCES users (id) ON DELETE SET NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (currency, base_currency, date_rate)
);
