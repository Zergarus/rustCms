//! Валюты и формат вывода цены (как CCurrencyLang в Битриксе).

use axum::{
    Extension, Form,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use minijinja::context;
use serde::{Deserialize, Serialize};

use super::require_shop;
use crate::{
    access::Access,
    admin::render,
    bxapi::{format_price, registry::Currency},
    currency,
    error::{AppError, AppResult},
    state::AppState,
};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct CurrencyForm {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub format_string: String,
    #[serde(default)]
    pub dec_point: String,
    #[serde(default)]
    pub thousands_sep: String,
    #[serde(default)]
    pub decimals: String,
    pub hide_zero: Option<String>,
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub numcode: String,
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub amount_cnt: String,
    #[serde(default)]
    pub amount: String,
    pub base: Option<String>,
}

/// Поля курса по умолчанию, разобранные из формы.
struct RateFields {
    sort: i32,
    amount_cnt: i32,
    amount: f64,
}

impl CurrencyForm {
    /// Номинал, курс по умолчанию и сортировка; запятая в десятичных допустима.
    fn rate_fields(&self) -> Result<RateFields, String> {
        let amount_cnt = match self.amount_cnt.trim().parse::<i32>() {
            Ok(n) if n > 0 => n,
            _ => return Err("Неверный номинал".into()),
        };
        let amount = self
            .amount
            .trim()
            .replace(',', ".")
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v > 0.0 && *v < 1e13)
            .ok_or("Некорректный курс")?;
        let sort = match self.sort.trim() {
            "" => 100,
            s => s.parse::<i32>().map_err(|_| "Некорректная сортировка")?,
        };
        Ok(RateFields {
            sort,
            amount_cnt,
            amount,
        })
    }
}

impl CurrencyForm {
    fn currency(&self) -> Result<Currency, String> {
        let code = self.code.trim().to_uppercase();
        if code.len() != 3 || !code.chars().all(|c| c.is_ascii_uppercase()) {
            return Err("Код валюты — три латинские буквы (RUB)".into());
        }
        if !self.format_string.contains('#') {
            return Err("В формате должен быть символ # — место числа".into());
        }
        Ok(Currency {
            code,
            format_string: self.format_string.clone(),
            dec_point: self.dec_point.clone(),
            thousands_sep: self.thousands_sep.clone(),
            decimals: self.decimals.trim().parse::<i32>().unwrap_or(2).clamp(0, 4),
            hide_zero: self.hide_zero.is_some(),
        })
    }
}

/// Пример вывода цены в формате валюты из формы.
pub fn preview(form: &CurrencyForm, value: f64) -> String {
    match form.currency() {
        Ok(c) => format_price(value, Some(&c)),
        Err(e) => e,
    }
}

/// Пример для вывода в HTML: формат задаёт администратор, поэтому разметка
/// экранируется, а сущности вида `&#8381;` и `&nbsp;` остаются сущностями.
pub fn preview_html(form: &CurrencyForm, value: f64) -> String {
    let escaped = preview(form, value)
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    let mut out = String::with_capacity(escaped.len());
    let mut rest = escaped.as_str();
    while let Some(pos) = rest.find("&amp;") {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos + 5..];
        let entity = tail.find(';').map(|end| &tail[..end]).filter(|name| {
            let name = name.strip_prefix('#').unwrap_or(name);
            !name.is_empty() && name.len() <= 8 && name.chars().all(|c| c.is_ascii_alphanumeric())
        });
        match entity {
            Some(name) => {
                out.push('&');
                out.push_str(name);
                out.push(';');
                rest = &tail[name.len() + 1..];
            }
            None => {
                out.push_str("&amp;");
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

#[derive(sqlx::FromRow, Serialize)]
struct CurrencyRow {
    code: String,
    full_name: String,
    format_string: String,
    decimals: i32,
    sort: i32,
    amount_cnt: i32,
    amount: f64,
    current_base_rate: f64,
    base: bool,
}

/// Есть ли базовая валюта; без неё цены в других валютах не пересчитываются.
pub async fn has_base(db: &sqlx::PgPool) -> AppResult<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM currencies WHERE base)")
            .fetch_one(db)
            .await?,
    )
}

async fn list_page(
    state: &AppState,
    user: Access,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let items: Vec<CurrencyRow> = sqlx::query_as(
        "SELECT code, full_name, format_string, decimals, sort, amount_cnt,
                amount::float8 AS amount, current_base_rate::float8 AS current_base_rate, base
         FROM currencies ORDER BY sort, code",
    )
    .fetch_all(&state.db)
    .await?;
    let site = currency::site_currency(&mut *state.db.acquire().await?).await?;
    let no_base = !has_base(&state.db).await?;
    render(
        state,
        "shop/currencies.html",
        context! { user, items, site, error, no_base },
    )
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    list_page(&state, user, None).await
}

#[derive(Deserialize)]
pub struct SiteForm {
    #[serde(default)]
    currency: String,
}

/// Выбор валюты сайта (`sale.default_currency`).
pub async fn set_site(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<SiteForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let code = form.currency.trim().to_uppercase();
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM currencies WHERE code = $1)")
            .bind(&code)
            .fetch_one(&state.db)
            .await?;
    if !exists {
        return Ok(list_page(&state, user, Some("Не указана валюта".into()))
            .await?
            .into_response());
    }
    sqlx::query(
        "INSERT INTO options (module, name, value) VALUES ('sale', 'default_currency', $1)
         ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(&code)
    .execute(&state.db)
    .await?;
    Ok(Redirect::to("/admin/shop/currencies").into_response())
}

/// Удаление: базовую, валюту сайта и используемую удалять нельзя.
pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(code): Path<String>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let base: Option<bool> = sqlx::query_scalar("SELECT base FROM currencies WHERE code = $1")
        .bind(&code)
        .fetch_optional(&state.db)
        .await?;
    let Some(base) = base else {
        return Err(AppError::NotFound);
    };
    let mut conn = state.db.acquire().await?;
    let error = if base {
        Some("Нельзя удалить базовую валюту".to_string())
    } else if currency::site_currency(&mut conn).await? == code {
        Some("Нельзя удалить валюту сайта".to_string())
    } else {
        let (prices, deliveries, orders): (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM catalog_prices WHERE currency = $1),
                    (SELECT count(*) FROM deliveries WHERE currency = $1),
                    (SELECT count(*) FROM orders WHERE currency = $1)",
        )
        .bind(&code)
        .fetch_one(&mut *conn)
        .await?;
        (prices + deliveries + orders > 0).then(|| {
            format!("Валюта используется: цены {prices}, доставки {deliveries}, заказы {orders}")
        })
    };
    if let Some(e) = error {
        drop(conn);
        return Ok(list_page(&state, user, Some(e)).await?.into_response());
    }
    sqlx::query("DELETE FROM currencies WHERE code = $1")
        .bind(&code)
        .execute(&mut *conn)
        .await?;
    currency::update_base_rates(&mut conn).await?;
    Ok(Redirect::to("/admin/shop/currencies").into_response())
}

fn form_page(
    state: &AppState,
    user: Access,
    code: Option<String>,
    form: CurrencyForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let sample = preview_html(&form, 1234567.5);
    render(
        state,
        "shop/currency_form.html",
        context! { user, code, form, error, sample },
    )
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let form = CurrencyForm {
        // Первая валюта становится базовой, если базовой ещё нет
        base: (!has_base(&state.db).await?).then(|| "on".into()),
        format_string: "#".into(),
        dec_point: ".".into(),
        thousands_sep: " ".into(),
        decimals: "2".into(),
        hide_zero: Some("on".into()),
        sort: "100".into(),
        amount_cnt: "1".into(),
        amount: "1".into(),
        ..Default::default()
    };
    form_page(&state, user, None, form, None)
}

#[derive(sqlx::FromRow)]
struct EditRow {
    code: String,
    format_string: String,
    dec_point: String,
    thousands_sep: String,
    decimals: i32,
    hide_zero: bool,
    full_name: String,
    numcode: String,
    sort: i32,
    amount_cnt: i32,
    amount: f64,
    base: bool,
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(code): Path<String>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let r: EditRow = sqlx::query_as(
        "SELECT code, format_string, dec_point, thousands_sep, decimals, hide_zero,
                full_name, numcode, sort, amount_cnt, amount::float8 AS amount, base
         FROM currencies WHERE code = $1",
    )
    .bind(&code)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let form = CurrencyForm {
        code: r.code,
        format_string: r.format_string,
        dec_point: r.dec_point,
        thousands_sep: r.thousands_sep,
        decimals: r.decimals.to_string(),
        hide_zero: r.hide_zero.then(|| "on".into()),
        full_name: r.full_name,
        numcode: r.numcode,
        sort: r.sort.to_string(),
        amount_cnt: r.amount_cnt.to_string(),
        amount: format!("{}", r.amount),
        base: r.base.then(|| "on".into()),
    };
    form_page(&state, user, Some(code), form, None)
}

async fn save(
    state: &AppState,
    user: Access,
    code: Option<String>,
    mut form: CurrencyForm,
) -> AppResult<Response> {
    // Код при правке только для чтения: переименования нет
    if let Some(old) = &code {
        form.code = old.clone();
    }
    let parsed = form
        .currency()
        .and_then(|c| form.rate_fields().map(|r| (c, r)));
    let (c, r) = match parsed {
        Ok(v) => v,
        Err(e) => return Ok(form_page(state, user, code, form, Some(e))?.into_response()),
    };
    if code.is_none() {
        let taken: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM currencies WHERE code = $1)")
                .bind(&c.code)
                .fetch_one(&state.db)
                .await?;
        if taken {
            let e = "Валюта с таким кодом уже есть".to_string();
            return Ok(form_page(state, user, code, form, Some(e))?.into_response());
        }
    }
    sqlx::query(
        "INSERT INTO currencies (code, format_string, dec_point, thousands_sep, decimals, hide_zero,
                                 full_name, numcode, sort, amount_cnt, amount)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::float8::numeric)
         ON CONFLICT (code) DO UPDATE SET format_string = EXCLUDED.format_string,
             dec_point = EXCLUDED.dec_point, thousands_sep = EXCLUDED.thousands_sep,
             decimals = EXCLUDED.decimals, hide_zero = EXCLUDED.hide_zero,
             full_name = EXCLUDED.full_name, numcode = EXCLUDED.numcode, sort = EXCLUDED.sort,
             amount_cnt = EXCLUDED.amount_cnt, amount = EXCLUDED.amount, updated_at = now()",
    )
    .bind(&c.code)
    .bind(&c.format_string)
    .bind(&c.dec_point)
    .bind(&c.thousands_sep)
    .bind(c.decimals)
    .bind(c.hide_zero)
    .bind(form.full_name.trim())
    .bind(form.numcode.trim())
    .bind(r.sort)
    .bind(r.amount_cnt)
    .bind(r.amount)
    .execute(&state.db)
    .await?;
    // У базовой валюты номинал и курс всегда 1 (как в Битриксе)
    sqlx::query("UPDATE currencies SET amount = 1, amount_cnt = 1 WHERE code = $1 AND base")
        .bind(&c.code)
        .execute(&state.db)
        .await?;
    let mut conn = state.db.acquire().await?;
    if form.base.is_some() {
        currency::set_base_currency(&mut conn, &c.code).await?;
    } else {
        currency::update_base_rates(&mut conn).await?;
    }
    Ok(Redirect::to("/admin/shop/currencies").into_response())
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<CurrencyForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, None, form).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(code): Path<String>,
    Form(form): Form<CurrencyForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, Some(code), form).await
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;
    use crate::{
        smoke_tests::{get, post_form},
        test_support::{admin_cookie, cart_fixture, seed_usd, test_state},
    };

    #[test]
    fn currency_preview() {
        let form = CurrencyForm {
            code: "RUB".into(),
            format_string: "# &#8381;".into(),
            dec_point: ".".into(),
            thousands_sep: "&nbsp;".into(),
            decimals: "2".into(),
            hide_zero: Some("on".into()),
            ..Default::default()
        };
        assert_eq!(preview(&form, 3949.0), "3&nbsp;949 &#8381;");
        assert_eq!(preview(&form, 597.4), "597.40 &#8381;");
    }

    #[test]
    fn preview_html_escapes_markup() {
        let form = CurrencyForm {
            code: "RUB".into(),
            format_string: "# <img src=x onerror=alert(1)> &#8381;".into(),
            dec_point: ".".into(),
            thousands_sep: "&nbsp;".into(),
            decimals: "0".into(),
            hide_zero: None,
            ..Default::default()
        };
        assert_eq!(
            preview_html(&form, 3949.0),
            "3&nbsp;949 &lt;img src=x onerror=alert(1)&gt; &#8381;"
        );
    }

    const FMT: &str = "format_string=%23&dec_point=.&thousands_sep=+&decimals=2";

    #[sqlx::test]
    async fn currency_crud_with_rates_fields(db: sqlx::PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let body = format!(
            "code=USD&{FMT}&full_name=Доллар США&numcode=840&sort=200&amount_cnt=1&amount=68,79"
        );
        let (s, _) = post_form(&app, "/admin/shop/currencies", &cookie, &body).await;
        assert!(s.is_redirection(), "{s}");
        let (_, html) = get(&app, "/admin/shop/currencies", &cookie).await;
        for want in ["Доллар США", "68.79"] {
            assert!(html.contains(want), "{want}: {html}");
        }
        let (_, form) = get(&app, "/admin/shop/currencies/USD", &cookie).await;
        assert!(form.contains("value=\"68.79\""), "{form}");
        assert!(form.contains("readonly"), "{form}");
        // Правка формата; другой код не переименовывает
        let body = "code=EUR&format_string=%23+USD&dec_point=,&thousands_sep=+&decimals=2&full_name=Доллар&numcode=840&sort=50&amount_cnt=1&amount=70";
        let (s, _) = post_form(&app, "/admin/shop/currencies/USD", &cookie, body).await;
        assert!(s.is_redirection(), "{s}");
        let rows: Vec<(String, String, i32)> =
            sqlx::query_as("SELECT code, format_string, sort FROM currencies")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(rows, vec![("USD".to_string(), "# USD".to_string(), 50)]);
    }

    #[sqlx::test]
    async fn currency_base_switch(db: sqlx::PgPool) {
        seed_usd(&db, Some(70.0)).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let body = format!("{FMT}&sort=100&amount_cnt=1&amount=70&base=on");
        let (s, _) = post_form(&app, "/admin/shop/currencies/USD", &cookie, &body).await;
        assert!(s.is_redirection(), "{s}");
        let rows: Vec<(String, bool)> =
            sqlx::query_as("SELECT code, base FROM currencies ORDER BY code")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![("RUB".to_string(), false), ("USD".to_string(), true)]
        );
        let amount: f64 =
            sqlx::query_scalar("SELECT amount::float8 FROM currencies WHERE code = 'USD'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(amount, 1.0);
    }

    #[sqlx::test]
    async fn currency_delete_rules(db: sqlx::PgPool) {
        let f = cart_fixture(&db).await;
        let _ = f;
        seed_usd(&db, Some(70.0)).await;
        sqlx::query("INSERT INTO currencies (code) VALUES ('EUR')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO currency_rates (currency, base_currency, date_rate, rate_cnt, rate)
             VALUES ('EUR', 'RUB', CURRENT_DATE, 1, 80)",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("UPDATE catalog_prices SET currency = 'USD' WHERE price = 1000")
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let del = |code: &'static str| format!("/admin/shop/currencies/{code}/delete");
        let (s, html) = post_form(&app, &del("RUB"), &cookie, "").await;
        assert_eq!(s, StatusCode::OK);
        assert!(html.contains("Нельзя удалить базовую валюту"), "{html}");
        let (s, html) = post_form(&app, &del("USD"), &cookie, "").await;
        assert_eq!(s, StatusCode::OK);
        assert!(
            html.contains("Валюта используется: цены 1, доставки 0, заказы 0"),
            "{html}"
        );
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ('sale', 'default_currency', 'EUR')",
        )
        .execute(&db)
        .await
        .unwrap();
        let (_, html) = post_form(&app, &del("EUR"), &cookie, "").await;
        assert!(html.contains("Нельзя удалить валюту сайта"), "{html}");
        sqlx::query("UPDATE options SET value = 'RUB' WHERE name = 'default_currency'")
            .execute(&db)
            .await
            .unwrap();
        let (s, _) = post_form(&app, &del("EUR"), &cookie, "").await;
        assert!(s.is_redirection(), "{s}");
        let left: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM currencies WHERE code = 'EUR')
                  + (SELECT count(*) FROM currency_rates WHERE currency = 'EUR')",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(left, 0);
    }

    #[sqlx::test]
    async fn site_currency_option(db: sqlx::PgPool) {
        seed_usd(&db, None).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s, _) = post_form(&app, "/admin/shop/currencies/site", &cookie, "currency=USD").await;
        assert!(s.is_redirection(), "{s}");
        let v: String = sqlx::query_scalar(
            "SELECT value FROM options WHERE module = 'sale' AND name = 'default_currency'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(v, "USD");
        let (_, html) = get(&app, "/admin/shop/currencies", &cookie).await;
        assert!(html.contains("валюта сайта"), "{html}");
        let (s, html) =
            post_form(&app, "/admin/shop/currencies/site", &cookie, "currency=XXX").await;
        assert_eq!(s, StatusCode::OK);
        assert!(html.contains("Не указана валюта"), "{html}");
    }

    #[sqlx::test]
    async fn currency_bad_amount(db: sqlx::PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        for (extra, want) in [
            ("amount_cnt=0&amount=1", "Неверный номинал"),
            ("amount_cnt=1&amount=-1", "Некорректный курс"),
        ] {
            let body = format!("code=USD&{FMT}&{extra}");
            let (s, html) = post_form(&app, "/admin/shop/currencies", &cookie, &body).await;
            assert_eq!(s, StatusCode::OK);
            assert!(html.contains(want), "{want}: {html}");
        }
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM currencies")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[sqlx::test]
    async fn base_currency_amount_stays_one(db: sqlx::PgPool) {
        seed_usd(&db, None).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let body = format!("{FMT}&sort=100&amount_cnt=2&amount=5");
        let (s, _) = post_form(&app, "/admin/shop/currencies/RUB", &cookie, &body).await;
        assert!(s.is_redirection(), "{s}");
        let (cnt, amount): (i32, f64) =
            sqlx::query_as("SELECT amount_cnt, amount::float8 FROM currencies WHERE code = 'RUB'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!((cnt, amount), (1, 1.0));
    }

    #[sqlx::test]
    async fn new_form_ticks_base_only_without_base(db: sqlx::PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (_, html) = get(&app, "/admin/shop/currencies/new", &cookie).await;
        assert!(html.contains("name=\"base\" checked"), "{html}");
        seed_usd(&db, None).await;
        let (_, html) = get(&app, "/admin/shop/currencies/new", &cookie).await;
        assert!(!html.contains("name=\"base\" checked"), "{html}");
    }

    #[sqlx::test]
    async fn pages_warn_without_base_currency(db: sqlx::PgPool) {
        const WARN: &str = "Не задана базовая валюта";
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        sqlx::query("INSERT INTO currencies (code) VALUES ('USD')")
            .execute(&db)
            .await
            .unwrap();
        for url in ["/admin/shop/currencies", "/admin/shop/currency-rates"] {
            let (_, html) = get(&app, url, &cookie).await;
            assert!(html.contains(WARN), "{url}: {html}");
        }
        sqlx::query("UPDATE currencies SET base = TRUE WHERE code = 'USD'")
            .execute(&db)
            .await
            .unwrap();
        for url in ["/admin/shop/currencies", "/admin/shop/currency-rates"] {
            let (_, html) = get(&app, url, &cookie).await;
            assert!(!html.contains(WARN), "{url}: {html}");
        }
    }

    /// Запасное правило миграции 0022: без базовой первая по (sort, code) становится базовой.
    #[sqlx::test]
    async fn migration_makes_first_currency_base_when_none(db: sqlx::PgPool) {
        let sql = include_str!("../../../migrations/0022_currency.sql");
        let from = sql
            .find("-- Запасное правило")
            .expect("в миграции нет запасного правила");
        let stmt = {
            let rest = &sql[from..];
            let start = rest.find("UPDATE").expect("нет UPDATE");
            &rest[start..rest.find(';').expect("нет ;")]
        };
        for (code, sort) in [("EUR", 200), ("USD", 100), ("CNY", 100)] {
            sqlx::query("INSERT INTO currencies (code, sort) VALUES ($1, $2)")
                .bind(code)
                .bind(sort)
                .execute(&db)
                .await
                .unwrap();
        }
        sqlx::query(stmt).execute(&db).await.unwrap();
        let base: Vec<String> = sqlx::query_scalar("SELECT code FROM currencies WHERE base")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(base, ["CNY"]);
        // Базовая уже есть — ничего не меняется
        sqlx::query(stmt).execute(&db).await.unwrap();
        let base: Vec<String> = sqlx::query_scalar("SELECT code FROM currencies WHERE base")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(base, ["CNY"]);
    }
}
