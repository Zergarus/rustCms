//! Правила округления цен (как «Правила округления» торгового каталога Битрикса).

use axum::{
    Extension, Form,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use super::require_shop;
use crate::{
    access::Access,
    admin::render,
    catalog::rounding::{PRESET_PRECISIONS, RoundType},
    error::{AppError, AppResult, is_unique_violation},
    state::AppState,
};

const LIST_URL: &str = "/admin/shop/rounding";

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct RoundingForm {
    #[serde(default)]
    pub price_type_id: String,
    #[serde(default)]
    pub price: String,
    #[serde(default)]
    pub round_type: String,
    /// Выбранный пресет точности; пусто — используется своё значение.
    #[serde(default)]
    pub precision_preset: String,
    #[serde(default)]
    pub precision_custom: String,
}

#[derive(Debug)]
pub struct RoundingInput {
    pub price_type_id: i64,
    pub price: f64,
    pub round_type: RoundType,
    pub precision: f64,
}

/// Число без хвостовых нулей: `100`, `0.5`.
fn fmt_num(v: f64) -> String {
    format!("{v}")
}

fn parse_num(raw: &str) -> Option<f64> {
    raw.trim()
        .replace(',', ".")
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

fn round_type_label(t: &str) -> &'static str {
    match RoundType::parse(t) {
        Some(RoundType::Up) => "Вверх",
        Some(RoundType::Down) => "Вниз",
        _ => "Математически",
    }
}

impl RoundingForm {
    pub fn validate(&self) -> Result<RoundingInput, String> {
        let price_type_id = self
            .price_type_id
            .trim()
            .parse::<i64>()
            .map_err(|_| "Выберите тип цены".to_string())?;
        let price = if self.price.trim().is_empty() {
            0.0
        } else {
            parse_num(&self.price)
                .filter(|p| (0.0..1e13).contains(p))
                .ok_or("Порог цены должен быть неотрицательным числом")?
        };
        let round_type =
            RoundType::parse(self.round_type.trim()).ok_or("Выберите способ округления")?;
        let raw = if self.precision_preset.trim().is_empty() {
            &self.precision_custom
        } else {
            &self.precision_preset
        };
        // В NUMERIC(18,4) значение хранится с 4 знаками: меньшее 0.00005 станет нулём
        let precision = parse_num(raw)
            .filter(|p| (*p * 1e4).round() > 0.0 && *p < 1e13)
            .ok_or("Точность должна быть числом больше нуля")?;
        Ok(RoundingInput {
            price_type_id,
            price,
            round_type,
            precision,
        })
    }
}

#[derive(FromRow)]
struct RuleRow {
    id: i64,
    type_name: String,
    price: f64,
    round_type: String,
    precision: f64,
}

#[derive(Serialize)]
struct RuleView {
    id: i64,
    price: String,
    round_type: &'static str,
    precision: String,
}

#[derive(Serialize)]
struct Group {
    type_name: String,
    rules: Vec<RuleView>,
}

async fn list_page(
    state: &AppState,
    user: Access,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let rows: Vec<RuleRow> = sqlx::query_as(
        "SELECT r.id, t.name AS type_name, r.price::float8 AS price, r.round_type,
                r.precision::float8 AS precision
         FROM catalog_rounding r JOIN catalog_price_types t ON t.id = r.price_type_id
         ORDER BY t.sort, t.id, r.price DESC",
    )
    .fetch_all(&state.db)
    .await?;
    let mut groups: Vec<Group> = Vec::new();
    for r in rows {
        if groups.last().is_none_or(|g| g.type_name != r.type_name) {
            groups.push(Group {
                type_name: r.type_name.clone(),
                rules: Vec::new(),
            });
        }
        if let Some(g) = groups.last_mut() {
            g.rules.push(RuleView {
                id: r.id,
                price: fmt_num(r.price),
                round_type: round_type_label(&r.round_type),
                precision: fmt_num(r.precision),
            });
        }
    }
    let total: usize = groups.iter().map(|g| g.rules.len()).sum();
    render(
        state,
        "shop/rounding.html",
        context! { user, groups, total, error },
    )
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    list_page(&state, user, None).await
}

async fn form_page(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: RoundingForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let price_types: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM catalog_price_types ORDER BY sort, id")
            .fetch_all(&state.db)
            .await?;
    let presets: Vec<String> = PRESET_PRECISIONS.iter().map(|p| fmt_num(*p)).collect();
    render(
        state,
        "shop/rounding_form.html",
        context! { user, id, form, error, price_types, presets },
    )
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    form_page(
        &state,
        user,
        None,
        RoundingForm {
            price: "0".into(),
            round_type: "math".into(),
            precision_preset: "1".into(),
            ..Default::default()
        },
        None,
    )
    .await
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let (price_type_id, price, round_type, precision): (i64, f64, String, f64) = sqlx::query_as(
        "SELECT price_type_id, price::float8, round_type, precision::float8
         FROM catalog_rounding WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let precision_text = fmt_num(precision);
    let is_preset = PRESET_PRECISIONS
        .iter()
        .any(|p| fmt_num(*p) == precision_text);
    let form = RoundingForm {
        price_type_id: price_type_id.to_string(),
        price: fmt_num(price),
        round_type,
        precision_preset: if is_preset {
            precision_text.clone()
        } else {
            String::new()
        },
        precision_custom: if is_preset {
            String::new()
        } else {
            precision_text
        },
    };
    form_page(&state, user, Some(id), form, None).await
}

async fn save(
    state: &AppState,
    user: Access,
    id: Option<i64>,
    form: RoundingForm,
) -> AppResult<Response> {
    let input = match form.validate() {
        Ok(i) => i,
        Err(e) => {
            return Ok(form_page(state, user, id, form, Some(e))
                .await?
                .into_response());
        }
    };
    let type_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM catalog_price_types WHERE id = $1)")
            .bind(input.price_type_id)
            .fetch_one(&state.db)
            .await?;
    if !type_exists {
        let e = "Неизвестный тип цены".to_string();
        return Ok(form_page(state, user, id, form, Some(e))
            .await?
            .into_response());
    }
    let uid = user.user.id;
    let saved: Result<i64, sqlx::Error> = match id {
        Some(id) => {
            sqlx::query_scalar(
                "UPDATE catalog_rounding SET price_type_id = $2, price = $3::float8, round_type = $4,
                        precision = $5::float8, updated_by = $6, updated_at = now()
                 WHERE id = $1 RETURNING id",
            )
            .bind(id)
            .bind(input.price_type_id)
            .bind(input.price)
            .bind(input.round_type.as_str())
            .bind(input.precision)
            .bind(uid)
            .fetch_one(&state.db)
            .await
        }
        None => {
            sqlx::query_scalar(
                "INSERT INTO catalog_rounding (price_type_id, price, round_type, precision, created_by, updated_by)
                 VALUES ($1, $2::float8, $3, $4::float8, $5, $5) RETURNING id",
            )
            .bind(input.price_type_id)
            .bind(input.price)
            .bind(input.round_type.as_str())
            .bind(input.precision)
            .bind(uid)
            .fetch_one(&state.db)
            .await
        }
    };
    match saved {
        Ok(_) => Ok(Redirect::to(LIST_URL).into_response()),
        Err(e) if is_unique_violation(&e) => {
            let e = "Правило с таким порогом уже есть".to_string();
            Ok(form_page(state, user, id, form, Some(e))
                .await?
                .into_response())
        }
        Err(sqlx::Error::RowNotFound) => Err(AppError::NotFound),
        Err(e) => Err(e.into()),
    }
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<RoundingForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    save(&state, user, None, form).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<RoundingForm>,
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
    sqlx::query("DELETE FROM catalog_rounding WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to(LIST_URL).into_response())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use crate::{
        smoke_tests::{get, post_form},
        test_support::{admin_cookie, permitted_cookie, test_state},
    };

    async fn price_type(db: &sqlx::PgPool) {
        sqlx::query("INSERT INTO catalog_price_types (id, code, name, is_base, sort) VALUES (1, 'BASE', 'Розница', TRUE, 100)")
            .execute(db)
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn rounding_crud(db: sqlx::PgPool) {
        price_type(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s, _) = get(&app, "/admin/shop/rounding/new", &cookie).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = post_form(
            &app,
            "/admin/shop/rounding",
            &cookie,
            "price_type_id=1&price=1000&round_type=up&precision_preset=100&precision_custom=",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let (_, html) = get(&app, "/admin/shop/rounding", &cookie).await;
        for want in ["Розница", "&gt; 1000", "Вверх", "100"] {
            assert!(html.contains(want), "{want}: {html}");
        }
        let (id, by): (i64, Option<i64>) =
            sqlx::query_as("SELECT id, created_by FROM catalog_rounding")
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(by.is_some());
        let (s, form) = get(&app, &format!("/admin/shop/rounding/{id}"), &cookie).await;
        assert_eq!(s, StatusCode::OK);
        assert!(form.contains("1000"), "{form}");
        let (s, _) = post_form(
            &app,
            &format!("/admin/shop/rounding/{id}"),
            &cookie,
            "price_type_id=1&price=500&round_type=down&precision_preset=&precision_custom=0,5",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let (_, html) = get(&app, "/admin/shop/rounding", &cookie).await;
        for want in ["&gt; 500", "Вниз", "0.5"] {
            assert!(html.contains(want), "{want}: {html}");
        }
        let (_, form) = get(&app, &format!("/admin/shop/rounding/{id}"), &cookie).await;
        assert!(form.contains("value=\"0.5\""), "{form}");
        let (s, _) = post_form(
            &app,
            &format!("/admin/shop/rounding/{id}/delete"),
            &cookie,
            "",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_rounding")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[sqlx::test]
    async fn rounding_rejects_bad_input(db: sqlx::PgPool) {
        price_type(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        for body in [
            "price_type_id=1&price=777&round_type=up&precision_preset=&precision_custom=0",
            "price_type_id=1&price=777&round_type=up&precision_preset=&precision_custom=-5",
            "price_type_id=1&price=777&round_type=up&precision_preset=&precision_custom=abc",
            // Меньше 0.00005 в NUMERIC(18,4) становится 0
            "price_type_id=1&price=777&round_type=up&precision_preset=&precision_custom=0.00001",
            "price_type_id=99&price=777&round_type=up&precision_preset=10&precision_custom=",
        ] {
            let (s, html) = post_form(&app, "/admin/shop/rounding", &cookie, body).await;
            assert_eq!(s, StatusCode::OK, "{body}");
            assert!(html.contains("class=\"alert\""), "{body}: {html}");
            assert!(html.contains("value=\"777\""), "{body}: {html}");
            if body.ends_with("0.00001") {
                assert!(html.contains("value=\"0.00001\""), "{body}: {html}");
            }
        }
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_rounding")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
        let ok = "price_type_id=1&price=1000&round_type=math&precision_preset=10&precision_custom=";
        let (s, _) = post_form(&app, "/admin/shop/rounding", &cookie, ok).await;
        assert!(s.is_redirection(), "{s}");
        let (s, html) = post_form(&app, "/admin/shop/rounding", &cookie, ok).await;
        assert_eq!(s, StatusCode::OK);
        assert!(html.contains("Правило с таким порогом уже есть"), "{html}");
    }

    #[sqlx::test]
    async fn rounding_requires_shop_permission(db: sqlx::PgPool) {
        price_type(&db).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = permitted_cookie(&db, "nobody", &[]).await;
        let (s, _) = get(&app, "/admin/shop/rounding", &cookie).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = get(&app, "/admin/shop/rounding/new", &cookie).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = post_form(
            &app,
            "/admin/shop/rounding",
            &cookie,
            "price_type_id=1&price=1&round_type=up&precision_preset=10",
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let ok = permitted_cookie(&db, "shopper", &["shop.manage"]).await;
        let (s, _) = get(&app, "/admin/shop/rounding", &ok).await;
        assert_eq!(s, StatusCode::OK);
    }
}
