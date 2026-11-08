use crate::test_support::{admin_cookie, get_raw, permitted_cookie, post_form_raw, test_state};
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

async fn fixture(db: &PgPool) -> (i64, i64, i64, i64) {
    let c = sqlx::query_scalar(
        "INSERT INTO collections(code,name,api_enabled) VALUES ('seo','Каталог',TRUE) RETURNING id",
    )
    .fetch_one(db)
    .await
    .unwrap();
    let a = sqlx::query_scalar("INSERT INTO collection_sections(collection_id,code,name) VALUES ($1,'root','Корень') RETURNING id").bind(c).fetch_one(db).await.unwrap();
    let b = sqlx::query_scalar("INSERT INTO collection_sections(collection_id,parent_id,code,name) VALUES ($1,$2,'child','Раздел') RETURNING id").bind(c).bind(a).fetch_one(db).await.unwrap();
    let i = sqlx::query_scalar("INSERT INTO collection_items(collection_id,section_id,code,name) VALUES ($1,$2,'item','Товар <b>') RETURNING id").bind(c).bind(b).fetch_one(db).await.unwrap();
    (c, a, b, i)
}
async fn save(app: &axum::Router, cookie: &str, kind: &str, id: i64, data: &[(&str, &str)]) {
    let body = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(data.iter().copied())
        .finish();
    let (status, _, body) =
        post_form_raw(app, &format!("/admin/seo/{kind}/{id}"), cookie, &body).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
}
async fn meta(app: &axum::Router, kind: &str, id: i64) -> Value {
    let (status, _, body) = get_raw(app, &format!("/api/v1/seo/{kind}/{id}"), "").await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice(&body).unwrap()
}
#[sqlx::test]
async fn seo_inheritance_edit_clear_move_delete(db: PgPool) {
    let (c, a, b, i) = fixture(&db).await;
    let cookie = admin_cookie(&db).await;
    let app = crate::app(test_state(db.clone()));
    save(
        &app,
        &cookie,
        "collections",
        c,
        &[
            ("title", "{name} — {collection}"),
            ("description", "Описание {code}"),
            ("h1", "{name}"),
        ],
    )
    .await;
    save(
        &app,
        &cookie,
        "sections",
        a,
        &[("title", "Купить {name}"), ("description", "В {section}")],
    )
    .await;
    save(&app, &cookie, "sections", b, &[("h1", "Выбор: {name}")]).await;
    assert_eq!(
        meta(&app, "items", i).await,
        json!({"title":"Купить Товар <b>","description":"В Раздел","h1":"Выбор: Товар <b>","canonical":"","robots":"index,follow"})
    );
    assert_eq!(meta(&app, "sections", b).await["title"], "Купить Раздел");
    save(&app, &cookie, "items", i, &[("title", "Особый {name}")]).await;
    assert_eq!(meta(&app, "items", i).await["title"], "Особый Товар <b>");
    let (_, _, html) = get_raw(&app, &format!("/admin/seo/items/{i}"), &cookie).await;
    let html = String::from_utf8(html).unwrap();
    assert!(html.contains("Особый Товар &lt;b&gt;"), "{html}");
    assert!(!html.contains("Товар <b>"));
    save(&app, &cookie, "items", i, &[]).await;
    assert_eq!(meta(&app, "items", i).await["title"], "Купить Товар <b>");
    sqlx::query("UPDATE collection_items SET section_id=NULL WHERE id=$1")
        .bind(i)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(meta(&app, "items", i).await["title"], "Товар <b> — Каталог");
    sqlx::query("DELETE FROM collections WHERE id=$1")
        .bind(c)
        .execute(&db)
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM seo_settings")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[sqlx::test]
async fn seo_visibility_and_write_permissions(db: PgPool) {
    let (c, a, _, i) = fixture(&db).await;
    let app = crate::app(test_state(db.clone()));
    let limited = permitted_cookie(&db, "seo_reader", &[]).await;
    for kind_id in [format!("collections/{c}"), format!("items/{i}")] {
        let path = format!("/admin/seo/{kind_id}");
        assert_eq!(get_raw(&app, &path, &limited).await.0, 403);
        assert_eq!(
            post_form_raw(&app, &path, &limited, "title=secret").await.0,
            403
        );
    }
    let path = format!("/api/v1/seo/items/{i}");
    for sql in [
        format!("UPDATE collection_items SET active=FALSE WHERE id={i}"),
        format!(
            "UPDATE collection_items SET active=TRUE,published_at=now()+interval '1 day' WHERE id={i}"
        ),
        format!(
            "UPDATE collection_items SET published_at=NULL WHERE id={i}; UPDATE collection_sections SET active=FALSE WHERE id={a}"
        ),
        format!(
            "UPDATE collection_sections SET active=TRUE WHERE id={a}; UPDATE collections SET api_enabled=FALSE WHERE id={c}"
        ),
    ] {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(get_raw(&app, &path, "").await.0, 404);
    }
    assert_eq!(get_raw(&app, "/api/v1/seo/unknown/1", "").await.0, 404);
    assert_eq!(get_raw(&app, "/api/v1/seo/items/9999999", "").await.0, 404);
}
#[sqlx::test]
async fn seo_invalid_template_preserves_data(db: PgPool) {
    let (_, _, _, i) = fixture(&db).await;
    let cookie = admin_cookie(&db).await;
    let app = crate::app(test_state(db.clone()));
    save(&app, &cookie, "items", i, &[("title", "Верный")]).await;
    for invalid in [
        "{unknown}",
        "{name",
        "{name}}",
        "{{name}}",
        &"я".repeat(2001),
    ] {
        let body = form_urlencoded::Serializer::new(String::new())
            .append_pair("title", invalid)
            .finish();
        let (status, _, html) =
            post_form_raw(&app, &format!("/admin/seo/items/{i}"), &cookie, &body).await;
        // HTMX boost swaps 200 responses; 422 would hide the form error.
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(html.contains("role=\"alert\""));
        assert_eq!(meta(&app, "items", i).await["title"], "Верный");
    }
    sqlx::query("UPDATE collection_items SET name='{collection}' WHERE id=$1")
        .bind(i)
        .execute(&db)
        .await
        .unwrap();
    save(&app, &cookie, "items", i, &[("title", "{name}")]).await;
    assert_eq!(meta(&app, "items", i).await["title"], "{collection}");
}

#[sqlx::test]
async fn seo_canonical_and_robots(db: PgPool) {
    let (c, a, _, i) = fixture(&db).await;
    let cookie = admin_cookie(&db).await;
    let app = crate::app(test_state(db.clone()));
    save(
        &app,
        &cookie,
        "collections",
        c,
        &[("robots", "noindex,follow"), ("canonical", "/catalog/")],
    )
    .await;
    assert_eq!(meta(&app, "items", i).await["robots"], "noindex,follow");
    assert_eq!(meta(&app, "items", i).await["canonical"], "");
    save(&app, &cookie, "sections", a, &[("robots", "index,follow")]).await;
    save(
        &app,
        &cookie,
        "items",
        i,
        &[("canonical", "https://example.org/product/")],
    )
    .await;
    assert_eq!(meta(&app, "items", i).await["robots"], "index,follow");
    assert_eq!(
        meta(&app, "items", i).await["canonical"],
        "https://example.org/product/"
    );
    for invalid in [
        "javascript:alert(1)",
        "//evil.org/",
        "https://user:pass@example.org/",
        "/a\nb",
    ] {
        let body = form_urlencoded::Serializer::new(String::new())
            .append_pair("canonical", invalid)
            .finish();
        let (_, _, html) =
            post_form_raw(&app, &format!("/admin/seo/items/{i}"), &cookie, &body).await;
        assert!(html.contains("role=\"alert\""));
        assert_eq!(
            meta(&app, "items", i).await["canonical"],
            "https://example.org/product/"
        );
    }
    save(&app, &cookie, "items", i, &[]).await;
    assert_eq!(meta(&app, "items", i).await["canonical"], "");
}
