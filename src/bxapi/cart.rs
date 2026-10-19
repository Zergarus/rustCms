//! Корзина по контракту bxapi: `cart`, `cart/items`, `cart/items/{id}/quantity|store|remove`,
//! `cart/clear`, купоны `cart/coupon/add|remove`. Каждый ответ — полный снимок корзины
//! (цены — со скидками).

use std::collections::{HashMap, HashSet};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde_json::{Map, Value, json};
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
    collection::sku,
    discount::{
        context::{self, Context, OrderCtx},
        engine::{self, CouponStatus, DiscountResult},
        model::Coupon,
        repo as discount_repo,
    },
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

/// Товар можно положить в корзину: есть, активен, из торгового каталога, не товар с
/// предложениями (продаются предложения), а у предложения — активный родитель.
pub(super) fn ensure_purchasable(info: Option<&PurchaseInfo>) -> Result<&PurchaseInfo, BxError> {
    info.filter(|i| catalog::sellable(i)).ok_or_else(|| {
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
    image: Option<String>,
}

/// Склады с полями по настройкам проекта; UF-поля — строками, как отдаёт Битрикс.
async fn load_stores(state: &AppState) -> Result<Vec<StoreInfo>, BxError> {
    let rows: Vec<StoreRow> = sqlx::query_as(
        "SELECT s.id, s.name, s.active, s.address, s.phone, s.email, s.schedule, s.extra,
                '/upload/' || f.path AS image
         FROM catalog_stores s LEFT JOIN files f ON f.id = s.image_id
         ORDER BY s.sort, s.id",
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
                image: r.image,
            }
        })
        .collect())
}

/// Пустое предложение берёт у товара: адрес, картинку, артикул и значения свойств
/// (`null` или `""` — пусто). Название остаётся своим.
fn offer_fallback(view: &mut ProductView, parent: &ProductView) {
    let empty = |v: &Value| v.is_null() || v.as_str() == Some("");
    if view.slug.is_empty() {
        view.slug = parent.slug.clone();
    }
    if empty(&view.image) {
        view.image = parent.image.clone();
    }
    if empty(&view.article) {
        view.article = parent.article.clone();
    }
    for (key, value) in view.properties.iter_mut() {
        if empty(value)
            && let Some(own) = parent.properties.get(key)
        {
            *value = own.clone();
        }
    }
}

/// Отображение покупаемых записей: URL, картинка, артикул, свойства. У предложения
/// (`product_ids` — id предложений) пустое добирается у его товара.
pub(super) async fn product_views(
    state: &AppState,
    product_ids: &[i64],
) -> Result<HashMap<i64, ProductView>, BxError> {
    let mut views = record_views(state, product_ids).await?;
    // предложение → его товар (`collection_items.product_id`)
    let parents: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT id, product_id FROM collection_items WHERE id = ANY($1) AND product_id IS NOT NULL",
    )
    .bind(product_ids)
    .fetch_all(&state.db)
    .await?;
    if parents.is_empty() {
        return Ok(views);
    }
    let parent_ids: Vec<i64> = parents
        .iter()
        .map(|(_, p)| *p)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let parent_views = record_views(state, &parent_ids).await?;
    for (offer, parent) in parents {
        if let (Some(view), Some(parent)) = (views.get_mut(&offer), parent_views.get(&parent)) {
            offer_fallback(view, parent);
        }
    }
    Ok(views)
}

/// Отображение записей как они есть, без подстановок от товара.
async fn record_views(
    state: &AppState,
    product_ids: &[i64],
) -> Result<HashMap<i64, ProductView>, BxError> {
    let rows = load_rows(state, product_ids).await?;
    let snap = state.registry.snapshot(&state.db).await?;
    let config = &state.project.cart;
    let widths: Vec<u32> = config.image_width.into_iter().collect();
    let mut by_iblock: HashMap<i64, Vec<_>> = HashMap::new();
    for row in rows {
        by_iblock.entry(row.collection_id).or_default().push(row);
    }
    let mut out = HashMap::new();
    for (collection_id, rows) in by_iblock {
        let Some(schema) = snap.get(collection_id).cloned() else {
            continue;
        };
        let code = schema.collection.code.clone();
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
            user_id: None,
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

/// Корзина без цен: позиции, данные покупки, склады, отображение и купоны покупателя.
pub(super) struct CartBase {
    pub buyer: Option<i64>,
    user_id: Option<i64>,
    pub items: Vec<CartItem>,
    pub info: HashMap<i64, PurchaseInfo>,
    pub stores: Vec<StoreInfo>,
    views: HashMap<i64, ProductView>,
    coupons: Vec<Coupon>,
}

/// Корзина для оформления: позиции, данные покупки, склады, скидки и снимок.
pub(super) struct CartState {
    pub buyer: Option<i64>,
    pub items: Vec<CartItem>,
    pub info: HashMap<i64, PurchaseInfo>,
    pub stores: Vec<StoreInfo>,
    pub discounts: DiscountResult,
    /// Стоимость товаров снимка до скидок.
    pub goods_base: f64,
    /// Цена единицы до скидок у позиций, участвующих в расчёте скидок.
    pub bases: HashMap<i64, f64>,
    pub snapshot: Value,
}

/// Корзина текущего покупателя (по сессии или cookie гостя), скидки — без данных заказа.
pub(super) async fn cart_state(state: &AppState, jar: &CookieJar) -> Result<CartState, BxError> {
    let base = cart_base(state, jar).await?;
    priced(state, base, OrderCtx::default()).await
}

/// Корзина текущего покупателя без цен — чтобы выбрать доставку и оплату для расчёта скидок.
pub(super) async fn cart_base(state: &AppState, jar: &CookieJar) -> Result<CartBase, BxError> {
    let buyer = current_buyer(state, jar).await?;
    load_base(state, buyer).await
}

/// Снимок корзины покупателя (нет покупателя — пустая корзина).
async fn snapshot(state: &AppState, buyer: Option<i64>) -> Result<Value, BxError> {
    let base = load_base(state, buyer).await?;
    Ok(priced(state, base, OrderCtx::default()).await?.snapshot)
}

async fn load_base(state: &AppState, buyer: Option<i64>) -> Result<CartBase, BxError> {
    let (items, user_id, coupons) = match buyer {
        Some(b) => {
            let user_id: Option<i64> =
                sqlx::query_scalar("SELECT user_id FROM buyers WHERE id = $1")
                    .bind(b)
                    .fetch_optional(&state.db)
                    .await?
                    .flatten();
            (
                repo::items(&state.db, b).await?,
                user_id,
                discount_repo::buyer_coupons(&state.db, b).await?,
            )
        }
        None => (Vec::new(), None, Vec::new()),
    };
    let ids: Vec<i64> = items.iter().map(|i| i.product_id).collect();
    let info = catalog::load(&state.db, &ids).await?;
    let views = product_views(state, &ids).await?;
    let stores = load_stores(state).await?;
    Ok(CartBase {
        buyer,
        user_id,
        items,
        info,
        stores,
        views,
        coupons,
    })
}

/// Купон в ответе корзины: `status` — `JS_STATUS` компонента корзины Битрикса
/// (`APPLYED`/`ENTERED`/`BAD`), `text` — пояснение `DiscountCouponsManager`.
async fn coupon_list(
    state: &AppState,
    base: &CartBase,
    discounts: &DiscountResult,
) -> Result<Vec<Value>, BxError> {
    let now = chrono::Utc::now();
    let mut out = Vec::with_capacity(base.coupons.len());
    for (cp, (_, status)) in base.coupons.iter().zip(&discounts.coupons) {
        let (js, text) = match discount_repo::check_coupon(&state.db, cp, base.user_id, now).await?
        {
            Some(problem) => ("BAD", problem.text),
            None if *status == CouponStatus::Applied => ("APPLYED", "применен".into()),
            None => ("ENTERED", "активен и может быть использован".into()),
        };
        out.push(json!({"coupon": cp.coupon, "status": js, "text": text}));
    }
    Ok(out)
}

/// Записывает цены позиций открытой корзины после пересчёта — только изменившиеся.
async fn save_prices(
    state: &AppState,
    ctx: &Context,
    discounts: &DiscountResult,
) -> Result<(), BxError> {
    let mut ids = Vec::new();
    let mut bases = Vec::new();
    let mut amounts = Vec::new();
    let mut prices = Vec::new();
    for it in &ctx.items {
        if let Some(r) = discounts.items.get(&it.key) {
            ids.push(it.key);
            bases.push(it.base_price);
            amounts.push(r.discount);
            prices.push(r.price);
        }
    }
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE cart_items c
         SET base_price = v.base::numeric(18, 2), discount_price = v.amount::numeric(18, 2),
             price = v.price::numeric(18, 2)
         FROM unnest($1::int8[], $2::float8[], $3::float8[], $4::float8[]) AS v(id, base, amount, price)
         WHERE c.id = v.id AND c.order_id IS NULL
           AND (c.base_price IS DISTINCT FROM v.base::numeric(18, 2)
                OR c.discount_price <> v.amount::numeric(18, 2)
                OR c.price IS DISTINCT FROM v.price::numeric(18, 2))",
    )
    .bind(&ids)
    .bind(&bases)
    .bind(&amounts)
    .bind(&prices)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Расчёт скидок по корзине и данным заказа, снимок с ценами со скидкой и запись цен позиций.
pub(super) async fn priced(
    state: &AppState,
    base: CartBase,
    order: OrderCtx,
) -> Result<CartState, BxError> {
    let rules = discount_repo::active_rules(&state.db, chrono::Utc::now()).await?;
    let mut conn = state.db.acquire().await?;
    let ctx = context::for_cart(
        &mut conn,
        &base.items,
        &base.info,
        base.user_id,
        base.coupons.clone(),
        order,
    )
    .await?;
    let hook = state.project.discount_condition_hook.as_deref();
    let discounts = engine::calculate(&rules, &ctx, hook);
    drop(conn);
    if base.buyer.is_some() {
        save_prices(state, &ctx, &discounts).await?;
    }

    let prices: HashMap<i64, f64> = discounts
        .items
        .iter()
        .map(|(key, r)| (*key, r.price))
        .collect();
    let config = &state.project.cart;
    let snapshot_config = SnapshotConfig {
        items_count_positions: config.items_count_positions,
        required_store: config.required_props.contains(&"store"),
        currency: "RUB".into(),
    };
    let mut snapshot = build_snapshot(
        &base.items,
        &base.info,
        &base.views,
        &base.stores,
        &prices,
        &snapshot_config,
    );
    if !base.coupons.is_empty() {
        let list = coupon_list(state, &base, &discounts).await?;
        snapshot["coupons"] = Value::Array(list);
    }

    // До скидок: позиция контекста — по своей базовой цене, остальные — как в снимке
    let ctx_base: HashMap<i64, f64> = ctx.items.iter().map(|i| (i.key, i.base_price)).collect();
    let goods_base: f64 = base
        .items
        .iter()
        .map(|it| {
            let unit = ctx_base.get(&it.id).copied().unwrap_or_else(|| {
                base.info
                    .get(&it.product_id)
                    .and_then(|p| catalog::main_price(&p.prices))
                    .map_or(0.0, |p| p.price)
            });
            unit * it.quantity
        })
        // Не `sum()`: у пустого итератора `f64` он даёт `-0.0` («Стоимость товаров» `-0.0`)
        .fold(0.0, |acc, x| acc + x);
    Ok(CartState {
        buyer: base.buyer,
        items: base.items,
        info: base.info,
        stores: base.stores,
        discounts,
        goods_base: (goods_base * 100.0).round() / 100.0,
        bases: ctx_base,
        snapshot,
    })
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
            let name: String =
                sqlx::query_scalar("SELECT name FROM collection_items WHERE id = $1")
                    .bind(product_id)
                    .fetch_one(&state.db)
                    .await?;
            let props = sku::basket_props(&state.db, product_id).await?;
            repo::add(
                &state.db, buyer, product_id, store_id, quantity, &name, &props,
            )
            .await?;
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
        let infos = catalog::load(&state.db, &[item.product_id]).await?;
        let info = ensure_purchasable(infos.get(&item.product_id))?;
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
    let infos = catalog::load(&state.db, &[item.product_id]).await?;
    let info = ensure_purchasable(infos.get(&item.product_id))?;
    let stores = load_stores(&state).await?;
    ensure_store(info, &stores, store_id)?;
    // После смены склада позиции одного товара на этом складе сливаются
    let twin: f64 = repo::items(&state.db, buyer)
        .await?
        .iter()
        .filter(|i| {
            i.id != item.id && i.product_id == item.product_id && i.store_id == Some(store_id)
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

fn coupon_error(code: &str, check: discount_repo::CouponCheck) -> BxError {
    let err = match check.status {
        CouponStatus::NotFound => "coupon_not_found",
        CouponStatus::Expired => "coupon_expired",
        CouponStatus::Foreign => "coupon_foreign",
        CouponStatus::Used => "coupon_used",
        _ => "coupon_inactive",
    };
    BxError::bad_request(err, check.text.clone())
        .with_custom(json!({"coupon": code, "status": "BAD", "text": check.text}))
}

/// `cart/coupon/add {coupon}`: купон покупателю (`DiscountCouponsManager::add`); негодный —
/// ошибка с текстом Битрикса, годный — снимок корзины со списком купонов.
pub async fn add_coupon(State(state): State<AppState>, jar: CookieJar, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let code = body
        .get("coupon")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let user_id = current_user_id(&state, &jar).await?;
    // Негодный купон — ошибка до создания покупателя и cookie
    if let Err(check) = discount_repo::valid_coupon(&state.db, &code, user_id).await? {
        return Err(coupon_error(&code, check));
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
    if let Err(check) = discount_repo::add_buyer_coupon(&state.db, buyer, &code, user_id).await? {
        return Err(coupon_error(&code, check));
    }
    Ok((jar, success(snapshot(&state, Some(buyer)).await?)).into_response())
}

/// `cart/coupon/remove {coupon}`: убирает купон покупателя; ответ — снимок корзины.
pub async fn remove_coupon(State(state): State<AppState>, jar: CookieJar, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let code = body
        .get("coupon")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let buyer = current_buyer(&state, &jar).await?;
    if let Some(b) = buyer {
        discount_repo::remove_buyer_coupon(&state.db, b, code).await?;
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
            product_type: crate::catalog::TYPE_SIMPLE,
            parent_id: None,
            parent_active: true,
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

    #[test]
    fn purchasable_rejects_sku_parents() {
        let err = |r: Result<&PurchaseInfo, BxError>| {
            r.unwrap_err().code.as_str().unwrap_or_default().to_string()
        };
        for kind in [crate::catalog::TYPE_SKU, crate::catalog::TYPE_EMPTY_SKU] {
            let mut i = info(true, true);
            i.product_type = kind;
            assert_eq!(err(ensure_purchasable(Some(&i))), "product_not_found");
        }
        let mut offer = info(true, true);
        offer.product_type = crate::catalog::TYPE_OFFER;
        offer.parent_id = Some(1);
        offer.parent_active = false;
        assert_eq!(err(ensure_purchasable(Some(&offer))), "product_not_found");
        offer.parent_active = true;
        assert!(ensure_purchasable(Some(&offer)).is_ok());
    }

    #[test]
    fn offer_view_falls_back_to_product() {
        let view = |slug: &str, image: Value, article: Value, color: Value| ProductView {
            name: "Предложение".into(),
            slug: slug.into(),
            article,
            image,
            properties: Map::from_iter([("color".to_string(), color)]),
        };
        let parent = view(
            "/catalog/tovar/",
            json!("/p.jpg"),
            json!("A-1"),
            json!("red"),
        );
        let mut offer = view("", Value::Null, json!(""), Value::Null);
        offer_fallback(&mut offer, &parent);
        assert_eq!(offer.name, "Предложение");
        assert_eq!(offer.slug, "/catalog/tovar/");
        assert_eq!(offer.image, json!("/p.jpg"));
        assert_eq!(offer.article, json!("A-1"));
        assert_eq!(offer.properties["color"], json!("red"));

        // собственные значения предложения не затираются
        let mut own = view("/o/", json!("/o.jpg"), json!("O-1"), json!("blue"));
        offer_fallback(&mut own, &parent);
        assert_eq!(own.slug, "/o/");
        assert_eq!(own.image, json!("/o.jpg"));
        assert_eq!(own.article, json!("O-1"));
        assert_eq!(own.properties["color"], json!("blue"));
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        cookie: &str,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        use tower::ServiceExt;
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header(axum::http::header::COOKIE, cookie);
        if body.is_some() {
            req = req.header(axum::http::header::CONTENT_TYPE, "application/json");
        }
        let req = req
            .body(axum::body::Body::from(
                body.map(|b| b.to_string()).unwrap_or_default(),
            ))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    /// Без правил и купонов ответы bxapi — байт в байт как до скидок (эталон записан кодом до
    /// изменений): корзина, форма, итоги.
    #[sqlx::test]
    async fn cart_without_rules_is_byte_identical(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let (s1, cart) = send(&app, "GET", "/api/v1/cart", &fx.cookie, None).await;
        let (s2, form) = send(&app, "GET", "/api/v1/order/form", &fx.cookie, None).await;
        let (s3, summary) = send(
            &app,
            "POST",
            "/api/v1/order/summary",
            &fx.cookie,
            Some(json!({"deliveryId": 1, "paymentId": 1})),
        )
        .await;
        assert_eq!(
            (s1, s2, s3),
            (StatusCode::OK, StatusCode::OK, StatusCode::OK)
        );
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/discounts");
        let got = [
            ("cart.json", cart),
            ("form.json", form),
            ("summary.json", summary),
        ];
        for (name, body) in &got {
            let want = std::fs::read_to_string(dir.join(name)).unwrap();
            assert_eq!(body, want.trim_end_matches('\n'), "{name}");
        }
    }

    /// Правило «−`perc` % на все позиции» (и скидка `delivery` руб. на доставку).
    async fn insert_rule(db: &sqlx::PgPool, perc: f64, delivery: f64, use_coupons: bool) -> i64 {
        let mut acts = vec![json!({
            "CLASS_ID": "ActSaleBsktGrp",
            "DATA": {"Type": "Discount", "Value": perc, "Unit": "Perc", "Max": 0, "All": "AND", "True": "True"},
            "CHILDREN": []
        })];
        if delivery > 0.0 {
            acts.push(json!({"CLASS_ID": "ActSaleDelivery", "DATA": {"Type": "Discount", "Value": delivery, "Unit": "Cur"}}));
        }
        sqlx::query_scalar(
            "INSERT INTO discounts (name, use_coupons, actions) VALUES ('Скидка', $1, $2) RETURNING id",
        )
        .bind(use_coupons)
        .bind(json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": acts}))
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn insert_coupon(db: &sqlx::PgPool, rule: i64, code: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO discount_coupons (discount_id, coupon) VALUES ($1, $2) RETURNING id",
        )
        .bind(rule)
        .bind(code)
        .fetch_one(db)
        .await
        .unwrap()
    }

    fn data(body: &str) -> Value {
        let v: Value = serde_json::from_str(body).unwrap();
        v["data"].clone()
    }

    #[sqlx::test]
    async fn cart_prices_with_rule(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let (_, before) = send(&app, "GET", "/api/v1/cart", &fx.cookie, None).await;
        let before = data(&before);
        insert_rule(&db, 10.0, 0.0, false).await;
        let (status, body) = send(&app, "GET", "/api/v1/cart", &fx.cookie, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let cart = data(&body);
        assert_eq!(cart["items"][0]["price"], json!(900));
        assert_eq!(cart["items"][0]["sum"], json!(1800));
        assert_eq!(cart["items"][1]["price"], json!(450));
        assert_eq!(cart["items"][1]["sum"], json!(450));
        assert_eq!(cart["totalPrice"], json!(2250));
        assert!(cart.get("coupons").is_none(), "без купонов ключа нет");
        assert_ne!(cart["snapshot"], before["snapshot"]);
        assert_ne!(cart["items"][0]["snapshot"], before["items"][0]["snapshot"]);
        let row: (f64, f64, f64) = sqlx::query_as(
            "SELECT base_price::float8, discount_price::float8, price::float8 FROM cart_items WHERE id = $1",
        )
        .bind(fx.f.item_id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(row, (1000.0, 100.0, 900.0));
    }

    #[sqlx::test]
    async fn coupon_rule_applies_only_with_coupon(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let rule = insert_rule(&db, 10.0, 0.0, true).await;
        insert_coupon(&db, rule, "SALE10").await;
        let (_, body) = send(&app, "GET", "/api/v1/cart", &fx.cookie, None).await;
        let cart = data(&body);
        assert_eq!(
            cart["items"][0]["price"],
            json!(1000),
            "без купона правило не действует"
        );
        assert!(cart.get("coupons").is_none());

        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/cart/coupon/add",
            &fx.cookie,
            Some(json!({"coupon": "SALE10"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let cart = data(&body);
        assert_eq!(cart["items"][0]["price"], json!(900));
        assert_eq!(cart["totalPrice"], json!(2250));
        assert_eq!(
            cart["coupons"],
            json!([{"coupon": "SALE10", "status": "APPLYED", "text": "применен"}])
        );
        let (_, body) = send(&app, "GET", "/api/v1/cart", &fx.cookie, None).await;
        assert_eq!(
            data(&body)["items"][0]["price"],
            json!(900),
            "купон хранится у покупателя"
        );

        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/cart/coupon/remove",
            &fx.cookie,
            Some(json!({"coupon": "SALE10"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let cart = data(&body);
        assert_eq!(cart["items"][0]["price"], json!(1000));
        assert!(cart.get("coupons").is_none());
    }

    #[sqlx::test]
    async fn coupon_add_remove_via_bxapi(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let rule = insert_rule(&db, 10.0, 0.0, true).await;
        let off = insert_rule(&db, 5.0, 0.0, true).await;
        sqlx::query("UPDATE discounts SET active = FALSE WHERE id = $1")
            .bind(off)
            .execute(&db)
            .await
            .unwrap();
        insert_coupon(&db, rule, "OK1").await;
        let old = insert_coupon(&db, rule, "OLD").await;
        sqlx::query(
            "UPDATE discount_coupons SET active_to = now() - interval '1 day' WHERE id = $1",
        )
        .bind(old)
        .execute(&db)
        .await
        .unwrap();
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO users (login, password_hash) VALUES ('other', '') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let mine = insert_coupon(&db, rule, "MINE").await;
        sqlx::query("UPDATE discount_coupons SET user_id = $2 WHERE id = $1")
            .bind(mine)
            .bind(other)
            .execute(&db)
            .await
            .unwrap();
        insert_coupon(&db, off, "OFF").await;

        let add = |code: &'static str| {
            let app = app.clone();
            let cookie = fx.cookie.clone();
            async move {
                let (status, body) = send(
                    &app,
                    "POST",
                    "/api/v1/cart/coupon/add",
                    &cookie,
                    Some(json!({"coupon": code})),
                )
                .await;
                (status, serde_json::from_str::<Value>(&body).unwrap())
            }
        };
        for (code, err, text) in [
            ("NOPE", "coupon_not_found", "не найден"),
            (
                "OLD",
                "coupon_expired",
                "дата окончания активности купона раньше текущей",
            ),
            ("MINE", "coupon_foreign", "неверный владелец купона"),
            ("OFF", "coupon_inactive", "правило не активно"),
        ] {
            let (status, body) = add(code).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{code}: {body}");
            let e = &body["errors"][0];
            assert_eq!(e["code"], err, "{code}");
            assert_eq!(e["message"], text, "{code}");
            assert_eq!(
                e["customData"],
                json!({"coupon": code, "status": "BAD", "text": text}),
                "{code}"
            );
        }
        let (_, body) = send(&app, "GET", "/api/v1/cart", &fx.cookie, None).await;
        assert!(
            data(&body).get("coupons").is_none(),
            "неверные купоны не сохраняются"
        );

        let (status, body) = add("OK1").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = add("OK1").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["data"]["coupons"],
            json!([{"coupon": "OK1", "status": "APPLYED", "text": "применен"}]),
            "повторное добавление — без дубля"
        );
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM buyer_coupons WHERE buyer_id = $1")
                .bind(fx.f.buyer_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count, 1);

        // Купон истёк, пока лежал в корзине: виден со статусом и текстом, правило не действует
        sqlx::query("UPDATE discount_coupons SET active_to = now() - interval '1 hour' WHERE coupon = 'OK1'")
            .execute(&db)
            .await
            .unwrap();
        let (_, body) = send(&app, "GET", "/api/v1/cart", &fx.cookie, None).await;
        let cart = data(&body);
        assert_eq!(
            cart["coupons"],
            json!([{"coupon": "OK1", "status": "BAD", "text": "дата окончания активности купона раньше текущей"}])
        );
        assert_eq!(cart["items"][0]["price"], json!(1000));

        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/cart/coupon/remove",
            &fx.cookie,
            Some(json!({"coupon": "OK1"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(data(&body).get("coupons").is_none());
    }

    #[sqlx::test]
    async fn delivery_discount_in_summary(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        insert_rule(&db, 10.0, 100.0, false).await;
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/order/summary",
            &fx.cookie,
            Some(json!({"deliveryId": 1, "paymentId": 1})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            data(&body)["summary"],
            json!([
                {"label": "Стоимость товаров", "value": 2500.0, "format": "price"},
                {"label": "Скидка", "value": -250.0, "format": "price"},
                {"label": "Доставка", "value": 200.0, "format": "price"},
                {"label": "Итого", "value": 2450.0, "type": "total", "format": "price", "hint": "Счёт"},
            ])
        );
        let (status, body) = send(&app, "GET", "/api/v1/order/form", &fx.cookie, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let form = data(&body);
        assert_eq!(form["summary"][1]["label"], "Скидка");
        assert_eq!(
            form["summary"][2]["value"], 200.0,
            "первая доставка — со скидкой"
        );
        assert_eq!(form["basket"]["totalPrice"], json!(2250));
    }

    #[sqlx::test]
    async fn failed_coupon_add_creates_no_buyer(db: sqlx::PgPool) {
        let _fx = crate::test_support::cart_fixture(&db).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let buyers = || async {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM buyers")
                .fetch_one(&db)
                .await
                .unwrap()
        };
        let before = buyers().await;
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/cart/coupon/add",
            "",
            Some(json!({"coupon": "NOPE"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(buyers().await, before, "покупатель без cookie не создан");
    }

    #[sqlx::test]
    async fn coupon_code_is_case_insensitive(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let rule = insert_rule(&db, 10.0, 0.0, true).await;
        insert_coupon(&db, rule, "sale10").await;
        let dup =
            sqlx::query("INSERT INTO discount_coupons (discount_id, coupon) VALUES ($1, 'SALE10')")
                .bind(rule)
                .execute(&db)
                .await;
        assert!(dup.is_err(), "код уникален без учёта регистра");
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/cart/coupon/add",
            &fx.cookie,
            Some(json!({"coupon": "SALE10"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let cart = data(&body);
        assert_eq!(cart["items"][0]["price"], json!(900));
        assert_eq!(cart["coupons"][0]["coupon"], "sale10");
        let (_, body) = send(
            &app,
            "POST",
            "/api/v1/cart/coupon/remove",
            &fx.cookie,
            Some(json!({"coupon": "Sale10"})),
        )
        .await;
        assert!(data(&body).get("coupons").is_none());
    }

    #[sqlx::test]
    async fn guest_coupons_move_to_user_on_login(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let rule = insert_rule(&db, 10.0, 0.0, true).await;
        insert_coupon(&db, rule, "ALL").await;
        let personal = insert_coupon(&db, rule, "PERS").await;
        for code in ["ALL", "PERS"] {
            let (status, body) = send(
                &app,
                "POST",
                "/api/v1/cart/coupon/add",
                &fx.cookie,
                Some(json!({"coupon": code})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{code}: {body}");
        }
        // Пока купон лежал в корзине гостя, его сделали личным купоном другого пользователя
        let (other, _) = crate::test_support::user_cookie(&db, "other", false).await;
        sqlx::query("UPDATE discount_coupons SET user_id = $2 WHERE id = $1")
            .bind(personal)
            .bind(other)
            .execute(&db)
            .await
            .unwrap();
        crate::test_support::user_cookie(&db, "buyer", false).await;
        // Вход с cookie гостя: корзина и купоны переходят пользователю
        let session = {
            use tower::ServiceExt;
            let req = axum::http::Request::post("/api/v1/auth/login")
                .header(axum::http::header::COOKIE, &fx.cookie)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    json!({"login": "buyer", "password": "password123"}).to_string(),
                ))
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            res.headers()
                .get_all(axum::http::header::SET_COOKIE)
                .iter()
                .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
                .find(|c| c.starts_with("CMS_SID="))
                .unwrap()
        };
        let guests: i64 = sqlx::query_scalar("SELECT count(*) FROM buyers WHERE id = $1")
            .bind(fx.f.buyer_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(guests, 0, "покупатель-гость удалён при входе");

        let (status, body) = send(&app, "GET", "/api/v1/cart", &session, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let cart = data(&body);
        assert_eq!(
            cart["coupons"],
            json!([
                {"coupon": "ALL", "status": "APPLYED", "text": "применен"},
                {"coupon": "PERS", "status": "BAD", "text": "неверный владелец купона"},
            ])
        );
        assert_eq!(cart["items"][0]["price"], json!(900));
    }
}
