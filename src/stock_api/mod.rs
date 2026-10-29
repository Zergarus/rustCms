//! API склада для внешних систем (WMS): `/api/stock/v1`, доступ по API-ключу.

pub mod error;
mod read;

use std::net::SocketAddr;

use axum::{
    Router,
    extract::{ConnectInfo, FromRequestParts, Query},
    http::{header, request::Parts},
    routing::get,
};
use serde::Deserialize;

use crate::{access::Access, apikeys, auth, state::AppState};
use error::{ApiError, ApiResult};

pub fn router() -> Router<AppState> {
    Router::new().route("/stores", get(read::stores))
}

/// Вызывающий API: ключ, его scope и права пользователя ключа.
pub struct ApiAccess {
    pub key_id: i64,
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
            key_id: owner.key_id,
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
}
