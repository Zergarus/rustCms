//! Правила округления цен (аналог `Catalog\Product\Price` и таблицы `b_catalog_round`).

use sqlx::PgConnection;

use crate::discount::model::round2;

/// Допуск сравнения (`Price::VALUE_EPS` Битрикса).
const VALUE_EPS: f64 = 1e-6;

/// Пресеты точности округления в форме правила.
pub const PRESET_PRECISIONS: &[f64] = &[
    0.0001, 0.001, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0,
    200.0, 500.0, 1000.0, 5000.0,
];

/// Способ округления.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundType {
    Math,
    Up,
    Down,
}

impl RoundType {
    pub fn as_str(self) -> &'static str {
        match self {
            RoundType::Math => "math",
            RoundType::Up => "up",
            RoundType::Down => "down",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "math" => Some(RoundType::Math),
            "up" => Some(RoundType::Up),
            "down" => Some(RoundType::Down),
            _ => None,
        }
    }

    /// Код типа в Битриксе: `ROUND_MATH` = 1, `ROUND_UP` = 2, `ROUND_DOWN` = 4.
    pub fn from_bitrix(n: i64) -> Option<Self> {
        match n {
            1 => Some(RoundType::Math),
            2 => Some(RoundType::Up),
            4 => Some(RoundType::Down),
            _ => None,
        }
    }
}

/// Правило округления: для цен выше порога `price` типа `price_type_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundRule {
    pub price_type_id: i64,
    pub price: f64,
    pub round_type: RoundType,
    pub precision: f64,
}

/// `Price::roundValue`: округление до точности `precision`.
pub fn round_value(value: f64, precision: f64, t: RoundType) -> f64 {
    if precision <= 0.0 {
        return 0.0;
    }
    // точность от 1 — только целая часть
    let precision = if precision >= 1.0 {
        precision.trunc()
    } else {
        precision
    };
    if value.abs() <= VALUE_EPS {
        return 0.0;
    }
    if precision < 1.0 {
        round_fraction(value, precision, t)
    } else {
        round_whole(value, precision, t)
    }
}

/// `Price::roundWhole`.
fn round_whole(value: f64, precision: f64, t: RoundType) -> f64 {
    let quotient = value / precision;
    let mut floor = quotient.floor();
    match t {
        RoundType::Up => {
            if quotient - floor > VALUE_EPS {
                floor += 1.0;
            }
        }
        RoundType::Down => {
            if floor < ((value + VALUE_EPS) / precision).floor() {
                floor += 1.0;
            }
        }
        RoundType::Math => {
            if quotient - floor + VALUE_EPS >= 0.5 {
                floor += 1.0;
            }
        }
    }
    floor * precision
}

/// `Price::roundFraction`.
fn round_fraction(value: f64, precision: f64, t: RoundType) -> f64 {
    let floor = value.floor();
    let fraction = value - floor;
    if fraction <= VALUE_EPS {
        return value;
    }
    floor + round_whole(fraction, precision, t)
}

/// Все правила округления магазина.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rounding {
    rules: Vec<RoundRule>,
}

impl Rounding {
    /// Правила сортируются по типу цены, порог — по убыванию (как `getRoundRules`).
    pub fn new(mut rules: Vec<RoundRule>) -> Self {
        rules.sort_by(|a, b| {
            a.price_type_id
                .cmp(&b.price_type_id)
                .then(b.price.total_cmp(&a.price))
        });
        Self { rules }
    }

    /// `searchRoundRule`: первое правило типа с порогом строго меньше цены.
    pub fn rule(&self, price_type_id: i64, price: f64) -> Option<&RoundRule> {
        self.rules
            .iter()
            .filter(|r| r.price_type_id == price_type_id)
            .find(|r| r.price < price)
    }

    /// `roundPrice`: цена ≤ 0, неизвестный тип или нет правила — без изменений.
    pub fn round(&self, price_type_id: Option<i64>, price: f64) -> f64 {
        if price <= 0.0 {
            return price;
        }
        let Some(rule) = price_type_id.and_then(|t| self.rule(t, price)) else {
            return price;
        };
        round2(round_value(price, rule.precision, rule.round_type))
    }
}

/// Загружает правила; `None`, если таблица пуста.
pub async fn load(db: &mut PgConnection) -> sqlx::Result<Option<Rounding>> {
    let rows: Vec<(i64, f64, String, f64)> = sqlx::query_as(
        "SELECT price_type_id, price::float8, round_type, precision::float8 FROM catalog_rounding",
    )
    .fetch_all(&mut *db)
    .await?;
    let rules: Vec<RoundRule> = rows
        .into_iter()
        .filter_map(|(price_type_id, price, t, precision)| {
            Some(RoundRule {
                price_type_id,
                price,
                round_type: RoundType::parse(&t)?,
                precision,
            })
        })
        .collect();
    Ok((!rules.is_empty()).then(|| Rounding::new(rules)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_value_matches_bitrix() {
        use RoundType::*;
        // (значение, точность, math, up, down) — посчитано кодом Битрикса (PHP)
        let table = [
            (1234.56, 10.0, 1230.0, 1240.0, 1230.0),
            (1235.0, 10.0, 1240.0, 1240.0, 1230.0),
            (1234.99, 1.0, 1235.0, 1235.0, 1234.0),
            (99.5, 1.0, 100.0, 100.0, 99.0),
            (0.05, 0.1, 0.1, 0.1, 0.0),
            (1234.56, 0.5, 1234.5, 1235.0, 1234.5),
            (1234.25, 0.5, 1234.5, 1234.5, 1234.0),
            (1234.75, 0.5, 1235.0, 1235.0, 1234.5),
            (1999.99, 100.0, 2000.0, 2000.0, 1900.0),
            (1950.0, 100.0, 2000.0, 2000.0, 1900.0),
            (1949.99, 100.0, 1900.0, 2000.0, 1900.0),
            (10.005, 0.01, 10.01, 10.01, 10.0),
            (0.3, 0.1, 0.3, 0.3, 0.3),
            (4455.0, 50.0, 4450.0, 4500.0, 4450.0),
            (4475.0, 50.0, 4500.0, 4500.0, 4450.0),
            (123.456, 0.05, 123.45, 123.5, 123.45),
            (100.0, 10.0, 100.0, 100.0, 100.0),
            (7.2, 5.0, 5.0, 10.0, 5.0),
            (1234.56, 0.99, 1234.99, 1234.99, 1234.0),
        ];
        for (v, p, m, u, d) in table {
            assert_eq!(round2(round_value(v, p, Math)), m, "{v} {p} math");
            assert_eq!(round2(round_value(v, p, Up)), u, "{v} {p} up");
            assert_eq!(round2(round_value(v, p, Down)), d, "{v} {p} down");
        }
    }

    #[test]
    fn rule_search_is_strictly_greater_than_threshold() {
        let r = Rounding::new(vec![
            RoundRule {
                price_type_id: 1,
                price: 0.0,
                round_type: RoundType::Math,
                precision: 10.0,
            },
            RoundRule {
                price_type_id: 1,
                price: 1000.0,
                round_type: RoundType::Up,
                precision: 100.0,
            },
        ]);
        assert_eq!(r.round(Some(1), 1001.0), 1100.0);
        assert_eq!(r.round(Some(1), 1000.0), 1000.0); // порог 0, math до 10
        assert_eq!(r.round(Some(1), 994.0), 990.0);
        assert_eq!(r.round(Some(1), 0.0), 0.0);
        assert_eq!(r.round(Some(2), 994.0), 994.0); // у типа 2 правил нет
        assert_eq!(r.round(None, 994.0), 994.0);
    }

    #[sqlx::test]
    async fn load_none_when_empty_and_sorted(db: sqlx::PgPool) {
        let mut conn = db.acquire().await.unwrap();
        assert_eq!(load(&mut conn).await.unwrap(), None);
        let t: i64 = sqlx::query_scalar(
            "INSERT INTO catalog_price_types (code, name, is_base) VALUES ('BASE', 'Базовая', TRUE) RETURNING id",
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        for (price, ty, prec) in [(0, "math", 10), (1000, "up", 100)] {
            sqlx::query("INSERT INTO catalog_rounding (price_type_id, price, round_type, precision) VALUES ($1, $2, $3, $4)")
                .bind(t)
                .bind(price)
                .bind(ty)
                .bind(prec)
                .execute(&mut *conn)
                .await
                .unwrap();
        }
        let r = load(&mut conn).await.unwrap().unwrap();
        let rule = r.rule(t, 1500.0).unwrap();
        assert_eq!(rule.price, 1000.0);
        assert_eq!(rule.round_type, RoundType::Up);
    }
}
