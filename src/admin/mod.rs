//! Админ-панель: серверный рендер (MiniJinja) + HTMX.

mod auth_pages;
mod elements;
mod groups;
mod iblocks;
mod sections;
mod shop;
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
        .route("/iblocks", get(iblocks::list).post(iblocks::create))
        .route("/iblocks/new", get(iblocks::new_form))
        .route(
            "/iblocks/{id}",
            get(iblocks::edit_form).post(iblocks::update),
        )
        .route("/iblocks/{id}/delete", post(iblocks::delete))
        .route("/iblocks/{id}/properties", post(iblocks::add_property))
        .route(
            "/properties/{id}",
            get(iblocks::property_form).post(iblocks::update_property),
        )
        .route("/properties/{id}/enums", post(iblocks::save_enums))
        .route("/properties/{id}/delete", post(iblocks::delete_property))
        .route("/iblocks/{id}/sections", post(sections::create))
        .route("/iblocks/{id}/sections/new", get(sections::new_form))
        .route(
            "/sections/{id}",
            get(sections::edit_form).post(sections::update),
        )
        .route("/sections/{id}/delete", post(sections::delete))
        .route(
            "/iblocks/{id}/elements",
            get(elements::list).post(elements::create),
        )
        .route("/iblocks/{id}/elements/new", get(elements::new_form))
        .route(
            "/elements/{id}",
            get(elements::edit_form).post(elements::update),
        )
        .route("/elements/{id}/delete", post(elements::delete))
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
        .route("/shop/orders", get(shop::orders::list))
        .route("/shop/orders/{id}", get(shop::orders::view))
        .route("/shop/orders/{id}/status", post(shop::orders::set_status))
        .route("/shop/orders/{id}/paid", post(shop::orders::set_paid))
        .route("/shop/orders/{id}/cancel", post(shop::orders::set_canceled))
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
            "/shop/currencies",
            get(shop::currencies::list).post(shop::currencies::create),
        )
        .route("/shop/currencies/new", get(shop::currencies::new_form))
        .route(
            "/shop/currencies/{code}",
            get(shop::currencies::edit_form).post(shop::currencies::update),
        )
        .route(
            "/shop/settings",
            get(shop::settings::form).post(shop::settings::save),
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
        Ok(access) if access.can_enter_admin() => {
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
    let (iblocks, elements, users): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM collections),
                (SELECT count(*) FROM collection_items),
                (SELECT count(*) FROM users)",
    )
    .fetch_one(&state.db)
    .await?;
    render(
        &state,
        "dashboard.html",
        context! { user, stats => context! { iblocks, elements, users } },
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
