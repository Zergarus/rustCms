//! Админ-панель: серверный рендер (MiniJinja) + HTMX.

mod auth_pages;
mod collections;
mod discounts;
mod exchange;
mod groups;
mod items;
mod sections;
mod shop;
mod sku;
mod users;

use axum::{
    Extension, Router,
    extract::{DefaultBodyLimit, Multipart, Request, State, multipart::MultipartError},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use axum_extra::extract::CookieJar;
use minijinja::{Value, context};
use tower_http::services::ServeDir;

use crate::{
    access::Access,
    auth,
    error::{AppError, AppResult},
    files::{self, FileRecord},
    state::AppState,
};

/// Предел размера запроса в админке (формы с файлами).
const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

pub fn router(state: AppState) -> Router<AppState> {
    let protected = Router::new()
        .route("/", get(dashboard))
        .route("/logout", post(auth_pages::logout))
        .route(
            "/collections",
            get(collections::list).post(collections::create),
        )
        .route("/collections/new", get(collections::new_form))
        .route(
            "/collections/{id}",
            get(collections::edit_form).post(collections::update),
        )
        .route("/collections/{id}/delete", post(collections::delete))
        .route("/collections/{id}/fields", post(collections::add_field))
        .route("/collections/{id}/offers", post(collections::set_offers))
        .route(
            "/fields/{id}",
            get(collections::field_form).post(collections::update_field),
        )
        .route("/fields/{id}/options", post(collections::save_options))
        .route("/fields/{id}/delete", post(collections::delete_field))
        .route("/collections/{id}/sections", post(sections::create))
        .route("/collections/{id}/sections/new", get(sections::new_form))
        .route(
            "/sections/{id}",
            get(sections::edit_form).post(sections::update),
        )
        .route("/sections/{id}/delete", post(sections::delete))
        .route(
            "/collections/{id}/items",
            get(items::list).post(items::create),
        )
        .route("/collections/{id}/items/new", get(items::new_form))
        .route("/items/{id}", get(items::edit_form).post(items::update))
        .route("/items/{id}/delete", post(items::delete))
        .route(
            "/items/{id}/offers/generate",
            get(sku::generate_form).post(sku::generate),
        )
        .route(
            "/shop/stores",
            get(shop::stores::list).post(shop::stores::create),
        )
        .route("/shop/stores/new", get(shop::stores::new_form))
        .route(
            "/shop/stores/{id}",
            get(shop::stores::edit_form).post(shop::stores::update),
        )
        .route("/shop/locations", get(shop::locations))
        .route(
            "/shop/store-docs",
            get(shop::store_docs::list).post(shop::store_docs::create),
        )
        .route("/shop/store-docs/new", get(shop::store_docs::new_form))
        .route("/shop/store-docs/products", get(shop::store_docs::products))
        .route(
            "/shop/store-docs/{id}",
            get(shop::store_docs::view).post(shop::store_docs::update),
        )
        .route(
            "/shop/store-docs/{id}/conduct",
            post(shop::store_docs::conduct),
        )
        .route(
            "/shop/store-docs/{id}/cancel",
            post(shop::store_docs::cancel),
        )
        .route(
            "/shop/store-docs/{id}/delete",
            post(shop::store_docs::delete),
        )
        .route(
            "/shop/contractors",
            get(shop::contractors::list).post(shop::contractors::create),
        )
        .route("/shop/contractors/new", get(shop::contractors::new_form))
        .route(
            "/shop/contractors/{id}",
            get(shop::contractors::edit_form).post(shop::contractors::update),
        )
        .route(
            "/shop/contractors/{id}/delete",
            post(shop::contractors::delete),
        )
        .route("/shop/orders", get(shop::orders::list))
        .route("/shop/orders/{id}", get(shop::orders::view))
        .route("/shop/orders/{id}/xml1c", get(shop::orders::xml1c))
        .route("/shop/orders/{id}/status", post(shop::orders::set_status))
        .route("/shop/orders/{id}/paid", post(shop::orders::set_paid))
        .route("/shop/orders/{id}/cancel", post(shop::orders::set_canceled))
        .route(
            "/shop/orders/{id}/discounts",
            post(shop::orders::recalculate_discounts),
        )
        .route(
            "/shop/orders/{id}/props",
            post(shop::orders::save_properties),
        )
        .route("/shop/orders/{id}/files", post(shop::orders::save_files))
        .route(
            "/shop/orders/{id}/shipment",
            post(shop::orders::save_shipment),
        )
        .route(
            "/shop/orders/{id}/items",
            post(shop::order_edit::save_items),
        )
        .route(
            "/shop/statuses",
            get(shop::statuses::list).post(shop::statuses::create),
        )
        .route("/shop/statuses/new", get(shop::statuses::new_form))
        .route(
            "/shop/statuses/{code}",
            get(shop::statuses::edit_form).post(shop::statuses::update),
        )
        .route("/shop/statuses/{code}/delete", post(shop::statuses::delete))
        .route(
            "/shop/person-types",
            get(shop::person_types::list).post(shop::person_types::create),
        )
        .route("/shop/person-types/new", get(shop::person_types::new_form))
        .route(
            "/shop/person-types/{id}",
            get(shop::person_types::edit_form).post(shop::person_types::update),
        )
        .route(
            "/shop/person-types/{id}/delete",
            post(shop::person_types::delete),
        )
        .route(
            "/shop/person-types/{id}/props",
            get(shop::order_props::page).post(shop::order_props::prop_create),
        )
        .route(
            "/shop/person-types/{id}/props/new",
            get(shop::order_props::prop_new),
        )
        .route(
            "/shop/person-types/{id}/groups",
            post(shop::order_props::group_create),
        )
        .route(
            "/shop/person-types/{id}/groups/new",
            get(shop::order_props::group_new),
        )
        .route(
            "/shop/order-prop-groups/{id}",
            get(shop::order_props::group_edit).post(shop::order_props::group_update),
        )
        .route(
            "/shop/order-prop-groups/{id}/delete",
            post(shop::order_props::group_delete),
        )
        .route(
            "/shop/order-props/{id}",
            get(shop::order_props::prop_edit).post(shop::order_props::prop_update),
        )
        .route(
            "/shop/order-props/{id}/delete",
            post(shop::order_props::prop_delete),
        )
        .route(
            "/shop/deliveries",
            get(shop::deliveries::list).post(shop::deliveries::create),
        )
        .route("/shop/deliveries/new", get(shop::deliveries::new_form))
        .route(
            "/shop/deliveries/{id}",
            get(shop::deliveries::edit_form).post(shop::deliveries::update),
        )
        .route(
            "/shop/deliveries/{id}/delete",
            post(shop::deliveries::delete),
        )
        .route(
            "/shop/pay-systems",
            get(shop::pay_systems::list).post(shop::pay_systems::create),
        )
        .route("/shop/pay-systems/new", get(shop::pay_systems::new_form))
        .route(
            "/shop/pay-systems/{id}",
            get(shop::pay_systems::edit_form).post(shop::pay_systems::update),
        )
        .route(
            "/shop/pay-systems/{id}/delete",
            post(shop::pay_systems::delete),
        )
        .route("/shop/carts", get(shop::carts::list))
        .route("/shop/carts/{id}", get(shop::carts::view))
        .route(
            "/shop/price-types",
            get(shop::price_types::list).post(shop::price_types::create),
        )
        .route("/shop/price-types/new", get(shop::price_types::new_form))
        .route(
            "/shop/price-types/{id}",
            get(shop::price_types::edit_form).post(shop::price_types::update),
        )
        .route(
            "/shop/price-types/{id}/delete",
            post(shop::price_types::delete),
        )
        .route(
            "/shop/rounding",
            get(shop::rounding::list).post(shop::rounding::create),
        )
        .route("/shop/rounding/new", get(shop::rounding::new_form))
        .route(
            "/shop/rounding/{id}",
            get(shop::rounding::edit_form).post(shop::rounding::update),
        )
        .route("/shop/rounding/{id}/delete", post(shop::rounding::delete))
        .route(
            "/shop/currencies",
            get(shop::currencies::list).post(shop::currencies::create),
        )
        .route("/shop/currencies/new", get(shop::currencies::new_form))
        .route("/shop/currencies/site", post(shop::currencies::set_site))
        .route(
            "/shop/currencies/{code}/delete",
            post(shop::currencies::delete),
        )
        .route(
            "/shop/currencies/{code}",
            get(shop::currencies::edit_form).post(shop::currencies::update),
        )
        .route(
            "/shop/currency-rates",
            get(shop::currency_rates::list).post(shop::currency_rates::create),
        )
        .route(
            "/shop/currency-rates/new",
            get(shop::currency_rates::new_form),
        )
        .route(
            "/shop/currency-rates/fetch",
            get(shop::currency_rates::fetch_rate),
        )
        .route(
            "/shop/currency-rates/settings",
            post(shop::currency_rates::settings),
        )
        .route(
            "/shop/currency-rates/{id}/delete",
            post(shop::currency_rates::delete),
        )
        .route(
            "/shop/currency-rates/{id}",
            get(shop::currency_rates::edit_form).post(shop::currency_rates::update),
        )
        .route(
            "/shop/settings",
            get(shop::settings::form).post(shop::settings::save),
        )
        .route("/discounts", get(discounts::list).post(discounts::create))
        .route("/discounts/new", get(discounts::new_form))
        .route("/discounts/names", get(discounts::names_lookup))
        .route(
            "/discounts/cumulative",
            get(discounts::cumulative_form).post(discounts::cumulative_create),
        )
        .route(
            "/discounts/{id}",
            get(discounts::edit_form).post(discounts::update),
        )
        .route("/discounts/{id}/active", post(discounts::set_active))
        .route("/discounts/{id}/delete", post(discounts::delete))
        .route(
            "/discounts/{id}/coupons",
            get(discounts::coupons).post(discounts::add_coupon),
        )
        .route(
            "/discounts/{id}/coupons/generate",
            post(discounts::generate_coupons),
        )
        .route(
            "/discounts/{id}/coupons/{coupon}/active",
            post(discounts::set_coupon_active),
        )
        .route(
            "/discounts/{id}/coupons/{coupon}/delete",
            post(discounts::delete_coupon),
        )
        .route("/exchange", get(exchange::form).post(exchange::save))
        .route("/exchange/sale", post(exchange::save_sale))
        .route(
            "/exchange/map/{person_type_id}",
            get(exchange::map_form).post(exchange::save_map_form),
        )
        .route("/users", get(users::list).post(users::create))
        .route("/users/new", get(users::new_form))
        .route("/users/{id}", get(users::edit_form).post(users::update))
        .route("/users/{id}/delete", post(users::delete))
        .route("/groups", get(groups::list).post(groups::create))
        .route("/groups/new", get(groups::new_form))
        .route("/groups/{id}", get(groups::edit_form).post(groups::update))
        .route("/groups/{id}/delete", post(groups::delete))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_access,
        ));

    let static_dir = state.config.static_dir.join("admin");
    Router::new()
        .route(
            "/login",
            get(auth_pages::login_form).post(auth_pages::login),
        )
        .merge(protected)
        .nest_service("/static", ServeDir::new(static_dir))
        .layer(middleware::from_fn(same_origin_guard))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
}

/// Пускает дальше пользователей с правом входа в админку (см. [`Access`]),
/// остальных — на страницу входа. Права пересчитываются на каждый запрос,
/// поэтому изменения групп действуют сразу.
async fn require_access(
    State(state): State<AppState>,
    jar: CookieJar,
    mut req: Request,
    next: Next,
) -> Response {
    let access = match auth::current_user(&state.db, &jar).await {
        Ok(Some(user)) => Access::load(&state.db, user).await,
        Ok(None) => return Redirect::to("/admin/login").into_response(),
        Err(err) => Err(err),
    };
    match access {
        Ok(mut access) if access.can_enter_admin() => {
            // Для вкладки «Документы» в меню «Магазин»
            access.store_control = match state.db.acquire().await {
                Ok(mut conn) => crate::stock::settings(&mut conn)
                    .await
                    .is_ok_and(|s| s.store_control),
                Err(_) => false,
            };
            req.extensions_mut().insert(access);
            next.run(req).await
        }
        Ok(_) => Redirect::to("/admin/login").into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

/// Защита от CSRF: изменяющие запросы принимаются только с того же origin.
/// Вместе с SameSite=Lax у cookie сессии этого достаточно для админки.
async fn same_origin_guard(req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let source = headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER))
        .and_then(|v| v.to_str().ok())
        .and_then(url_authority);
    match (host, source) {
        (Some(host), Some(source)) if host.eq_ignore_ascii_case(source) => next.run(req).await,
        _ => (StatusCode::FORBIDDEN, "Запрос отклонён: неверный Origin").into_response(),
    }
}

fn url_authority(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    rest.split(['/', '?', '#']).next()
}

/// Поля multipart-формы: текстовые значения (с повторами, в порядке прихода) и
/// загруженные файлы — уже сохранённые, с именем поля, из которого пришли.
pub(crate) struct UploadForm {
    pub fields: Vec<(String, String)>,
    pub uploads: Vec<(String, FileRecord)>,
    /// Отклонённые файлы — показываются пользователю как ошибки формы.
    pub rejected: Vec<String>,
}

/// Читает multipart-форму и сразу сохраняет файлы в хранилище `subdir`.
/// Пустые файловые поля (ничего не выбрано) пропускаются.
pub(crate) async fn read_upload_form(
    state: &AppState,
    mut multipart: Multipart,
    subdir: &str,
) -> AppResult<UploadForm> {
    let mut form = UploadForm {
        fields: Vec::new(),
        uploads: Vec::new(),
        rejected: Vec::new(),
    };
    let bad = |e: MultipartError| AppError::BadRequest(e.body_text());
    while let Some(field) = multipart.next_field().await.map_err(bad)? {
        let name = field.name().unwrap_or_default().to_string();
        match field.file_name().map(str::to_string) {
            None => form.fields.push((name, field.text().await.map_err(bad)?)),
            Some(file_name) => {
                let content_type = field.content_type().unwrap_or_default().to_string();
                let data = field.bytes().await.map_err(bad)?;
                if file_name.is_empty() || data.is_empty() {
                    continue;
                }
                let saved = files::save(
                    &state.db,
                    &state.config.upload_dir,
                    subdir,
                    &file_name,
                    &content_type,
                    &data,
                )
                .await;
                match saved {
                    Ok(file) => form.uploads.push((name, file)),
                    Err(files::SaveError::Rejected(msg)) => form.rejected.push(msg),
                    Err(files::SaveError::Failed(e)) => return Err(e.into()),
                }
            }
        }
    }
    Ok(form)
}

pub(crate) fn render(state: &AppState, name: &str, ctx: Value) -> AppResult<Html<String>> {
    let tmpl = state.templates.get_template(name)?;
    Ok(Html(tmpl.render(ctx)?))
}

pub(crate) fn parse_sort(raw: &str) -> i32 {
    raw.trim().parse().unwrap_or(500)
}

async fn dashboard(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    let (collections, elements, users): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM collections),
                (SELECT count(*) FROM collection_items),
                (SELECT count(*) FROM users)",
    )
    .fetch_one(&state.db)
    .await?;
    render(
        &state,
        "dashboard.html",
        context! { user, stats => context! { collections, elements, users } },
    )
}

#[cfg(test)]
mod tests {
    use super::url_authority;

    #[test]
    fn authority() {
        assert_eq!(
            url_authority("http://localhost:3000/admin/x"),
            Some("localhost:3000")
        );
        assert_eq!(url_authority("https://example.com"), Some("example.com"));
        assert_eq!(url_authority("null"), None);
    }
}
