//! Курсы валют по датам (как currencies_rates.php в Битриксе) и загрузка из банка.

use axum::{
    Extension, Form, Json,
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{NaiveDate, NaiveTime};
use minijinja::context;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::require_shop;
use crate::{
    access::Access,
    admin::render,
    currency::{self, auto, bank},
    error::{AppError, AppResult},
    state::AppState,
};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct RateForm {
    #[serde(default)]
    pub currency: String,
    #[serde(default)]
    pub base_currency: String,
    #[serde(default)]
    pub date_rate: String,
    #[serde(default)]
    pub rate_cnt: String,
    #[serde(default)]
    pub rate: String,
}

#[derive(Default, Deserialize)]
pub struct Filter {
    #[serde(default)]
    currency: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()
}

#[derive(sqlx::FromRow, Serialize)]
struct RateRow {
    id: i64,
    currency: String,
    base_currency: String,
    date_rate: String,
    rate_cnt: i32,
    rate: f64,
}

/// Список курсов с фильтром, настройками загрузки и итогом последней загрузки.
async fn list_page(
    state: &AppState,
    user: Access,
    filter: Filter,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let from = parse_date(&filter.from);
    let to = parse_date(&filter.to);
    let items: Vec<RateRow> = sqlx::query_as(
        "SELECT id, currency, base_currency, date_rate::text AS date_rate, rate_cnt,
                rate::float8 AS rate
         FROM currency_rates
         WHERE ($1 = '' OR currency = $1)
           AND ($2::date IS NULL OR date_rate >= $2)
           AND ($3::date IS NULL OR date_rate <= $3)
         ORDER BY date_rate DESC, currency, id DESC
         LIMIT 500",
    )
    .bind(filter.currency.trim().to_uppercase())
    .bind(from)
    .bind(to)
    .fetch_all(&state.db)
    .await?;
    let currencies: Vec<String> =
        sqlx::query_scalar("SELECT code FROM currencies ORDER BY sort, code")
            .fetch_all(&state.db)
            .await?;
    let options: Vec<(String, String)> = sqlx::query_as(
        "SELECT name, value FROM options
         WHERE module = 'currency' AND name IN ('auto_load', 'auto_load_time')",
    )
    .fetch_all(&state.db)
    .await?;
    let opt = |name: &str| {
        options
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    let auto_load = opt("auto_load").as_deref() == Some("Y");
    let auto_load_time = opt("auto_load_time").unwrap_or_else(|| "08:00".into());
    let last = auto::last_result(&state.db).await?;
    let last = last.map(|r| {
        json!({
            "at": r.at.with_timezone(&chrono::FixedOffset::east_opt(3 * 3600).expect("смещение")).format("%d.%m.%Y %H:%M").to_string(),
            "date": r.date.format("%d.%m.%Y").to_string(),
            "loaded": r.loaded,
            "skipped": r.skipped,
            "error": r.error,
        })
    });
    let no_base = !super::currencies::has_base(&state.db).await?;
    render(
        state,
        "shop/currency_rates.html",
        context! { user, items, currencies, no_base, filter => (filter.currency, filter.from, filter.to),
        auto_load, auto_load_time, last, error },
    )
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(filter): Query<Filter>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    list_page(&state, user, filter, None).await
}

#[derive(Deserialize)]
pub struct SettingsForm {
    auto_load: Option<String>,
    #[serde(default)]
    auto_load_time: String,
}

/// Настройки ежедневной загрузки: `currency.auto_load`, `currency.auto_load_time`.
pub async fn settings(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<SettingsForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let Ok(time) = NaiveTime::parse_from_str(form.auto_load_time.trim(), "%H:%M") else {
        let e = "Время — ЧЧ:ММ".to_string();
        return Ok(list_page(&state, user, Filter::default(), Some(e))
            .await?
            .into_response());
    };
    let on = if form.auto_load.as_deref() == Some("Y") {
        "Y"
    } else {
        "N"
    };
    for (name, value) in [
        ("auto_load", on.to_string()),
        ("auto_load_time", time.format("%H:%M").to_string()),
    ] {
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ('currency', $1, $2)
             ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(name)
        .bind(value)
        .execute(&state.db)
        .await?;
    }
    Ok(Redirect::to("/admin/shop/currency-rates").into_response())
}

#[derive(Deserialize)]
pub struct FetchQuery {
    #[serde(default)]
    currency: String,
    #[serde(default)]
    base: String,
    #[serde(default)]
    date: String,
}

/// Курс из банка для кнопки «Получить курс» (как `tools/get_rate.php`).
pub async fn fetch_rate(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<FetchQuery>,
) -> AppResult<Json<serde_json::Value>> {
    require_shop(&user)?;
    let error =
        |m: &str| Json(json!({ "STATUS": "ERROR", "MESSAGE": m, "RATE_CNT": null, "RATE": null }));
    let currency = q.currency.trim().to_uppercase();
    if currency.is_empty() {
        return Ok(error("Не указана валюта"));
    }
    let Some(date) = parse_date(&q.date) else {
        return Ok(error("Дата курса валюты не установлена"));
    };
    let mut base = q.base.trim().to_uppercase();
    if base.is_empty() {
        base = sqlx::query_scalar("SELECT code FROM currencies WHERE base")
            .fetch_optional(&state.db)
            .await?
            .unwrap_or_default();
    }
    if base.is_empty() {
        return Ok(error("Не задана базовая валюта"));
    }
    let client = reqwest::Client::new();
    let origin = state.config.currency_bank_url.as_deref();
    let found = bank::fetch(&client, origin, &base, date)
        .await
        .and_then(|rates| {
            rates
                .get(&currency)
                .copied()
                .ok_or_else(|| "Невозможно получить курс".to_string())
        });
    Ok(match found {
        Ok((cnt, rate)) => Json(json!({
            "STATUS": "OK", "MESSAGE": "", "RATE_CNT": cnt, "RATE": rate
        })),
        Err(e) => error(&e),
    })
}

async fn form_page(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: RateForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let currencies: Vec<(String, bool)> =
        sqlx::query_as("SELECT code, base FROM currencies ORDER BY sort, code")
            .fetch_all(&state.db)
            .await?;
    render(
        state,
        "shop/currency_rate_form.html",
        context! { user, id, form, currencies, error },
    )
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let base: Option<String> = sqlx::query_scalar("SELECT code FROM currencies WHERE base")
        .fetch_optional(&state.db)
        .await?;
    let form = RateForm {
        base_currency: base.unwrap_or_default(),
        date_rate: currency::today().to_string(),
        rate_cnt: "1".into(),
        ..Default::default()
    };
    form_page(&state, user, None, form, None).await
}

#[derive(sqlx::FromRow)]
struct EditRow {
    currency: String,
    base_currency: String,
    date_rate: String,
    rate_cnt: i32,
    rate: f64,
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let r: EditRow = sqlx::query_as(
        "SELECT currency, base_currency, date_rate::text AS date_rate, rate_cnt,
                rate::float8 AS rate
         FROM currency_rates WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let form = RateForm {
        currency: r.currency,
        base_currency: r.base_currency,
        date_rate: r.date_rate,
        rate_cnt: r.rate_cnt.to_string(),
        rate: format!("{}", r.rate),
    };
    form_page(&state, user, Some(id), form, None).await
}

/// Проверка формы; тексты ошибок — как в Битриксе.
async fn validate(
    state: &AppState,
    id: Option<i64>,
    form: &RateForm,
) -> AppResult<Result<(String, String, NaiveDate, i32, f64), String>> {
    let currency = form.currency.trim().to_uppercase();
    let base = form.base_currency.trim().to_uppercase();
    if currency.is_empty() {
        return Ok(Err("Не указана валюта".into()));
    }
    if base.is_empty() {
        return Ok(Err("Не задана базовая валюта".into()));
    }
    let Some(date) = parse_date(&form.date_rate) else {
        return Ok(Err("Дата курса валюты не установлена".into()));
    };
    let cnt = match form.rate_cnt.trim().parse::<i32>() {
        Ok(n) if n > 0 => n,
        _ => return Ok(Err("Неверный номинал".into())),
    };
    let Some(rate) = form
        .rate
        .trim()
        .replace(',', ".")
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0 && *v < 1e13)
    else {
        return Ok(Err("Некорректный курс".into()));
    };
    if currency == base {
        return Ok(Err("Нельзя создать курс валюты для самой себя".into()));
    }
    let is_base: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM currencies WHERE code = $1 AND base)")
            .bind(&currency)
            .fetch_one(&state.db)
            .await?;
    if is_base {
        return Ok(Err("Нельзя создать курс для базовой валюты".into()));
    }
    let known: i64 = sqlx::query_scalar("SELECT count(*) FROM currencies WHERE code IN ($1, $2)")
        .bind(&currency)
        .bind(&base)
        .fetch_one(&state.db)
        .await?;
    if known != 2 {
        return Ok(Err("Не указана валюта".into()));
    }
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM currency_rates
         WHERE currency = $1 AND base_currency = $2 AND date_rate = $3
           AND ($4::bigint IS NULL OR id <> $4))",
    )
    .bind(&currency)
    .bind(&base)
    .bind(date)
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    if taken {
        return Ok(Err("Возможно на эту дату курс уже задан.".into()));
    }
    Ok(Ok((currency, base, date, cnt, rate)))
}

async fn save(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: RateForm,
) -> AppResult<Response> {
    let (currency, base, date, cnt, rate) = match validate(state, id, &form).await? {
        Ok(v) => v,
        Err(e) => {
            return Ok(form_page(state, user, id, form, Some(e))
                .await?
                .into_response());
        }
    };
    let by = user.user.id;
    let result = match id {
        None => {
            sqlx::query(
                "INSERT INTO currency_rates
                    (currency, base_currency, date_rate, rate_cnt, rate, created_by, updated_by)
                 VALUES ($1, $2, $3, $4, $5::float8::numeric, $6, $6)",
            )
            .bind(&currency)
            .bind(&base)
            .bind(date)
            .bind(cnt)
            .bind(rate)
            .bind(by)
            .execute(&state.db)
            .await
        }
        Some(id) => {
            let r = sqlx::query(
                "UPDATE currency_rates SET currency = $2, base_currency = $3, date_rate = $4,
                    rate_cnt = $5, rate = $6::float8::numeric, updated_by = $7, updated_at = now()
                 WHERE id = $1",
            )
            .bind(id)
            .bind(&currency)
            .bind(&base)
            .bind(date)
            .bind(cnt)
            .bind(rate)
            .bind(by)
            .execute(&state.db)
            .await;
            if matches!(&r, Ok(r) if r.rows_affected() == 0) {
                return Err(AppError::NotFound);
            }
            r
        }
    };
    // Гонка двух одинаковых запросов: уникальный индекс отвечает так же, как проверка
    if let Err(sqlx::Error::Database(e)) = &result
        && e.is_unique_violation()
    {
        let e = "Возможно на эту дату курс уже задан.".to_string();
        return Ok(form_page(state, user, id, form, Some(e))
            .await?
            .into_response());
    }
    result?;
    let mut conn = state.db.acquire().await?;
    currency::update_base_rates(&mut conn).await?;
    Ok(Redirect::to("/admin/shop/currency-rates").into_response())
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<RateForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, None, form).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<RateForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, Some(id), form).await
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let n = sqlx::query("DELETE FROM currency_rates WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound);
    }
    let mut conn = state.db.acquire().await?;
    currency::update_base_rates(&mut conn).await?;
    Ok(Redirect::to("/admin/shop/currency-rates").into_response())
}

#[cfg(test)]
mod tests {
    use axum::{Router, http::StatusCode, routing::get};

    use crate::{
        smoke_tests::{get as http_get, post_form},
        test_support::{admin_cookie, permitted_cookie, seed_usd, test_state},
    };

    const BASE: &str = "/admin/shop/currency-rates";

    async fn rate_rows(db: &sqlx::PgPool) -> Vec<(String, String, String, i32, f64)> {
        sqlx::query_as(
            "SELECT currency, base_currency, date_rate::text, rate_cnt, rate::float8
             FROM currency_rates ORDER BY id",
        )
        .fetch_all(db)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn rate_crud(db: sqlx::PgPool) {
        seed_usd(&db, None).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let today = crate::currency::today();
        let body = format!("currency=USD&base_currency=RUB&date_rate={today}&rate_cnt=1&rate=81,5");
        let (s, _) = post_form(&app, BASE, &cookie, &body).await;
        assert!(s.is_redirection(), "{s}");
        assert_eq!(
            rate_rows(&db).await,
            vec![("USD".into(), "RUB".into(), today.to_string(), 1, 81.5)]
        );
        let (_, html) = http_get(&app, BASE, &cookie).await;
        assert!(html.contains("81.5"), "{html}");
        let cur: f64 = sqlx::query_scalar(
            "SELECT current_base_rate::float8 FROM currencies WHERE code = 'USD'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(cur, 81.5);
        let id: i64 = sqlx::query_scalar("SELECT id FROM currency_rates")
            .fetch_one(&db)
            .await
            .unwrap();
        let (s, form) = http_get(&app, &format!("{BASE}/{id}"), &cookie).await;
        assert_eq!(s, StatusCode::OK);
        assert!(form.contains("value=\"81.5\""), "{form}");
        let body = format!("currency=USD&base_currency=RUB&date_rate={today}&rate_cnt=1&rate=82");
        let (s, _) = post_form(&app, &format!("{BASE}/{id}"), &cookie, &body).await;
        assert!(s.is_redirection(), "{s}");
        assert_eq!(rate_rows(&db).await[0].4, 82.0);
        let cur: f64 = sqlx::query_scalar(
            "SELECT current_base_rate::float8 FROM currencies WHERE code = 'USD'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(cur, 82.0);
        let (s, _) = post_form(&app, &format!("{BASE}/{id}/delete"), &cookie, "").await;
        assert!(s.is_redirection(), "{s}");
        assert!(rate_rows(&db).await.is_empty());
    }

    #[sqlx::test]
    async fn rate_validation(db: sqlx::PgPool) {
        seed_usd(&db, None).await;
        sqlx::query("INSERT INTO currencies (code) VALUES ('EUR')")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let ok = "currency=USD&base_currency=RUB&date_rate=2026-10-27&rate_cnt=1&rate=80";
        let (s, _) = post_form(&app, BASE, &cookie, ok).await;
        assert!(s.is_redirection(), "{s}");
        for (body, want) in [
            (ok, "Возможно на эту дату курс уже задан."),
            (
                "currency=USD&base_currency=USD&date_rate=2026-10-28&rate_cnt=1&rate=80",
                "Нельзя создать курс валюты для самой себя",
            ),
            (
                "currency=RUB&base_currency=USD&date_rate=2026-10-28&rate_cnt=1&rate=80",
                "Нельзя создать курс для базовой валюты",
            ),
            (
                "currency=EUR&base_currency=RUB&date_rate=2026-10-28&rate_cnt=0&rate=80",
                "Неверный номинал",
            ),
            (
                "currency=EUR&base_currency=RUB&date_rate=2026-10-28&rate_cnt=1&rate=",
                "Некорректный курс",
            ),
        ] {
            let (s, html) = post_form(&app, BASE, &cookie, body).await;
            assert_eq!(s, StatusCode::OK, "{want}");
            assert!(html.contains(want), "{want}: {html}");
        }
        // Введённое сохраняется в форме
        let (_, html) = post_form(
            &app,
            BASE,
            &cookie,
            "currency=EUR&base_currency=RUB&date_rate=2026-10-28&rate_cnt=5&rate=",
        )
        .await;
        assert!(html.contains("value=\"2026-10-28\""), "{html}");
        assert!(html.contains("value=\"5\""), "{html}");
        assert_eq!(rate_rows(&db).await.len(), 1);
    }

    #[sqlx::test]
    async fn rate_list_filter(db: sqlx::PgPool) {
        seed_usd(&db, None).await;
        sqlx::query("INSERT INTO currencies (code) VALUES ('EUR')")
            .execute(&db)
            .await
            .unwrap();
        for (c, d, r) in [
            ("USD", "2026-10-01", 71.5),
            ("USD", "2026-10-20", 72.5),
            ("EUR", "2026-10-20", 91.5),
        ] {
            sqlx::query(
                "INSERT INTO currency_rates (currency, base_currency, date_rate, rate_cnt, rate)
                 VALUES ($1, 'RUB', $2::date, 1, $3::float8::numeric)",
            )
            .bind(c)
            .bind(d)
            .bind(r)
            .execute(&db)
            .await
            .unwrap();
        }
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (_, html) = http_get(&app, BASE, &cookie).await;
        for want in ["71.5", "72.5", "91.5"] {
            assert!(html.contains(&format!("<td>{want}</td>")), "{want}: {html}");
        }
        let (_, html) = http_get(&app, &format!("{BASE}?currency=USD"), &cookie).await;
        assert!(html.contains("<td>71.5</td>") && html.contains("<td>72.5</td>"));
        assert!(!html.contains("<td>91.5</td>"), "{html}");
        let (_, html) = http_get(
            &app,
            &format!("{BASE}?currency=USD&from=2026-10-10&to=2026-10-31"),
            &cookie,
        )
        .await;
        assert!(html.contains("<td>72.5</td>"), "{html}");
        assert!(!html.contains("<td>71.5</td>"), "{html}");
    }

    async fn bank() -> String {
        let app = Router::new().route(
            "/scripts/XML_daily.asp",
            get(|| async { include_bytes!("../../../tests/fixtures/currency/cbr.xml").to_vec() }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[sqlx::test]
    async fn fetch_rate_button(db: sqlx::PgPool) {
        seed_usd(&db, None).await;
        let mut state = test_state(db.clone());
        let mut config = (*state.config).clone();
        config.currency_bank_url = Some(bank().await);
        state.config = std::sync::Arc::new(config);
        let app = crate::app(state);
        let cookie = admin_cookie(&db).await;
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let (s, body) = http_get(
            &app,
            &format!("{BASE}/fetch?currency=USD&base=RUB&date=2026-10-27"),
            &cookie,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let v = json(body);
        assert_eq!(v["STATUS"], "OK");
        assert_eq!(v["RATE_CNT"], 1);
        assert_eq!(v["RATE"], 81.5);
        // База по умолчанию — базовая валюта магазина
        let (_, body) = http_get(
            &app,
            &format!("{BASE}/fetch?currency=UAH&date=2026-10-27"),
            &cookie,
        )
        .await;
        let v = json(body);
        assert_eq!(
            (v["RATE_CNT"].clone(), v["RATE"].clone()),
            (10.into(), 19.6543.into())
        );
        for (q, want) in [
            ("currency=USD&base=RUB", "Дата курса валюты не установлена"),
            ("base=RUB&date=2026-10-27", "Не указана валюта"),
            (
                "currency=GBP&base=RUB&date=2026-10-27",
                "Невозможно получить курс",
            ),
        ] {
            let (_, body) = http_get(&app, &format!("{BASE}/fetch?{q}"), &cookie).await;
            let v = json(body);
            assert_eq!(v["STATUS"], "ERROR", "{q}");
            assert_eq!(v["MESSAGE"], want, "{q}");
        }
        sqlx::query("UPDATE currencies SET base = FALSE")
            .execute(&db)
            .await
            .unwrap();
        let (_, body) = http_get(
            &app,
            &format!("{BASE}/fetch?currency=USD&date=2026-10-27"),
            &cookie,
        )
        .await;
        let v = json(body);
        assert_eq!(v["STATUS"], "ERROR");
        assert_eq!(v["MESSAGE"], "Не задана базовая валюта");
    }

    #[sqlx::test]
    async fn auto_load_settings(db: sqlx::PgPool) {
        seed_usd(&db, None).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s, _) = post_form(
            &app,
            &format!("{BASE}/settings"),
            &cookie,
            "auto_load=Y&auto_load_time=07:30",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let opts: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, value FROM options WHERE module = 'currency' ORDER BY name",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            opts,
            vec![
                ("auto_load".to_string(), "Y".to_string()),
                ("auto_load_time".to_string(), "07:30".to_string())
            ]
        );
        let (_, html) = http_get(&app, BASE, &cookie).await;
        assert!(html.contains("value=\"07:30\""), "{html}");
        let (s, html) = post_form(
            &app,
            &format!("{BASE}/settings"),
            &cookie,
            "auto_load=Y&auto_load_time=25:00",
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(html.contains("Время — ЧЧ:ММ"), "{html}");
        // Флажок снят — N
        post_form(
            &app,
            &format!("{BASE}/settings"),
            &cookie,
            "auto_load_time=07:30",
        )
        .await;
        let v: String = sqlx::query_scalar(
            "SELECT value FROM options WHERE module = 'currency' AND name = 'auto_load'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(v, "N");
        // Итог последней загрузки
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ('currency', 'auto_load_result',
             '{\"at\":\"2026-10-27T05:00:00Z\",\"date\":\"2026-10-27\",\"loaded\":[\"USD\"],\"skipped\":[\"EUR\"],\"error\":null}')",
        )
        .execute(&db)
        .await
        .unwrap();
        let (_, html) = http_get(&app, BASE, &cookie).await;
        assert!(html.contains("Последняя загрузка"), "{html}");
        assert!(html.contains("USD") && html.contains("EUR"), "{html}");
    }

    #[sqlx::test]
    async fn rates_require_shop_permission(db: sqlx::PgPool) {
        seed_usd(&db, None).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = permitted_cookie(&db, "nobody", &[]).await;
        for path in [
            BASE.to_string(),
            format!("{BASE}/new"),
            format!("{BASE}/fetch?currency=USD&date=2026-10-27"),
        ] {
            let (s, _) = http_get(&app, &path, &cookie).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{path}");
        }
        let (s, _) = post_form(&app, &format!("{BASE}/settings"), &cookie, "auto_load=Y").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let ok = permitted_cookie(&db, "shopper", &["shop.manage"]).await;
        let (s, _) = http_get(&app, BASE, &ok).await;
        assert_eq!(s, StatusCode::OK);
    }
}
