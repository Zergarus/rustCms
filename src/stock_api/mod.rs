//! API склада для внешних систем (WMS): `/api/stock/v1`, доступ по API-ключу.

pub mod error;
mod read;
mod write;

use std::net::SocketAddr;

use axum::{
    Router,
    extract::{ConnectInfo, FromRequestParts, Path, Query},
    http::{header, request::Parts},
    routing::{get, post},
};
use serde::Deserialize;

use crate::{access::Access, apikeys, auth, state::AppState};
use error::{ApiError, ApiResult};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/stores", get(read::stores))
        .route("/rests", get(read::rests))
        .route("/products/lookup", get(read::lookup))
        .route(
            "/documents",
            get(read::documents).post(write::create_document),
        )
        .route(
            "/documents/{id}",
            get(read::document)
                .put(write::replace_document)
                .delete(write::delete_document),
        )
        .route("/documents/{id}/conduct", post(write::conduct_document))
        .route("/documents/{id}/cancel", post(write::cancel_document))
        .route(
            "/contractors",
            get(read::contractors).post(write::create_contractor),
        )
        .route("/contractors/{id}", get(read::contractor))
        .fallback(|| async { ApiError::NotFound })
        .method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed })
}

/// Числовой `{id}` из пути; неверный — JSON 400 вместо текстового ответа axum.
pub struct Id(pub i64);

impl<S: Send + Sync> FromRequestParts<S> for Id {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<i64>::from_request_parts(parts, state)
            .await
            .map(|Path(id)| Id(id))
            .map_err(|_| ApiError::BadRequest("Некорректный id в адресе".into()))
    }
}

/// Вызывающий API: ключ, его scope и права пользователя ключа.
pub struct ApiAccess {
    pub scopes: Vec<String>,
    pub access: Access,
}

impl ApiAccess {
    /// Нужны и scope ключа, и право пользователя (сначала проверяется scope).
    pub fn require(&self, scope: &str, permission: &str) -> ApiResult<()> {
        if !self.scopes.iter().any(|s| s == scope) {
            return Err(ApiError::Forbidden(format!("Нужен scope {scope}")));
        }
        if !self.access.can(permission) {
            return Err(ApiError::Forbidden(format!("Нужно право {permission}")));
        }
        Ok(())
    }
}

/// Ключ из `Authorization: Bearer <ключ>` (схема без учёта регистра).
fn bearer(parts: &Parts) -> Option<&str> {
    let value = parts.headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, key) = value.trim().split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let key = key.trim();
    (!key.is_empty()).then_some(key)
}

impl FromRequestParts<AppState> for ApiAccess {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let raw = bearer(parts).ok_or(ApiError::Unauthorized)?;
        // Адрес клиента есть, только если сервер запущен с `ConnectInfo`.
        let ip = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip().to_string());
        let owner = apikeys::authenticate(&state.db, raw, ip.as_deref())
            .await?
            .ok_or(ApiError::Unauthorized)?;
        let user = sqlx::query_as::<_, auth::User>(
            "SELECT id, login, email, is_admin, active FROM users WHERE id = $1",
        )
        .bind(owner.user_id)
        .fetch_optional(&state.db)
        .await?
        .filter(|u| u.active)
        .ok_or(ApiError::Unauthorized)?;
        let access = Access::load(&state.db, user).await?;
        Ok(ApiAccess {
            scopes: owner.scopes,
            access,
        })
    }
}

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

/// Постраничность списков: `?limit=1..500` (по умолчанию 50) и `?offset>=0`.
pub struct Page {
    pub limit: i64,
    pub offset: i64,
}

#[derive(Deserialize)]
struct RawPage {
    limit: Option<i64>,
    offset: Option<i64>,
}

impl<S: Send + Sync> FromRequestParts<S> for Page {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(raw) = Query::<RawPage>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::BadRequest("Некорректные limit/offset".into()))?;
        let limit = raw.limit.unwrap_or(DEFAULT_LIMIT);
        let offset = raw.offset.unwrap_or(0);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(ApiError::BadRequest(format!(
                "limit должен быть от 1 до {MAX_LIMIT}"
            )));
        }
        if offset < 0 {
            return Err(ApiError::BadRequest(
                "offset не может быть отрицательным".into(),
            ));
        }
        Ok(Page { limit, offset })
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use serde_json::Value;
    use sqlx::PgPool;
    use tower::ServiceExt;

    use crate::{
        apikeys::{self, NewKey},
        test_support::{permitted_cookie, test_state},
    };

    /// Пользователь с правами `perms` и выпущенный ему ключ; возвращает (user_id, ключ).
    async fn key_for(db: &PgPool, perms: &[&str], scopes: &[&str]) -> String {
        key_for_user(db, perms, scopes).await.1
    }

    async fn key_for_user(db: &PgPool, perms: &[&str], scopes: &[&str]) -> (i64, String) {
        permitted_cookie(db, "wms", perms).await;
        let uid: i64 = sqlx::query_scalar("SELECT id FROM users WHERE login = 'wms'")
            .fetch_one(db)
            .await
            .unwrap();
        let key = NewKey {
            user_id: uid,
            name: "WMS".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            expires_at: None,
        };
        let (_, full) = apikeys::issue(db, &key, uid).await.unwrap();
        (uid, full)
    }

    async fn get(
        db: &PgPool,
        path: &str,
        authorization: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, Value) {
        let app = crate::app(test_state(db.clone()));
        let mut req = Request::get(path);
        if let Some(a) = authorization {
            req = req.header(header::AUTHORIZATION, a);
        }
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, body)
    }

    const UNAUTHORIZED: &str = "Неверный или отозванный API-ключ";

    #[sqlx::test]
    async fn no_header_401(db: PgPool) {
        let (status, headers, body) = get(&db, "/api/stock/v1/stores", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
        assert_eq!(body["error"], "unauthorized");
        assert_eq!(body["message"], UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn bad_key_401(db: PgPool) {
        let key = key_for(&db, &["shop.manage"], &["stock.read"]).await;
        let broken = format!("{}X", key);
        for auth in [
            format!("Bearer {broken}"),
            "Bearer cms_abcd1234_nope".to_string(),
            "Bearer ".to_string(),
            format!("Basic {key}"),
            key.clone(),
        ] {
            let (status, headers, body) = get(&db, "/api/stock/v1/stores", Some(&auth)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{auth}");
            assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
            assert_eq!(body["message"], UNAUTHORIZED);
        }
    }

    #[sqlx::test]
    async fn missing_scope_403(db: PgPool) {
        let key = key_for(&db, &["shop.manage"], &["stock.write"]).await;
        let (status, _, body) =
            get(&db, "/api/stock/v1/stores", Some(&format!("Bearer {key}"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "forbidden");
        assert_eq!(body["message"], "Нужен scope stock.read");
    }

    #[sqlx::test]
    async fn missing_right_403(db: PgPool) {
        let key = key_for(&db, &[], &["stock.read"]).await;
        let (status, _, body) =
            get(&db, "/api/stock/v1/stores", Some(&format!("Bearer {key}"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "forbidden");
        assert_eq!(body["message"], "Нужно право shop.manage");
    }

    #[sqlx::test]
    async fn stores_list(db: PgPool) {
        sqlx::query(
            "INSERT INTO catalog_stores (id, name, address, xml_id, is_default, active)
             VALUES (2, 'Второй', 'ул. Б', 'S2', FALSE, FALSE), (1, 'Основной', 'ул. А', 'S1', TRUE, TRUE)",
        )
        .execute(&db)
        .await
        .unwrap();
        let key = key_for(&db, &["shop.manage"], &["stock.read"]).await;
        let (status, _, body) =
            get(&db, "/api/stock/v1/stores", Some(&format!("Bearer {key}"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 2);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items[0]["id"], 1);
        assert_eq!(items[0]["xml_id"], "S1");
        assert_eq!(items[0]["name"], "Основной");
        assert_eq!(items[0]["address"], "ул. А");
        assert_eq!(items[0]["active"], true);
        assert_eq!(items[0]["is_default"], true);
        assert_eq!(items[1]["id"], 2);
        assert_eq!(items[1]["active"], false);
        assert_eq!(items[1]["is_default"], false);
    }

    #[sqlx::test]
    async fn stores_paging_and_bad_page(db: PgPool) {
        for i in 1..=3 {
            sqlx::query("INSERT INTO catalog_stores (id, name) VALUES ($1, 'С')")
                .bind(i as i64)
                .execute(&db)
                .await
                .unwrap();
        }
        let key = key_for(&db, &["shop.manage"], &["stock.read"]).await;
        let auth = format!("Bearer {key}");
        let (status, _, body) =
            get(&db, "/api/stock/v1/stores?limit=1&offset=1", Some(&auth)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 3);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        assert_eq!(body["items"][0]["id"], 2);
        for bad in ["limit=0", "limit=501", "offset=-1", "limit=abc"] {
            let (status, _, body) =
                get(&db, &format!("/api/stock/v1/stores?{bad}"), Some(&auth)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(body["error"], "bad_request");
        }
    }

    #[sqlx::test]
    async fn bearer_scheme_case_insensitive(db: PgPool) {
        let key = key_for(&db, &["shop.manage"], &["stock.read"]).await;
        for auth in [format!("bearer {key}"), format!("BEARER   {key}  ")] {
            let (status, _, _) = get(&db, "/api/stock/v1/stores", Some(&auth)).await;
            assert_eq!(status, StatusCode::OK, "{auth}");
        }
    }

    #[sqlx::test]
    async fn right_revoked_takes_effect(db: PgPool) {
        let key = key_for(&db, &["shop.manage"], &["stock.read"]).await;
        let auth = format!("Bearer {key}");
        let (status, _, _) = get(&db, "/api/stock/v1/stores", Some(&auth)).await;
        assert_eq!(status, StatusCode::OK);
        sqlx::query("DELETE FROM group_permissions WHERE permission = 'shop.manage'")
            .execute(&db)
            .await
            .unwrap();
        let (status, _, body) = get(&db, "/api/stock/v1/stores", Some(&auth)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["message"], "Нужно право shop.manage");
    }

    #[sqlx::test]
    async fn inactive_user_401(db: PgPool) {
        let (uid, key) = key_for_user(&db, &["shop.manage"], &["stock.read"]).await;
        sqlx::query("UPDATE users SET active = FALSE WHERE id = $1")
            .bind(uid)
            .execute(&db)
            .await
            .unwrap();
        let (status, headers, body) =
            get(&db, "/api/stock/v1/stores", Some(&format!("Bearer {key}"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
        assert_eq!(body["message"], UNAUTHORIZED);
    }

    const BASE: &str = "/api/stock/v1";

    async fn read_key(db: &PgPool, perms: &[&str]) -> String {
        format!("Bearer {}", key_for(db, perms, &["stock.read"]).await)
    }

    /// Два товара (простой и предложение), два склада, остатки.
    async fn stock_fixture(db: &PgPool) -> (i64, i64) {
        let f = crate::test_support::order_fixture(db).await;
        sqlx::query("INSERT INTO catalog_stores (id, name) VALUES (2, 'Второй')")
            .execute(db)
            .await
            .unwrap();
        let coll: i64 =
            sqlx::query_scalar("SELECT collection_id FROM collection_items WHERE id = $1")
                .bind(f.product_id)
                .fetch_one(db)
                .await
                .unwrap();
        sqlx::query("UPDATE collection_items SET xml_id = 'X-1' WHERE id = $1")
            .bind(f.product_id)
            .execute(db)
            .await
            .unwrap();
        let second: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name, product_id, xml_id)
             VALUES ($1, 'offer', 'Предложение', $2, 'X-2') RETURNING id",
        )
        .bind(coll)
        .bind(f.product_id)
        .fetch_one(db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO catalog_products (item_id, quantity) VALUES ($1, 3)")
            .bind(second)
            .execute(db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO catalog_store_amounts (item_id, store_id, amount, quantity_reserved)
             VALUES ($1, 2, 7, 1), ($2, 1, 3, 0)",
        )
        .bind(f.product_id)
        .bind(second)
        .execute(db)
        .await
        .unwrap();
        (f.product_id, second)
    }

    /// Отдельный простой товар (не родитель предложений).
    async fn simple_item(db: &PgPool, a: i64, xml: &str) -> i64 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name, xml_id)
             SELECT collection_id, 'simple', 'Простой', $2 FROM collection_items WHERE id = $1
             RETURNING id",
        )
        .bind(a)
        .bind(xml)
        .fetch_one(db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO catalog_products (item_id) VALUES ($1)")
            .bind(id)
            .execute(db)
            .await
            .unwrap();
        id
    }

    #[sqlx::test]
    async fn rests_filters(db: PgPool) {
        let (a, b) = stock_fixture(&db).await;
        let auth = read_key(&db, &["shop.manage"]).await;
        let (status, _, body) = get(&db, &format!("{BASE}/rests"), Some(&auth)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 3);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items[0]["item_id"], a);
        assert_eq!(items[0]["store_id"], 1);
        assert_eq!(items[0]["amount"], 5.0);
        assert_eq!(items[1]["store_id"], 2);
        assert_eq!(items[1]["amount"], 7.0);
        assert_eq!(items[1]["quantity_reserved"], 1.0);
        assert_eq!(items[2]["item_id"], b);
        let (_, _, body) = get(&db, &format!("{BASE}/rests?item_id={b}"), Some(&auth)).await;
        assert_eq!(body["total"], 1);
        let (_, _, body) = get(
            &db,
            &format!("{BASE}/rests?item_id={a},{b}&store_id=1"),
            Some(&auth),
        )
        .await;
        assert_eq!(body["total"], 2);
        let (_, _, body) = get(&db, &format!("{BASE}/rests?store_id=2"), Some(&auth)).await;
        assert_eq!(body["items"][0]["item_id"], a);
        for bad in ["item_id=1,x", "item_id=", "store_id=abc"] {
            let (status, _, body) = get(&db, &format!("{BASE}/rests?{bad}"), Some(&auth)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(body["error"], "bad_request");
        }
    }

    #[sqlx::test]
    async fn lookup_by_barcode_xml_id_id(db: PgPool) {
        let (a, b) = stock_fixture(&db).await;
        sqlx::query(
            "INSERT INTO catalog_barcodes (item_id, barcode, store_id, order_id) VALUES
             ($1, 'BC-1', NULL, NULL), ($1, 'BC-UNIT', 1, NULL)",
        )
        .bind(a)
        .execute(&db)
        .await
        .unwrap();
        let auth = read_key(&db, &["shop.manage"]).await;
        for q in ["barcode=BC-1", "xml_id=X-1", &format!("id={a}")] {
            let (status, _, body) =
                get(&db, &format!("{BASE}/products/lookup?{q}"), Some(&auth)).await;
            assert_eq!(status, StatusCode::OK, "{q}");
            assert_eq!(body["id"], a);
            assert_eq!(body["xml_id"], "X-1");
            assert_eq!(body["name"], "Товар");
            // Родитель предложений получает тип «с предложениями» — не simple/offer.
            assert_eq!(body["type"], "other");
            assert_eq!(body["barcode_multi"], false);
            assert_eq!(body["barcodes"], serde_json::json!(["BC-1"]));
            let stores = body["stores"].as_array().unwrap();
            assert_eq!(stores.len(), 2);
            assert_eq!(stores[0]["store_id"], 1);
            assert_eq!(stores[1]["amount"], 7.0);
            assert_eq!(stores[1]["quantity_reserved"], 1.0);
        }
        let (_, _, body) = get(&db, &format!("{BASE}/products/lookup?id={b}"), Some(&auth)).await;
        assert_eq!(body["type"], "offer");
        let c = simple_item(&db, a, "X-3").await;
        let (_, _, body) = get(&db, &format!("{BASE}/products/lookup?id={c}"), Some(&auth)).await;
        assert_eq!(body["type"], "simple");
        for q in ["barcode=NOPE", "xml_id=NOPE", "id=999999"] {
            let (status, _, body) =
                get(&db, &format!("{BASE}/products/lookup?{q}"), Some(&auth)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{q}");
            assert_eq!(body["error"], "not_found");
        }
    }

    #[sqlx::test]
    async fn lookup_one_param_required(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        let auth = read_key(&db, &["shop.manage"]).await;
        for q in [
            String::new(),
            format!("?id={a}&xml_id=X-1"),
            "?barcode=1&id=1".into(),
            "?id=abc".into(),
        ] {
            let (status, _, body) =
                get(&db, &format!("{BASE}/products/lookup{q}"), Some(&auth)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{q}");
            assert_eq!(body["error"], "bad_request");
        }
    }

    #[sqlx::test]
    async fn lookup_purchase_price_needs_right(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        let a = simple_item(&db, a, "X-4").await;
        sqlx::query(
            "UPDATE catalog_products SET purchasing_price = 12.5, purchasing_currency = 'RUB' WHERE item_id = $1",
        )
        .bind(a)
        .execute(&db)
        .await
        .unwrap();
        let auth = read_key(&db, &["shop.manage"]).await;
        let (_, _, body) = get(&db, &format!("{BASE}/products/lookup?id={a}"), Some(&auth)).await;
        assert!(body.get("purchasing_price").is_none());
        assert!(body.get("purchasing_currency").is_none());
        let gid: i64 = sqlx::query_scalar("SELECT id FROM groups WHERE code = 'g_wms'")
            .fetch_one(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO group_permissions (group_id, permission) VALUES ($1, 'catalog.purchase_info')",
        )
        .bind(gid)
        .execute(&db)
        .await
        .unwrap();
        let (_, _, body) = get(&db, &format!("{BASE}/products/lookup?id={a}"), Some(&auth)).await;
        assert_eq!(body["purchasing_price"], 12.5);
        assert_eq!(body["purchasing_currency"], "RUB");
    }

    async fn doc(
        db: &PgPool,
        t: &str,
        status: &str,
        date: &str,
        xml: &str,
        contractor: Option<i64>,
    ) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO store_documents (doc_type, title, doc_number, date_document, status, xml_id,
                                          contractor_id, commentary, total)
             VALUES ($1, 'Т', 'N-1', $2::date, $3, $4, $5, 'комм', 150.5) RETURNING id",
        )
        .bind(t)
        .bind(date)
        .bind(status)
        .bind(xml)
        .bind(contractor)
        .fetch_one(db)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn documents_list_filters(db: PgPool) {
        control_on(&db).await;
        let c: i64 =
            sqlx::query_scalar("INSERT INTO contractors (company) VALUES ('ООО') RETURNING id")
                .fetch_one(&db)
                .await
                .unwrap();
        let d1 = doc(&db, "A", "N", "2026-10-01", "W-1", Some(c)).await;
        let d2 = doc(&db, "S", "Y", "2026-10-15", "W-2", None).await;
        let d3 = doc(&db, "A", "Y", "2026-10-20", "", None).await;
        let auth = read_key(&db, &["shop.manage"]).await;
        let list = |q: String| {
            let (db, auth) = (db.clone(), auth.clone());
            async move { get(&db, &format!("{BASE}/documents{q}"), Some(&auth)).await }
        };
        let ids = |b: &Value| -> Vec<i64> {
            b["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["id"].as_i64().unwrap())
                .collect()
        };
        let (status, _, body) = list(String::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 3);
        assert_eq!(ids(&body), vec![d3, d2, d1]);
        let first = &body["items"][2];
        assert_eq!(first["type"], "A");
        assert_eq!(first["number"], "N-1");
        assert_eq!(first["date"], "2026-10-01");
        assert_eq!(first["status"], "draft");
        assert_eq!(first["contractor_id"], c);
        assert_eq!(first["comment"], "комм");
        assert_eq!(first["total"], 150.5);
        assert_eq!(first["xml_id"], "W-1");
        assert!(first.get("lines").is_none());
        assert_eq!(body["items"][0]["status"], "conducted");
        assert_eq!(body["items"][0]["xml_id"], "");
        assert_eq!(ids(&list("?type=A".into()).await.2), vec![d3, d1]);
        assert_eq!(ids(&list("?status=conducted".into()).await.2), vec![d3, d2]);
        assert_eq!(ids(&list("?status=draft".into()).await.2), vec![d1]);
        assert_eq!(
            ids(&list("?from=2026-10-10&to=2026-10-15".into()).await.2),
            vec![d2]
        );
        assert_eq!(ids(&list(format!("?contractor_id={c}")).await.2), vec![d1]);
        assert_eq!(ids(&list("?xml_id=W-2".into()).await.2), vec![d2]);
        for bad in [
            "type=Z",
            "status=Y",
            "from=2026-13-01",
            "to=abc",
            "contractor_id=x",
        ] {
            let (status, _, body) = list(format!("?{bad}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(body["error"], "bad_request");
        }
    }

    #[sqlx::test]
    async fn document_with_lines_and_barcodes(db: PgPool) {
        control_on(&db).await;
        let (a, b) = stock_fixture(&db).await;
        let id = doc(&db, "M", "N", "2026-10-02", "W-9", None).await;
        let l1: i64 = sqlx::query_scalar(
            "INSERT INTO store_document_items (doc_id, item_id, store_from, store_to, amount, purchasing_price)
             VALUES ($1, $2, 1, 2, 2.5, 10) RETURNING id",
        )
        .bind(id)
        .bind(a)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO store_document_items (doc_id, item_id, store_to, amount) VALUES ($1, $2, 2, 1)")
            .bind(id)
            .bind(b)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO store_document_barcodes (doc_item_id, barcode) VALUES ($1, 'U-2'), ($1, 'U-1')")
            .bind(l1)
            .execute(&db)
            .await
            .unwrap();
        let auth = read_key(&db, &["shop.manage"]).await;
        let (status, _, body) = get(&db, &format!("{BASE}/documents/{id}"), Some(&auth)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], id);
        assert_eq!(body["type"], "M");
        let lines = body["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["id"], l1);
        assert_eq!(lines[0]["item_id"], a);
        assert_eq!(lines[0]["store_from"], 1);
        assert_eq!(lines[0]["store_to"], 2);
        assert_eq!(lines[0]["amount"], 2.5);
        assert_eq!(lines[0]["purchasing_price"], 10.0);
        assert_eq!(lines[0]["barcodes"], serde_json::json!(["U-1", "U-2"]));
        assert_eq!(lines[1]["store_from"], Value::Null);
        assert_eq!(lines[1]["purchasing_price"], Value::Null);
        assert_eq!(lines[1]["barcodes"], serde_json::json!([]));
        let (status, _, body) = get(&db, &format!("{BASE}/documents/999999"), Some(&auth)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_found");
    }

    #[sqlx::test]
    async fn contractors_list_and_get(db: PgPool) {
        let c2: i64 = sqlx::query_scalar(
            "INSERT INTO contractors (person_type, company, inn, email) VALUES ('C', 'ООО Ромашка', '7700', 'a@b.c') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let auth = read_key(&db, &["shop.manage"]).await;
        let (status, _, body) = get(&db, &format!("{BASE}/contractors"), Some(&auth)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 1);
        assert_eq!(body["items"][0]["id"], c2);
        let (status, _, body) = get(&db, &format!("{BASE}/contractors/{c2}"), Some(&auth)).await;
        assert_eq!(status, StatusCode::OK);
        for (k, v) in [
            ("person_type", "C"),
            ("company", "ООО Ромашка"),
            ("inn", "7700"),
            ("email", "a@b.c"),
            ("kpp", ""),
            ("post_index", ""),
        ] {
            assert_eq!(body[k], v, "{k}");
        }
        assert!(body["created_at"].is_string());
        let (status, _, _) = get(&db, &format!("{BASE}/contractors/999999"), Some(&auth)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn list_params_bounds(db: PgPool) {
        control_on(&db).await;
        let auth = read_key(&db, &["shop.manage"]).await;
        for path in ["rests", "documents", "contractors"] {
            for (q, want) in [
                ("limit=100000", 400),
                ("offset=-1", 400),
                ("limit=500", 200),
            ] {
                let (status, _, _) = get(&db, &format!("{BASE}/{path}?{q}"), Some(&auth)).await;
                assert_eq!(status.as_u16(), want, "{path}?{q}");
            }
        }
    }

    // --- запись ---

    async fn control_on(db: &PgPool) {
        let mut tx = db.begin().await.unwrap();
        crate::stock::set_store_control(&mut tx, true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    async fn write_key(db: &PgPool) -> (i64, String) {
        let (uid, key) = key_for_user(db, &["shop.manage"], &["stock.write", "stock.read"]).await;
        (uid, format!("Bearer {key}"))
    }

    async fn send(
        db: &PgPool,
        method: &str,
        path: &str,
        auth: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let app = crate::app(test_state(db.clone()));
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, auth)
            .header(header::CONTENT_TYPE, "application/json");
        let req = req
            .body(match body {
                Some(b) => Body::from(b.to_string()),
                None => Body::empty(),
            })
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn doc_count(db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM store_documents")
            .fetch_one(db)
            .await
            .unwrap()
    }

    fn arrival(item: Value) -> Value {
        let mut line = serde_json::json!({"store_to": 1, "amount": 10, "purchasing_price": 100});
        line.as_object_mut()
            .unwrap()
            .extend(item.as_object().unwrap().clone());
        serde_json::json!({
            "type": "A", "xml_id": "WMS-1042", "number": "WMS-1042", "date": "2026-10-30",
            "currency": "RUB", "comment": "из WMS", "lines": [line]
        })
    }

    #[sqlx::test]
    async fn create_draft_by_item_id(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        control_on(&db).await;
        let (_, auth) = write_key(&db).await;
        let (status, body) = send(
            &db,
            "POST",
            &format!("{BASE}/documents"),
            &auth,
            Some(arrival(serde_json::json!({"item_id": a}))),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["status"], "draft");
        assert_eq!(body["type"], "A");
        assert_eq!(body["xml_id"], "WMS-1042");
        assert_eq!(body["number"], "WMS-1042");
        assert_eq!(body["comment"], "из WMS");
        assert_eq!(body["lines"][0]["item_id"], a);
        assert_eq!(body["lines"][0]["amount"], 10.0);
        assert_eq!(body["total"], 1000.0);
    }

    #[sqlx::test]
    async fn create_by_item_xml_id(db: PgPool) {
        let (a, b) = stock_fixture(&db).await;
        control_on(&db).await;
        let (_, auth) = write_key(&db).await;
        let (status, body) = send(
            &db,
            "POST",
            &format!("{BASE}/documents"),
            &auth,
            Some(arrival(serde_json::json!({"item_xml_id": "X-2"}))),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["lines"][0]["item_id"], b);
        assert_ne!(a, b);
        // оба или ни одного — 400
        for item in [
            serde_json::json!({"item_id": a, "item_xml_id": "X-1"}),
            serde_json::json!({}),
        ] {
            let mut req = arrival(item);
            req["xml_id"] = "OTHER".into();
            let (status, body) =
                send(&db, "POST", &format!("{BASE}/documents"), &auth, Some(req)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(body["error"], "bad_request");
        }
        assert_eq!(doc_count(&db).await, 1);
    }

    #[sqlx::test]
    async fn unknown_item_xml_id_rejected(db: PgPool) {
        stock_fixture(&db).await;
        control_on(&db).await;
        let (_, auth) = write_key(&db).await;
        let mut req = arrival(serde_json::json!({"item_xml_id": "NOPE"}));
        req["lines"].as_array_mut().unwrap().push(serde_json::json!(
            {"item_xml_id": "X-1", "store_to": 1, "amount": "много"}
        ));
        let (status, body) =
            send(&db, "POST", &format!("{BASE}/documents"), &auth, Some(req)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"], "rejected");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "{body}");
        assert!(
            messages
                .iter()
                .any(|m| m == "Не найден товар с внешним кодом NOPE")
        );
        assert_eq!(doc_count(&db).await, 0);
    }

    #[sqlx::test]
    async fn unknown_item_id_rejected(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        control_on(&db).await;
        let (_, auth) = write_key(&db).await;
        let plain: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name)
             SELECT collection_id, 'news', 'Новость' FROM collection_items WHERE id = $1
             RETURNING id",
        )
        .bind(a)
        .fetch_one(&db)
        .await
        .unwrap();
        let mut req = arrival(serde_json::json!({"item_id": 999999}));
        req["lines"].as_array_mut().unwrap().push(serde_json::json!(
            {"item_id": plain, "store_to": 1, "amount": 1}
        ));
        let (status, body) =
            send(&db, "POST", &format!("{BASE}/documents"), &auth, Some(req)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let messages = body["messages"].as_array().unwrap();
        assert!(
            messages.iter().any(|m| m == "Не найден товар #999999"),
            "{body}"
        );
        assert!(
            messages
                .iter()
                .any(|m| *m == format!("Не найден товар #{plain}")),
            "{body}"
        );
        assert_eq!(doc_count(&db).await, 0);
    }

    #[sqlx::test]
    async fn xml_id_too_long_400(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        control_on(&db).await;
        let (_, auth) = write_key(&db).await;
        let mut req = arrival(serde_json::json!({"item_id": a}));
        req["xml_id"] = "x".repeat(101).into();
        let (status, _) = send(&db, "POST", &format!("{BASE}/documents"), &auth, Some(req)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(doc_count(&db).await, 0);
    }

    #[sqlx::test]
    async fn duplicate_xml_id_conflict(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        control_on(&db).await;
        let (_, auth) = write_key(&db).await;
        let req = arrival(serde_json::json!({"item_id": a}));
        let (_, first) = send(
            &db,
            "POST",
            &format!("{BASE}/documents"),
            &auth,
            Some(req.clone()),
        )
        .await;
        let (status, body) =
            send(&db, "POST", &format!("{BASE}/documents"), &auth, Some(req)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "conflict");
        assert_eq!(
            body["message"],
            "Документ с внешним кодом WMS-1042 уже есть"
        );
        assert_eq!(body["id"], first["id"]);
        assert_eq!(doc_count(&db).await, 1);
    }

    #[sqlx::test]
    async fn put_replaces_lines(db: PgPool) {
        let (a, b) = stock_fixture(&db).await;
        control_on(&db).await;
        let (_, auth) = write_key(&db).await;
        let (_, created) = send(
            &db,
            "POST",
            &format!("{BASE}/documents"),
            &auth,
            Some(arrival(serde_json::json!({"item_id": a}))),
        )
        .await;
        let id = created["id"].as_i64().unwrap();
        let mut req = arrival(serde_json::json!({"item_id": b}));
        req["comment"] = "новый".into();
        req["xml_id"] = "WMS-2".into();
        let (status, body) = send(
            &db,
            "PUT",
            &format!("{BASE}/documents/{id}"),
            &auth,
            Some(req),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["id"], id);
        assert_eq!(body["comment"], "новый");
        assert_eq!(body["xml_id"], "WMS-2");
        let lines = body["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["item_id"], b);
        assert_eq!(body["title"], created["title"]);
        let (status, _) = send(
            &db,
            "PUT",
            &format!("{BASE}/documents/999999"),
            &auth,
            Some(arrival(serde_json::json!({"item_id": a}))),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn put_conducted_conflict(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        control_on(&db).await;
        let (_, auth) = write_key(&db).await;
        let id = doc(&db, "A", "Y", "2026-10-02", "", None).await;
        let (status, body) = send(
            &db,
            "PUT",
            &format!("{BASE}/documents/{id}"),
            &auth,
            Some(arrival(serde_json::json!({"item_id": a}))),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "conflict");
        assert_eq!(
            body["message"],
            "Проведённый документ нельзя изменить — сначала отмените проведение"
        );
    }

    #[sqlx::test]
    async fn delete_draft_and_conducted(db: PgPool) {
        stock_fixture(&db).await;
        control_on(&db).await;
        let (_, auth) = write_key(&db).await;
        let draft = doc(&db, "A", "N", "2026-10-02", "", None).await;
        let done = doc(&db, "A", "Y", "2026-10-02", "", None).await;
        let (status, _) = send(
            &db,
            "DELETE",
            &format!("{BASE}/documents/{draft}"),
            &auth,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, body) = send(
            &db,
            "DELETE",
            &format!("{BASE}/documents/{done}"),
            &auth,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(
            body["message"],
            "Проведённый документ нельзя удалить — сначала отмените проведение"
        );
        let (status, _) = send(
            &db,
            "DELETE",
            &format!("{BASE}/documents/{draft}"),
            &auth,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(doc_count(&db).await, 1);
    }

    #[sqlx::test]
    async fn author_is_key_user(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        control_on(&db).await;
        let (uid, auth) = write_key(&db).await;
        let (_, body) = send(
            &db,
            "POST",
            &format!("{BASE}/documents"),
            &auth,
            Some(arrival(serde_json::json!({"item_id": a}))),
        )
        .await;
        assert_eq!(body["created_by"], uid);
        let responsible: Option<i64> =
            sqlx::query_scalar("SELECT responsible_id FROM store_documents WHERE id = $1")
                .bind(body["id"].as_i64().unwrap())
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(responsible, Some(uid));
    }

    #[sqlx::test]
    async fn multi_barcodes_in_lines(db: PgPool) {
        let (a0, _) = stock_fixture(&db).await;
        let a = simple_item(&db, a0, "S-1").await;
        control_on(&db).await;
        sqlx::query("UPDATE catalog_products SET barcode_multi = TRUE WHERE item_id = $1")
            .bind(a)
            .execute(&db)
            .await
            .unwrap();
        let (_, auth) = write_key(&db).await;
        let mut req = arrival(serde_json::json!({"item_id": a, "amount": 2,
            "barcodes": ["B-2", "B-1"]}));
        req["xml_id"] = "".into();
        let c: i64 =
            sqlx::query_scalar("INSERT INTO contractors (company) VALUES ('ООО') RETURNING id")
                .fetch_one(&db)
                .await
                .unwrap();
        req["contractor_id"] = c.into();
        let (status, body) =
            send(&db, "POST", &format!("{BASE}/documents"), &auth, Some(req)).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(
            body["lines"][0]["barcodes"],
            serde_json::json!(["B-1", "B-2"])
        );
        // проведение через склад принимает коды
        let id = body["id"].as_i64().unwrap();
        let mut tx = db.begin().await.unwrap();
        crate::stock::documents::conduct(&mut tx, id, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    #[sqlx::test]
    async fn store_control_off_conflict(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        let (_, auth) = write_key(&db).await;
        let id = doc(&db, "A", "N", "2026-10-02", "", None).await;
        let req = arrival(serde_json::json!({"item_id": a}));
        let cases = [
            ("GET", format!("{BASE}/documents"), None),
            ("GET", format!("{BASE}/documents/{id}"), None),
            ("POST", format!("{BASE}/documents"), Some(req.clone())),
            ("PUT", format!("{BASE}/documents/{id}"), Some(req)),
            ("DELETE", format!("{BASE}/documents/{id}"), None),
        ];
        for (method, path, body) in cases {
            let (status, resp) = send(&db, method, &path, &auth, body).await;
            assert_eq!(status, StatusCode::CONFLICT, "{method} {path}: {resp}");
            assert_eq!(resp["message"], "Складской учёт выключен");
        }
        assert_eq!(doc_count(&db).await, 1);
    }

    #[sqlx::test]
    async fn create_contractor(db: PgPool) {
        let (uid, auth) = write_key(&db).await;
        let (status, body) = send(
            &db,
            "POST",
            &format!("{BASE}/contractors"),
            &auth,
            Some(
                serde_json::json!({"person_type": "C", "company": "ООО Ромашка",
                "inn": "7700000000", "phone": "+7"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["company"], "ООО Ромашка");
        assert_eq!(body["inn"], "7700000000");
        assert!(body["id"].as_i64().is_some());
        for bad in [
            serde_json::json!({"person_type": "X", "company": "a"}),
            serde_json::json!({"person_type": "C"}),
            serde_json::json!({"company": "a"}),
        ] {
            let (status, _) = send(
                &db,
                "POST",
                &format!("{BASE}/contractors"),
                &auth,
                Some(bad),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        // без scope stock.write — 403
        let new_key = NewKey {
            user_id: uid,
            name: "RO".into(),
            scopes: vec!["stock.read".into()],
            expires_at: None,
        };
        let (_, full) = apikeys::issue(&db, &new_key, uid).await.unwrap();
        let read_only = format!("Bearer {full}");
        let (status, _) = send(
            &db,
            "POST",
            &format!("{BASE}/contractors"),
            &read_only,
            Some(serde_json::json!({"person_type": "C", "company": "a"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    // --- проведение и отмена ---

    async fn conduct_key(db: &PgPool) -> String {
        let key = key_for(
            db,
            &["shop.manage"],
            &["stock.write", "stock.read", "stock.conduct"],
        )
        .await;
        format!("Bearer {key}")
    }

    async fn contractor(db: &PgPool) -> i64 {
        sqlx::query_scalar("INSERT INTO contractors (company) VALUES ('Поставщик') RETURNING id")
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn draft_arrival(db: &PgPool, auth: &str, item: i64, contractor: Option<i64>) -> i64 {
        let mut body = arrival(serde_json::json!({"item_id": item}));
        if let Some(c) = contractor {
            body["contractor_id"] = serde_json::json!(c);
        }
        let (status, resp) = send(db, "POST", &format!("{BASE}/documents"), auth, Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{resp}");
        resp["id"].as_i64().unwrap()
    }

    #[sqlx::test]
    async fn conduct_changes_rests_and_batches(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        let a = simple_item(&db, a, "S-1").await;
        control_on(&db).await;
        let mut tx = db.begin().await.unwrap();
        assert!(
            crate::stock::batches::set_method(&mut tx, crate::stock::batches::Method::Fifo)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
        let auth = conduct_key(&db).await;
        let c = contractor(&db).await;
        let id = draft_arrival(&db, &auth, a, Some(c)).await;
        let (status, body) = send(
            &db,
            "POST",
            &format!("{BASE}/documents/{id}/conduct"),
            &auth,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "conducted");
        let rest: f64 = sqlx::query_scalar(
            "SELECT amount::float8 FROM catalog_store_amounts WHERE item_id = $1 AND store_id = 1",
        )
        .bind(a)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(rest, 10.0);
        let batch: (f64, f64) = sqlx::query_as(
            "SELECT available::float8, price::float8 FROM catalog_store_batches
             WHERE item_id = $1 AND store_id = 1",
        )
        .bind(a)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(batch, (10.0, 100.0));
    }

    #[sqlx::test]
    async fn conduct_rejected_422(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        control_on(&db).await;
        let auth = conduct_key(&db).await;
        let id = draft_arrival(&db, &auth, a, None).await;
        let (status, body) = send(
            &db,
            "POST",
            &format!("{BASE}/documents/{id}/conduct"),
            &auth,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"], "rejected");
        assert_eq!(body["messages"][0], "Не указан поставщик.");
        let (status, _) = send(
            &db,
            "POST",
            &format!("{BASE}/documents/999999/conduct"),
            &auth,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn cancel_ok(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        let a = simple_item(&db, a, "S-1").await;
        control_on(&db).await;
        let auth = conduct_key(&db).await;
        let c = contractor(&db).await;
        let id = draft_arrival(&db, &auth, a, Some(c)).await;
        let url = |what: &str| format!("{BASE}/documents/{id}/{what}");
        let (status, body) = send(&db, "POST", &url("cancel"), &auth, None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["messages"][0], "Документ не проведён");
        let (status, _) = send(&db, "POST", &url("conduct"), &auth, None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = send(&db, "POST", &url("cancel"), &auth, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "draft");
        let rest: f64 = sqlx::query_scalar(
            "SELECT amount::float8 FROM catalog_store_amounts WHERE item_id = $1 AND store_id = 1",
        )
        .bind(a)
        .fetch_optional(&db)
        .await
        .unwrap()
        .unwrap_or(0.0);
        assert_eq!(rest, 0.0);
        let (status, _) = send(&db, "POST", &url("cancel"), &auth, None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test]
    async fn write_key_cannot_conduct(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        control_on(&db).await;
        let (_, auth) = write_key(&db).await;
        let id = draft_arrival(&db, &auth, a, None).await;
        for what in ["conduct", "cancel"] {
            let (status, body) = send(
                &db,
                "POST",
                &format!("{BASE}/documents/{id}/{what}"),
                &auth,
                None,
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
            assert_eq!(body["message"], "Нужен scope stock.conduct");
        }
    }

    #[sqlx::test]
    async fn unknown_currency_rejected(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        control_on(&db).await;
        let (_, auth) = write_key(&db).await;
        let mut req = arrival(serde_json::json!({"item_id": a}));
        req["currency"] = "XXX".into();
        req["lines"].as_array_mut().unwrap().push(serde_json::json!(
            {"item_xml_id": "NOPE", "store_to": 1, "amount": 1}
        ));
        let (status, body) =
            send(&db, "POST", &format!("{BASE}/documents"), &auth, Some(req)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "{body}");
        assert!(messages.iter().any(|m| m == "Неизвестная валюта XXX"));
        assert_eq!(doc_count(&db).await, 0);
        // валюта из справочника принимается
        sqlx::query("INSERT INTO currencies (code) VALUES ('USD')")
            .execute(&db)
            .await
            .unwrap();
        let mut req = arrival(serde_json::json!({"item_id": a}));
        req["currency"] = "USD".into();
        let (status, body) =
            send(&db, "POST", &format!("{BASE}/documents"), &auth, Some(req)).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["currency"], "USD");
    }

    #[sqlx::test]
    async fn bad_path_json_400(db: PgPool) {
        let (_, auth) = write_key(&db).await;
        for path in ["documents/abc", "contractors/abc", "documents/abc/conduct"] {
            let method = if path.ends_with("conduct") {
                "POST"
            } else {
                "GET"
            };
            let (status, body) = send(&db, method, &format!("{BASE}/{path}"), &auth, None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
            assert_eq!(body["error"], "bad_request", "{path}");
        }
    }

    #[sqlx::test]
    async fn unknown_route_json_404(db: PgPool) {
        let (_, auth) = write_key(&db).await;
        let (status, body) = send(&db, "GET", &format!("{BASE}/nope"), &auth, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_found");
        assert_eq!(body["message"], "Не найдено");
    }

    #[sqlx::test]
    async fn wrong_method_json_405(db: PgPool) {
        let (_, auth) = write_key(&db).await;
        let (status, body) = send(&db, "PATCH", &format!("{BASE}/documents"), &auth, None).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(body["error"], "method_not_allowed");
        assert!(body["message"].is_string());
    }

    #[sqlx::test]
    async fn lookup_xml_id_ignores_non_catalog(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        // не товар каталога с тем же внешним кодом и меньшим id
        let plain: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name, xml_id)
             SELECT collection_id, 'news', 'Новость', 'SAME' FROM collection_items WHERE id = $1
             RETURNING id",
        )
        .bind(a)
        .fetch_one(&db)
        .await
        .unwrap();
        let product = simple_item(&db, a, "SAME").await;
        assert!(plain < product);
        let auth = read_key(&db, &["shop.manage"]).await;
        let (status, _, body) = get(
            &db,
            &format!("{BASE}/products/lookup?xml_id=SAME"),
            Some(&auth),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["id"], product);
    }

    #[sqlx::test]
    async fn put_without_xml_id_keeps_it(db: PgPool) {
        let (a, _) = stock_fixture(&db).await;
        control_on(&db).await;
        let (_, auth) = write_key(&db).await;
        let (_, created) = send(
            &db,
            "POST",
            &format!("{BASE}/documents"),
            &auth,
            Some(arrival(serde_json::json!({"item_id": a}))),
        )
        .await;
        let id = created["id"].as_i64().unwrap();
        let url = format!("{BASE}/documents/{id}");
        let mut req = arrival(serde_json::json!({"item_id": a}));
        req.as_object_mut().unwrap().remove("xml_id");
        let (status, body) = send(&db, "PUT", &url, &auth, Some(req.clone())).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["xml_id"], "WMS-1042");
        for clear in [serde_json::json!(""), Value::Null] {
            let mut req = req.clone();
            req["xml_id"] = clear;
            let (status, body) = send(&db, "PUT", &url, &auth, Some(req)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["xml_id"], "");
            // вернуть код для следующего круга
            let mut back = arrival(serde_json::json!({"item_id": a}));
            back["xml_id"] = "WMS-1042".into();
            send(&db, "PUT", &url, &auth, Some(back)).await;
        }
    }

    #[sqlx::test]
    async fn contractor_accepts_number_and_null(db: PgPool) {
        let (_, auth) = write_key(&db).await;
        let (status, body) = send(
            &db,
            "POST",
            &format!("{BASE}/contractors"),
            &auth,
            Some(serde_json::json!({
                "person_type": "C", "company": "ООО", "inn": 7700000000_i64, "kpp": null
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["inn"], "7700000000");
        assert_eq!(body["kpp"], "");
    }
}
