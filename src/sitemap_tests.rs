use crate::sitemap::{Ctx, normalize, render};
use crate::test_support::{get_raw, test_state};
use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;

/// Коллекция с шаблонами адресов. `detail` — шаблон записи, `section` — раздела.
async fn collection(db: &PgPool, code: &str, detail: &str, section: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO collections(code,name,api_enabled,detail_page_url,section_page_url)
         VALUES ($1,$1,TRUE,$2,$3) RETURNING id",
    )
    .bind(code)
    .bind(detail)
    .bind(section)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn item(
    db: &PgPool,
    collection_id: i64,
    code: &str,
    active: bool,
    published: Option<&str>,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO collection_items(collection_id,code,name,active,published_at)
         VALUES ($1,$2,$3,$4,$5::timestamptz) RETURNING id",
    )
    .bind(collection_id)
    .bind(code)
    .bind(format!("Запись {code}"))
    .bind(active)
    .bind(published)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn section(db: &PgPool, collection_id: i64, parent: Option<i64>, code: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO collection_sections(collection_id,parent_id,code,name)
         VALUES ($1,$2,$3,$3) RETURNING id",
    )
    .bind(collection_id)
    .bind(parent)
    .bind(code)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn sitemap(
    db: &PgPool,
    name: &str,
    template: &str,
    include: &str,
    sort: i32,
    active: bool,
    collections: &[i64],
) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO seo_sitemap(name,path_template,include,sort,active)
         VALUES ($1,$2,$3,$4,$5) RETURNING id",
    )
    .bind(name)
    .bind(template)
    .bind(include)
    .bind(sort)
    .bind(active)
    .fetch_one(db)
    .await
    .unwrap();
    for collection_id in collections {
        sqlx::query("INSERT INTO seo_sitemap_collection(sitemap_id,collection_id) VALUES ($1,$2)")
            .bind(id)
            .bind(collection_id)
            .execute(db)
            .await
            .unwrap();
    }
    id
}

/// ТрансОпт-подобная конфигурация: товары в двух коллекциях, новости и схемы из
/// шаблона коллекции, разделы каталога, пустой / выключенный тип.
async fn fixture(db: &PgPool) -> PgPool {
    let catalog = collection(
        db,
        "catalog",
        "/catalog/#SECTION_CODE_PATH#/#ELEMENT_ID#/",
        "/catalog/#SECTION_CODE_PATH#/",
    )
    .await;
    let second = collection(db, "catalog2", "/catalog2/#ELEMENT_CODE#/", "/catalog2/").await;
    let info = collection(db, "info", "/news/#ELEMENT_CODE#/", "/news/").await;
    let schemes = collection(db, "schemes", "/schemes/#ELEMENT_CODE#/", "/schemes/").await;

    item(db, catalog, "catalog1", true, None).await;
    item(db, catalog, "CATALOG2", true, None).await;
    item(db, catalog, "", true, None).await;
    item(db, catalog, "catalog4", false, None).await;
    item(db, catalog, "catalog5", true, Some("2999-01-01")).await;
    item(db, second, "second1", true, None).await;
    item(db, info, "info1", true, None).await;
    item(db, info, "info2", true, None).await;
    item(db, schemes, "schemes1", true, None).await;

    let root = section(db, catalog, None, "Root").await;
    section(db, catalog, Some(root), "Child").await;

    sitemap(
        db,
        "products",
        "/product/#ELEMENT_CODE_OR_ID#/",
        "items",
        100,
        true,
        &[catalog, second],
    )
    .await;
    sitemap(db, "catalog", "", "sections", 200, true, &[catalog]).await;
    sitemap(db, "news", "", "items", 300, true, &[info]).await;
    sitemap(db, "schemes", "", "items", 400, true, &[schemes]).await;
    sitemap(db, "contacts", "", "items", 500, true, &[]).await;
    sitemap(db, "hidden", "", "items", 600, false, &[info]).await;
    db.clone()
}

fn item_paths(json: &Value) -> Vec<String> {
    json["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap().to_string())
        .collect()
}

#[sqlx::test]
async fn sitemap_index_counts_and_order(db: PgPool) {
    let app = crate::app(test_state(fixture(&db).await));
    let (status, _, body) = get_raw(&app, "/api/v1/seo/sitemap", "").await;

    assert_eq!(status, StatusCode::OK, "{body:?}");
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "success");
    assert_eq!(json["errors"], serde_json::json!([]));

    let items = json["data"]["items"].as_array().unwrap();
    let types: Vec<&str> = items.iter().map(|i| i["type"].as_str().unwrap()).collect();
    // `hidden` выключен и в индекс не попадает, порядок — sort, id
    assert_eq!(
        types,
        ["products", "catalog", "news", "schemes", "contacts"]
    );

    let count = |t: &str| {
        items.iter().find(|i| i["type"] == t).unwrap()["count"]
            .as_i64()
            .unwrap()
    };
    assert_eq!(count("products"), 4, "две коллекции: 3 + 1 активных");
    assert_eq!(count("catalog"), 2);
    assert_eq!(count("news"), 2);
    assert_eq!(count("schemes"), 1);
    assert_eq!(count("contacts"), 0);

    let contacts = items.iter().find(|i| i["type"] == "contacts").unwrap();
    assert!(contacts["lastmod"].is_null());
    let lastmod = items[0]["lastmod"].as_str().unwrap();
    assert!(lastmod.ends_with("+00:00"), "{lastmod}");
}

#[sqlx::test]
async fn sitemap_items_paths_and_types(db: PgPool) {
    let app = crate::app(test_state(fixture(&db).await));

    let (status, _, body) = get_raw(&app, "/api/v1/seo/sitemap/products", "").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["data"]["type"], "products");
    let paths = item_paths(&json);
    assert_eq!(paths.len(), 4, "{paths:?}");
    assert!(paths.contains(&"/product/catalog1/".into()), "{paths:?}");
    // код в нижнем регистре
    assert!(paths.contains(&"/product/catalog2/".into()), "{paths:?}");
    // пустой код — путь по id
    assert!(
        paths.iter().any(|p| p
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap()
            .parse::<i64>()
            .is_ok()),
        "{paths:?}"
    );
    assert!(!paths.iter().any(|p| p.contains("//")), "{paths:?}");

    let (_, _, body) = get_raw(&app, "/api/v1/seo/sitemap/news", "").await;
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(item_paths(&json), ["/news/info1/", "/news/info2/"]);

    let (_, _, body) = get_raw(&app, "/api/v1/seo/sitemap/schemes", "").await;
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(item_paths(&json), ["/schemes/schemes1/"]);

    // include = sections: путь раздела собирается из шаблона коллекции
    let (_, _, body) = get_raw(&app, "/api/v1/seo/sitemap/catalog", "").await;
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        item_paths(&json),
        ["/catalog/root/", "/catalog/root/child/"]
    );

    let (_, _, body) = get_raw(&app, "/api/v1/seo/sitemap/contacts", "").await;
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert!(item_paths(&json).is_empty());

    // выключенный и неизвестный тип — 404 в обычном формате ошибок API
    for t in ["hidden", "nope"] {
        let (status, _, body) = get_raw(&app, &format!("/api/v1/seo/sitemap/{t}"), "").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{t}");
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "not_found", "{t}");
    }
}

#[sqlx::test]
async fn sitemap_without_configuration_is_empty(db: PgPool) {
    let app = crate::app(test_state(db.clone()));
    let (status, _, body) = get_raw(&app, "/api/v1/seo/sitemap", "").await;

    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["data"]["items"], serde_json::json!([]));

    let (status, _, _) = get_raw(&app, "/api/v1/seo/sitemap/products", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn sitemap_render_and_normalize() {
    let ctx = Ctx {
        section_path: "Root/Child",
        section_code: "Child",
        element_id: 42,
        element_code: "",
    };
    assert_eq!(
        render("/product/#ELEMENT_CODE_OR_ID#/", &ctx),
        "/product/42/"
    );
    assert_eq!(render("#SITE_DIR#/x/#ELEMENT_ID#/", &ctx), "/x/42/");

    let ctx = Ctx {
        element_code: "AbC",
        ..ctx
    };
    assert_eq!(
        render("/p/#ELEMENT_CODE#/#ELEMENT_CODE_OR_ID#/", &ctx),
        "/p/abc/abc/"
    );
    assert_eq!(render("/c/#SECTION_CODE_PATH#/", &ctx), "/c/root/child/");

    assert_eq!(normalize("//A//b//"), "/a/b/");
    assert_eq!(normalize("a"), "/a/");
    assert_eq!(normalize("/"), "/");
}

fn form(name: &str, active: bool, collections: &[i64], include: &str, template: &str) -> String {
    let mut pairs = form_urlencoded::Serializer::new(String::new());
    pairs.append_pair("name", name);
    pairs.append_pair("sort", "100");
    pairs.append_pair("include", include);
    pairs.append_pair("path_template", template);
    if active {
        pairs.append_pair("active", "on");
    }
    for collection in collections {
        pairs.append_pair("collection", &collection.to_string());
    }
    pairs.finish()
}

#[sqlx::test]
async fn sitemap_admin_crud(db: PgPool) {
    let catalog = collection(&db, "catalog", "/c/#ELEMENT_CODE#/", "/c/").await;
    item(&db, catalog, "one", true, None).await;
    let app = crate::app(test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;

    let (status, _, body) = get_raw(&app, "/admin/seo/sitemap", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains("Типы не настроены"),
        "пустой список"
    );

    let body = form(
        "products",
        true,
        &[catalog],
        "items",
        "/product/#ELEMENT_CODE_OR_ID#/",
    );
    let (status, _, _) =
        crate::test_support::post_form_raw(&app, "/admin/seo/sitemap", &cookie, &body).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (_, _, body) = get_raw(&app, "/api/v1/seo/sitemap/products", "").await;
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(item_paths(&json), ["/product/one/"]);

    let id: i64 = sqlx::query_scalar("SELECT id FROM seo_sitemap WHERE name = 'products'")
        .fetch_one(&db)
        .await
        .unwrap();

    let (status, _, body) = get_raw(&app, &format!("/admin/seo/sitemap/{id}"), &cookie).await;
    assert_eq!(status, StatusCode::OK, "форма правки: {body:?}");

    // Выключенный тип исчезает из API.
    let body = form("products", false, &[catalog], "items", "");
    let (status, _, _) = crate::test_support::post_form_raw(
        &app,
        &format!("/admin/seo/sitemap/{id}"),
        &cookie,
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, _) = get_raw(&app, "/api/v1/seo/sitemap/products", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Невалидное имя — страница с ошибкой, данные не меняются.
    let (status, _, body) = crate::test_support::post_form_raw(
        &app,
        &format!("/admin/seo/sitemap/{id}"),
        &cookie,
        "name=Bad_Name&include=items",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Имя типа"));
    let name: String = sqlx::query_scalar("SELECT name FROM seo_sitemap WHERE id = $1")
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(name, "products");

    let (status, _, _) = crate::test_support::post_form_raw(
        &app,
        &format!("/admin/seo/sitemap/{id}/delete"),
        &cookie,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM seo_sitemap")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test]
async fn sitemap_admin_requires_collections_manage(db: PgPool) {
    let app = crate::app(test_state(db.clone()));
    let cookie = crate::test_support::permitted_cookie(&db, "reader", &[]).await;
    let (status, _, _) = get_raw(&app, "/admin/seo/sitemap", &cookie).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn sitemap_skips_sources_without_address_template(db: PgPool) {
    // Коллекция без detail_page_url и тип без своего шаблона: адресов нет,
    // поэтому и счётчик должен быть нулевым — иначе витрина сделает пустой файл.
    let plain = collection(&db, "plain", "", "").await;
    item(&db, plain, "one", true, None).await;
    sitemap(&db, "plain", "", "items", 100, true, &[plain]).await;

    let app = crate::app(test_state(db));
    let (_, _, body) = get_raw(&app, "/api/v1/seo/sitemap", "").await;
    let json: Value = serde_json::from_slice(&body).unwrap();
    let item = json["data"]["items"].as_array().unwrap().first().unwrap();
    assert_eq!(item["type"], "plain");
    assert_eq!(item["count"], 0);
    assert!(item["lastmod"].is_null());

    let (_, _, body) = get_raw(&app, "/api/v1/seo/sitemap/plain", "").await;
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert!(item_paths(&json).is_empty());
}
