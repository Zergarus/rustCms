//! Страница «Модули»: включение и выключение магазина (только суперадмин).

use axum::{
    Extension,
    extract::State,
    response::{Html, Response},
};
use axum_extra::extract::Form;
use minijinja::context;
use serde::Deserialize;

use super::{
    flash::{self, FlashKind},
    nav, render,
};
use crate::{access::Access, error::AppResult, state::AppState};

pub(super) async fn form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require_super()?;
    let shop = nav::shop_enabled(&state.db).await?;
    render(&state, "modules.html", context! { user, shop })
}

#[derive(Deserialize)]
pub(super) struct ModulesForm {
    shop: Option<String>,
}

pub(super) async fn save(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<ModulesForm>,
) -> AppResult<Response> {
    user.require_super()?;
    nav::set_shop_enabled(&state.db, form.shop.as_deref() == Some("on")).await?;
    Ok(flash::redirect(
        "/admin/modules",
        "Настройки модулей сохранены",
        FlashKind::Ok,
    ))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use crate::{
        admin::nav,
        smoke_tests::get,
        test_support::{admin_cookie, permitted_cookie, post_form_raw, test_state},
    };

    #[sqlx::test]
    async fn modules_page_super_only(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let limited = permitted_cookie(&db, "shopper", &["shop.manage"]).await;
        assert_eq!(get(&app, "/admin/modules", &limited).await.0, 403);

        let cookie = admin_cookie(&db).await;
        let (status, body) = get(&app, "/admin/modules", &cookie).await;
        assert_eq!(status, 200);
        assert!(body.contains("Магазин"));
        let (status, _, _) = post_form_raw(&app, "/admin/modules", &cookie, "").await;
        assert_eq!(status, 303);
        assert!(!nav::shop_enabled(&db).await.unwrap());
        post_form_raw(&app, "/admin/modules", &cookie, "shop=on").await;
        assert!(nav::shop_enabled(&db).await.unwrap());
    }
}
