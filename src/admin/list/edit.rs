//! Сохранение значения одной ячейки списка.

use std::collections::HashMap;

use axum::{Extension, Form, extract::State, http::StatusCode, response::Response};

use super::{
    spec::{EditKind, EditTarget, StdField},
    views,
};
use crate::{
    access::Access,
    admin::flash::FlashKind,
    collection::{FieldOption, fields, is_valid_slug},
    error::{AppError, AppResult, is_unique_violation},
    state::AppState,
};

/// `POST /admin/lists/cell` — сохраняет одну ячейку и перерисовывает `#list-body`.
pub async fn handle(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let raw = |key: &str| form.get(key).map(String::as_str).unwrap_or("");

    let Ok(row) = raw("row").trim().parse::<i64>() else {
        return Ok(views::reject(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Неверная запись",
        ));
    };
    let list_key = raw("list_key");
    let spec = views::spec_for_key(&state.db, &user, list_key).await?;
    let Some(expected) = views::list_path(list_key) else {
        return Ok(views::reject(StatusCode::NOT_FOUND, "Список не найден"));
    };
    if raw("path") != expected {
        return Ok(views::reject(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Неверный адрес списка",
        ));
    }
    let Some(column) = spec.column(raw("column")) else {
        return Ok(views::reject(StatusCode::FORBIDDEN, "Поле недоступно"));
    };
    let Some(edit) = &column.editable else {
        return Ok(views::reject(StatusCode::FORBIDDEN, "Поле недоступно"));
    };
    let Some(collection_id) = list_key
        .strip_prefix("collection:")
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return Ok(views::reject(StatusCode::NOT_FOUND, "Список не найден"));
    };
    let version = raw("version").trim();
    if version.is_empty() {
        return Ok(views::reject(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Обновите список",
        ));
    }

    let result = match edit.target {
        EditTarget::Standard(std_field) => {
            let (sql, value) = match std_field {
                StdField::Name => {
                    let v = raw("value").trim();
                    if v.is_empty() {
                        return Ok(views::reject(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "Укажите название",
                        ));
                    }
                    (
                        concat!(
                            "UPDATE collection_items SET name = $1, updated_at = now() ",
                            "WHERE id = $2 AND collection_id = $3 AND updated_at = $4::timestamptz"
                        ),
                        v.to_string(),
                    )
                }
                StdField::Code => {
                    let v = raw("value").trim();
                    if !v.is_empty() && !is_valid_slug(v) {
                        return Ok(views::reject(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "Код: недопустимые символы",
                        ));
                    }
                    (
                        concat!(
                            "UPDATE collection_items SET code = $1, updated_at = now() ",
                            "WHERE id = $2 AND collection_id = $3 AND updated_at = $4::timestamptz"
                        ),
                        v.to_string(),
                    )
                }
                StdField::Sort => match raw("value").trim().parse::<i32>() {
                    Ok(n) => (
                        concat!(
                            "UPDATE collection_items SET sort = $1::int, updated_at = now() ",
                            "WHERE id = $2 AND collection_id = $3 AND updated_at = $4::timestamptz"
                        ),
                        n.to_string(),
                    ),
                    Err(_) => {
                        return Ok(views::reject(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "Сортировка: ожидается число",
                        ));
                    }
                },
                StdField::Active => {
                    let flag = matches!(
                        raw("value").trim().to_lowercase().as_str(),
                        "1" | "true" | "on"
                    );
                    (
                        concat!(
                            "UPDATE collection_items SET active = $1::bool, updated_at = now() ",
                            "WHERE id = $2 AND collection_id = $3 AND updated_at = $4::timestamptz"
                        ),
                        flag.to_string(),
                    )
                }
            };
            sqlx::query(sql)
                .bind(value)
                .bind(row)
                .bind(collection_id)
                .bind(version)
                .execute(&state.db)
                .await
        }
        EditTarget::Field(_) => {
            let kind = match edit.kind {
                EditKind::Text => "string",
                EditKind::Number => "number",
                EditKind::Bool => "boolean",
                EditKind::Date => "date",
                EditKind::List => "list",
            };
            let options: Vec<FieldOption> = edit
                .options
                .iter()
                .filter_map(|o| {
                    o.value.parse::<i64>().ok().map(|id| FieldOption {
                        id,
                        field_id: 0,
                        value: o.label.clone(),
                        xml_id: String::new(),
                        sort: 0,
                        is_default: false,
                    })
                })
                .collect();
            let value = match fields::parse_single(
                kind,
                edit.required,
                &column.title,
                &options,
                raw("value"),
            ) {
                Ok(v) => v,
                Err(msg) => return Ok(views::reject(StatusCode::UNPROCESSABLE_ENTITY, &msg)),
            };
            sqlx::query(
                "UPDATE collection_items
                 SET field_values = jsonb_set(field_values, ARRAY[$1], $2::jsonb, true),
                     updated_at = now()
                 WHERE id = $3 AND collection_id = $4 AND updated_at = $5::timestamptz",
            )
            .bind(&column.code)
            .bind(value.to_string())
            .bind(row)
            .bind(collection_id)
            .bind(version)
            .execute(&state.db)
            .await
        }
    };

    match result {
        Ok(done) if done.rows_affected() == 1 => {}
        Ok(_) => {
            let exists: Option<i32> = sqlx::query_scalar(
                "SELECT 1 FROM collection_items WHERE id = $1 AND collection_id = $2",
            )
            .bind(row)
            .bind(collection_id)
            .fetch_optional(&state.db)
            .await?;
            return Ok(match exists {
                None => views::reject(StatusCode::NOT_FOUND, "Запись не найдена"),
                Some(_) => {
                    views::reject(StatusCode::CONFLICT, "Запись изменилась, список обновлён")
                }
            });
        }
        Err(e) if is_unique_violation(&e) => {
            return Ok(views::reject(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Код уже занят",
            ));
        }
        Err(e) => {
            tracing::error!(error = ?e, row, "не удалось сохранить ячейку");
            return Err(AppError::Db(e));
        }
    }

    let resp = views::rerender(&state, &user, list_key, &expected, raw("query")).await?;
    Ok(views::with_toast(
        resp,
        StatusCode::OK,
        "Сохранено",
        FlashKind::Ok,
    ))
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{HeaderMap, Request, StatusCode, header},
    };
    use sqlx::PgPool;
    use tower::ServiceExt;

    use crate::test_support::{admin_cookie, content_fixture, permitted_cookie, test_state};

    fn app(db: &PgPool) -> Router {
        crate::app(test_state(db.clone()))
    }

    fn enc(s: &str) -> String {
        form_urlencoded::byte_serialize(s.as_bytes()).collect()
    }

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let res = app.clone().oneshot(req).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    async fn post(app: &Router, cookie: &str, body: &str) -> (StatusCode, HeaderMap, String) {
        let req = Request::post("/admin/lists/cell")
            .header(header::COOKIE, cookie)
            .header(header::HOST, "127.0.0.1:3000")
            .header(header::ORIGIN, "http://127.0.0.1:3000")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("hx-request", "true")
            .header("hx-target", "list-body")
            .body(Body::from(body.to_string()))
            .unwrap();
        send(app, req).await
    }

    async fn get(app: &Router, path: &str, cookie: &str) -> (StatusCode, HeaderMap, String) {
        let req = Request::get(path)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap();
        send(app, req).await
    }

    async fn version(db: &PgPool, id: i64) -> String {
        sqlx::query_scalar("SELECT updated_at::text FROM collection_items WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn field(db: &PgPool, c: i64, code: &str, kind: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO collection_fields (collection_id, code, name, kind) VALUES ($1,$2,$2,$3) RETURNING id",
        )
        .bind(c)
        .bind(code)
        .bind(kind)
        .fetch_one(db)
        .await
        .unwrap()
    }

    fn list_key(cid: i64) -> String {
        format!("collection:{cid}")
    }

    fn list_path(cid: i64) -> String {
        format!("/admin/collections/{cid}/items")
    }

    fn cell_form(
        key: &str,
        path: &str,
        query: &str,
        row: i64,
        column: &str,
        value: &str,
        version: &str,
    ) -> String {
        format!(
            "list_key={}&path={}&query={}&row={row}&column={}&value={}&version={}",
            enc(key),
            enc(path),
            enc(query),
            enc(column),
            enc(value),
            enc(version)
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn save(
        app: &Router,
        db: &PgPool,
        cookie: &str,
        cid: i64,
        id: i64,
        column: &str,
        value: &str,
        query: &str,
    ) -> (StatusCode, HeaderMap, String) {
        let version = version(db, id).await;
        post(
            app,
            cookie,
            &cell_form(
                &list_key(cid),
                &list_path(cid),
                query,
                id,
                column,
                value,
                &version,
            ),
        )
        .await
    }

    async fn field_value(db: &PgPool, id: i64, code: &str) -> Option<String> {
        sqlx::query_scalar("SELECT field_values->>$1 FROM collection_items WHERE id = $2")
            .bind(code)
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn cell_save_standard_and_custom(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let id = c.item_id;

        let (st, _, body) = save(
            &app,
            &db,
            &cookie,
            c.collection_id,
            id,
            "name",
            "Новое имя",
            "",
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(body.contains("Новое имя"), "{body}");
        let name: String = sqlx::query_scalar("SELECT name FROM collection_items WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(name, "Новое имя");

        let (st, _, body) = save(
            &app,
            &db,
            &cookie,
            c.collection_id,
            id,
            "code",
            "new-code",
            "",
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let code: String = sqlx::query_scalar("SELECT code FROM collection_items WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(code, "new-code");

        let (st, _, body) = save(&app, &db, &cookie, c.collection_id, id, "sort", "5", "").await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let sort: i32 = sqlx::query_scalar("SELECT sort FROM collection_items WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(sort, 5);

        let (st, _, body) = save(
            &app,
            &db,
            &cookie,
            c.collection_id,
            id,
            "active",
            "false",
            "",
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let active: bool = sqlx::query_scalar("SELECT active FROM collection_items WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(!active);

        field(&db, c.collection_id, "s", "string").await;
        field(&db, c.collection_id, "n", "number").await;
        field(&db, c.collection_id, "b", "boolean").await;
        field(&db, c.collection_id, "d", "date").await;

        for (column, value, expected) in [
            ("s", "x", "x"),
            ("n", "2,5", "2.5"),
            ("b", "true", "true"),
            ("d", "2026-05-06", "2026-05-06"),
            ("color", &c.option_id.to_string(), &c.option_id.to_string()),
        ] {
            let (st, _, body) =
                save(&app, &db, &cookie, c.collection_id, id, column, value, "").await;
            assert_eq!(st, StatusCode::OK, "{column}: {body}");
            assert_eq!(
                field_value(&db, id, column).await.as_deref(),
                Some(expected)
            );
        }
    }

    #[sqlx::test]
    async fn cell_required_empty_422(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (st, headers, _) = save(
            &app,
            &db,
            &cookie,
            c.collection_id,
            c.item_id,
            "name",
            "",
            "",
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(headers["HX-Reswap"], "none");
        let name: String = sqlx::query_scalar("SELECT name FROM collection_items WHERE id = $1")
            .bind(c.item_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(name, "Первая новость");
    }

    #[sqlx::test]
    async fn cell_foreign_option_422(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (st, headers, _) = save(
            &app,
            &db,
            &cookie,
            c.collection_id,
            c.item_id,
            "color",
            "999999",
            "",
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(headers["HX-Reswap"], "none");
        let expected = c.option_id.to_string();
        assert_eq!(
            field_value(&db, c.item_id, "color").await.as_deref(),
            Some(expected.as_str())
        );
    }

    #[sqlx::test]
    async fn cell_code_conflict_422(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name) VALUES ($1, 'other', 'Другая') RETURNING id",
        )
        .bind(c.collection_id)
        .fetch_one(&db)
        .await
        .unwrap();
        let (st, headers, _) = save(
            &app,
            &db,
            &cookie,
            c.collection_id,
            other,
            "code",
            "pervaya",
            "",
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(headers["HX-Reswap"], "none");
        let code: String = sqlx::query_scalar("SELECT code FROM collection_items WHERE id = $1")
            .bind(other)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(code, "other");
    }

    #[sqlx::test]
    async fn cell_filtered_row_may_disappear(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (st, _, body) = save(
            &app,
            &db,
            &cookie,
            c.collection_id,
            c.item_id,
            "active",
            "false",
            "f.active=eq:Y",
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(body.contains("id=\"list-body\""), "{body}");
        assert!(
            !body.contains(&format!("data-id=\"{}\"", c.item_id)),
            "{body}"
        );
    }

    #[sqlx::test]
    async fn cell_stale_version_409(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let stale = version(&db, c.item_id).await;
        sqlx::query(
            "UPDATE collection_items SET updated_at = now() + interval '1 hour' WHERE id = $1",
        )
        .bind(c.item_id)
        .execute(&db)
        .await
        .unwrap();
        let (st, headers, _) = post(
            &app,
            &cookie,
            &cell_form(
                &list_key(c.collection_id),
                &list_path(c.collection_id),
                "",
                c.item_id,
                "name",
                "Другое",
                &stale,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(headers["HX-Reswap"], "none");
        let name: String = sqlx::query_scalar("SELECT name FROM collection_items WHERE id = $1")
            .bind(c.item_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(name, "Первая новость");
    }

    #[sqlx::test]
    async fn cell_missing_404(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let current = version(&db, c.item_id).await;
        let (st, headers, _) = post(
            &app,
            &cookie,
            &cell_form(
                &list_key(c.collection_id),
                &list_path(c.collection_id),
                "",
                999_999,
                "name",
                "Нет такой",
                &current,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(headers["HX-Reswap"], "none");
    }

    #[sqlx::test]
    async fn cell_foreign_collection_404(db: PgPool) {
        let c = content_fixture(&db).await;
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name) VALUES ('other', 'Другая') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let foreign: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name) VALUES ($1, 'foreign', 'Чужая') RETURNING id",
        )
        .bind(other)
        .fetch_one(&db)
        .await
        .unwrap();
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let current = version(&db, c.item_id).await;
        let (st, headers, _) = post(
            &app,
            &cookie,
            &cell_form(
                &list_key(c.collection_id),
                &list_path(c.collection_id),
                "",
                foreign,
                "name",
                "Своя",
                &current,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(headers["HX-Reswap"], "none");
    }

    #[sqlx::test]
    async fn cell_readonly_403_and_no_attrs(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let reader = permitted_cookie(&db, "reader", &[]).await;
        let gid: i64 = sqlx::query_scalar("SELECT id FROM groups WHERE code = 'g_reader'")
            .fetch_one(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO collection_access (collection_id, group_id, level) VALUES ($1, $2, 'read')",
        )
        .bind(c.collection_id)
        .bind(gid)
        .execute(&db)
        .await
        .unwrap();

        let path = list_path(c.collection_id);
        let (st, _, body) = get(&app, &path, &reader).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(!body.contains("data-edit"), "{body}");
        assert!(!body.contains("data-version"), "{body}");
        assert!(body.contains("\"edit_columns\":[]"), "{body}");
        let (st, _, _) = save(
            &app,
            &db,
            &reader,
            c.collection_id,
            c.item_id,
            "name",
            "Чужое",
            "",
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);

        let cookie = admin_cookie(&db).await;
        let (st, _, body) = get(&app, &path, &cookie).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        for code in ["name", "code", "active"] {
            assert!(body.contains(&format!("data-edit=\"{code}\"")), "{body}");
        }
        assert!(!body.contains("data-edit=\"\""), "{body}");
        assert!(body.contains("data-version"), "{body}");
        assert!(!body.contains("\"edit_columns\":[]"), "{body}");
        assert!(body.contains("\"edit_columns\":[{"), "{body}");
    }

    #[sqlx::test]
    async fn cell_response_is_list_body(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (st, _, body) = save(
            &app,
            &db,
            &cookie,
            c.collection_id,
            c.item_id,
            "name",
            "Другое имя",
            "",
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(body.contains("id=\"list-body\""), "{body}");
        assert!(body.contains("Другое имя"), "{body}");
    }

    #[sqlx::test]
    async fn cell_bad_path_422(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let current = version(&db, c.item_id).await;
        let (st, headers, _) = post(
            &app,
            &cookie,
            &cell_form(
                &list_key(c.collection_id),
                "/admin/shop/orders",
                "",
                c.item_id,
                "name",
                "Другое",
                &current,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(headers["HX-Reswap"], "none");
    }
}
