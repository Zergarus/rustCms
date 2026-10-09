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

fn multipart_request(path: &str, cookie: &str, fields: &[(&str, &str)]) -> Request<Body> {
    let boundary = "smokeboundary";
    let mut body = String::new();
    for (name, value) in fields {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    Request::post(path)
        .header(header::COOKIE, cookie)
        .header(header::HOST, "127.0.0.1:3000")
        .header(header::ORIGIN, ORIGIN)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

/// POST multipart-формы админки (формы записей и разделов).
pub async fn post_multipart(
    app: &Router,
    path: &str,
    cookie: &str,
    fields: &[(&str, &str)],
) -> (StatusCode, String) {
    send(app, multipart_request(path, cookie, fields)).await
}

/// То же, но вместо тела ответа — заголовок `Location` редиректа.
async fn post_multipart_location(
    app: &Router,
    path: &str,
    cookie: &str,
    fields: &[(&str, &str)],
) -> (StatusCode, String) {
    let res = app
        .clone()
        .oneshot(multipart_request(path, cookie, fields))
        .await
        .unwrap();
    let location = res
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    (res.status(), location)
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
    assert!(
        !html.contains("нфоблок"),
        "{path}: осталось слово «инфоблок»"
    );
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
    page_has(&app, &cookie, "/admin/collections", &["Новости"]).await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/collections/{col}"),
        &["Цвет", "Связь"],
    )
    .await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/fields/{field}"),
        &["Красный"],
    )
    .await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/collections/{col}/sections/new"),
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
        &format!("/admin/collections/{col}/items?section={sec}"),
        &["Первая новость", "Раздел А"],
    )
    .await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/collections/{col}/items/new"),
        &["Красный"],
    )
    .await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/items/{item}"),
        &["Первая новость", "Красный"],
    )
    .await;
    page_has(
        &app,
        &cookie,
        &format!("/admin/groups/{group}"),
        &[
            "Новости",
            &format!("name=\"collection_{col}\" value=\"write\" checked"),
        ],
    )
    .await;
    page_has(&app, &cookie, "/admin", &["Коллекции"]).await;
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
        .bind(f.product_id)
        .execute(&db)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cart_items WHERE item_id = $1 AND order_id IS NULL",
    )
    .bind(f.product_id)
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
    page_has(&app, &cookie, "/admin/collections", &["Новости"]).await;
    page_has(&app, &cookie, "/admin", &["href=\"/admin/collections\""]).await;
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

/// Поле-привязка: в настройках коллекции видна цель привязки, новая привязка сохраняется.
#[sqlx::test]
async fn field_link_roundtrip(db: PgPool) {
    let c = content_fixture(&db).await;
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let col = c.collection_id;
    page_has(
        &app,
        &cookie,
        &format!("/admin/collections/{col}"),
        &["→ Новости"],
    )
    .await;
    let (status, _) = post_form(
        &app,
        &format!("/admin/collections/{col}/fields"),
        &cookie,
        &format!("code=rel&name=Ещё&kind=element&sort=500&link_collection_id={col}"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let link: Option<i64> =
        sqlx::query_scalar("SELECT link_collection_id FROM collection_fields WHERE code = 'rel'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(link, Some(col));
    let _ = c.link_field_id;
}

#[sqlx::test]
async fn old_admin_urls_gone(db: PgPool) {
    let c = content_fixture(&db).await;
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    for path in [
        "/admin/iblocks".to_string(),
        format!("/admin/elements/{}", c.item_id),
        format!("/admin/properties/{}", c.field_id),
    ] {
        assert_eq!(
            get(&app, &path, &cookie).await.0,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[sqlx::test]
async fn group_collection_access_roundtrip(db: PgPool) {
    let c = content_fixture(&db).await;
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (col, group) = (c.collection_id, c.group_id);
    let (status, _) = post_form(
        &app,
        &format!("/admin/groups/{group}"),
        &cookie,
        &format!(
            "code=editors&name=Редакторы&sort=500&permission=admin.access&collection_{col}=read"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    page_has(
        &app,
        &cookie,
        &format!("/admin/groups/{group}"),
        &[&format!("name=\"collection_{col}\" value=\"read\" checked")],
    )
    .await;
    let (user_id, user_cookie) = crate::test_support::user_cookie(&db, "editor", false).await;
    sqlx::query("INSERT INTO user_groups (user_id, group_id) VALUES ($1, $2)")
        .bind(user_id)
        .bind(group)
        .execute(&db)
        .await
        .unwrap();
    page_has(
        &app,
        &user_cookie,
        &format!("/admin/collections/{col}/items"),
        &[],
    )
    .await;
    let (status, _) = post_multipart(
        &app,
        &format!("/admin/collections/{col}/items"),
        &user_cookie,
        &[("name", "Чужая"), ("sort", "500")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Карточка корзины ведёт на запись, подсказка формы коллекции — на наш API.
#[sqlx::test]
async fn shop_cart_links_and_api_hint(db: PgPool) {
    let f = crate::test_support::order_fixture(&db).await;
    let c = content_fixture(&db).await;
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (status, html) = get(&app, &format!("/admin/shop/carts/{}", f.buyer_id), &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains(&format!("/admin/items/{}", f.product_id)),
        "{html}"
    );
    assert!(!html.contains("/admin/elements/"));
    page_has(
        &app,
        &cookie,
        &format!("/admin/collections/{}", c.collection_id),
        &["/api/v1/iblocks/код"],
    )
    .await;
}

/// Тексты ошибок bxapi — часть контракта, переименование их не меняет.
#[sqlx::test]
async fn bxapi_error_messages_unchanged(db: PgPool) {
    let c = content_fixture(&db).await;
    sqlx::query("INSERT INTO collection_fields (collection_id, code, name, kind) VALUES ($1, 'free', 'Без привязки', 'element')")
        .bind(c.collection_id)
        .execute(&db)
        .await
        .unwrap();
    let app = app(&db);
    let (_, body) = post_json(&app, "/api/v1/iblock/nope/element/list", json!({})).await;
    assert_eq!(
        body["errors"][0]["message"],
        "Iblock with API_CODE=\"nope\" not found"
    );
    let (_, body) = post_json(
        &app,
        "/api/v1/iblock/news/element/list",
        json!({"filter": {"free.element.name": "x"}}),
    )
    .await;
    assert_eq!(
        body["errors"][0]["message"],
        "Property free has no linked iblock"
    );
}

/// Пустой список коллекций и текст права «Работа с заказами».
#[sqlx::test]
async fn empty_collections_and_order_permission_text(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    page_has(
        &app(&db),
        &cookie,
        "/admin/collections",
        &["Создать первую"],
    )
    .await;
    let orders = crate::access::PERMISSIONS
        .iter()
        .find(|p| p.code == crate::access::ORDERS_MANAGE)
        .unwrap();
    assert!(orders.description.contains("правка свойств и состава"));
}

/// Обломки автозамены словаря не должны попадать в интерфейс.
#[test]
fn no_broken_russian_in_ui() {
    let broken = [
        "коллекциих",
        "этого коллекции",
        "этом коллекции",
        "полейа",
        "полейо",
        "записьу",
    ];
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for dir in [
        "templates/admin",
        "templates/admin/shop",
        "src/admin",
        "src/admin/shop",
        "src/collection",
    ] {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                files.push(path);
            }
        }
    }
    files.push("src/access.rs".into());
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        for b in broken {
            assert!(!text.contains(b), "{}: «{b}»", path.display());
        }
    }
}

/// Каталог с товаром и предложением через `create_offer_collection`.
async fn sku_fixture(db: &PgPool) -> (i64, i64) {
    let products: i64 = sqlx::query_scalar(
        "INSERT INTO collections (code, name, is_catalog) VALUES ('catalog', 'Каталог', TRUE) RETURNING id",
    )
    .fetch_one(db)
    .await
    .unwrap();
    let product: i64 = sqlx::query_scalar(
        "INSERT INTO collection_items (collection_id, code, name) VALUES ($1, 'tovar', 'Товар') RETURNING id",
    )
    .bind(products)
    .fetch_one(db)
    .await
    .unwrap();
    let parent = crate::collection::repo::get_collection(db, products)
        .await
        .unwrap()
        .unwrap();
    let offers = crate::collection::repo::create_offer_collection(db, &parent)
        .await
        .unwrap();
    let offer: i64 = sqlx::query_scalar(
        "INSERT INTO collection_items (collection_id, code, name, product_id)
         VALUES ($1, 'offer', 'Предложение', $2) RETURNING id",
    )
    .bind(offers.id)
    .bind(product)
    .fetch_one(db)
    .await
    .unwrap();
    (product, offer)
}

#[sqlx::test]
async fn bxapi_filters_and_selects_cml2link(db: PgPool) {
    let (product, offer) = sku_fixture(&db).await;
    let app = app(&db);
    let (status, body) = post_json(
        &app,
        "/api/v1/iblock/catalog_offers/element/list",
        json!({"select": ["id", "cml2Link.element.name"],
               "filter": {"cml2Link.element.id": [product]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"status": "success", "data": {"items": [{
            "id": offer, "cml2Link": "Товар", "stocks": [], "name": "Предложение",
            "code": "offer", "active": true, "sort": 500, "dateCreate": body["data"]["items"][0]["dateCreate"],
            "previewText": "", "detailText": "", "iblockSectionId": 0,
        }]}, "errors": []})
    );
    let (_, none) = post_json(
        &app,
        "/api/v1/iblock/catalog_offers/element/list",
        json!({"select": ["id"], "filter": {"cml2Link.element.id": [product + 1000]}}),
    )
    .await;
    assert_eq!(none["data"]["items"], json!([]));
}

#[sqlx::test]
async fn own_api_shows_product_link(db: PgPool) {
    let (product, offer) = sku_fixture(&db).await;
    let app = app(&db);
    let offers = get_json(&app, "/api/v1/iblocks/catalog_offers/elements").await;
    let first = &offers["items"][0];
    assert_eq!(first["id"], json!(offer));
    assert_eq!(first["product_id"], json!(product));
    assert_eq!(first["catalog"], json!({"type": 4}));
    assert_eq!(first["properties"]["cml2_link"], json!(product));
    let products = get_json(&app, "/api/v1/iblocks/catalog/elements").await;
    let first = &products["items"][0];
    assert!(first.get("product_id").is_none());
    assert_eq!(first["catalog"], json!({"type": 3}));
}

/// Товар `news` (каталог) и подключённая через админку коллекция предложений.
async fn sku_setup(db: &PgPool, app: &Router, cookie: &str) -> (crate::test_support::Content, i64) {
    let c = content_fixture(db).await;
    sqlx::query("UPDATE collections SET is_catalog = TRUE WHERE id = $1")
        .bind(c.collection_id)
        .execute(db)
        .await
        .unwrap();
    let (status, _) = post_form(
        app,
        &format!("/admin/collections/{}/offers", c.collection_id),
        cookie,
        "mode=create",
    )
    .await;
    assert!(status.is_redirection() || status.is_success(), "{status}");
    let offers: i64 = sqlx::query_scalar("SELECT id FROM collections WHERE code = 'news_offers'")
        .fetch_one(db)
        .await
        .unwrap();
    (c, offers)
}

async fn create_offer(
    app: &Router,
    cookie: &str,
    offers: i64,
    product: i64,
    name: &str,
) -> (StatusCode, String) {
    post_multipart_location(
        app,
        &format!("/admin/collections/{offers}/items"),
        cookie,
        &[
            ("name", name),
            ("sort", "500"),
            ("active", "on"),
            ("prop_cml2_link", &product.to_string()),
            ("weight", "250"),
        ],
    )
    .await
}

#[sqlx::test]
async fn offers_tab_and_offer_form(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (c, offers) = sku_setup(&db, &app, &cookie).await;
    let link: (Option<i64>, i64) = sqlx::query_as(
        "SELECT sku_field_id, (SELECT count(*) FROM collection_fields WHERE collection_id = c.id AND code = 'cml2_link')
         FROM collections c WHERE c.id = $1",
    )
    .bind(offers)
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(link.0.is_some());
    assert_eq!(link.1, 1);

    let new_path = format!(
        "/admin/collections/{offers}/items/new?product={}",
        c.item_id
    );
    page_has(&app, &cookie, &new_path, &["Первая новость", "Вес"]).await;

    let (status, location) = create_offer(&app, &cookie, offers, c.item_id, "Вариант 1").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location, format!("/admin/items/{}#offers", c.item_id));

    page_has(
        &app,
        &cookie,
        &format!("/admin/items/{}", c.item_id),
        &[
            "Торговые предложения",
            "Вариант 1",
            "Цены и остатки задаются у предложений",
        ],
    )
    .await;
    let (weight, kind): (f64, i16) = sqlx::query_as(
        "SELECT c.weight::float8, c.type FROM catalog_products c
         JOIN collection_items i ON i.id = c.item_id WHERE i.name = 'Вариант 1'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!((weight, kind), (250.0, 4));
}

#[sqlx::test]
async fn offer_requires_product(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (c, offers) = sku_setup(&db, &app, &cookie).await;
    let path = format!("/admin/collections/{offers}/items");
    let error = "Укажите товар из коллекции Новости";
    let (status, html) = post_multipart(
        &app,
        &path,
        &cookie,
        &[("name", "Без товара"), ("sort", "1")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(error), "{html}");
    // запись другой коллекции (сама коллекция предложений) тоже не подходит
    let foreign: i64 = sqlx::query_scalar(
        "INSERT INTO collection_items (collection_id, code, name) VALUES ($1, 'x', 'x') RETURNING id",
    )
    .bind(offers)
    .fetch_one(&db)
    .await
    .unwrap();
    let (status, html) = post_multipart(
        &app,
        &path,
        &cookie,
        &[
            ("name", "Чужой"),
            ("sort", "1"),
            ("prop_cml2_link", &foreign.to_string()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(error), "{html}");
    // не обязательное поле связи тоже не отключает проверку
    sqlx::query("UPDATE collection_fields SET is_required = FALSE WHERE code = 'cml2_link'")
        .execute(&db)
        .await
        .unwrap();
    let (_, html) = post_multipart(
        &app,
        &path,
        &cookie,
        &[("name", "Без товара"), ("sort", "1")],
    )
    .await;
    assert!(html.contains(error), "{html}");
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM collection_items WHERE collection_id = $1 AND id <> $2",
    )
    .bind(offers)
    .bind(foreign)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(count, 0);
    let _ = c;
}

#[sqlx::test]
async fn unlink_refused_with_offers(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (c, offers) = sku_setup(&db, &app, &cookie).await;
    create_offer(&app, &cookie, offers, c.item_id, "Вариант 1").await;
    let (status, html) = post_form(
        &app,
        &format!("/admin/collections/{}/offers", c.collection_id),
        &cookie,
        "mode=none",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Сначала удалите предложения (1)"), "{html}");
    let linked: Option<i64> =
        sqlx::query_scalar("SELECT product_collection_id FROM collections WHERE id = $1")
            .bind(offers)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(linked, Some(c.collection_id));

    let field: i64 = sqlx::query_scalar("SELECT sku_field_id FROM collections WHERE id = $1")
        .bind(offers)
        .fetch_one(&db)
        .await
        .unwrap();
    let (status, html) =
        post_form(&app, &format!("/admin/fields/{field}/delete"), &cookie, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Системное поле связи с товаром"), "{html}");
    // смена привязки тоже запрещена
    let (_, html) = post_form(
        &app,
        &format!("/admin/fields/{field}"),
        &cookie,
        &format!(
            "name=Товар&sort=100&is_required=on&link_collection_id={}",
            offers
        ),
    )
    .await;
    assert!(html.contains("Системное поле связи с товаром"), "{html}");
}

#[sqlx::test]
async fn relink_refused_with_foreign_offers(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (c, offers) = sku_setup(&db, &app, &cookie).await;
    create_offer(&app, &cookie, offers, c.item_id, "Вариант 1").await;
    // другая коллекция товаров хочет забрать коллекцию с чужими предложениями
    let other: i64 = sqlx::query_scalar(
        "INSERT INTO collections (code, name, is_catalog) VALUES ('shoes', 'Обувь', TRUE) RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    let (status, html) = post_form(
        &app,
        &format!("/admin/collections/{other}/offers"),
        &cookie,
        &format!("mode=existing&offers_id={offers}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("другой коллекции товаров"), "{html}");
    let linked: Option<i64> =
        sqlx::query_scalar("SELECT product_collection_id FROM collections WHERE id = $1")
            .bind(offers)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(linked, Some(c.collection_id));
}

#[sqlx::test]
async fn existing_collection_becomes_offers(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let c = content_fixture(&db).await;
    sqlx::query("UPDATE collections SET is_catalog = TRUE WHERE id = $1")
        .bind(c.collection_id)
        .execute(&db)
        .await
        .unwrap();
    let spare: i64 = sqlx::query_scalar(
        "INSERT INTO collections (code, name, is_catalog) VALUES ('spare', 'Запас', TRUE) RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    let (status, _) = post_form(
        &app,
        &format!("/admin/collections/{}/offers", c.collection_id),
        &cookie,
        &format!("mode=existing&offers_id={spare}"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let row: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT product_collection_id, sku_field_id FROM collections WHERE id = $1")
            .bind(spare)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(row.0, Some(c.collection_id));
    assert!(row.1.is_some());
    let (status, _) = post_form(
        &app,
        &format!("/admin/collections/{}/offers", c.collection_id),
        &cookie,
        "mode=none",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let row: Option<i64> =
        sqlx::query_scalar("SELECT product_collection_id FROM collections WHERE id = $1")
            .bind(spare)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(row, None);
}

#[sqlx::test]
async fn field_flags_saved(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let c = content_fixture(&db).await;
    let (status, _) = post_form(
        &app,
        &format!("/admin/collections/{}/fields", c.collection_id),
        &cookie,
        "name=Размер&code=razmer&kind=list&sort=10&in_basket=on&offer_tree=on",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let flags: (bool, bool) =
        sqlx::query_as("SELECT in_basket, offer_tree FROM collection_fields WHERE code = 'razmer'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(flags, (true, true));
    // изменение на странице поля
    let id: i64 = sqlx::query_scalar("SELECT id FROM collection_fields WHERE code = 'razmer'")
        .fetch_one(&db)
        .await
        .unwrap();
    post_form(
        &app,
        &format!("/admin/fields/{id}"),
        &cookie,
        "name=Размер&sort=10",
    )
    .await;
    let flags: (bool, bool) =
        sqlx::query_as("SELECT in_basket, offer_tree FROM collection_fields WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(flags, (false, false));
    page_has(
        &app,
        &cookie,
        &format!("/admin/fields/{id}"),
        &["Показывать в корзине", "Поле выбора предложения"],
    )
    .await;
    // у текстового вида — ошибка формы, поле не создаётся
    let (status, html) = post_form(
        &app,
        &format!("/admin/collections/{}/fields", c.collection_id),
        &cookie,
        "name=Заметка&code=note&kind=text&sort=10&offer_tree=on",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("Поле выбора предложения — только список, привязка или строка"),
        "{html}"
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM collection_fields WHERE code = 'note'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test]
async fn admin_save_keeps_weight_and_offer_stock(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (c, offers) = sku_setup(&db, &app, &cookie).await;
    let path = format!("/admin/items/{}", c.item_id);
    // простой товар: вес сохраняется, показывается в форме и не обнуляется повторным сохранением
    let (status, _) = post_multipart(
        &app,
        &path,
        &cookie,
        &[
            ("name", "Первая новость"),
            ("sort", "500"),
            ("weight", "1200"),
            ("available", "on"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let weight = |id: i64| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, f64>(
                "SELECT weight::float8 FROM catalog_products WHERE item_id = $1",
            )
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    assert_eq!(weight(c.item_id).await, 1200.0);
    let (_, html) = get(&app, &path, &cookie).await;
    assert!(html.contains("value=\"1200"), "{html}");

    // у товара с предложениями цены и доступность принадлежат предложениям
    sqlx::query("UPDATE catalog_products SET quantity = 7, available = TRUE WHERE item_id = $1")
        .bind(c.item_id)
        .execute(&db)
        .await
        .unwrap();
    create_offer(&app, &cookie, offers, c.item_id, "Вариант 1").await;
    sqlx::query(
        "UPDATE catalog_products SET available = TRUE, quantity = 5, quantity_trace = TRUE
         WHERE item_id = (SELECT id FROM collection_items WHERE name = 'Вариант 1')",
    )
    .execute(&db)
    .await
    .unwrap();
    let before: (bool, f64) = sqlx::query_as(
        "SELECT available, quantity::float8 FROM catalog_products WHERE item_id = $1",
    )
    .bind(c.item_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(before.0);
    let (status, _) = post_multipart(
        &app,
        &path,
        &cookie,
        &[("name", "Первая новость"), ("sort", "500")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let after: (bool, f64) = sqlx::query_as(
        "SELECT available, quantity::float8 FROM catalog_products WHERE item_id = $1",
    )
    .bind(c.item_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(after, before);
    assert_eq!(weight(c.item_id).await, 1200.0);
}

#[sqlx::test]
async fn offers_block_only_for_catalog(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let c = content_fixture(&db).await;
    let path = format!("/admin/collections/{}", c.collection_id);
    let (_, html) = get(&app, &path, &cookie).await;
    assert!(!html.contains("Торговые предложения"), "{html}");
    let (status, html) = post_form(&app, &format!("{path}/offers"), &cookie, "mode=create").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("торгового каталога"), "{html}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM collections WHERE code = 'news_offers'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(n, 0);
    sqlx::query("UPDATE collections SET is_catalog = TRUE WHERE id = $1")
        .bind(c.collection_id)
        .execute(&db)
        .await
        .unwrap();
    page_has(&app, &cookie, &path, &["Торговые предложения"]).await;
}

#[sqlx::test]
async fn offer_needs_write_on_both_collections(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (c, offers) = sku_setup(&db, &app, &cookie).await;
    let (user_id, user_cookie) = crate::test_support::user_cookie(&db, "editor", false).await;
    sqlx::query("INSERT INTO user_groups (user_id, group_id) VALUES ($1, $2)")
        .bind(user_id)
        .bind(c.group_id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO group_permissions (group_id, permission) VALUES ($1, 'admin.access')")
        .bind(c.group_id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO collection_access (collection_id, group_id, level) VALUES ($1, $2, 'write')",
    )
    .bind(offers)
    .bind(c.group_id)
    .execute(&db)
    .await
    .unwrap();
    // право записи есть на предложения и на товары (fixture) — создание проходит
    let (status, loc) = create_offer(&app, &user_cookie, offers, c.item_id, "Вариант 1").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(loc, format!("/admin/items/{}#offers", c.item_id));
    // права на товары нет — 403 на форму, создание и удаление
    sqlx::query("DELETE FROM collection_access WHERE collection_id = $1")
        .bind(c.collection_id)
        .execute(&db)
        .await
        .unwrap();
    let new_path = format!(
        "/admin/collections/{offers}/items/new?product={}",
        c.item_id
    );
    assert_eq!(
        get(&app, &new_path, &user_cookie).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, loc) = create_offer(&app, &user_cookie, offers, c.item_id, "Вариант 2").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{loc}");
    let offer: i64 = sqlx::query_scalar("SELECT id FROM collection_items WHERE name = 'Вариант 1'")
        .fetch_one(&db)
        .await
        .unwrap();
    let (status, _) = post_form(
        &app,
        &format!("/admin/items/{offer}/delete"),
        &user_cookie,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn existing_mode_refuses_linked_or_non_catalog(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (c, offers) = sku_setup(&db, &app, &cookie).await;
    let other: i64 = sqlx::query_scalar(
        "INSERT INTO collections (code, name, is_catalog) VALUES ('shoes', 'Обувь', TRUE) RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    // уже предложения другой коллекции товаров (пустые)
    let (status, html) = post_form(
        &app,
        &format!("/admin/collections/{other}/offers"),
        &cookie,
        &format!("mode=existing&offers_id={offers}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("уже подключена"), "{html}");
    let linked: Option<i64> =
        sqlx::query_scalar("SELECT product_collection_id FROM collections WHERE id = $1")
            .bind(offers)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(linked, Some(c.collection_id));
    // не каталог
    let plain: i64 = sqlx::query_scalar(
        "INSERT INTO collections (code, name) VALUES ('plain', 'Простая') RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    let (status, html) = post_form(
        &app,
        &format!("/admin/collections/{other}/offers"),
        &cookie,
        &format!("mode=existing&offers_id={plain}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("торговым каталогом"), "{html}");
    let linked: Option<i64> =
        sqlx::query_scalar("SELECT product_collection_id FROM collections WHERE id = $1")
            .bind(plain)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(linked, None);
}

/// POST формы админки, но вместо тела — заголовок `Location` редиректа.
async fn post_form_location(
    app: &Router,
    path: &str,
    cookie: &str,
    body: &str,
) -> (StatusCode, String) {
    let req = Request::post(path)
        .header(header::COOKIE, cookie)
        .header(header::HOST, "127.0.0.1:3000")
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let location = res
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    (res.status(), location)
}

/// Предложения `news_offers` с полями выбора: `volume` (строка) и `color` (список из двух
/// вариантов); базовая цена и склад 1. Возвращает (id поля `color`, id вариантов).
async fn generator_setup(db: &PgPool, offers: i64) -> (i64, Vec<i64>) {
    for sql in [
        "INSERT INTO catalog_price_types (id, code, name, is_base) VALUES (1, 'BASE', 'Базовая', TRUE)",
        "INSERT INTO catalog_stores (id, name) VALUES (1, 'Склад')",
    ] {
        sqlx::query(sql).execute(db).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO collection_fields (collection_id, code, name, kind, offer_tree)
         VALUES ($1, 'volume', 'Объём', 'string', TRUE)",
    )
    .bind(offers)
    .execute(db)
    .await
    .unwrap();
    let color: i64 = sqlx::query_scalar(
        "INSERT INTO collection_fields (collection_id, code, name, kind, offer_tree)
         VALUES ($1, 'color', 'Цвет', 'list', TRUE) RETURNING id",
    )
    .bind(offers)
    .fetch_one(db)
    .await
    .unwrap();
    let mut options = Vec::new();
    for value in ["Красный", "Синий"] {
        options.push(
            sqlx::query_scalar(
                "INSERT INTO collection_field_options (field_id, value, xml_id) VALUES ($1, $2, $2) RETURNING id",
            )
            .bind(color)
            .bind(value)
            .fetch_one(db)
            .await
            .unwrap(),
        );
    }
    (color, options)
}

#[sqlx::test]
async fn generator_creates_offers(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (c, offers) = sku_setup(&db, &app, &cookie).await;
    generator_setup(&db, offers).await;
    let path = format!("/admin/items/{}/offers/generate", c.item_id);
    page_has(
        &app,
        &cookie,
        &path,
        &["Объём", "Цвет", "Красный", "Базовая"],
    )
    .await;

    let (status, location) = post_form_location(
        &app,
        &path,
        &cookie,
        "axis_volume=1+%D0%BB%2C+4+%D0%BB&template=%23PRODUCT_NAME%23+%28%23VOLUME%23%29&active=on&price_1=500&amount_1=3&weight=900",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location,
        format!("/admin/items/{}?generated=2&skipped=0#offers", c.item_id)
    );
    page_has(
        &app,
        &cookie,
        &format!("/admin/items/{}?generated=2&skipped=0", c.item_id),
        &["Создано 2, пропущено 0 (уже есть)", "Первая новость (4 л)"],
    )
    .await;

    let rows: Vec<(i64, String, String, f64, f64, f64, i16)> = sqlx::query_as(
        "SELECT i.id, i.name, i.xml_id, pr.price::float8, a.amount::float8, p.weight::float8, p.type
         FROM collection_items i
         JOIN catalog_prices pr ON pr.item_id = i.id AND pr.price_type_id = 1
         JOIN catalog_store_amounts a ON a.item_id = i.id AND a.store_id = 1
         JOIN catalog_products p ON p.item_id = i.id
         WHERE i.collection_id = $1 AND i.product_id = $2 ORDER BY i.id",
    )
    .bind(offers)
    .bind(c.item_id)
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, "Первая новость (1 л)");
    assert_eq!(rows[1].1, "Первая новость (4 л)");
    for r in &rows {
        assert_eq!((r.3, r.4, r.5, r.6), (500.0, 3.0, 900.0, 4));
        assert_eq!(r.2.len(), 36, "xml_id не UUID: {}", r.2);
        assert_eq!(r.2.matches('-').count(), 4);
    }
    let volumes: Vec<String> = sqlx::query_scalar(
        "SELECT field_values->>'volume' FROM collection_items
         WHERE collection_id = $1 ORDER BY id",
    )
    .bind(offers)
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(volumes, ["1 л", "4 л"]);
    // связь с товаром читается как значение CML2_LINK
    let link = crate::collection::repo::get_item(&db, rows[0].0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link.field_values["cml2_link"], json!(c.item_id));
    let kind: i16 = sqlx::query_scalar("SELECT type FROM catalog_products WHERE item_id = $1")
        .bind(c.item_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(kind, 3);
}

#[sqlx::test]
async fn generator_skips_existing_any_order(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (c, offers) = sku_setup(&db, &app, &cookie).await;
    let (_, options) = generator_setup(&db, offers).await;
    let path = format!("/admin/items/{}/offers/generate", c.item_id);
    let colors = format!("axis_color={}&axis_color={}", options[0], options[1]);

    let (status, location) = post_form_location(
        &app,
        &path,
        &cookie,
        &format!("{colors}&axis_volume=1+%D0%BB%2C+4+%D0%BB&price_1=10&active=on"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location.contains("generated=4&skipped=0"), "{location}");

    // те же значения, оси в другом порядке (поля формы переставлены)
    let (status, location) = post_form_location(
        &app,
        &path,
        &cookie,
        &format!("axis_volume=4+%D0%BB%2C1+%D0%BB&{colors}&price_1=10&active=on"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location,
        format!("/admin/items/{}?generated=0&skipped=4#offers", c.item_id)
    );
    page_has(
        &app,
        &cookie,
        &format!("/admin/items/{}?generated=0&skipped=4", c.item_id),
        &["Создано 0, пропущено 4 (уже есть)"],
    )
    .await;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM collection_items WHERE product_id = $1")
            .bind(c.item_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(count, 4);

    // новое значение добавляет только недостающие
    let (_, location) = post_form_location(
        &app,
        &path,
        &cookie,
        &format!("{colors}&axis_volume=1+%D0%BB%2C+10+%D0%BB&price_1=10"),
    )
    .await;
    assert!(location.contains("generated=2&skipped=2"), "{location}");
}

#[sqlx::test]
async fn generator_rejects_bad_input_and_writes_nothing(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (c, offers) = sku_setup(&db, &app, &cookie).await;
    generator_setup(&db, offers).await;
    let path = format!("/admin/items/{}/offers/generate", c.item_id);
    let count = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM collection_items WHERE product_id = $1")
            .bind(c.item_id)
            .fetch_one(&db)
            .await
            .unwrap()
    };

    let (status, html) = post_form(&app, &path, &cookie, "template=x").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("Выберите значения хотя бы одного поля"),
        "{html}"
    );

    let many: Vec<String> = (0..101).map(|i| format!("v{i}")).collect();
    let (status, html) = post_form(
        &app,
        &path,
        &cookie,
        &format!("axis_volume={}", many.join("%2C")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("Сочетаний 101, больше 100 за раз нельзя"),
        "{html}"
    );

    let (status, html) = post_form(&app, &path, &cookie, "axis_volume=1+%D0%BB&price_1=abc").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Цена: ожидается число"), "{html}");
    assert_eq!(count().await, 0);
}

#[sqlx::test]
async fn generator_needs_write_on_both_collections(db: PgPool) {
    let cookie = admin_cookie(&db).await;
    let app = app(&db);
    let (c, offers) = sku_setup(&db, &app, &cookie).await;
    generator_setup(&db, offers).await;
    let (user_id, user_cookie) = crate::test_support::user_cookie(&db, "editor", false).await;
    sqlx::query("INSERT INTO user_groups (user_id, group_id) VALUES ($1, $2)")
        .bind(user_id)
        .bind(c.group_id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO group_permissions (group_id, permission) VALUES ($1, 'admin.access')")
        .bind(c.group_id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM collection_access WHERE collection_id = $1")
        .bind(c.collection_id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO collection_access (collection_id, group_id, level) VALUES ($1, $2, 'write')",
    )
    .bind(offers)
    .bind(c.group_id)
    .execute(&db)
    .await
    .unwrap();
    let path = format!("/admin/items/{}/offers/generate", c.item_id);
    assert_eq!(
        get(&app, &path, &user_cookie).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, _) = post_form(&app, &path, &user_cookie, "axis_volume=1").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM collection_items WHERE product_id = $1")
            .bind(c.item_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

/// Товар `catalog` в разделе и два предложения: 500 (склад 1: 2) и 300 (склад 1: 1, склад 2: 4).
/// Поле предложений `volume` (строка): «4 л» и «1 л».
async fn price_from_fixture(db: &PgPool) -> (i64, i64, i64, i64) {
    let (product, cheap) = sku_fixture(db).await;
    let offers: i64 =
        sqlx::query_scalar("SELECT collection_id FROM collection_items WHERE id = $1")
            .bind(cheap)
            .fetch_one(db)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO collection_fields (collection_id, code, name, kind) VALUES ($1, 'volume', 'Объём', 'string')",
    )
    .bind(offers)
    .execute(db)
    .await
    .unwrap();
    let dear: i64 = sqlx::query_scalar(
        "INSERT INTO collection_items (collection_id, code, name, product_id, field_values)
         VALUES ($1, 'offer2', 'Дорогое', $2, '{\"volume\": \"1 л\"}') RETURNING id",
    )
    .bind(offers)
    .bind(product)
    .fetch_one(db)
    .await
    .unwrap();
    sqlx::query("UPDATE collection_items SET field_values = '{\"volume\": \"4 л\"}' WHERE id = $1")
        .bind(cheap)
        .execute(db)
        .await
        .unwrap();
    let base: i64 = sqlx::query_scalar(
        "INSERT INTO catalog_price_types (code, name, is_base) VALUES ('BASE', 'Базовая', TRUE) RETURNING id",
    )
    .fetch_one(db)
    .await
    .unwrap();
    for (id, price) in [(dear, 500), (cheap, 300)] {
        sqlx::query(
            "INSERT INTO catalog_prices (item_id, price_type_id, price, currency) VALUES ($1, $2, $3, 'RUB')",
        )
        .bind(id)
        .bind(base)
        .bind(price)
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO catalog_products (item_id, quantity) VALUES ($1, 5)
             ON CONFLICT (item_id) DO UPDATE SET quantity = 5",
        )
        .bind(id)
        .execute(db)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO catalog_stores (id, name) VALUES (1, 'Склад 1'), (2, 'Склад 2')")
        .execute(db)
        .await
        .unwrap();
    for (id, store, amount) in [(dear, 1, 2), (cheap, 1, 1), (cheap, 2, 4)] {
        sqlx::query(
            "INSERT INTO catalog_store_amounts (item_id, store_id, amount) VALUES ($1, $2, $3)",
        )
        .bind(id)
        .bind(store)
        .bind(amount)
        .execute(db)
        .await
        .unwrap();
    }
    (product, cheap, dear, offers)
}

async fn bx_list(app: &Router, code: &str, body: Value) -> Value {
    let (status, body) = post_json(app, &format!("/api/v1/iblock/{code}/element/list"), body).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[sqlx::test]
async fn price_from_and_stocks(db: PgPool) {
    let (product, _, _, _) = price_from_fixture(&db).await;
    let app = app(&db);
    let body = bx_list(
        &app,
        "catalog",
        json!({"select": ["id", "catalogPrice", "stocks", "catalogType"]}),
    )
    .await;
    let item = &body["data"]["items"][0];
    assert_eq!(item["id"], json!(product));
    assert_eq!(item["catalogType"], json!(3));
    assert_eq!(item["catalogPrice"]["value"], json!(300));
    assert_eq!(item["catalogPrice"]["fromOffers"], json!(true));
    assert_eq!(
        item["stocks"],
        json!([
            {"storeName": "Склад 1", "amount": 3},
            {"storeName": "Склад 2", "amount": 4},
        ])
    );
    // Предложение: тип 4, цена своя, без fromOffers
    let body = bx_list(
        &app,
        "catalog_offers",
        json!({"select": ["id", "catalogPrice", "catalogType"], "order": {"id": "asc"}}),
    )
    .await;
    let first = &body["data"]["items"][0];
    assert_eq!(first["catalogType"], json!(4));
    assert!(first["catalogPrice"].get("fromOffers").is_none());
}

#[sqlx::test]
async fn price_from_falls_back_to_active(db: PgPool) {
    let (_, cheap, dear, _) = price_from_fixture(&db).await;
    let app = app(&db);
    sqlx::query("UPDATE catalog_products SET available = FALSE WHERE item_id = ANY($1)")
        .bind(vec![cheap, dear])
        .execute(&db)
        .await
        .unwrap();
    let body = bx_list(
        &app,
        "catalog",
        json!({"select": ["catalogPrice", "catalogType"]}),
    )
    .await;
    assert_eq!(
        body["data"]["items"][0]["catalogPrice"]["value"],
        json!(300)
    );
    sqlx::query("UPDATE collection_items SET active = FALSE WHERE id = ANY($1)")
        .bind(vec![cheap, dear])
        .execute(&db)
        .await
        .unwrap();
    let body = bx_list(
        &app,
        "catalog",
        json!({"select": ["catalogPrice", "stocks", "catalogType"]}),
    )
    .await;
    let item = &body["data"]["items"][0];
    assert_eq!(item["catalogPrice"], json!([]));
    assert_eq!(item["stocks"], json!([]));
    assert_eq!(item["catalogType"], json!(3));
}

#[sqlx::test]
async fn catalog_type_null_outside_catalog(db: PgPool) {
    content_fixture(&db).await;
    let app = app(&db);
    let body = bx_list(&app, "news", json!({"select": ["id", "catalogType"]})).await;
    assert_eq!(body["data"]["items"][0]["catalogType"], json!(null));
}

#[sqlx::test]
async fn catalog_type_defaults_to_simple(db: PgPool) {
    let c = content_fixture(&db).await;
    sqlx::query("UPDATE collections SET is_catalog = TRUE WHERE id = $1")
        .bind(c.collection_id)
        .execute(&db)
        .await
        .unwrap();
    let app = app(&db);
    let body = bx_list(&app, "news", json!({"select": ["id", "catalogType"]})).await;
    assert_eq!(body["data"]["items"][0]["catalogType"], json!(1));
}

#[sqlx::test]
async fn filter_by_offers(db: PgPool) {
    let (a, _, dear, offers) = price_from_fixture(&db).await;
    let section: i64 = sqlx::query_scalar(
        "INSERT INTO collection_sections (collection_id, name, code)
         SELECT collection_id, 'Раздел А', 'a' FROM collection_items WHERE id = $1 RETURNING id",
    )
    .bind(a)
    .fetch_one(&db)
    .await
    .unwrap();
    sqlx::query("UPDATE collection_items SET section_id = $2 WHERE id = $1")
        .bind(a)
        .bind(section)
        .execute(&db)
        .await
        .unwrap();
    // Товар B: одно предложение «1 л»
    let products: i64 =
        sqlx::query_scalar("SELECT collection_id FROM collection_items WHERE id = $1")
            .bind(a)
            .fetch_one(&db)
            .await
            .unwrap();
    let b: i64 = sqlx::query_scalar(
        "INSERT INTO collection_items (collection_id, code, name) VALUES ($1, 'b', 'Б') RETURNING id",
    )
    .bind(products)
    .fetch_one(&db)
    .await
    .unwrap();
    sqlx::query("UPDATE collection_items SET product_id = $2 WHERE id = $1")
        .bind(dear)
        .bind(b)
        .execute(&db)
        .await
        .unwrap();
    let _ = offers;
    let app = app(&db);
    let ids = |v: &Value| -> Vec<i64> {
        let mut ids: Vec<i64> = v["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_i64().unwrap())
            .collect();
        ids.sort();
        ids
    };
    let body = bx_list(
        &app,
        "catalog",
        json!({"select": ["id"], "filter": {"offers": {"volume": "4 л"}}}),
    )
    .await;
    assert_eq!(ids(&body), vec![a]);
    let body = bx_list(
        &app,
        "catalog",
        json!({"select": ["id"], "filter": {"logic": "OR", "0": {"offers": {"volume": "1 л"}}, "1": {"id": a}}}),
    )
    .await;
    let mut want = vec![a, b];
    want.sort();
    assert_eq!(ids(&body), want);
    let (status, body) = post_json(
        &app,
        "/api/v1/iblock/catalog/section/list",
        json!({"select": ["id", "name"], "hasElements": {"filter": {"offers": {"volume": "4 л"}}}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 1);
    let (status, body) = post_json(
        &app,
        "/api/v1/iblock/catalog/section/list",
        json!({"select": ["id", "name"], "hasElements": {"filter": {"offers": {"volume": "1 л"}}}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["items"], json!([]));
    // offers внутри offers и на коллекции без предложений
    for (code, filter) in [
        ("catalog", json!({"offers": {"offers": {}}})),
        ("catalog_offers", json!({"offers": {}})),
    ] {
        let (status, body) = post_json(
            &app,
            &format!("/api/v1/iblock/{code}/element/list"),
            json!({"select": ["id"], "filter": filter}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("error"), "{body}");
        assert!(body.to_string().contains("Unknown filter field"), "{body}");
    }
}

#[sqlx::test]
async fn offers_filter_rejects_operator_prefix(db: PgPool) {
    price_from_fixture(&db).await;
    let app = app(&db);
    for key in ["!offers", "=offers", "@offers", "%offers"] {
        let (status, body) = post_json(
            &app,
            "/api/v1/iblock/catalog/element/list",
            json!({"select": ["id"], "filter": {key: {"volume": "4 л"}}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("error"), "{key}: {body}");
        assert!(
            body.to_string()
                .contains(&format!("Unknown filter field: {key}")),
            "{key}: {body}"
        );
    }
}
