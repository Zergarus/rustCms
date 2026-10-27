//! Валюты и курсы — порт модуля currency Битрикса: пересчёт по `GetConvertFactorEx`.

pub mod auto;
pub mod bank;

use std::collections::HashMap;

use chrono::{FixedOffset, NaiveDate, Utc};
use sqlx::PgConnection;

use crate::catalog::PurchaseInfo;
use crate::discount::model::round2;

/// Курс валюты: номинал и значение.
type Rate = (i32, f64);

/// Базовая валюта, курс по умолчанию каждой валюты и курсы к текущей базовой.
#[derive(Debug, Clone, Default)]
pub struct Rates {
    base: Option<String>,
    /// Код → (`amount_cnt`, `amount`).
    amounts: HashMap<String, Rate>,
    /// Код → курсы к базовой по убыванию даты.
    rates: HashMap<String, Vec<(NaiveDate, Rate)>>,
}

impl Rates {
    /// Для тестов: курсы собираются из списков, порядок дат любой.
    #[cfg(test)]
    pub fn new(
        base: Option<&str>,
        amounts: &[(&str, i32, f64)],
        rates: &[(&str, NaiveDate, i32, f64)],
    ) -> Rates {
        let mut out = Rates {
            base: base.map(str::to_string),
            ..Rates::default()
        };
        for (code, cnt, amount) in amounts {
            out.amounts.insert(code.to_string(), (*cnt, *amount));
        }
        for (code, date, cnt, rate) in rates {
            out.rates
                .entry(code.to_string())
                .or_default()
                .push((*date, (*cnt, *rate)));
        }
        for list in out.rates.values_mut() {
            list.sort_by_key(|(date, _)| std::cmp::Reverse(*date));
        }
        out
    }

    /// Курс валюты на дату (`_get_last_rates`): последний на дату или раньше, иначе
    /// номинал по умолчанию; `None` — данных нет.
    fn rate_on(&self, code: &str, date: NaiveDate) -> Option<Rate> {
        self.base.as_ref()?;
        self.rates
            .get(code)
            .and_then(|list| list.iter().find(|(d, _)| *d <= date))
            .map(|(_, rate)| *rate)
            .or_else(|| self.amounts.get(code).copied())
    }

    /// Множитель пересчёта суммы из `from` в `to` на дату (`GetConvertFactorEx`);
    /// 0 — пересчитать нельзя.
    pub fn factor(&self, from: &str, to: &str, date: NaiveDate) -> f64 {
        if from.is_empty() || to.is_empty() {
            return 0.0;
        }
        if from == to {
            return 1.0;
        }
        // У исходной валюты без данных 0/0, у целевой — 1/1
        let (from_cnt, from_rate) = self.rate_on(from, date).unwrap_or((0, 0.0));
        let (to_cnt, to_rate) = self.rate_on(to, date).unwrap_or((1, 1.0));
        if from_cnt == 0 || from_rate == 0.0 || to_cnt == 0 || to_rate == 0.0 {
            return 0.0;
        }
        from_rate * f64::from(to_cnt) / to_rate / f64::from(from_cnt)
    }

    /// Сумма в `to`, округлённая до копеек.
    pub fn convert(&self, sum: f64, from: &str, to: &str, date: NaiveDate) -> f64 {
        round2(sum * self.factor(from, to, date))
    }
}

/// Читает валюты и курсы к текущей базовой.
pub async fn load_rates(db: &mut PgConnection) -> sqlx::Result<Rates> {
    let currencies: Vec<(String, i32, f64, bool)> =
        sqlx::query_as("SELECT code, amount_cnt, amount::float8, base FROM currencies")
            .fetch_all(&mut *db)
            .await?;
    let mut out = Rates::default();
    for (code, cnt, amount, base) in currencies {
        if base {
            out.base = Some(code.clone());
        }
        out.amounts.insert(code, (cnt, amount));
    }
    let Some(base) = &out.base else {
        return Ok(out);
    };
    let rows: Vec<(String, NaiveDate, i32, f64)> = sqlx::query_as(
        "SELECT currency, date_rate, rate_cnt, rate::float8 FROM currency_rates
         WHERE base_currency = $1 ORDER BY date_rate DESC",
    )
    .bind(base)
    .fetch_all(&mut *db)
    .await?;
    for (code, date, cnt, rate) in rows {
        out.rates.entry(code).or_default().push((date, (cnt, rate)));
    }
    Ok(out)
}

/// Сегодня по Москве.
pub fn today() -> NaiveDate {
    let msk = FixedOffset::east_opt(3 * 3600).expect("смещение в пределах суток");
    Utc::now().with_timezone(&msk).date_naive()
}

/// Валюта сайта: опция `sale.default_currency`, иначе базовая, иначе `RUB`.
pub async fn site_currency(db: &mut PgConnection) -> sqlx::Result<String> {
    sqlx::query_scalar(
        "SELECT COALESCE(
            (SELECT value FROM options
             WHERE module = 'sale' AND name = 'default_currency' AND value <> ''),
            (SELECT code FROM currencies WHERE base),
            'RUB')",
    )
    .fetch_one(db)
    .await
}

/// Пересчёт сумм в валюту `to` по курсам на сегодня.
#[derive(Debug, Clone)]
pub struct Converter {
    pub to: String,
    rates: Rates,
    date: NaiveDate,
}

impl Converter {
    /// Курсы читаются, только если среди непустых `from` есть отличная от `to`.
    pub async fn new(db: &mut PgConnection, to: &str, from: &[&str]) -> sqlx::Result<Converter> {
        let need = from.iter().any(|c| !c.is_empty() && *c != to);
        let rates = if need {
            load_rates(db).await?
        } else {
            Rates::default()
        };
        Ok(Converter {
            to: to.to_string(),
            rates,
            date: today(),
        })
    }

    /// Без курсов: все суммы должны быть уже в `to`.
    pub fn identity(to: &str) -> Converter {
        Converter {
            to: to.to_string(),
            rates: Rates::default(),
            date: today(),
        }
    }

    /// Та же валюта — сумма как есть; множитель 0 — `None`.
    pub fn convert(&self, sum: f64, from: &str) -> Option<f64> {
        if from == self.to {
            return Some(sum);
        }
        let factor = self.factor(from);
        (factor != 0.0).then(|| round2(sum * factor))
    }

    pub fn factor(&self, from: &str) -> f64 {
        self.rates.factor(from, &self.to, self.date)
    }
}

/// Переводит цены товаров в валюту `conv.to`; цена, которую пересчитать нельзя, удаляется.
pub fn convert_prices(info: &mut HashMap<i64, PurchaseInfo>, conv: &Converter) {
    for item in info.values_mut() {
        item.prices
            .retain_mut(|p| match conv.convert(p.price, &p.currency) {
                Some(price) => {
                    p.price = price;
                    p.currency = conv.to.clone();
                    true
                }
                None => false,
            });
    }
}

/// Переводит цены загруженных товаров в валюту `to`: курсы читаются, только если есть цена в другой.
pub async fn convert_info(
    db: &mut PgConnection,
    to: &str,
    info: &mut HashMap<i64, PurchaseInfo>,
) -> sqlx::Result<()> {
    // Список валют собран заранее — для Send в обработчиках
    let from: Vec<&str> = info
        .values()
        .flat_map(|i| &i.prices)
        .map(|p| p.currency.as_str())
        .collect();
    let conv = Converter::new(db, to, &from).await?;
    convert_prices(info, &conv);
    Ok(())
}

/// Пересчитывает `current_base_rate` всех валют к базовой; без базовой — ничего.
pub async fn update_base_rates(db: &mut PgConnection) -> sqlx::Result<()> {
    let rates = load_rates(db).await?;
    let Some(base) = rates.base.clone() else {
        return Ok(());
    };
    let date = today();
    for code in rates.amounts.keys() {
        let value = if *code == base {
            1.0
        } else {
            rates.factor(code, &base, date)
        };
        sqlx::query(
            "UPDATE currencies SET current_base_rate = $2::float8::numeric WHERE code = $1",
        )
        .bind(code)
        .bind(value)
        .execute(&mut *db)
        .await?;
    }
    Ok(())
}

/// Назначает базовую валюту (`updateBaseCurrency`); `false`, если такой валюты нет.
pub async fn set_base_currency(db: &mut PgConnection, code: &str) -> sqlx::Result<bool> {
    let mut tx = sqlx::Connection::begin(&mut *db).await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM currencies WHERE code = $1)")
            .bind(code)
            .fetch_one(&mut *tx)
            .await?;
    if !exists {
        return Ok(false);
    }
    // Сначала снять у прочих: базовая в уникальном индексе одна
    sqlx::query("UPDATE currencies SET base = FALSE WHERE base AND code <> $1")
        .bind(code)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE currencies SET base = TRUE, amount = 1, amount_cnt = 1, updated_at = now()
         WHERE code = $1",
    )
    .bind(code)
    .execute(&mut *tx)
    .await?;
    update_base_rates(&mut tx).await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn sample(base: Option<&str>) -> Rates {
        Rates::new(
            base,
            &[
                ("RUB", 1, 1.0),
                ("USD", 1, 68.79),
                ("EUR", 1, 78.32),
                ("UAH", 10, 25.11),
            ],
            &[
                ("USD", d(2026, 10, 20), 1, 80.0),
                ("USD", d(2026, 10, 25), 1, 82.0),
                ("USD", d(2026, 10, 30), 1, 90.0),
                ("UAH", d(2026, 10, 25), 10, 19.5),
            ],
        )
    }

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn factor_matches_bitrix() {
        let r = sample(Some("RUB"));
        let day = d(2026, 10, 27);
        close(r.factor("RUB", "RUB", day), 1.0);
        close(r.factor("", "RUB", day), 0.0);
        close(r.factor("RUB", "", day), 0.0);
        close(r.factor("USD", "RUB", day), 82.0);
        close(r.factor("USD", "RUB", d(2026, 10, 22)), 80.0);
        close(r.factor("USD", "RUB", d(2026, 10, 19)), 68.79);
        close(r.factor("USD", "RUB", d(2026, 10, 30)), 90.0);
        // Курс на будущую дату (30.10) сегодня не берётся
        close(r.factor("USD", "RUB", day), 82.0);
        close(r.factor("EUR", "RUB", day), 78.32);
        close(r.factor("UAH", "RUB", day), 1.95);
        close(r.factor("RUB", "USD", day), 1.0 / 82.0);
        close(r.factor("USD", "EUR", day), 82.0 / 78.32);
        close(r.factor("XXX", "RUB", day), 0.0);
        close(r.factor("RUB", "XXX", day), 1.0);
        close(sample(None).factor("USD", "RUB", day), 0.0);
    }

    #[test]
    fn convert_rounds_to_kopecks() {
        let r = sample(Some("RUB"));
        let day = d(2026, 10, 27);
        close(r.convert(10.0, "UAH", "RUB", day), 19.5);
        close(r.convert(1.0, "RUB", "USD", day), 0.01);
    }

    #[test]
    fn converter_identity_keeps_value() {
        let c = Converter::identity("RUB");
        assert_eq!(c.convert(1004.567, "RUB"), Some(1004.567));
        assert_eq!(c.convert(1.0, "USD"), None);
    }

    async fn seed(db: &sqlx::PgPool, rows: &[(&str, bool, f64)]) {
        for (code, base, amount) in rows {
            sqlx::query(
                "INSERT INTO currencies (code, base, amount) VALUES ($1, $2, $3::float8::numeric)",
            )
            .bind(code)
            .bind(base)
            .bind(amount)
            .execute(db)
            .await
            .unwrap();
        }
    }

    async fn add_rate(db: &sqlx::PgPool, code: &str, base: &str, date: NaiveDate, rate: f64) {
        sqlx::query(
            "INSERT INTO currency_rates (currency, base_currency, date_rate, rate_cnt, rate)
             VALUES ($1, $2, $3, 1, $4::float8::numeric)",
        )
        .bind(code)
        .bind(base)
        .bind(date)
        .bind(rate)
        .execute(db)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn site_currency_fallbacks(db: sqlx::PgPool) {
        let mut conn = db.acquire().await.unwrap();
        assert_eq!(site_currency(&mut conn).await.unwrap(), "RUB");
        seed(&db, &[("RUB", false, 1.0), ("USD", true, 1.0)]).await;
        assert_eq!(site_currency(&mut conn).await.unwrap(), "USD");
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ('sale', 'default_currency', 'EUR')",
        )
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(site_currency(&mut conn).await.unwrap(), "EUR");
    }

    #[sqlx::test]
    async fn converter_skips_rates_when_same(db: sqlx::PgPool) {
        seed(&db, &[("RUB", true, 1.0), ("USD", false, 68.79)]).await;
        add_rate(&db, "USD", "RUB", today(), 82.0).await;
        let mut conn = db.acquire().await.unwrap();
        let c = Converter::new(&mut conn, "RUB", &["RUB", ""])
            .await
            .unwrap();
        assert_eq!(c.convert(100.0, "USD"), None);
        assert!(c.rates.rates.is_empty());
        let c = Converter::new(&mut conn, "RUB", &["USD"]).await.unwrap();
        assert_eq!(c.convert(100.0, "USD"), Some(8200.0));
    }

    async fn current_rate(db: &sqlx::PgPool, code: &str) -> f64 {
        sqlx::query_scalar("SELECT current_base_rate::float8 FROM currencies WHERE code = $1")
            .bind(code)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn update_base_rates_sets_current(db: sqlx::PgPool) {
        seed(&db, &[("RUB", true, 1.0), ("USD", false, 68.79)]).await;
        add_rate(&db, "USD", "RUB", today(), 82.0).await;
        let mut conn = db.acquire().await.unwrap();
        update_base_rates(&mut conn).await.unwrap();
        close(current_rate(&db, "USD").await, 82.0);
        close(current_rate(&db, "RUB").await, 1.0);
    }

    #[sqlx::test]
    async fn set_base_currency_recomputes(db: sqlx::PgPool) {
        seed(&db, &[("RUB", true, 1.0), ("USD", false, 68.79)]).await;
        add_rate(&db, "USD", "RUB", today(), 82.0).await;
        let mut conn = db.acquire().await.unwrap();
        assert!(set_base_currency(&mut conn, "USD").await.unwrap());
        let (base, amount, cnt): (bool, f64, i32) = sqlx::query_as(
            "SELECT base, amount::float8, amount_cnt FROM currencies WHERE code = 'USD'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(base);
        close(amount, 1.0);
        assert_eq!(cnt, 1);
        let rub_base: bool = sqlx::query_scalar("SELECT base FROM currencies WHERE code = 'RUB'")
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(!rub_base);
        // Курс USD к RUB больше не используется
        let rates = load_rates(&mut conn).await.unwrap();
        close(rates.factor("RUB", "USD", today()), 1.0);
        close(current_rate(&db, "RUB").await, 1.0);
        assert!(!set_base_currency(&mut conn, "XXX").await.unwrap());
    }
}
