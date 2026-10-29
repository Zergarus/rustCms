//! Настройки торгового каталога: значения флагов товара по умолчанию.

use axum::{
    Extension, Form,
    extract::State,
    response::{IntoResponse, Response},
};
use minijinja::context;
use serde::Deserialize;

use super::require_shop;
use crate::{
    access::Access,
    admin::render,
    error::AppResult,
    state::AppState,
    stock::{
        self, StockError,
        batches::{self, Method},
        rebuild,
    },
};

const QUANTITY_TRACE: &str = "default_quantity_trace";
const CAN_BUY_ZERO: &str = "default_can_buy_zero";

async fn flag(state: &AppState, name: &str, fallback: bool) -> sqlx::Result<bool> {
    let v: Option<String> =
        sqlx::query_scalar("SELECT value FROM options WHERE module = 'catalog' AND name = $1")
            .bind(name)
            .fetch_optional(&state.db)
            .await?;
    Ok(v.map_or(fallback, |v| v == "Y"))
}

pub async fn form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Response> {
    require_shop(&user)?;
    page(&state, user, None, None).await
}

fn method_name(m: Method) -> &'static str {
    match m {
        Method::Fifo => "FIFO",
        Method::Average => "Средняя",
    }
}

/// Страница настроек; `error` — отказ при выборе метода, `confirm` — метод, ждущий подтверждения.
async fn page(
    state: &AppState,
    user: Access,
    error: Option<String>,
    confirm: Option<Method>,
) -> AppResult<Response> {
    let quantity_trace = flag(state, QUANTITY_TRACE, true).await?;
    let can_buy_zero = flag(state, CAN_BUY_ZERO, false).await?;
    let stock = stock::settings(&mut *state.db.acquire().await?).await?;
    let condition = stock.condition.code();
    let conditions: Vec<minijinja::Value> = [
        ("O", "При оформлении заказа"),
        ("R", "При оплате заказа (в том числе частичной)"),
        ("P", "При полной оплате заказа"),
        ("D", "При разрешении доставки"),
        ("S", "При отгрузке"),
    ]
    .iter()
    .map(|(code, name)| context! { code, name })
    .collect();
    let mut conn = state.db.acquire().await?;
    let method = batches::method(&mut conn).await?;
    let method_label = method.map(method_name);
    let confirm_code = confirm.map(Method::code);
    let confirm_label = confirm.map(method_name);
    // Число готовых товаров не больше общего числа
    let rebuilding = rebuild::progress(&mut conn)
        .await?
        .map(|(done, total)| (done.min(total), total));
    drop(conn);
    Ok(render(
        state,
        "shop/settings.html",
        context! {
            error, method_label, confirm_code, confirm_label, rebuilding,
            user, quantity_trace, can_buy_zero, store_control => stock.store_control,
            reservation => stock.reservation, allow_negative => stock.allow_negative,
            condition, conditions, clear_period => stock.clear_period,
            deduct_on_delivery => stock.deduct_on_delivery,
        },
    )?
    .into_response())
}

#[derive(Deserialize)]
pub struct SettingsForm {
    quantity_trace: Option<String>,
    can_buy_zero: Option<String>,
    store_control: Option<String>,
    reservation: Option<String>,
    allow_negative: Option<String>,
    #[serde(default)]
    condition: String,
    #[serde(default)]
    clear_period: String,
    deduct_on_delivery: Option<String>,
    /// Метод себестоимости: `fifo` | `average`; пусто — не выбран.
    #[serde(default)]
    cost_method: String,
    /// Второй шаг выбора метода.
    #[serde(default)]
    confirm: String,
}

async fn put_option(
    tx: &mut sqlx::PgConnection,
    module: &str,
    name: &str,
    value: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO options (module, name, value) VALUES ($1, $2, $3)
         ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(module)
    .bind(name)
    .bind(value)
    .execute(tx)
    .await?;
    Ok(())
}

fn yn(on: bool) -> &'static str {
    if on { "Y" } else { "N" }
}

pub async fn save(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<SettingsForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let wanted = Method::from_code(form.cost_method.trim());
    let current = batches::method(&mut *state.db.acquire().await?).await?;
    // Метод выбирается один раз; прочие настройки при этом не трогаем
    if let (Some(cur), Some(w)) = (current, wanted)
        && cur != w
    {
        let message = "Метод расчёта себестоимости уже выбран".to_string();
        return page(&state, user, Some(message), None).await;
    }
    if form.confirm == "1" {
        let Some(method) = wanted.filter(|_| current.is_none()) else {
            return Ok(axum::response::Redirect::to("/admin/shop/settings").into_response());
        };
        // Метод и перестройка — в одной транзакции, блокировок товаров она не берёт
        let mut tx = state.db.begin().await?;
        let res = match batches::set_method(&mut tx, method).await {
            Ok(_) => rebuild::start(&mut tx).await.map_err(StockError::from),
            Err(e) => Err(e),
        };
        return match res {
            Ok(_) => {
                tx.commit().await?;
                Ok(axum::response::Redirect::to("/admin/shop/settings").into_response())
            }
            Err(StockError::Rejected(m)) => {
                tx.rollback().await?;
                page(&state, user, Some(m.join("; ")), None).await
            }
            Err(StockError::Db(e)) => Err(e.into()),
        };
    }
    let condition = stock::Condition::from_code(form.condition.trim()).code();
    let clear_period = form
        .clear_period
        .trim()
        .parse::<i32>()
        .map_or(0, |d| d.max(0));
    let mut tx = state.db.begin().await?;
    for (module, name, value) in [
        ("catalog", QUANTITY_TRACE, yn(form.quantity_trace.is_some())),
        ("catalog", CAN_BUY_ZERO, yn(form.can_buy_zero.is_some())),
        (
            "catalog",
            "enable_reservation",
            yn(form.reservation.is_some()),
        ),
        (
            "catalog",
            "allow_negative_amount",
            yn(form.allow_negative.is_some()),
        ),
        ("sale", "product_reserve_condition", condition),
        (
            "sale",
            "product_reserve_clear_period",
            &clear_period.to_string(),
        ),
        (
            "sale",
            "allow_deduction_on_delivery",
            yn(form.deduct_on_delivery.is_some()),
        ),
    ] {
        put_option(&mut tx, module, name, value).await?;
    }
    let on = form.store_control.is_some();
    if on {
        // Включение пересчитывает все товары одним UPDATE: строки блокируем заранее в порядке id,
        // как при оформлении заказа, чтобы не словить взаимную блокировку
        sqlx::query("SELECT item_id FROM catalog_products ORDER BY item_id FOR UPDATE")
            .execute(&mut *tx)
            .await?;
    }
    stock::set_store_control(&mut tx, on).await?;
    tx.commit().await?;
    match wanted.filter(|_| current.is_none()) {
        // Первый шаг выбора метода: предупреждение и кнопка «Выбрать»
        Some(method) => page(&state, user, None, Some(method)).await,
        None => Ok(axum::response::Redirect::to("/admin/shop/settings").into_response()),
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use crate::{
        smoke_tests::{get, post_form},
        test_support::{admin_cookie, order_fixture, post_form_raw, test_state},
    };

    async fn option(db: &PgPool, module: &str, name: &str) -> Option<String> {
        sqlx::query_scalar("SELECT value FROM options WHERE module = $1 AND name = $2")
            .bind(module)
            .bind(name)
            .fetch_optional(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn settings_save_all(db: PgPool) {
        let f = order_fixture(&db).await;
        // неверное доступное и ненулевой резерв: пересчёт должен дать Σ amount − резерв = 5 − 2
        sqlx::query(
            "UPDATE catalog_products SET quantity = 99, quantity_reserved = 2 WHERE item_id = $1",
        )
        .bind(f.product_id)
        .execute(&db)
        .await
        .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (_, html) = get(&app, "/admin/shop/settings", &cookie).await;
        for want in ["store_control", "reservation", "condition", "clear_period"] {
            assert!(html.contains(want), "{want}: {html}");
        }
        let (s, _) = post_form(
            &app,
            "/admin/shop/settings",
            &cookie,
            "quantity_trace=on&store_control=on&allow_negative=on&condition=P&clear_period=7&deduct_on_delivery=on",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        for (module, name, want) in [
            ("catalog", "default_quantity_trace", "Y"),
            ("catalog", "default_can_buy_zero", "N"),
            ("catalog", "default_use_store_control", "Y"),
            ("catalog", "enable_reservation", "N"),
            ("catalog", "allow_negative_amount", "Y"),
            ("sale", "product_reserve_condition", "P"),
            ("sale", "product_reserve_clear_period", "7"),
            ("sale", "allow_deduction_on_delivery", "Y"),
        ] {
            assert_eq!(
                option(&db, module, name).await.as_deref(),
                Some(want),
                "{name}"
            );
        }
        // включение учёта пересчитало доступное: Σ amount (5) − резерв (2)
        let (quantity, _) = crate::test_support::amounts(&db, f.product_id).await;
        assert_eq!(quantity, 3.0);
    }

    #[sqlx::test]
    async fn settings_choose_method_needs_confirm(db: PgPool) {
        order_fixture(&db).await;
        let mut tx = db.begin().await.unwrap();
        crate::stock::set_store_control(&mut tx, true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        // первый шаг: предупреждение, метод не выбран
        let (s, _, html) = post_form_raw(
            &app,
            "/admin/shop/settings",
            &cookie,
            "store_control=on&cost_method=fifo",
        )
        .await;
        assert_eq!(s, 200);
        assert!(
            html.contains("Метод выбирается один раз, сменить его потом будет нельзя"),
            "{html}"
        );
        assert!(html.contains("Выбрать"), "{html}");
        assert_eq!(
            option(&db, "catalog", "cost_price_calculation_method").await,
            None
        );
        // второй шаг
        let (s, _, _) = post_form_raw(
            &app,
            "/admin/shop/settings",
            &cookie,
            "cost_method=fifo&confirm=1",
        )
        .await;
        assert!(s.is_redirection(), "{s}");
        assert_eq!(
            option(&db, "catalog", "cost_price_calculation_method")
                .await
                .as_deref(),
            Some("fifo")
        );
        let (_, html) = get(&app, "/admin/shop/settings", &cookie).await;
        assert!(
            html.contains("FIFO") && !html.contains("name=\"cost_method\""),
            "{html}"
        );
    }

    #[sqlx::test]
    async fn settings_method_readonly_after_choice(db: PgPool) {
        order_fixture(&db).await;
        let mut tx = db.begin().await.unwrap();
        crate::stock::set_store_control(&mut tx, true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        post_form_raw(
            &app,
            "/admin/shop/settings",
            &cookie,
            "cost_method=average&confirm=1",
        )
        .await;
        for body in [
            "cost_method=fifo&confirm=1",
            "store_control=on&cost_method=fifo",
        ] {
            let (s, _, html) = post_form_raw(&app, "/admin/shop/settings", &cookie, body).await;
            assert_eq!(s, 200);
            assert!(html.contains("уже выбран"), "{html}");
            assert_eq!(
                option(&db, "catalog", "cost_price_calculation_method")
                    .await
                    .as_deref(),
                Some("average")
            );
        }
    }

    #[sqlx::test]
    async fn settings_condition_labels(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (_, html) = get(&app, "/admin/shop/settings", &cookie).await;
        for (code, name) in [
            ("O", "При оформлении заказа"),
            ("R", "При оплате заказа (в том числе частичной)"),
            ("P", "При полной оплате заказа"),
            ("D", "При разрешении доставки"),
            ("S", "При отгрузке"),
        ] {
            let selected = if code == "O" { "selected" } else { "" };
            let want = format!("<option value=\"{code}\" {selected}>{name}</option>");
            assert!(html.contains(&want), "{want}: {html}");
        }
    }
}
