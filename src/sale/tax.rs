//! НДС заказа: ставка и «цена уже с НДС» — в настройках магазина.

use sqlx::PgPool;

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Ставка НДС и признак «в цене», из опций модуля `sale`.
pub async fn settings(db: &PgPool) -> sqlx::Result<(f64, bool)> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT name, value FROM options WHERE module = 'sale' AND name IN ('vat_rate','vat_included')",
    )
    .fetch_all(db)
    .await?;
    let mut rate: f64 = 0.0;
    let mut included = false;
    for (name, value) in rows {
        match name.as_str() {
            "vat_rate" => rate = value.trim().replace(',', ".").parse().unwrap_or(0.0),
            "vat_included" => included = value == "Y",
            _ => {}
        }
    }
    Ok((rate.max(0.0), included))
}

/// Сумма НДС с оборота `total`: из неё (если цена с НДС) или сверху.
pub fn amount(total: f64, rate: f64, included: bool) -> f64 {
    if rate <= 0.0 || total <= 0.0 {
        return 0.0;
    }
    let tax = if included {
        total * rate / (100.0 + rate)
    } else {
        total * rate / 100.0
    };
    round2(tax)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn settings_come_from_options(db: sqlx::PgPool) {
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES
             ('sale', 'vat_rate', '20'), ('sale', 'vat_included', 'Y')",
        )
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(settings(&db).await.unwrap(), (20.0, true));
    }

    #[test]
    fn included_and_added() {
        assert_eq!(amount(120.0, 20.0, true), 20.0);
        assert_eq!(amount(100.0, 20.0, false), 20.0);
        assert_eq!(amount(100.0, 0.0, false), 0.0);
        assert_eq!(amount(0.0, 20.0, false), 0.0);
        assert_eq!(amount(99.99, 20.0, true), 16.67);
    }
}
