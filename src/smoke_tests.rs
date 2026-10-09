//! Страницы админки, наш API и bxapi через роутер приложения: закрепляют поведение,
//! которое не должно меняться при переделках (в том числе шаблоны — MiniJinja
//! молча выводит пусто на месте неизвестного поля).

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::test_support::{admin_cookie, content_fixture, test_state};

const ORIGIN: &str = "http://127.0.0.1:3000";

fn app(db: &PgPool) -> Router {
    crate::app(test_state(db.clone()))
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

pub async fn get(app: &Router, path: &str, cookie: &str) -> (StatusCode, String) {
    let req = Request::get(path)
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    send(app, req).await
}

/// POST формы админки (`application/x-www-form-urlencoded`) с правильным Origin.
pub async fn post_form(app: &Router, path: &str, cookie: &str, body: &str) -> (StatusCode, String) {
    let req = Request::post(path)
        .header(header::COOKIE, cookie)
        .header(header::HOST, "127.0.0.1:3000")
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(app, req).await
}

pub async fn post_json(app: &Router, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, text) = send(app, req).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn get_json(app: &Router, path: &str) -> Value {
    let (status, text) = get(app, path, "").await;
    assert_eq!(status, StatusCode::OK, "{path}: {text}");
    serde_json::from_str(&text).unwrap()
}

/// Страница отдаётся (200) и содержит каждую из строк.
async fn page_has(app: &Router, cookie: &str, path: &str, needles: &[&str]) {
    let (status, html) = get(app, path, cookie).await;
    assert_eq!(status, StatusCode::OK, "{path}");
    for n in needles {
        assert!(html.contains(n), "{path}: нет «{n}»");
    }
}

#[sqlx::test]
async fn admin_pages_render(db: PgPool) {
    let c = content_fixture(&db).await;
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (col, sec, field, item, group) = (
        c.collection_id,
        c.section_id,
        c.field_id,
        c.item_id,
        c.group_id,
    );
    page_has(&app, &cookie, "/admin/iblocks", &["Новости"]).await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/iblocks/{col}"),
        &["Цвет", "Связь"],
    )
    .await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/properties/{field}"),
        &["Красный"],
    )
    .await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/iblocks/{col}/sections/new"),
        &["Раздел А"],
    )
    .await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/sections/{sec}"),
        &["Раздел А"],
    )
    .await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/iblocks/{col}/elements?section={sec}"),
        &["Первая новость", "Раздел А"],
    )
    .await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/iblocks/{col}/elements/new"),
        &["Красный"],
    )
    .await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/elements/{item}"),
        &["Первая новость", "Красный"],
    )
    .await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/groups/{group}"),
        &[
            "Новости",
            &format!("name=\"iblock_{col}\" value=\"write\" checked"),
        ],
    )
    .await;
    page_has(&app, &cookie, "/admin", &["Инфоблоки"]).await;
}

#[sqlx::test]
async fn own_api_unchanged(db: PgPool) {
    let c = content_fixture(&db).await;
    let app = app(&db);
    let id = c.item_id;
    let element = json!({
        "id": id, "code": "pervaya", "name": "Первая новость", "sort": 500,
        "preview_text": "", "detail_text": "", "published_at": "2026-01-02T03:04:05Z",
        "properties": {"link": id, "color": c.option_id},
        "created_at": "2026-01-02T03:04:05Z", "updated_at": "2026-01-02T03:04:05Z",
    });
    assert_eq!(
        get_json(&app, "/api/v1/iblocks").await,
        json!({"items": [{"code": "news", "name": "Новости", "description": ""}]})
    );
    assert_eq!(
        get_json(&app, "/api/v1/iblocks/news").await,
        json!({"code": "news", "name": "Новости", "description": "", "properties": [
            {"code": "color", "name": "Цвет", "kind": "list", "required": false},
            {"code": "link", "name": "Связь", "kind": "element", "required": false},
        ]})
    );
    assert_eq!(
        get_json(&app, "/api/v1/iblocks/news/elements").await,
        json!({"items": [element.clone()], "pagination": {"page": 1, "per_page": 20, "total": 1, "pages": 1}})
    );
    assert_eq!(
        get_json(&app, "/api/v1/iblocks/news/elements/pervaya").await,
        element
    );
}

#[sqlx::test]
async fn bxapi_list_unchanged(db: PgPool) {
    let c = content_fixture(&db).await;
    let app = app(&db);
    let (status, body) = post_json(
        &app,
        "/api/v1/iblock/news/element/list",
        json!({"select": ["id", "name", "color", "color.item.value", "link.element.name"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"status": "success", "data": {"items": [{
            "id": c.item_id, "name": "Первая новость", "color": "Красный", "link": "Первая новость",
            "code": "pervaya", "active": true, "sort": 500, "dateCreate": "02.01.2026 06:04:05",
            "previewText": "", "detailText": "", "iblockSectionId": c.section_id,
        }]}, "errors": []})
    );
}
