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

/// Миграция 0014 только переименовывает: данные, связи и последовательности целы.
#[sqlx::test(migrations = false)]
async fn migration_0014_keeps_rows(db: PgPool) {
    let full = sqlx::migrate!();
    let mut before = sqlx::migrate!();
    before.migrations = full
        .migrations
        .iter()
        .filter(|m| m.version <= 13)
        .cloned()
        .collect::<Vec<_>>()
        .into();
    before.run(&db).await.unwrap();
    for sql in [
        "INSERT INTO iblocks (id, code, name, is_catalog) VALUES (1, 'cat', 'Каталог', TRUE)",
        "INSERT INTO iblock_sections (id, iblock_id, name) VALUES (1, 1, 'Раздел')",
        "INSERT INTO iblock_properties (id, iblock_id, code, name, kind, link_iblock_id) VALUES (1, 1, 'color', 'Цвет', 'list', 1)",
        "INSERT INTO iblock_property_enums (id, property_id, value, xml_id) VALUES (1, 1, 'Красный', 'red')",
        "INSERT INTO iblock_elements (id, iblock_id, section_id, code, name, properties) VALUES (1, 1, 1, 't', 'Товар', '{\"color\": 1}')",
        "INSERT INTO catalog_price_types (id, code, name, is_base) VALUES (1, 'BASE', 'Базовая', TRUE)",
        "INSERT INTO catalog_prices (element_id, price_type_id, price, currency) VALUES (1, 1, 100, 'RUB')",
        "INSERT INTO catalog_stores (id, name) VALUES (1, 'Склад')",
        "INSERT INTO catalog_store_amounts (element_id, store_id, amount) VALUES (1, 1, 5)",
        "INSERT INTO catalog_products (element_id, quantity) VALUES (1, 5)",
        "INSERT INTO buyers (id, token_hash) VALUES (1, 'g')",
        "INSERT INTO cart_items (buyer_id, element_id, quantity) VALUES (1, 1, 1)",
        "INSERT INTO groups (id, code, name) VALUES (50, 'ed', 'Редакторы')",
        "INSERT INTO iblock_group_access (iblock_id, group_id, level) VALUES (1, 50, 'write')",
        "INSERT INTO group_permissions (group_id, permission) VALUES (50, 'iblocks.manage')",
    ] {
        sqlx::query(sql).execute(&db).await.unwrap();
    }
    full.run(&db).await.unwrap();
    for (table, key) in [
        ("collections", "id"),
        ("collection_sections", "collection_id"),
        ("collection_fields", "link_collection_id"),
        ("collection_field_options", "field_id"),
        ("collection_items", "collection_id"),
        ("catalog_prices", "item_id"),
        ("catalog_store_amounts", "item_id"),
        ("catalog_products", "item_id"),
        ("cart_items", "item_id"),
        ("collection_access", "collection_id"),
    ] {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE {key} = 1"
        )))
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(n, 1, "{table}");
    }
    let values: Value =
        sqlx::query_scalar("SELECT field_values FROM collection_items WHERE id = 1")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(values, json!({"color": 1}));
    let perms: Vec<String> =
        sqlx::query_scalar("SELECT permission FROM group_permissions WHERE group_id = 50")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(perms, ["collections.manage"]);
    sqlx::query("SELECT setval('collections_id_seq', 1)")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO collections (code, name) VALUES ('new', 'Новая')")
        .execute(&db)
        .await
        .unwrap();
}

#[sqlx::test]
async fn item_delete_cleans_cart(db: PgPool) {
    let f = crate::test_support::order_fixture(&db).await;
    sqlx::query("DELETE FROM collection_items WHERE id = $1")
        .bind(f.element_id)
        .execute(&db)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cart_items WHERE item_id = $1 AND order_id IS NULL",
    )
    .bind(f.element_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test]
async fn collections_manage_permission(db: PgPool) {
    content_fixture(&db).await;
    let (user_id, cookie) = crate::test_support::user_cookie(&db, "manager", false).await;
    let group: i64 = sqlx::query_scalar(
        "INSERT INTO groups (code, name) VALUES ('mgr', 'Менеджеры') RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO group_permissions (group_id, permission)
         VALUES ($1, 'admin.access'), ($1, 'collections.manage')",
    )
    .bind(group)
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO user_groups (user_id, group_id) VALUES ($1, $2)")
        .bind(user_id)
        .bind(group)
        .execute(&db)
        .await
        .unwrap();
    let app = app(&db);
    page_has(&app, &cookie, "/admin/iblocks", &["Новости"]).await;
    page_has(&app, &cookie, "/admin", &["href=\"/admin/iblocks\""]).await;
}

/// Фильтр и сортировка по полям собираются в SQL на лету — проверяем их через API.
#[sqlx::test]
async fn field_filters_and_order(db: PgPool) {
    let c = content_fixture(&db).await;
    let app = app(&db);
    let ids = |body: &Value| -> Vec<i64> {
        body["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_i64().unwrap())
            .collect()
    };
    let list = "/api/v1/iblock/news/element/list";
    let (_, hit) = post_json(
        &app,
        list,
        json!({"select": ["id"], "filter": {"color": c.option_id}}),
    )
    .await;
    assert_eq!(ids(&hit), [c.item_id]);
    let (_, miss) = post_json(
        &app,
        list,
        json!({"select": ["id"], "filter": {"color.item.xmlId": "blue"}}),
    )
    .await;
    assert_eq!(ids(&miss), Vec::<i64>::new());
    let (_, linked) = post_json(
        &app,
        list,
        json!({"select": ["id"], "filter": {"link.element.name": "Первая новость"}}),
    )
    .await;
    assert_eq!(ids(&linked), [c.item_id]);
    let (_, ordered) = post_json(
        &app,
        list,
        json!({"select": ["id"], "order": {"color": "asc"}}),
    )
    .await;
    assert_eq!(ids(&ordered), [c.item_id]);
    let own = get_json(
        &app,
        &format!("/api/v1/iblocks/news/elements?prop.color={}", c.option_id),
    )
    .await;
    assert_eq!(own["pagination"]["total"], 1);
    let own_miss = get_json(&app, "/api/v1/iblocks/news/elements?prop.color=999").await;
    assert_eq!(own_miss["pagination"]["total"], 0);
}
