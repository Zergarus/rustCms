//! Корзина по контракту bxapi: `cart`, `cart/items`, `cart/items/{id}/quantity|store|remove`,
//! `cart/clear`. Каждый ответ — полный снимок корзины.

use std::collections::{HashMap, HashSet};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde_json::{Map, Value};
use sqlx::FromRow;

use super::{
    BxError, BxResult,
    auth::current_user_id,
    parse_body,
    query::{Select, number},
    serialize::{Env, Mode, load_rows, serialize},
    success,
};
use crate::{
    cart::{
        CartItem, ProductView, SnapshotConfig, StoreInfo,
        repo::{self, AddTarget, Owner},
        snapshot::build_snapshot,
    },
    catalog::{self, PurchaseInfo, check_quantity},
    state::AppState,
};

pub const BUYER_COOKIE: &str = "CMS_BUYER";

fn invalid_quantity() -> BxError {
    BxError::bad_request(
        "invalid_quantity",
        "Количество должно быть целым положительным числом",
    )
}

fn change_failed(message: impl Into<String>) -> BxError {
    BxError::bad_request("cart_change_failed", message)
}

fn item_not_found() -> BxError {
    BxError::with_status(
        StatusCode::NOT_FOUND,
        "cart_item_not_found",
        "Позиция корзины не найдена",
    )
}

/// Количество — целое число; `0` допустим только при изменении (удаляет позицию).
pub(super) fn parse_quantity(v: Option<&Value>, allow_zero: bool) -> Result<f64, BxError> {
    let n: i64 = match v {
        Some(Value::Number(n)) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => i,
            (None, Some(f)) if f.fract() == 0.0 => f as i64,
            _ => return Err(invalid_quantity()),
        },
        Some(Value::String(s)) => s.trim().parse().map_err(|_| invalid_quantity())?,
        _ => return Err(invalid_quantity()),
    };
    if n < 0 || (n == 0 && !allow_zero) {
        return Err(invalid_quantity());
    }
    Ok(n as f64)
}

/// Товар можно положить в корзину: есть, активен, из торгового каталога.
pub(super) fn ensure_purchasable(info: Option<&PurchaseInfo>) -> Result<&PurchaseInfo, BxError> {
    info.filter(|i| i.active && i.is_catalog).ok_or_else(|| {
        BxError::with_status(
            StatusCode::NOT_FOUND,
            "product_not_found",
            "Товар не найден",
        )
    })
}

/// Склад можно выбрать: активный и с записью остатка товара.
fn ensure_store(info: &PurchaseInfo, stores: &[StoreInfo], store_id: i64) -> Result<(), BxError> {
    let active = stores.iter().any(|s| s.id == store_id && s.active);
    if active && info.amounts.contains_key(&store_id) {
        Ok(())
    } else {
        Err(change_failed(format!("Склад {store_id} недоступен")))
    }
}

async fn owner(state: &AppState, jar: &CookieJar) -> Result<Option<Owner>, BxError> {
    if let Some(user_id) = current_user_id(state, jar).await? {
        return Ok(Some(Owner::User(user_id)));
    }
    Ok(jar
        .get(BUYER_COOKIE)
        .map(|c| c.value().trim().to_string())
        .filter(|t| !t.is_empty())
        .map(Owner::Guest))
}

async fn current_buyer(state: &AppState, jar: &CookieJar) -> Result<Option<i64>, BxError> {
    match owner(state, jar).await? {
        Some(o) => Ok(repo::find_buyer(&state.db, &o).await?),
        None => Ok(None),
    }
}

#[derive(FromRow)]
struct StoreRow {
    id: i64,
    name: String,
    active: bool,
    address: String,
    phone: String,
    email: String,
    schedule: String,
    extra: sqlx::types::Json<Map<String, Value>>,
}

/// Склады с полями по настройкам проекта; UF-поля — строками, как отдаёт Битрикс.
async fn load_stores(state: &AppState) -> Result<Vec<StoreInfo>, BxError> {
    let rows: Vec<StoreRow> = sqlx::query_as(
        "SELECT id, name, active, address, phone, email, schedule, extra FROM catalog_stores ORDER BY sort, id",
    )
    .fetch_all(&state.db)
    .await?;
    let config = &state.project.cart;
    Ok(rows
        .into_iter()
        .map(|r| {
            let mut fields = Map::new();
            for f in &config.store_fields {
                let v = match *f {
                    "address" => &r.address,
                    "phone" => &r.phone,
                    "email" => &r.email,
                    "schedule" => &r.schedule,
                    _ => continue,
                };
                fields.insert(f.to_string(), Value::from(v.clone()));
            }
            for (uf, key) in &config.store_user_fields {
                let v = match r.extra.get(*uf) {
                    Some(Value::String(s)) => Value::from(s.clone()),
                    Some(Value::Null) | None => Value::Null,
                    Some(other) => Value::from(other.to_string()),
                };
                fields.insert(key.to_string(), v);
            }
            StoreInfo {
                id: r.id,
                name: r.name,
                active: r.active,
                fields,
            }
        })
        .collect())
}

/// Отображение товаров позиций: URL, картинка, артикул, свойства — через сериализацию элементов.
async fn product_views(
    state: &AppState,
    element_ids: &[i64],
) -> Result<HashMap<i64, ProductView>, BxError> {
    let rows = load_rows(state, element_ids).await?;
    let snap = state.registry.snapshot(&state.db).await?;
    let config = &state.project.cart;
    let widths: Vec<u32> = config.image_width.into_iter().collect();
    let mut by_iblock: HashMap<i64, Vec<_>> = HashMap::new();
    for row in rows {
        by_iblock.entry(row.iblock_id).or_default().push(row);
    }
    let mut out = HashMap::new();
    for (iblock_id, rows) in by_iblock {
        let Some(schema) = snap.get(iblock_id).cloned() else {
            continue;
        };
        let code = schema.iblock.code.clone();
        let mut select = Select::default();
        for path in ["detailPageUrl", "image"] {
            select.add(path, &state.project, &code);
        }
        if let Some(article) = config.article_property {
            select.add(article, &state.project, &code);
        }
        for (path, _) in &config.item_properties {
            select.add(path, &state.project, &code);
        }
        let env = Env {
            state,
            snap: &snap,
            project: &state.project,
            image_resize: (!widths.is_empty()).then_some(widths.as_slice()),
        };
        let items = serialize(&env, &schema, &rows, &select, Mode::Related, 0).await?;
        for (row, item) in rows.iter().zip(items) {
            let image = match item.get("image") {
                // С шириной — путь уменьшенной копии строкой
                Some(Value::Object(o)) => o
                    .get("resized")
                    .and_then(|r| r.get(0))
                    .and_then(|r| r.get("path"))
                    .cloned()
                    .unwrap_or(Value::Null),
                Some(v) => v.clone(),
                None => Value::Null,
            };
            let article = config
                .article_property
                .and_then(|a| item.get(a.split('.').next().unwrap_or(a)))
                .cloned()
                .filter(|v| !v.is_null() && v.as_str() != Some(""))
                .unwrap_or(Value::Null);
            let mut properties = Map::new();
            for (path, alias) in &config.item_properties {
                let root = path.split('.').next().unwrap_or(path);
                let key = alias.unwrap_or(root);
                properties.insert(
                    key.to_string(),
                    item.get(root).cloned().unwrap_or(Value::Null),
                );
            }
            let slug = item
                .get("detailPageUrl")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            out.insert(
                row.id,
                ProductView {
                    name: row.name.clone(),
                    slug,
                    article,
                    image,
                    properties,
                },
            );
        }
    }
    Ok(out)
}

/// Снимок корзины покупателя (нет покупателя — пустая корзина).
async fn snapshot(state: &AppState, buyer: Option<i64>) -> Result<Value, BxError> {
    let items: Vec<CartItem> = match buyer {
        Some(b) => repo::items(&state.db, b).await?,
        None => Vec::new(),
    };
    let ids: Vec<i64> = items.iter().map(|i| i.element_id).collect();
    let info = catalog::load(&state.db, &ids).await?;
    let views = product_views(state, &ids).await?;
    let stores = load_stores(state).await?;
    let config = &state.project.cart;
    let snapshot_config = SnapshotConfig {
        items_count_positions: config.items_count_positions,
        required_store: config.required_props.contains(&"store"),
        currency: "RUB".into(),
    };
    Ok(build_snapshot(
        &items,
        &info,
        &views,
        &stores,
        &snapshot_config,
    ))
}

pub async fn get_cart(State(state): State<AppState>, jar: CookieJar) -> BxResult {
    let buyer = current_buyer(&state, &jar).await?;
    Ok(success(snapshot(&state, buyer).await?))
}

fn buyer_cookie(state: &AppState, token: String) -> Cookie<'static> {
    Cookie::build((BUYER_COOKIE, token))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.config.cookie_secure)
        .max_age(time::Duration::days(365))
        .build()
}

pub async fn add_item(State(state): State<AppState>, jar: CookieJar, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let product_id = number(body.get("productId"))
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            BxError::with_status(
                StatusCode::NOT_FOUND,
                "product_not_found",
                "Товар не найден",
            )
        })?;
    let quantity = parse_quantity(body.get("quantity").or(Some(&Value::from(1))), false)?;
    let store_id = number(body.get("storeId")).filter(|id| *id > 0);

    let infos = catalog::load(&state.db, &[product_id]).await?;
    let info = ensure_purchasable(infos.get(&product_id))?;
    let stores = load_stores(&state).await?;
    if let Some(store) = store_id {
        ensure_store(info, &stores, store)?;
    }

    let (owner, jar) = match owner(&state, &jar).await? {
        Some(o) => (o, jar),
        None => {
            let token = hex::encode(rand::random::<[u8; 32]>());
            let jar = jar.add(buyer_cookie(&state, token.clone()));
            (Owner::Guest(token), jar)
        }
    };
    let buyer = repo::ensure_buyer(&state.db, &owner).await?;
    let items = repo::items(&state.db, buyer).await?;
    let active: HashSet<i64> = stores.iter().filter(|s| s.active).map(|s| s.id).collect();
    match repo::add_target(&items, product_id, store_id, &active) {
        AddTarget::Existing { item_id, set_store } => {
            let item = items
                .iter()
                .find(|i| i.id == item_id)
                .ok_or_else(item_not_found)?;
            // Склад позиции после добавления; деактивированный — как без склада
            let store = set_store.or(item.store_id.filter(|s| active.contains(s)));
            check_quantity(info, store, item.quantity + quantity).map_err(change_failed)?;
            repo::add_to_item(&state.db, buyer, item_id, quantity, set_store).await?;
        }
        AddTarget::New => {
            check_quantity(info, store_id, quantity).map_err(change_failed)?;
            let name: String = sqlx::query_scalar("SELECT name FROM iblock_elements WHERE id = $1")
                .bind(product_id)
                .fetch_one(&state.db)
                .await?;
            repo::add(&state.db, buyer, product_id, store_id, quantity, &name).await?;
        }
    }
    Ok((jar, success(snapshot(&state, Some(buyer)).await?)).into_response())
}

/// Покупатель и позиция из запроса; нет — 404.
async fn buyer_item(
    state: &AppState,
    jar: &CookieJar,
    item_id: i64,
) -> Result<(i64, CartItem), BxError> {
    let buyer = current_buyer(state, jar)
        .await?
        .ok_or_else(item_not_found)?;
    let item = repo::item(&state.db, buyer, item_id)
        .await?
        .ok_or_else(item_not_found)?;
    Ok((buyer, item))
}

pub async fn set_quantity(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(item_id): Path<i64>,
    body: Bytes,
) -> BxResult {
    let body = parse_body(&body)?;
    let (buyer, item) = buyer_item(&state, &jar, item_id).await?;
    let quantity = parse_quantity(body.get("quantity"), true)?;
    if quantity == 0.0 {
        repo::remove(&state.db, buyer, item_id).await?;
    } else {
        let infos = catalog::load(&state.db, &[item.element_id]).await?;
        let info = ensure_purchasable(infos.get(&item.element_id))?;
        check_quantity(info, item.store_id, quantity).map_err(change_failed)?;
        repo::set_quantity(&state.db, buyer, item_id, quantity).await?;
    }
    Ok(success(snapshot(&state, Some(buyer)).await?))
}

pub async fn set_store(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(item_id): Path<i64>,
    body: Bytes,
) -> BxResult {
    let body = parse_body(&body)?;
    let (buyer, item) = buyer_item(&state, &jar, item_id).await?;
    let store_id = number(body.get("storeId"))
        .filter(|id| *id > 0)
        .ok_or_else(|| change_failed("Не указан склад"))?;
    let infos = catalog::load(&state.db, &[item.element_id]).await?;
    let info = ensure_purchasable(infos.get(&item.element_id))?;
    let stores = load_stores(&state).await?;
    ensure_store(info, &stores, store_id)?;
    // После смены склада позиции одного товара на этом складе сливаются
    let twin: f64 = repo::items(&state.db, buyer)
        .await?
        .iter()
        .filter(|i| {
            i.id != item.id && i.element_id == item.element_id && i.store_id == Some(store_id)
        })
        .map(|i| i.quantity)
        .sum();
    check_quantity(info, Some(store_id), item.quantity + twin).map_err(change_failed)?;
    repo::set_store(&state.db, buyer, item_id, store_id).await?;
    Ok(success(snapshot(&state, Some(buyer)).await?))
}

pub async fn remove_item(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(item_id): Path<i64>,
) -> BxResult {
    let (buyer, _) = buyer_item(&state, &jar, item_id).await?;
    repo::remove(&state.db, buyer, item_id).await?;
    Ok(success(snapshot(&state, Some(buyer)).await?))
}

pub async fn clear(State(state): State<AppState>, jar: CookieJar) -> BxResult {
    let buyer = current_buyer(&state, &jar).await?;
    if let Some(b) = buyer {
        repo::clear(&state.db, b).await?;
    }
    Ok(success(snapshot(&state, buyer).await?))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::catalog::{Price, PurchaseInfo};

    fn code(r: Result<f64, BxError>) -> String {
        r.unwrap_err().code.as_str().unwrap_or_default().to_string()
    }

    #[test]
    fn quantity_parsing() {
        assert_eq!(parse_quantity(Some(&json!(1)), false).unwrap(), 1.0);
        assert_eq!(parse_quantity(Some(&json!("2")), false).unwrap(), 2.0);
        assert_eq!(parse_quantity(Some(&json!(0)), true).unwrap(), 0.0);
        for bad in [json!(-1), json!("abc"), json!(1.5), json!(null)] {
            assert_eq!(
                code(parse_quantity(Some(&bad), true)),
                "invalid_quantity",
                "{bad}"
            );
        }
        assert_eq!(code(parse_quantity(None, true)), "invalid_quantity");
    }

    #[test]
    fn add_requires_positive() {
        assert_eq!(
            code(parse_quantity(Some(&json!(0)), false)),
            "invalid_quantity"
        );
    }

    fn info(active: bool, is_catalog: bool) -> PurchaseInfo {
        PurchaseInfo {
            active,
            is_catalog,
            available: true,
            quantity_trace: true,
            can_buy_zero: false,
            prices: vec![Price {
                type_id: 1,
                type_name: String::new(),
                is_base: true,
                price: 10.0,
                currency: "RUB".into(),
                quantity_from: None,
                quantity_to: None,
            }],
            amounts: HashMap::new(),
            total: 0.0,
        }
    }

    #[test]
    fn purchasable_checks() {
        let err = |r: Result<&PurchaseInfo, BxError>| {
            r.unwrap_err().code.as_str().unwrap_or_default().to_string()
        };
        assert_eq!(err(ensure_purchasable(None)), "product_not_found");
        assert_eq!(
            err(ensure_purchasable(Some(&info(false, true)))),
            "product_not_found"
        );
        assert_eq!(
            err(ensure_purchasable(Some(&info(true, false)))),
            "product_not_found"
        );
        assert!(ensure_purchasable(Some(&info(true, true))).is_ok());
    }
}
