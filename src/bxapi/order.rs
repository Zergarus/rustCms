//! Оформление заказа по контракту bxapi: `order/form`, `order/summary`, `order/submit`.

use std::collections::{HashMap, HashSet};

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
};
use axum_extra::extract::cookie::CookieJar;
use serde_json::{Map, Value, json};

use super::{
    BxError, BxResult,
    auth::current_user_id,
    cart::{CartState, cart_state},
    parse_body,
    query::number,
    success,
};
use crate::{
    cart::CartItem,
    catalog,
    sale::{
        self, Delivery, SaleSettings,
        form::{self, Profile},
        guest,
        repo::{self, NewOrder},
        stock::{self, StockLine},
        validate::{SubmitError, parse_selection, pick_items, validate_properties},
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
    cart: &CartState,
) -> Option<HashSet<i64>> {
    state
        .project
        .order
        .delivery_filter
        .as_ref()
        .map(|f| f.allowed(&settings.deliveries, items, &cart.info))
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
    let cart = cart_state(&state, &jar).await?;
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
    let allowed = allowed_deliveries(&state, &settings, &cart.items, &cart);
    let deliveries = form::deliveries(&settings, cfg, allowed.as_ref(), &cart.stores);
    let payments = form::payments(&settings, cfg);
    let delivery_price = deliveries
        .first()
        .and_then(|d| d["price"].as_f64())
        .unwrap_or(0.0);
    let payment_name = payments.first().and_then(|p| p["name"].as_str());
    let summary = form::summary(goods_total(&cart.snapshot), delivery_price, payment_name);
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
    let cart = cart_state(&state, &jar).await?;
    let allowed = allowed_deliveries(&state, &settings, &cart.items, &cart);
    let delivery_price = number(body.get("deliveryId"))
        .and_then(|id| form::find_delivery(&settings, cfg, allowed.as_ref(), id))
        .map_or(0.0, |d| d.price);
    let payment_name = number(body.get("paymentId"))
        .and_then(|id| form::find_payment(&settings, cfg, id))
        .map(|p| p.name.as_str());
    Ok(success(json!({
        "summary": form::summary(goods_total(&cart.snapshot), delivery_price, payment_name),
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

/// Значения свойств-местоположений, которые есть в базе (по коду или id).
async fn known_locations(
    state: &AppState,
    input: &Map<String, Value>,
) -> Result<HashSet<String>, BxError> {
    let values: Vec<String> = input
        .values()
        .filter_map(|v| match v {
            Value::String(s) => Some(s.trim().to_string()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .filter(|v| !v.is_empty())
        .collect();
    let found: Vec<String> = sqlx::query_scalar(
        "SELECT code FROM locations WHERE code = ANY($1)
         UNION SELECT id::text FROM locations WHERE id::text = ANY($1)",
    )
    .bind(&values)
    .fetch_all(&state.db)
    .await?;
    Ok(found.into_iter().collect())
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
    let allowed = allowed_deliveries(&state, &settings, &selected_owned, &cart);
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
    let input = body
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let known = known_locations(&state, &input).await?;
    let values = validate_properties(&props, &settings.variants, &input, &known).map_err(fail)?;

    // 5. Состав
    let ids = picked.map_err(fail)?;
    let snapshot_items = basket["items"].as_array().cloned().unwrap_or_default();
    let mut order_items = Vec::new();
    let mut lines = Vec::new();
    let mut goods = 0.0;
    for item in selected.iter().filter(|i| ids.contains(&i.id)) {
        let snap = snapshot_items
            .iter()
            .find(|s| s["id"].as_i64() == Some(item.id))
            .ok_or_else(|| fail(SubmitError::basket_changed()))?;
        let price = snap["price"].as_f64().unwrap_or(0.0);
        let name = snap["name"].as_str().unwrap_or(&item.name).to_string();
        goods += price * item.quantity;
        order_items.push((item.id, price, name));
        lines.push(StockLine {
            element_id: item.element_id,
            store_id: item.store_id,
            quantity: item.quantity,
        });
    }
    let goods = (goods * 100.0).round() / 100.0;
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
            match guest::find_user(&state.db, &contacts).await? {
                Some(id) => id,
                None => guest::create_user(&state.db, &contacts, &cfg.guest_group_ids)
                    .await
                    .map_err(|e| {
                        tracing::warn!(error = %e, "оформление: пользователь гостя не создан");
                        fail(provision)
                    })?,
            }
        }
    };

    let element_ids: Vec<i64> = lines.iter().map(|l| l.element_id).collect();
    let mut tx = state.db.begin().await?;
    let info = catalog::load_locked(&mut tx, &element_ids).await?;
    if let Err(message) = stock::check(&lines, &info) {
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
    let property_values = values
        .iter()
        .filter_map(|(id, value)| {
            let p = props.iter().find(|p| p.id == *id)?;
            Some((p.id, p.code.clone(), p.name.clone(), value.clone()))
        })
        .collect();
    let new_order = NewOrder {
        user_id: Some(buyer),
        person_type_id: person_type,
        status,
        currency: currency(&basket),
        goods_price: goods,
        delivery_price: delivery.price,
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
            delivery.price,
            shipment_store(delivery, &selected),
        ),
        payment: (payment.id, payment.name.clone()),
        items: order_items,
        stock: stock::plan(&[], &lines, &traced),
    };
    let order_id = match repo::create(&mut tx, &new_order).await {
        Ok(id) => id,
        Err(e) => {
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

    let total = ((goods + delivery.price) * 100.0).round() / 100.0;
    Ok(success(json!({
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
    })))
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
        assert_eq!(e.custom, json!({"field": "fio"}));
        let e = to_bx(
            SubmitError::basket_changed(),
            Some(json!({"snapshot": "S"})),
        );
        assert_eq!(e.status, StatusCode::CONFLICT);
        assert_eq!(e.custom["basket"]["snapshot"], "S");
        let e = to_bx(SubmitError::new("payment_forbidden", "Нет доступа"), None);
        assert_eq!(e.status, StatusCode::FORBIDDEN);
        assert_eq!(e.custom, serde_json::Value::Null);
        let e = to_bx(SubmitError::new("user_provision_failed", "x"), None);
        assert_eq!(e.status, StatusCode::INTERNAL_SERVER_ERROR);
    }
}
