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
    format_string: String,
    decimals: i32,
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let items: Vec<CurrencyRow> =
        sqlx::query_as("SELECT code, format_string, decimals FROM currencies ORDER BY code")
            .fetch_all(&state.db)
            .await?;
    render(&state, "shop/currencies.html", context! { user, items })
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
        format_string: "#".into(),
        dec_point: ".".into(),
        thousands_sep: " ".into(),
        decimals: "2".into(),
        hide_zero: Some("on".into()),
        ..Default::default()
    };
    form_page(&state, user, None, form, None)
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(code): Path<String>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let c: Currency = sqlx::query_as(
        "SELECT code, format_string, dec_point, thousands_sep, decimals, hide_zero FROM currencies WHERE code = $1",
    )
    .bind(&code)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let form = CurrencyForm {
        code: c.code,
        format_string: c.format_string,
        dec_point: c.dec_point,
        thousands_sep: c.thousands_sep,
        decimals: c.decimals.to_string(),
        hide_zero: c.hide_zero.then(|| "on".into()),
    };
    form_page(&state, user, Some(code), form, None)
}

async fn save(
    state: &AppState,
    user: Access,
    code: Option<String>,
    form: CurrencyForm,
) -> AppResult<Response> {
    let c = match form.currency() {
        Ok(c) => c,
        Err(e) => return Ok(form_page(state, user, code, form, Some(e))?.into_response()),
    };
    if let Some(old) = &code
        && *old != c.code
    {
        sqlx::query("DELETE FROM currencies WHERE code = $1")
            .bind(old)
            .execute(&state.db)
            .await?;
    }
    sqlx::query(
        "INSERT INTO currencies (code, format_string, dec_point, thousands_sep, decimals, hide_zero)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (code) DO UPDATE SET format_string = EXCLUDED.format_string,
             dec_point = EXCLUDED.dec_point, thousands_sep = EXCLUDED.thousands_sep,
             decimals = EXCLUDED.decimals, hide_zero = EXCLUDED.hide_zero",
    )
    .bind(&c.code)
    .bind(&c.format_string)
    .bind(&c.dec_point)
    .bind(&c.thousands_sep)
    .bind(c.decimals)
    .bind(c.hide_zero)
    .execute(&state.db)
    .await?;
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
    use super::*;

    #[test]
    fn currency_preview() {
        let form = CurrencyForm {
            code: "RUB".into(),
            format_string: "# &#8381;".into(),
            dec_point: ".".into(),
            thousands_sep: "&nbsp;".into(),
            decimals: "2".into(),
            hide_zero: Some("on".into()),
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
        };
        assert_eq!(
            preview_html(&form, 3949.0),
            "3&nbsp;949 &lt;img src=x onerror=alert(1)&gt; &#8381;"
        );
    }
}
