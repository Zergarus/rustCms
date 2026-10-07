//! Местоположения (модуль sale): `location/search`, `location/set`, `location/current`.
//! Выбор гостя хранится в cookie [`COOKIE`], авторизованного — ещё и в профиле
//! (`users.extra.location_id`). Дерево небольшое и держится в памяти.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::{Query, State},
    response::IntoResponse,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::FromRow;
use tokio::sync::RwLock;

use super::{
    BxError, BxResult, auth::current_user_id, collate_key, parse_body, query::number, success,
};
use crate::state::AppState;

pub const COOKIE: &str = "CMS_LOCATION";
const CACHE_TTL: Duration = Duration::from_secs(300);
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;

#[derive(Debug, Clone, FromRow)]
struct Location {
    id: i64,
    code: String,
    parent_id: Option<i64>,
    type_code: String,
    name: String,
}

struct Tree {
    by_id: HashMap<i64, Location>,
    loaded_at: Instant,
}

impl Tree {
    /// Предки от корня до непосредственного родителя.
    fn ancestors(&self, loc: &Location) -> Vec<&Location> {
        let mut out = Vec::new();
        let mut cursor = loc.parent_id.and_then(|p| self.by_id.get(&p));
        while let Some(p) = cursor {
            out.push(p);
            if out.len() > 16 {
                break;
            }
            cursor = p.parent_id.and_then(|id| self.by_id.get(&id));
        }
        out.reverse();
        out
    }

    /// Как в Битриксе: «Город, округ, регион, район, Страна» — сам узел,
    /// предки сверху вниз без страны, страна в конце.
    fn json(&self, loc: &Location, show_country: bool) -> Value {
        let ancestors = self.ancestors(loc);
        let find = |t: &str| {
            ancestors
                .iter()
                .find(|a| a.type_code == t)
                .map(|a| a.name.clone())
        };
        let mut parts = vec![loc.name.clone()];
        parts.extend(
            ancestors
                .iter()
                .filter(|a| a.type_code != "COUNTRY")
                .map(|a| a.name.clone()),
        );
        let country = find("COUNTRY");
        if show_country && let Some(c) = &country {
            parts.push(c.clone());
        }
        json!({
            "id": loc.id,
            "code": loc.code,
            "name": loc.name,
            "typeCode": loc.type_code,
            "displayName": parts.join(", "),
            "region": find("REGION").unwrap_or_default(),
            "country": country.unwrap_or_default(),
        })
    }
}

static TREE: OnceLock<RwLock<Option<Arc<Tree>>>> = OnceLock::new();

async fn tree(state: &AppState) -> Result<Arc<Tree>, BxError> {
    let cache = TREE.get_or_init(|| RwLock::new(None));
    if let Some(t) = cache.read().await.as_ref()
        && t.loaded_at.elapsed() < CACHE_TTL
    {
        return Ok(t.clone());
    }
    let mut guard = cache.write().await;
    if let Some(t) = guard.as_ref()
        && t.loaded_at.elapsed() < CACHE_TTL
    {
        return Ok(t.clone());
    }
    let rows: Vec<Location> =
        sqlx::query_as("SELECT id, code, parent_id, type_code, name FROM locations")
            .fetch_all(&state.db)
            .await?;
    let t = Arc::new(Tree {
        by_id: rows.into_iter().map(|l| (l.id, l)).collect(),
        loaded_at: Instant::now(),
    });
    *guard = Some(t.clone());
    Ok(t)
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: Option<String>,
    limit: Option<String>,
}

pub async fn search(State(state): State<AppState>, Query(params): Query<SearchQuery>) -> BxResult {
    let q = params.q.unwrap_or_default().trim().to_lowercase();
    if q.is_empty() {
        return Ok(success(json!({ "items": [] })));
    }
    let limit = params
        .limit
        .and_then(|l| l.trim().parse::<usize>().ok())
        .filter(|l| *l >= 1)
        .unwrap_or(DEFAULT_LIMIT)
        .min(MAX_LIMIT);
    let config = &state.project.location;
    let tree = tree(&state).await?;
    let mut found: Vec<(u8, &Location)> = tree
        .by_id
        .values()
        .filter(|l| config.search_types.contains(&l.type_code.as_str()))
        .filter_map(|l| {
            let name = l.name.to_lowercase();
            // Как в Битриксе — по началу названия; точное совпадение первым
            let rank = if name == q {
                0
            } else if name.starts_with(&q) {
                1
            } else {
                return None;
            };
            Some((rank, l))
        })
        .collect();
    found.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| collate_key(&a.1.name).cmp(&collate_key(&b.1.name)))
    });
    let items: Vec<Value> = found
        .into_iter()
        .take(limit)
        .map(|(_, l)| tree.json(l, config.show_country))
        .collect();
    Ok(success(json!({ "items": items })))
}

pub async fn set(State(state): State<AppState>, jar: CookieJar, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let id = number(body.get("id")).filter(|id| *id > 0).ok_or_else(|| {
        BxError::bad_request("location_id_required", "Не передан id местоположения")
    })?;
    let tree = tree(&state).await?;
    let loc = tree
        .by_id
        .get(&id)
        .ok_or_else(|| BxError::new("location_not_found", "Местоположение не найдено"))?;

    let user_id = current_user_id(&state, &jar).await?;
    if let Some(user_id) = user_id {
        sqlx::query("UPDATE users SET extra = extra || jsonb_build_object('location_id', $2::bigint) WHERE id = $1")
            .bind(user_id)
            .bind(id)
            .execute(&state.db)
            .await?;
    }
    let cookie = Cookie::build((COOKIE, id.to_string()))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.config.cookie_secure)
        .max_age(time::Duration::days(365))
        .build();
    let data = json!({
        "saved": true,
        "savedToUser": user_id.is_some(),
        "userField": state.project.location.user_field,
        "location": tree.json(loc, state.project.location.show_country),
    });
    Ok((jar.add(cookie), success(data)).into_response())
}

/// Выбор из профиля авторизованного, иначе из cookie.
pub async fn current(State(state): State<AppState>, jar: CookieJar) -> BxResult {
    let mut id = None;
    if let Some(user_id) = current_user_id(&state, &jar).await? {
        let saved: Option<(Option<Value>,)> =
            sqlx::query_as("SELECT extra -> 'location_id' FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_optional(&state.db)
                .await?;
        id = saved.and_then(|(v,)| v).and_then(|v| v.as_i64());
    }
    if id.is_none() {
        id = jar.get(COOKIE).and_then(|c| c.value().parse().ok());
    }
    let tree = tree(&state).await?;
    let location = id
        .and_then(|id| tree.by_id.get(&id))
        .map(|l| tree.json(l, state.project.location.show_country));
    Ok(success(json!({ "location": location })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(id: i64, parent: Option<i64>, t: &str, name: &str) -> Location {
        Location {
            id,
            code: id.to_string(),
            parent_id: parent,
            type_code: t.into(),
            name: name.into(),
        }
    }

    #[test]
    fn display_name() {
        let tree = Tree {
            by_id: [
                loc(1, None, "COUNTRY", "Россия"),
                loc(2, Some(1), "COUNTRY_DISTRICT", "Сибирь"),
                loc(3, Some(2), "REGION", "Новосибирская область"),
                loc(4, Some(3), "CITY", "Новосибирск"),
            ]
            .into_iter()
            .map(|l| (l.id, l))
            .collect(),
            loaded_at: Instant::now(),
        };
        let v = tree.json(&tree.by_id[&4], true);
        assert_eq!(
            v["displayName"],
            "Новосибирск, Сибирь, Новосибирская область, Россия"
        );
        assert_eq!(v["region"], "Новосибирская область");
        assert_eq!(v["country"], "Россия");
        let short = tree.json(&tree.by_id[&4], false);
        assert_eq!(
            short["displayName"],
            "Новосибирск, Сибирь, Новосибирская область"
        );
    }
}
