//! Оформление заказа по контракту bxapi: `order/form`, `order/summary`, `order/submit`.

use std::collections::{HashMap, HashSet};

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
};
use axum_extra::extract::cookie::CookieJar;
use serde_json::{Value, json};

use super::{
    BxError, BxResult,
    auth::current_user_id,
    cart::{CartBase, CartState, cart_base, cart_state, priced},
    parse_body,
    query::number,
    success,
};
use crate::{
    cart::CartItem,
    catalog,
    discount::{
        context::{self, OrderCtx},
        engine::{self, Applied, CouponStatus, ItemResult},
        repo as discount_repo,
    },
    sale::{
        self, Delivery, SaleSettings,
        form::{self, Profile},
        guest,
        repo::{self, NewOrder},
        stock::{self, StockLine},
        validate::{
            SubmitError, parse_selection, pick_items, validate_properties, with_profile_defaults,
        },
    },
    state::AppState,
};

const SUBMIT_TEXT: &str = "Оформить заказ";

/// Ошибка оформления в ответ bxapi; `basket_changed` несёт актуальную корзину.
fn to_bx(e: SubmitError, basket: Option<Value>) -> BxError {
    let status = match e.code {
        "payment_forbidden" => StatusCode::FORBIDDEN,
        "basket_changed" => StatusCode::CONFLICT,
        "user_provision_failed" | "order_write_exception" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    let custom = match (&e.field, e.code) {
        (Some(field), _) => json!({ "field": field }),
        (None, "basket_changed") => json!({ "basket": basket.unwrap_or(Value::Null) }),
        _ => Value::Null,
    };
    BxError::with_status(status, e.code, e.message).with_custom(custom)
}

/// Доставки, разрешённые фильтром проекта для позиций (нет фильтра — `None`).
fn allowed_deliveries(
    state: &AppState,
    settings: &SaleSettings,
    items: &[CartItem],
    info: &HashMap<i64, catalog::PurchaseInfo>,
) -> Option<HashSet<i64>> {
    state
        .project
        .order
        .delivery_filter
        .as_ref()
        .map(|f| f.allowed(&settings.deliveries, items, info))
}

/// Итоги по корзине со скидками: товары до скидок, скидка на товары, доставка со скидкой.
fn cart_summary(cart: &CartState, delivery_price: f64, payment_name: Option<&str>) -> Vec<Value> {
    let goods = goods_total(&cart.snapshot);
    let discount = ((cart.goods_base - goods) * 100.0).round() / 100.0;
    let delivery = cart.discounts.delivery_price.unwrap_or(delivery_price);
    form::summary(cart.goods_base, discount, delivery, payment_name)
}

/// Скидки с данными заказа: тип плательщика, оплата, доставка и её цена.
async fn priced_for(
    state: &AppState,
    base: CartBase,
    person_type: Option<i64>,
    payment: Option<i64>,
    delivery: Option<(i64, f64)>,
) -> Result<CartState, BxError> {
    let order = OrderCtx {
        person_type_id: person_type,
        pay_system_id: payment,
        delivery_id: delivery.map(|d| d.0),
        delivery_price: delivery.map(|d| d.1),
    };
    priced(state, base, order).await
}

async fn profile(state: &AppState, user_id: Option<i64>) -> Result<Option<Profile>, BxError> {
    let Some(id) = user_id else { return Ok(None) };
    let row: Option<(Option<String>, String, String)> = sqlx::query_as(
        "SELECT email, phone, trim(concat_ws(' ', NULLIF(last_name, ''), NULLIF(name, ''), NULLIF(second_name, '')))
         FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|(email, phone, full_name)| Profile {
        email: email.unwrap_or_default(),
        phone,
        full_name,
    }))
}

fn goods_total(basket: &Value) -> f64 {
    basket
        .get("totalPrice")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

/// Сумма позиций `ids` снимка корзины.
fn selected_sum(basket: &Value, ids: &[i64]) -> f64 {
    let sum = basket["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|it| it["id"].as_i64().is_some_and(|id| ids.contains(&id)))
        .filter_map(|it| it["sum"].as_f64())
        .sum::<f64>();
    (sum * 100.0).round() / 100.0
}

fn currency(basket: &Value) -> String {
    basket
        .get("currency")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .unwrap_or("RUB")
        .to_string()
}

pub async fn form(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(query): Query<HashMap<String, String>>,
) -> BxResult {
    let settings = sale::load_settings(&state.db).await?;
    let cfg = &state.project.order;
    let base = cart_base(&state, &jar).await?;
    let user_id = current_user_id(&state, &jar).await?;
    let types = form::person_types(&settings, cfg);
    let Some(person_type) =
        form::select_person_type(&types, query.get("personType").map(String::as_str))
    else {
        return Err(BxError::bad_request(
            "invalid_person_type",
            "Нет доступных типов плательщика",
        ));
    };
    let profile = profile(&state, user_id).await?;
    let allowed = allowed_deliveries(&state, &settings, &base.items, &base.info);
    let deliveries = form::deliveries(&settings, cfg, allowed.as_ref(), &base.stores);
    let payments = form::payments(&settings, cfg);
    let delivery_price = deliveries
        .first()
        .and_then(|d| d["price"].as_f64())
        .unwrap_or(0.0);
    let delivery = deliveries
        .first()
        .and_then(|d| d["id"].as_i64())
        .map(|id| (id, delivery_price));
    let payment_name = payments.first().and_then(|p| p["name"].as_str());
    let payment_id = payments.first().and_then(|p| p["id"].as_i64());
    let cart = priced_for(&state, base, Some(person_type), payment_id, delivery).await?;
    let summary = cart_summary(&cart, delivery_price, payment_name);
    Ok(success(json!({
        "personTypes": types
            .iter()
            .map(|p| json!({ "id": p.id, "code": p.code, "name": p.name }))
            .collect::<Vec<_>>(),
        "selectedPersonType": person_type,
        "blocks": form::blocks(&settings, person_type, cfg, profile.as_ref()),
        "deliveries": deliveries,
        "payments": payments,
        "summary": summary,
        "submitButtonText": SUBMIT_TEXT,
        "currency": currency(&cart.snapshot),
        "basket": cart.snapshot,
    })))
}

pub async fn summary(State(state): State<AppState>, jar: CookieJar, body: Bytes) -> BxResult {
    let body = parse_body(&body)?;
    let settings = sale::load_settings(&state.db).await?;
    let cfg = &state.project.order;
    let base = cart_base(&state, &jar).await?;
    let allowed = allowed_deliveries(&state, &settings, &base.items, &base.info);
    let delivery = number(body.get("deliveryId"))
        .and_then(|id| form::find_delivery(&settings, cfg, allowed.as_ref(), id));
    let delivery_price = delivery.map_or(0.0, |d| d.price);
    let payment =
        number(body.get("paymentId")).and_then(|id| form::find_payment(&settings, cfg, id));
    let payment_name = payment.map(|p| p.name.as_str());
    let person_type = number(body.get("personTypeId"));
    let cart = priced_for(
        &state,
        base,
        person_type,
        payment.map(|p| p.id),
        delivery.map(|d| (d.id, d.price)),
    )
    .await?;
    Ok(success(json!({
        "summary": cart_summary(&cart, delivery_price, payment_name),
        "submitButtonText": SUBMIT_TEXT,
    })))
}

/// Склад отгрузки: общий склад позиций, если он среди складов самовывоза службы.
fn shipment_store(delivery: &Delivery, items: &[&CartItem]) -> Option<i64> {
    let stores: HashSet<Option<i64>> = items.iter().map(|i| i.store_id).collect();
    match stores.into_iter().collect::<Vec<_>>().as_slice() {
        [Some(store)] if delivery.store_ids.contains(store) => Some(*store),
        _ => None,
    }
}

pub async fn submit(State(state): State<AppState>, jar: CookieJar, body: Bytes) -> BxResult {
    let body = Value::Object(parse_body(&body)?);
    let settings = sale::load_settings(&state.db).await?;
    let cfg = &state.project.order;
    let cart = cart_state(&state, &jar).await?;
    let basket = cart.snapshot.clone();
    let fail = |e: SubmitError| to_bx(e, Some(basket.clone()));
    let user_id = current_user_id(&state, &jar).await?;

    // 1. Тип плательщика
    let types = form::person_types(&settings, cfg);
    let person_type = number(body.get("personTypeId"))
        .filter(|id| types.iter().any(|t| t.id == *id))
        .ok_or_else(|| {
            fail(SubmitError::new(
                "invalid_person_type",
                "Неверный тип плательщика",
            ))
        })?;

    // Состав нужен фильтру доставок; его ошибки — после проверки доставки и оплаты
    let picked = parse_selection(&body).and_then(|sel| pick_items(&sel, &basket));
    let selected: Vec<&CartItem> = match &picked {
        Ok(ids) => cart.items.iter().filter(|i| ids.contains(&i.id)).collect(),
        Err(_) => cart.items.iter().collect(),
    };
    let selected_owned: Vec<CartItem> = selected.iter().map(|i| (*i).clone()).collect();

    // 2. Доставка
    let allowed = allowed_deliveries(&state, &settings, &selected_owned, &cart.info);
    let delivery = number(body.get("deliveryId"))
        .and_then(|id| form::find_delivery(&settings, cfg, allowed.as_ref(), id))
        .ok_or_else(|| fail(SubmitError::new("invalid_delivery", "Нет такой доставки")))?;

    // 3. Оплата
    let payment = number(body.get("paymentId"))
        .and_then(|id| form::find_payment(&settings, cfg, id))
        .ok_or_else(|| {
            fail(SubmitError::new(
                "invalid_payment",
                "Нет такой платёжной системы",
            ))
        })?;
    if !payment.group_ids.is_empty() {
        let groups: Vec<i64> = match user_id {
            Some(id) => {
                sqlx::query_scalar("SELECT group_id FROM user_groups WHERE user_id = $1")
                    .bind(id)
                    .fetch_all(&state.db)
                    .await?
            }
            None => Vec::new(),
        };
        if !payment.group_ids.iter().any(|g| groups.contains(g)) {
            return Err(fail(SubmitError::new(
                "payment_forbidden",
                "Платёжная система вам недоступна",
            )));
        }
    }

    // 4. Свойства — только те, что относятся к выбранным оплате и доставке
    let props: Vec<_> = form::visible_properties(&settings, person_type, cfg)
        .into_iter()
        .filter(|p| settings.property_applies(p.id, payment.id, delivery.id))
        .collect();
    let mut input = body
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(profile) = profile(&state, user_id).await? {
        with_profile_defaults(&props, &mut input, &profile);
    }
    let known = repo::known_locations(&state.db, &input).await?;
    let values = validate_properties(&props, &settings.variants, &input, &known).map_err(fail)?;

    // 5. Состав
    let ids = picked.map_err(fail)?;
    let cart_buyer = cart
        .buyer
        .ok_or_else(|| fail(SubmitError::new("empty_basket", "Корзина пуста")))?;
    let snapshot_items = basket["items"].as_array().cloned().unwrap_or_default();
    let mut order_items = Vec::new();
    let mut ordered = Vec::new();
    let mut lines = Vec::new();
    for item in selected.iter().filter(|i| ids.contains(&i.id)) {
        let snap = snapshot_items
            .iter()
            .find(|s| s["id"].as_i64() == Some(item.id))
            .ok_or_else(|| fail(SubmitError::basket_changed()))?;
        let price = snap["price"].as_f64().unwrap_or(0.0);
        let name = snap["name"].as_str().unwrap_or(&item.name).to_string();
        order_items.push(repo::NewItem::plain(item.id, item.quantity, price, name));
        ordered.push((*item).clone());
        lines.push(StockLine {
            product_id: item.product_id,
            store_id: item.store_id,
            quantity: item.quantity,
        });
    }
    let status = settings
        .default_status()
        .map(|s| s.code.clone())
        .ok_or_else(|| {
            fail(SubmitError::new(
                "order_write_exception",
                "Не настроены статусы заказа",
            ))
        })?;

    // 6. Покупатель
    let buyer = match user_id {
        Some(id) => id,
        None => {
            let contacts = guest::guest_contacts(&props, &values);
            let provision = SubmitError::new(
                "user_provision_failed",
                "Не удалось найти или создать пользователя",
            );
            guest::find_or_create(&state.db, &contacts, &cfg.guest_group_ids)
                .await
                .map_err(|e| {
                    tracing::warn!(error = %e, "оформление: пользователь гостя не найден и не создан");
                    fail(provision)
                })?
        }
    };

    let product_ids: Vec<i64> = lines.iter().map(|l| l.product_id).collect();
    // Правила — до транзакции: в ней чтения только через её соединение
    let rules = discount_repo::active_rules(&state.db, chrono::Utc::now()).await?;
    // Стоимость выбранных товаров, которую покупатель видел в итогах (с данными заказа)
    let shown = if rules.is_empty() {
        None
    } else {
        let base = cart_base(&state, &jar).await?;
        let seen = priced_for(
            &state,
            base,
            Some(person_type),
            Some(payment.id),
            Some((delivery.id, delivery.price)),
        )
        .await?;
        Some(selected_sum(&seen.snapshot, &ids))
    };
    let mut tx = state.db.begin().await?;
    let info = catalog::load_locked(&mut tx, &product_ids).await?;
    let checked = match lines
        .iter()
        .find(|l| !info.get(&l.product_id).is_some_and(catalog::sellable))
    {
        Some(_) => Err("Товар недоступен для покупки".to_string()),
        None => stock::check(&lines, &info),
    };
    if let Err(message) = checked {
        tx.rollback().await?;
        let mut e = SubmitError::basket_changed();
        e.message = message;
        return Err(fail(e));
    }
    let traced: HashSet<i64> = info
        .iter()
        .filter(|(_, i)| i.quantity_trace)
        .map(|(id, _)| *id)
        .collect();
    let order_ctx = OrderCtx {
        person_type_id: Some(person_type),
        pay_system_id: Some(payment.id),
        delivery_id: Some(delivery.id),
        delivery_price: Some(delivery.price),
    };
    let Some(pricing) = price_order(
        &state,
        &mut tx,
        &rules,
        &cart,
        // Цены — по сессии, как в корзине: гость считается гостем, даже если по его
        // контактам найден пользователь (тот — только владелец заказа)
        (cart_buyer, user_id),
        &ordered,
        &info,
        order_ctx,
    )
    .await?
    else {
        tx.rollback().await?;
        return Err(fail(SubmitError::basket_changed()));
    };
    for it in &mut order_items {
        if let Some((base, r)) = pricing.items.get(&it.id) {
            it.base_price = *base;
            it.discount = r.discount;
            it.price = r.price;
        }
    }
    let goods = round2(order_items.iter().map(|i| i.price * i.quantity).sum());
    let delivery_price = round2(pricing.delivery_price.unwrap_or(delivery.price));
    let property_values = values
        .iter()
        .filter_map(|(id, value)| {
            let p = props.iter().find(|p| p.id == *id)?;
            Some((p.id, p.code.clone(), p.name.clone(), value.clone()))
        })
        .collect();
    let new_order = NewOrder {
        user_id: Some(buyer),
        buyer_id: cart_buyer,
        person_type_id: person_type,
        status,
        currency: currency(&basket),
        goods_price: goods,
        delivery_price,
        user_comment: body
            .get("comment")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        properties: property_values,
        delivery: (
            delivery.id,
            delivery.name.clone(),
            delivery_price,
            shipment_store(delivery, &selected),
        ),
        payment: (payment.id, payment.name.clone()),
        delivery_base_price: delivery.price,
        items: order_items,
        stock: stock::plan(&[], &lines, &traced),
        discounts: pricing.applied,
        coupons: pricing.coupons,
    };
    let order_id = match repo::create(&mut tx, &new_order).await {
        Ok(id) => id,
        Err(repo::CreateError::BasketChanged) => {
            tx.rollback().await?;
            return Err(fail(SubmitError::basket_changed()));
        }
        Err(repo::CreateError::Db(e)) => {
            tracing::error!(error = ?e, "оформление: заказ не записан");
            return Err(fail(SubmitError::new(
                "order_write_exception",
                "Не удалось сохранить заказ",
            )));
        }
    };
    tx.commit().await?;

    let mail_state = state.clone();
    tokio::spawn(async move {
        sale::mail::notify(&mail_state, "SALE_NEW_ORDER", order_id).await;
    });

    let total = round2(goods + delivery_price);
    let mut data = json!({
        "orderId": order_id,
        "accountNumber": order_id.to_string(),
        "total": total,
        "paymentUrl": null,
        "payment": null,
        "paymentSystem": {
            "id": payment.id,
            "name": payment.name,
            "type": form::payment_type(payment, cfg),
        },
        "deliveryStores": form::delivery_stores(delivery, &cart.stores),
    });
    let mut messages = pricing.messages;
    if let Some(text) = shown.and_then(|s| price_change_message(s, goods, &messages)) {
        messages.push(text);
    }
    // Только если есть что сказать — иначе ответ прежний байт в байт
    if !messages.is_empty() {
        data["messages"] = json!(messages);
    }
    Ok(success(data))
}

/// Сообщение о том, что стоимость товаров заказа не та, что покупатель видел в корзине с
/// данными заказа (`shown`); расхождение из-за купона уже объяснено его сообщением.
fn price_change_message(shown: f64, goods: f64, messages: &[String]) -> Option<String> {
    ((shown - goods).abs() > 0.005 && messages.is_empty())
        .then(|| format!("Стоимость товаров изменилась: {goods:.2} вместо {shown:.2}"))
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Скидки заказа, посчитанные в транзакции оформления.
struct OrderPricing {
    /// Позиция → (цена до скидок, итог).
    items: HashMap<i64, (f64, ItemResult)>,
    delivery_price: Option<f64>,
    applied: Vec<Applied>,
    /// Применённые купоны — к отметке использованными.
    coupons: Vec<i64>,
    /// Купоны, которые были применены в корзине, но к заказу уже не годны.
    messages: Vec<String>,
}

/// Скидки с данными заказа внутри его транзакции. Купоны покупателя блокируются и
/// проверяются заново (`saveApplied` Битрикса): занятый параллельным оформлением купон
/// дождётся его и, если тот купон израсходовал, заказ считается без него — с сообщением.
/// Купон, правило которого не сработало, не применяется и не расходуется. Базовая цена
/// позиции под блокировкой не та, что в снимке, — `None` (корзина изменилась).
#[allow(clippy::too_many_arguments)]
async fn price_order(
    state: &AppState,
    tx: &mut sqlx::PgConnection,
    rules: &[crate::discount::model::Rule],
    cart: &CartState,
    (cart_buyer, user_id): (i64, Option<i64>),
    items: &[CartItem],
    info: &HashMap<i64, catalog::PurchaseInfo>,
    order: OrderCtx,
) -> Result<Option<OrderPricing>, BxError> {
    let now = chrono::Utc::now();
    let mut coupons = Vec::new();
    let mut messages = Vec::new();
    for locked in discount_repo::lock_buyer_coupons(tx, cart_buyer).await? {
        let cp = locked.coupon;
        match discount_repo::save_problem(&cp, locked.rule, user_id, now) {
            None => coupons.push(cp),
            Some(text) => {
                let was_applied = cart.discounts.coupons.iter().any(|(code, st)| {
                    *st == CouponStatus::Applied && code.eq_ignore_ascii_case(&cp.coupon)
                });
                if was_applied {
                    messages.push(format!("Купон {}: {text}", cp.coupon));
                }
            }
        }
    }
    let mut ctx = context::for_cart(&mut *tx, items, info, user_id, coupons, order).await?;
    context::load_extras(&mut *tx, &mut ctx, rules).await?;
    if ctx.items.iter().any(|it| {
        cart.bases
            .get(&it.key)
            .is_none_or(|b| (b - it.base_price).abs() > 0.005)
    }) {
        return Ok(None);
    }
    let hook = state.project.discount_condition_hook.as_deref();
    let result = engine::calculate(rules, &ctx, hook);
    let used: Vec<i64> = ctx
        .coupons
        .iter()
        .filter(|cp| {
            result
                .applied
                .iter()
                .any(|a| a.coupon.as_deref() == Some(cp.coupon.as_str()))
        })
        .map(|cp| cp.id)
        .collect();
    let items = ctx
        .items
        .iter()
        .filter_map(|it| Some((it.key, (it.base_price, result.items.get(&it.key)?.clone()))))
        .collect();
    Ok(Some(OrderPricing {
        items,
        delivery_price: result.delivery_price,
        applied: result.applied,
        coupons: used,
        messages,
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use super::*;
    use crate::sale::validate::SubmitError;

    #[test]
    fn submit_error_to_bx() {
        let e = to_bx(
            SubmitError {
                code: "invalid_property",
                message: "Заполните поле «ФИО»".into(),
                field: Some("fio".into()),
            },
            None,
        );
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert_eq!(e.custom.as_deref(), Some(&json!({"field": "fio"})));
        let e = to_bx(
            SubmitError::basket_changed(),
            Some(json!({"snapshot": "S"})),
        );
        assert_eq!(e.status, StatusCode::CONFLICT);
        assert_eq!(e.custom.as_deref().unwrap()["basket"]["snapshot"], "S");
        let e = to_bx(SubmitError::new("payment_forbidden", "Нет доступа"), None);
        assert_eq!(e.status, StatusCode::FORBIDDEN);
        assert!(e.custom.is_none());
        let e = to_bx(SubmitError::new("user_provision_failed", "x"), None);
        assert_eq!(e.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        cookie: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
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
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// Правило «−`perc` % на позиции» (и `delivery` руб. с доставки) с условиями `conditions`.
    async fn rule(
        db: &sqlx::PgPool,
        perc: f64,
        delivery: f64,
        use_coupons: bool,
        conditions: Option<Value>,
    ) -> i64 {
        let mut acts = vec![json!({
            "CLASS_ID": "ActSaleBsktGrp",
            "DATA": {"Type": "Discount", "Value": perc, "Unit": "Perc", "Max": 0, "All": "AND", "True": "True"},
            "CHILDREN": []
        })];
        if delivery > 0.0 {
            acts.push(json!({"CLASS_ID": "ActSaleDelivery", "DATA": {"Type": "Discount", "Value": delivery, "Unit": "Cur"}}));
        }
        let conditions = conditions.unwrap_or_else(
            || json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": []}),
        );
        sqlx::query_scalar(
            "INSERT INTO discounts (name, use_coupons, conditions, actions)
             VALUES ('Скидка ' || $1::text || '%', $2, $3, $4) RETURNING id",
        )
        .bind(perc.to_string())
        .bind(use_coupons)
        .bind(conditions)
        .bind(json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": acts}))
        .fetch_one(db)
        .await
        .unwrap()
    }

    /// Купон правила (`extra` — доп. присваивания `SET`), введённый покупателями `buyers`.
    async fn coupon(db: &sqlx::PgPool, rule: i64, code: &str, extra: &str, buyers: &[i64]) -> i64 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO discount_coupons (discount_id, coupon) VALUES ($1, $2) RETURNING id",
        )
        .bind(rule)
        .bind(code)
        .fetch_one(db)
        .await
        .unwrap();
        if !extra.is_empty() {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE discount_coupons SET {extra} WHERE id = $1"
            )))
            .bind(id)
            .execute(db)
            .await
            .unwrap();
        }
        for b in buyers {
            sqlx::query("INSERT INTO buyer_coupons (buyer_id, coupon_id) VALUES ($1, $2)")
                .bind(b)
                .bind(id)
                .execute(db)
                .await
                .unwrap();
        }
        id
    }

    /// Пользователь `login` — владелец покупателя `buyer`, с сессией bxapi; Cookie сессии.
    async fn login(db: &sqlx::PgPool, buyer: i64, login: &str) -> (i64, String) {
        let user: i64 = sqlx::query_scalar(
            "INSERT INTO users (login, email, password_hash) VALUES ($1, $1 || '@e.x', '') RETURNING id",
        )
        .bind(login)
        .fetch_one(db)
        .await
        .unwrap();
        sqlx::query("UPDATE buyers SET user_id = $2 WHERE id = $1")
            .bind(buyer)
            .bind(user)
            .execute(db)
            .await
            .unwrap();
        let token = format!("sess-{login}");
        sqlx::query(
            "INSERT INTO site_sessions (token_hash, user_id, expires_at) VALUES ($1, $2, now() + interval '1 day')",
        )
        .bind(crate::cart::repo::token_hash(&token))
        .bind(user)
        .execute(db)
        .await
        .unwrap();
        (user, format!("{}={token}", super::super::auth::COOKIE))
    }

    /// Оформление всей корзины: доставка 1, платёжка 1.
    async fn checkout(app: &axum::Router, cookie: &str, person_type: i64) -> Value {
        let (_, cart) = send(app, "GET", "/api/v1/cart", cookie, None).await;
        let (status, body) = send(
            app,
            "POST",
            "/api/v1/order/submit",
            cookie,
            Some(
                json!({"personTypeId": person_type, "deliveryId": 1, "paymentId": 1,
                        "basketSnapshot": cart["data"]["snapshot"]}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "success", "{body}");
        body["data"].clone()
    }

    async fn line(db: &sqlx::PgPool, id: i64) -> (f64, f64, f64) {
        sqlx::query_as(
            "SELECT base_price::float8, discount_price::float8, price::float8 FROM cart_items WHERE id = $1",
        )
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn coupon_state(db: &sqlx::PgPool, id: i64) -> (bool, i32) {
        sqlx::query_as("SELECT active, use_count FROM discount_coupons WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    /// Пустая корзина: «Стоимость товаров» — `0.0`, как до скидок (сумма пустого итератора
    /// `f64` в Rust — `-0.0`).
    #[sqlx::test]
    async fn empty_cart_summary_is_positive_zero(db: sqlx::PgPool) {
        use tower::ServiceExt;
        let fx = crate::test_support::cart_fixture(&db).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let summary = json!({"personTypeId": fx.f.person_type_id, "deliveryId": 1, "paymentId": 1});
        for (method, path, body) in [
            ("GET", "/api/v1/order/form", String::new()),
            ("POST", "/api/v1/order/summary", summary.to_string()),
        ] {
            let req = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(body))
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{path}");
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap();
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(
                text.contains(r#"{"label":"Стоимость товаров","value":0.0,"format":"price"}"#),
                "{path}: {text}"
            );
            assert!(!text.contains("-0.0"), "{path}: {text}");
        }
    }

    #[sqlx::test]
    async fn order_fixes_discounted_prices(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let (_, cookie) = login(&db, fx.f.buyer_id, "ivan").await;
        let rule_id = rule(&db, 10.0, 100.0, false, None).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let data = checkout(&app, &cookie, fx.f.person_type_id).await;
        assert_eq!(data["total"], json!(2450.0));
        assert!(data.get("messages").is_none(), "без сообщений ключа нет");
        let order_id = data["orderId"].as_i64().unwrap();

        assert_eq!(line(&db, fx.f.item_id).await, (1000.0, 100.0, 900.0));
        assert_eq!(line(&db, fx.offer_item_id).await, (500.0, 50.0, 450.0));
        let shipment: (f64, f64) = sqlx::query_as(
            "SELECT base_price::float8, price::float8 FROM shipments WHERE order_id = $1",
        )
        .bind(order_id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(shipment, (300.0, 200.0));
        let totals: (f64, f64, f64, f64) = sqlx::query_as(
            "SELECT o.goods_price::float8, o.delivery_price::float8, o.price::float8, p.sum::float8
             FROM orders o JOIN payments p ON p.order_id = o.id WHERE o.id = $1",
        )
        .bind(order_id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(totals, (2250.0, 200.0, 2450.0, 2450.0));
        let applied: Vec<(Option<i64>, String, String, Value, f64)> = sqlx::query_as(
            "SELECT discount_id, name, coupon, applied, sum::float8 FROM order_discounts WHERE order_id = $1",
        )
        .bind(order_id)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(applied.len(), 1);
        let (id, name, coupon, json_applied, sum) = &applied[0];
        assert_eq!(*id, Some(rule_id));
        assert_eq!(name, "Скидка 10%");
        assert_eq!(coupon, "");
        assert_eq!(*sum, 350.0);
        assert_eq!(
            *json_applied,
            json!({"items": [{"cart_item_id": fx.f.item_id, "value": 200.0},
                             {"cart_item_id": fx.offer_item_id, "value": 50.0}],
                   "delivery": 100.0})
        );
    }

    /// Округление без правил скидок: в заказ уходит округлённая цена, скидка — разница с базовой.
    #[sqlx::test]
    async fn order_stores_rounded_prices(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let (_, cookie) = login(&db, fx.f.buyer_id, "ivan").await;
        sqlx::query("UPDATE catalog_prices SET price = 1004 WHERE item_id = $1")
            .bind(fx.f.product_id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO catalog_rounding (price_type_id, price, round_type, precision)
             VALUES (1, 0, 'math', 10)",
        )
        .execute(&db)
        .await
        .unwrap();
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let data = checkout(&app, &cookie, fx.f.person_type_id).await;
        assert!(data["orderId"].as_i64().is_some(), "{data}");
        // (базовая, скидка, цена): 1004 → 1000; предложение 500 кратно 10
        assert_eq!(line(&db, fx.f.item_id).await, (1004.0, 4.0, 1000.0));
        assert_eq!(line(&db, fx.offer_item_id).await, (500.0, 0.0, 500.0));
    }

    #[sqlx::test]
    async fn coupon_consumed_on_order(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let (_, cookie) = login(&db, fx.f.buyer_id, "ivan").await;
        let b = fx.f.buyer_id;
        let basket = coupon(
            &db,
            rule(&db, 5.0, 0.0, true, None).await,
            "ROW",
            "kind = 'basket'",
            &[b],
        )
        .await;
        let order = coupon(
            &db,
            rule(&db, 4.0, 0.0, true, None).await,
            "ONE",
            "kind = 'order'",
            &[b],
        )
        .await;
        let last = coupon(
            &db,
            rule(&db, 3.0, 0.0, true, None).await,
            "LAST",
            "kind = 'multi', max_use = 2, use_count = 1",
            &[b],
        )
        .await;
        let multi = coupon(
            &db,
            rule(&db, 2.0, 0.0, true, None).await,
            "MANY",
            "kind = 'multi'",
            &[b],
        )
        .await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let data = checkout(&app, &cookie, fx.f.person_type_id).await;
        assert!(data.get("messages").is_none(), "{data}");
        let order_id = data["orderId"].as_i64().unwrap();

        assert_eq!(coupon_state(&db, basket).await, (false, 1), "на позицию");
        assert_eq!(coupon_state(&db, order).await, (false, 1), "на заказ");
        assert_eq!(
            coupon_state(&db, last).await,
            (false, 2),
            "последнее использование"
        );
        assert_eq!(coupon_state(&db, multi).await, (true, 1), "многоразовый");
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM buyer_coupons")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(left, 0, "купоны покупателя очищены");
        let mut coupons: Vec<String> =
            sqlx::query_scalar("SELECT coupon FROM order_discounts WHERE order_id = $1")
                .bind(order_id)
                .fetch_all(&db)
                .await
                .unwrap();
        coupons.sort();
        assert_eq!(coupons, ["LAST", "MANY", "ONE", "ROW"]);
    }

    #[sqlx::test]
    async fn coupon_without_effect_is_not_consumed(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let (_, cookie) = login(&db, fx.f.buyer_id, "ivan").await;
        let b = fx.f.buyer_id;
        // Заказ дороже 2000: в корзине 2500, после уменьшения — 1500 + доставка 300
        let summ = json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"},
            "CHILDREN": [{"CLASS_ID": "CondSaleOrderSumm", "DATA": {"logic": "Great", "value": 2000}}]});
        let cond = coupon(
            &db,
            rule(&db, 10.0, 0.0, true, Some(summ)).await,
            "BIG",
            "",
            &[b],
        )
        .await;
        let expiring = coupon(&db, rule(&db, 5.0, 0.0, true, None).await, "SOON", "", &[b]).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let (_, cart) = send(&app, "GET", "/api/v1/cart", &cookie, None).await;
        let statuses: Vec<&str> = cart["data"]["coupons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["status"].as_str().unwrap())
            .collect();
        assert_eq!(statuses, ["APPLYED", "APPLYED"], "{cart}");

        sqlx::query("UPDATE cart_items SET quantity = 1 WHERE id = $1")
            .bind(fx.f.item_id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE discount_coupons SET active_to = now() - interval '1 minute' WHERE id = $1",
        )
        .bind(expiring)
        .execute(&db)
        .await
        .unwrap();
        let data = checkout(&app, &cookie, fx.f.person_type_id).await;
        assert_eq!(data["total"], json!(1800.0), "по ценам без скидки");
        assert!(data.get("messages").is_none(), "{data}");
        assert_eq!(coupon_state(&db, cond).await, (true, 0));
        assert_eq!(coupon_state(&db, expiring).await, (true, 0));
        let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM order_discounts")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(applied, 0);
        assert_eq!(line(&db, fx.f.item_id).await, (1000.0, 0.0, 1000.0));
    }

    /// Два оформления с одним одноразовым купоном, пока строка купона занята: скидку
    /// получает ровно одно, второе — по ценам без купона и с сообщением Битрикса.
    #[sqlx::test]
    async fn one_time_coupon_race(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        // Второй покупатель — с предложением, первый — с товаром (разные строки товаров)
        let other: i64 =
            sqlx::query_scalar("INSERT INTO buyers (token_hash) VALUES ('other') RETURNING id")
                .fetch_one(&db)
                .await
                .unwrap();
        sqlx::query("UPDATE cart_items SET buyer_id = $2 WHERE id = $1")
            .bind(fx.offer_item_id)
            .bind(other)
            .execute(&db)
            .await
            .unwrap();
        let (_, first) = login(&db, fx.f.buyer_id, "first").await;
        let (_, second) = login(&db, other, "second").await;
        let once = coupon(
            &db,
            rule(&db, 10.0, 0.0, true, None).await,
            "ONCE",
            "kind = 'order'",
            &[fx.f.buyer_id, other],
        )
        .await;
        let app = crate::app(crate::test_support::test_state(db.clone()));

        let mut hold = db.begin().await.unwrap();
        sqlx::query("SELECT id FROM discount_coupons WHERE id = $1 FOR UPDATE")
            .bind(once)
            .execute(&mut *hold)
            .await
            .unwrap();
        let pt = fx.f.person_type_id;
        let (a, b) = (app.clone(), app.clone());
        let one = tokio::spawn(async move { checkout(&a, &first, pt).await });
        let two = tokio::spawn(async move { checkout(&b, &second, pt).await });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        hold.commit().await.unwrap();
        let results = [one.await.unwrap(), two.await.unwrap()];

        // Без скидки: товар 2000 + 300, предложение 500 + 300
        let full = [2300.0, 800.0];
        let discounted = [2100.0, 750.0];
        let winners: Vec<usize> = (0..2)
            .filter(|&i| results[i]["total"] == json!(discounted[i]))
            .collect();
        assert_eq!(winners.len(), 1, "{results:?}");
        let loser = 1 - winners[0];
        assert_eq!(results[loser]["total"], json!(full[loser]), "{results:?}");
        assert_eq!(
            results[loser]["messages"],
            json!(["Купон ONCE: купон на один заказ уже использован"])
        );
        assert!(results[winners[0]].get("messages").is_none());
        assert_eq!(coupon_state(&db, once).await, (false, 1));
        let used: i64 =
            sqlx::query_scalar("SELECT count(*) FROM order_discounts WHERE coupon = 'ONCE'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(used, 1);
    }

    #[sqlx::test]
    async fn profile_history_base_price(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let (_, cookie) = login(&db, fx.f.buyer_id, "ivan").await;
        rule(&db, 10.0, 0.0, false, None).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        checkout(&app, &cookie, fx.f.person_type_id).await;
        let (status, body) = send(&app, "GET", "/api/v1/profile/orders", &cookie, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let items = body["data"]["orders"][0]["items"]
            .as_array()
            .unwrap()
            .clone();
        let item = items
            .iter()
            .find(|i| i["id"] == json!(fx.f.item_id))
            .unwrap();
        assert_eq!(item["price"], json!(900.0));
        assert_eq!(item["basePrice"], json!(1000.0));
    }

    /// Гость без сессии оформляет с email пользователя из группы со скидкой −10 %: цена —
    /// гостевая (как в корзине и итогах, что он видел), заказ — на найденного пользователя.
    #[sqlx::test]
    async fn guest_with_member_email_gets_guest_price(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let group: i64 = sqlx::query_scalar(
            "INSERT INTO groups (code, name) VALUES ('vip', 'VIP') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let member: i64 = sqlx::query_scalar(
            "INSERT INTO users (login, email, password_hash) VALUES ('vip', 'vip@e.x', '') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO user_groups (user_id, group_id) VALUES ($1, $2)")
            .bind(member)
            .bind(group)
            .execute(&db)
            .await
            .unwrap();
        let rule_id = rule(&db, 10.0, 0.0, false, None).await;
        sqlx::query("INSERT INTO discount_groups (discount_id, group_id) VALUES ($1, $2)")
            .bind(rule_id)
            .bind(group)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO order_properties (person_type_id, code, name, is_email) VALUES ($1, 'EMAIL', 'E-mail', TRUE)",
        )
        .bind(fx.f.person_type_id)
        .execute(&db)
        .await
        .unwrap();
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let (_, cart) = send(&app, "GET", "/api/v1/cart", &fx.cookie, None).await;
        assert_eq!(cart["data"]["totalPrice"], json!(2500), "{cart}");
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/order/submit",
            &fx.cookie,
            Some(
                json!({"personTypeId": fx.f.person_type_id, "deliveryId": 1, "paymentId": 1,
                        "properties": {"EMAIL": "VIP@e.x"},
                        "basketSnapshot": cart["data"]["snapshot"]}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let data = &body["data"];
        assert_eq!(data["total"], json!(2800.0), "{body}");
        assert!(data.get("messages").is_none(), "{body}");
        let order_id = data["orderId"].as_i64().unwrap();
        let (user, goods): (Option<i64>, f64) =
            sqlx::query_as("SELECT user_id, goods_price::float8 FROM orders WHERE id = $1")
                .bind(order_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(user, Some(member), "заказ — на найденного пользователя");
        assert_eq!(goods, 2500.0);
        assert_eq!(line(&db, fx.f.item_id).await, (1000.0, 0.0, 1000.0));
    }

    #[test]
    fn price_change_message_only_on_difference() {
        assert_eq!(price_change_message(2500.0, 2500.0, &[]), None);
        assert_eq!(price_change_message(2500.0, 2500.004, &[]), None);
        assert_eq!(
            price_change_message(2500.0, 2250.0, &[]).as_deref(),
            Some("Стоимость товаров изменилась: 2250.00 вместо 2500.00")
        );
        // Расхождение уже объяснено сообщением о купоне
        assert_eq!(
            price_change_message(2500.0, 2250.0, &["Купон X: купон неактивен".into()]),
            None
        );
    }

    /// Ждёт, пока `n` запросов встанут на блокировке строк.
    async fn wait_locked(db: &sqlx::PgPool, n: i64) {
        for _ in 0..1000 {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(db)
            .await
            .unwrap();
            if waiting >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("оформления не дошли до блокировки");
    }

    /// Базовая цена под блокировкой не та, что видел покупатель (цена сменилась, пока
    /// оформление ждало строку товара), — `basket_changed`.
    #[sqlx::test]
    async fn base_price_changed_after_snapshot(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let (_, cookie) = login(&db, fx.f.buyer_id, "ivan").await;
        rule(&db, 10.0, 0.0, false, None).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let (_, cart) = send(&app, "GET", "/api/v1/cart", &cookie, None).await;
        let mut hold = db.begin().await.unwrap();
        sqlx::query("SELECT item_id FROM catalog_products WHERE item_id = $1 FOR UPDATE")
            .bind(fx.f.product_id)
            .execute(&mut *hold)
            .await
            .unwrap();
        let pt = fx.f.person_type_id;
        let task = tokio::spawn(async move {
            send(
                &app,
                "POST",
                "/api/v1/order/submit",
                &cookie,
                Some(json!({"personTypeId": pt, "deliveryId": 1, "paymentId": 1,
                            "basketSnapshot": cart["data"]["snapshot"]})),
            )
            .await
        });
        wait_locked(&db, 1).await;
        sqlx::query("UPDATE catalog_prices SET price = 1100 WHERE item_id = $1")
            .bind(fx.f.product_id)
            .execute(&mut *hold)
            .await
            .unwrap();
        hold.commit().await.unwrap();
        let (status, body) = task.await.unwrap();
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["errors"][0]["code"], "basket_changed");
        let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM orders")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(orders, 0);
    }

    /// Оформлений столько же, сколько соединений в пуле, — ни одно не ждёт свободного
    /// соединения, держа транзакцию.
    #[sqlx::test]
    async fn checkouts_fill_the_pool_without_stall(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        rule(&db, 10.0, 0.0, false, None).await;
        let size = 3;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(size)
            .connect_with((*db.connect_options()).clone())
            .await
            .unwrap();
        let mut cookies = Vec::new();
        for n in 0..size {
            let buyer: i64 =
                sqlx::query_scalar("INSERT INTO buyers (token_hash) VALUES ($1) RETURNING id")
                    .bind(format!("b{n}"))
                    .fetch_one(&db)
                    .await
                    .unwrap();
            sqlx::query(
                "INSERT INTO cart_items (buyer_id, item_id, quantity, name) VALUES ($1, $2, 1, 'Масло 1 л')",
            )
            .bind(buyer)
            .bind(fx.offer_id)
            .execute(&db)
            .await
            .unwrap();
            cookies.push(login(&db, buyer, &format!("u{n}")).await.1);
        }
        let app = crate::app(crate::test_support::test_state(pool));
        let mut snapshots = Vec::new();
        for c in &cookies {
            let (_, cart) = send(&app, "GET", "/api/v1/cart", c, None).await;
            snapshots.push(cart["data"]["snapshot"].clone());
        }
        // Все оформления встают в своих транзакциях на строке товара, пока её держит тест
        let mut hold = db.begin().await.unwrap();
        sqlx::query("SELECT item_id FROM catalog_products WHERE item_id = $1 FOR UPDATE")
            .bind(fx.offer_id)
            .execute(&mut *hold)
            .await
            .unwrap();
        let pt = fx.f.person_type_id;
        let tasks: Vec<_> = cookies
            .into_iter()
            .zip(snapshots)
            .map(|(c, snap)| {
                let app = app.clone();
                tokio::spawn(async move {
                    send(
                        &app,
                        "POST",
                        "/api/v1/order/submit",
                        &c,
                        Some(json!({"personTypeId": pt, "deliveryId": 1, "paymentId": 1,
                                    "basketSnapshot": snap})),
                    )
                    .await
                })
            })
            .collect();
        wait_locked(&db, size as i64).await;
        hold.commit().await.unwrap();
        let all = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            let mut out = Vec::new();
            for t in tasks {
                out.push(t.await.unwrap());
            }
            out
        })
        .await
        .expect("оформления встали на пуле");
        for (status, body) in all {
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["data"]["total"], json!(750.0), "{body}");
        }
    }
}
