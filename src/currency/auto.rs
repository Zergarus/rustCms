//! Ежедневная загрузка курсов из банка с захватом дня одним экземпляром сервера.

use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};

use super::bank;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::state::AppState;

/// Итог загрузки курсов (`currency.auto_load_result`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadResult {
    pub at: DateTime<Utc>,
    pub date: NaiveDate,
    pub loaded: Vec<String>,
    pub skipped: Vec<String>,
    pub error: Option<String>,
}

/// Читает опцию модуля `currency`.
async fn option(db: &PgPool, name: &str) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT value FROM options WHERE module = 'currency' AND name = $1")
        .bind(name)
        .fetch_optional(db)
        .await
}

async fn set_option(db: &PgPool, name: &str, value: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO options (module, name, value) VALUES ('currency', $1, $2)
         ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(name)
    .bind(value)
    .execute(db)
    .await?;
    Ok(())
}

/// Загружает курсы всех небазовых валют из банка базовой на дату; существующие не трогает.
/// Итог пишется в `currency.auto_load_result`.
pub async fn load_for_date(
    db: &PgPool,
    client: &reqwest::Client,
    origin: Option<&str>,
    date: NaiveDate,
) -> sqlx::Result<LoadResult> {
    let mut result = LoadResult {
        at: Utc::now(),
        date,
        loaded: Vec::new(),
        skipped: Vec::new(),
        error: None,
    };
    let base: Option<String> = sqlx::query_scalar("SELECT code FROM currencies WHERE base")
        .fetch_optional(db)
        .await?;
    match base {
        None => result.error = Some("Не задана базовая валюта".to_string()),
        Some(base) => match bank::fetch(client, origin, &base, date).await {
            Err(e) => result.error = Some(e),
            Ok(rates) => {
                let codes: Vec<String> =
                    sqlx::query_scalar("SELECT code FROM currencies WHERE NOT base ORDER BY code")
                        .fetch_all(db)
                        .await?;
                for code in codes {
                    let Some((cnt, rate)) = rates.get(&code).filter(|(c, r)| *c > 0 && *r > 0.0)
                    else {
                        result.skipped.push(code);
                        continue;
                    };
                    let inserted = sqlx::query(
                        "INSERT INTO currency_rates (currency, base_currency, date_rate, rate_cnt, rate)
                         VALUES ($1, $2, $3, $4, $5::float8::numeric)
                         ON CONFLICT (currency, base_currency, date_rate) DO NOTHING",
                    )
                    .bind(&code)
                    .bind(&base)
                    .bind(date)
                    .bind(cnt)
                    .bind(rate)
                    .execute(db)
                    .await?
                    .rows_affected();
                    if inserted > 0 {
                        result.loaded.push(code);
                    }
                }
                let mut conn = db.acquire().await?;
                super::update_base_rates(&mut conn).await?;
            }
        },
    }
    let json = serde_json::to_string(&result).unwrap_or_default();
    set_option(db, "auto_load_result", &json).await?;
    Ok(result)
}

/// Берёт день за этим экземпляром: `true` — день не был занят.
pub async fn claim(db: &PgPool, day: NaiveDate) -> sqlx::Result<bool> {
    let row: Option<i32> = sqlx::query_scalar(
        "INSERT INTO options (module, name, value) VALUES ('currency', 'auto_load_last', $1)
         ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value
         WHERE options.value <> EXCLUDED.value
         RETURNING 1",
    )
    .bind(day.format("%Y-%m-%d").to_string())
    .fetch_optional(db)
    .await?;
    Ok(row.is_some())
}

/// Раз в минуту: после `auto_load_time` (МСК) один экземпляр за день загружает курсы
/// (`auto_load = Y`) или только пересчитывает `current_base_rate`.
pub async fn tick(state: &AppState, now: DateTime<Utc>) -> anyhow::Result<()> {
    let msk = chrono::FixedOffset::east_opt(3 * 3600).expect("смещение в пределах суток");
    let local = now.with_timezone(&msk);
    let at = option(&state.db, "auto_load_time")
        .await?
        .and_then(|v| NaiveTime::parse_from_str(v.trim(), "%H:%M").ok())
        .unwrap_or_else(|| NaiveTime::from_hms_opt(8, 0, 0).expect("корректное время"));
    if local.time() < at || !claim(&state.db, local.date_naive()).await? {
        return Ok(());
    }
    if option(&state.db, "auto_load").await?.as_deref() == Some("Y") {
        let client = reqwest::Client::new();
        let origin = state.config.currency_bank_url.as_deref();
        let res = load_for_date(&state.db, &client, origin, local.date_naive()).await?;
        match &res.error {
            Some(e) => tracing::warn!("курсы валют не загружены: {e}"),
            None => tracing::info!("курсы валют: загружено {}", res.loaded.len()),
        }
    } else {
        let mut conn = state.db.acquire().await?;
        super::update_base_rates(&mut conn).await?;
    }
    Ok(())
}

/// Фоновая задача: проверка раз в минуту, ошибки — в лог.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(60));
        loop {
            timer.tick().await;
            if let Err(e) = tick(&state, Utc::now()).await {
                tracing::error!("ежедневная загрузка курсов: {e:#}");
            }
        }
    });
}

/// Итог последней загрузки.
pub async fn last_result(db: &PgPool) -> sqlx::Result<Option<LoadResult>> {
    Ok(option(db, "auto_load_result")
        .await?
        .and_then(|v| serde_json::from_str(&v).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::currency::today;
    use axum::{Router, http::StatusCode, routing::get};
    use chrono::TimeZone;

    /// Локальный «банк»: отдаёт образец ЦБ или ошибку 500 с HTML.
    async fn bank(fail: bool) -> String {
        let app = Router::new().route(
            "/scripts/XML_daily.asp",
            get(move || async move {
                if fail {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "<html>500</html>".as_bytes().to_vec(),
                    )
                } else {
                    (
                        StatusCode::OK,
                        include_bytes!("../../tests/fixtures/currency/cbr.xml").to_vec(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn seed(db: &PgPool) {
        for (code, base) in [
            ("RUB", true),
            ("USD", false),
            ("UAH", false),
            ("EUR", false),
        ] {
            sqlx::query("INSERT INTO currencies (code, base) VALUES ($1, $2)")
                .bind(code)
                .bind(base)
                .execute(db)
                .await
                .unwrap();
        }
    }

    async fn rate_count(db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM currency_rates")
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn set_option(db: &PgPool, name: &str, value: &str) {
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ('currency', $1, $2)
             ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(name)
        .bind(value)
        .execute(db)
        .await
        .unwrap();
    }

    async fn option(db: &PgPool, name: &str) -> Option<String> {
        sqlx::query_scalar("SELECT value FROM options WHERE module = 'currency' AND name = $1")
            .bind(name)
            .fetch_optional(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn load_for_date_inserts_rates(db: PgPool) {
        seed(&db).await;
        let origin = bank(false).await;
        let day = today();
        let res = load_for_date(&db, &reqwest::Client::new(), Some(&origin), day)
            .await
            .unwrap();
        let mut loaded = res.loaded.clone();
        loaded.sort();
        assert_eq!(loaded, ["UAH", "USD"]);
        assert_eq!(res.skipped, ["EUR"]);
        assert_eq!(res.error, None);
        let usd: (i32, f64) = sqlx::query_as(
            "SELECT rate_cnt, rate::float8 FROM currency_rates WHERE currency = 'USD'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(usd, (1, 81.5));
        let current: f64 = sqlx::query_scalar(
            "SELECT current_base_rate::float8 FROM currencies WHERE code = 'USD'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(current, 81.5);
        let last = last_result(&db).await.unwrap().unwrap();
        assert_eq!(last.date, day);
        assert_eq!(last.skipped, ["EUR"]);
    }

    #[sqlx::test]
    async fn load_keeps_existing_rate(db: PgPool) {
        seed(&db).await;
        let day = today();
        sqlx::query(
            "INSERT INTO currency_rates (currency, base_currency, date_rate, rate_cnt, rate)
             VALUES ('USD', 'RUB', $1, 1, 70)",
        )
        .bind(day)
        .execute(&db)
        .await
        .unwrap();
        let origin = bank(false).await;
        let res = load_for_date(&db, &reqwest::Client::new(), Some(&origin), day)
            .await
            .unwrap();
        assert_eq!(res.loaded, ["UAH"]);
        let usd: f64 =
            sqlx::query_scalar("SELECT rate::float8 FROM currency_rates WHERE currency = 'USD'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(usd, 70.0);
    }

    #[sqlx::test]
    async fn bank_error_keeps_rates(db: PgPool) {
        seed(&db).await;
        sqlx::query(
            "INSERT INTO currency_rates (currency, base_currency, date_rate, rate_cnt, rate)
             VALUES ('USD', 'RUB', '2026-01-01', 1, 70)",
        )
        .execute(&db)
        .await
        .unwrap();
        let origin = bank(true).await;
        let res = load_for_date(&db, &reqwest::Client::new(), Some(&origin), today())
            .await
            .unwrap();
        assert_eq!(res.error.as_deref(), Some("Невозможно получить курс"));
        assert!(res.loaded.is_empty());
        assert_eq!(rate_count(&db).await, 1);
    }

    #[sqlx::test]
    async fn no_base_currency_is_error(db: PgPool) {
        let res = load_for_date(&db, &reqwest::Client::new(), None, today())
            .await
            .unwrap();
        assert_eq!(res.error.as_deref(), Some("Не задана базовая валюта"));
    }

    #[sqlx::test]
    async fn auto_load_claims_once_per_day(db: PgPool) {
        let d1 = NaiveDate::from_ymd_opt(2026, 10, 27).unwrap();
        let (a, b) = tokio::join!(claim(&db, d1), claim(&db, d1));
        let wins = [a.unwrap(), b.unwrap()].iter().filter(|w| **w).count();
        assert_eq!(wins, 1);
        assert!(!claim(&db, d1).await.unwrap());
        assert!(claim(&db, d1.succ_opt().unwrap()).await.unwrap());
    }

    #[sqlx::test]
    async fn tick_respects_time_and_flag(db: PgPool) {
        seed(&db).await;
        let origin = bank(false).await;
        let mut state = crate::test_support::test_state(db.clone());
        let mut config = (*state.config).clone();
        config.currency_bank_url = Some(origin);
        state.config = std::sync::Arc::new(config);
        let msk = chrono::FixedOffset::east_opt(3 * 3600).unwrap();
        let at = |h, m| {
            msk.with_ymd_and_hms(2026, 10, 27, h, m, 0)
                .unwrap()
                .with_timezone(&Utc)
        };

        // Загрузка выключена: курсов нет, но день занят
        tick(&state, at(9, 0)).await.unwrap();
        assert_eq!(rate_count(&db).await, 0);
        assert_eq!(
            option(&db, "auto_load_last").await.as_deref(),
            Some("2026-10-27")
        );

        // Включена, время 10:00: в 09:00 ничего, в 10:01 загрузка
        sqlx::query("DELETE FROM options WHERE name = 'auto_load_last'")
            .execute(&db)
            .await
            .unwrap();
        set_option(&db, "auto_load", "Y").await;
        set_option(&db, "auto_load_time", "10:00").await;
        tick(&state, at(9, 0)).await.unwrap();
        assert_eq!(rate_count(&db).await, 0);
        assert_eq!(option(&db, "auto_load_last").await, None);
        tick(&state, at(10, 1)).await.unwrap();
        assert_eq!(rate_count(&db).await, 2);
        let last = last_result(&db).await.unwrap().unwrap();
        assert_eq!(last.date, NaiveDate::from_ymd_opt(2026, 10, 27).unwrap());
    }
}
