//! Подарки по правилам скидок: список в снимке корзины (`gifts`) и
//! `GET /catalog/gifts?productId=` — что подарят, если положить товар в корзину
//! (аналог `sale.gift.product`).

use std::collections::{HashMap, HashSet};

use axum::extract::{Query, State};
use axum_extra::extract::CookieJar;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{
    BxResult,
    cart::{cart_base, ensure_purchasable, gift_offers},
    success,
};
use crate::{
    cart::CartItem,
    catalog,
    discount::{
        model::{GiftOffer, GiftType, Node, Rule},
        repo as discount_repo,
    },
    state::AppState,
};

/// Ключ позиции товара, которого ещё нет в корзине (настоящие ключи — положительные id).
const TEMP_KEY: i64 = -1;

/// Среди правил есть действие «Подарок» (тогда в корзине появляется `gifts`).
pub(super) fn has_gift_rules(rules: &[Rule]) -> bool {
    fn walk(node: &Node) -> bool {
        node.class_id == "GiftCondGroup" || node.children.iter().any(walk)
    }
    rules.iter().any(|r| walk(&r.actions))
}

/// Подарок в ответе: `(правило, тип, товары)`. Раздел раскрыт в активные товары и очищен от
/// подаренного (движок у раздела его не вычитает); подарок с пустым списком не отдаётся.
async fn resolve(
    db: &PgPool,
    offers: &[GiftOffer],
) -> sqlx::Result<Vec<(i64, GiftType, Vec<i64>)>> {
    let sections: Vec<i64> = offers.iter().filter_map(|o| o.section_id).collect();
    let by_section = discount_repo::section_products(db, &sections).await?;
    Ok(offers
        .iter()
        .filter_map(|o| {
            let ids: Vec<i64> = match o.section_id {
                Some(s) => by_section
                    .get(&s)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|p| !o.gifted.contains(p))
                    .collect(),
                None => o.product_ids.clone(),
            };
            (!ids.is_empty()).then_some((o.rule_id, o.kind, ids))
        })
        .collect())
}

fn to_json(rule_id: i64, kind: GiftType, product_ids: &[i64]) -> Value {
    let kind = match kind {
        GiftType::One => "one",
        GiftType::All => "all",
    };
    json!({"ruleId": rule_id, "type": kind, "productIds": product_ids})
}

/// `gifts` снимка корзины.
pub(super) async fn offers_json(db: &PgPool, offers: &[GiftOffer]) -> sqlx::Result<Vec<Value>> {
    Ok(resolve(db, offers)
        .await?
        .iter()
        .map(|(rule, kind, ids)| to_json(*rule, *kind, ids))
        .collect())
}

/// `GET /catalog/gifts?productId=`: подарки, которые появятся при добавлении товара в количестве 1,
/// — разница по `(правило, товар)` между расчётом корзины с товаром и без него.
pub async fn catalog_gifts(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(query): Query<HashMap<String, String>>,
) -> BxResult {
    let product_id = query
        .get("productId")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|id| *id > 0);
    let Some(product_id) = product_id else {
        return Ok(success(json!([])));
    };
    let rules = discount_repo::active_rules(&state.db, chrono::Utc::now()).await?;
    if !has_gift_rules(&rules) {
        return Ok(success(json!([])));
    }
    let infos = catalog::load(&state.db, &[product_id]).await?;
    let Ok(purchase) = ensure_purchasable(infos.get(&product_id)) else {
        return Ok(success(json!([])));
    };
    if !purchase.available || catalog::main_price(&purchase.prices).is_none() {
        return Ok(success(json!([])));
    }
    let purchase = purchase.clone();

    let base = cart_base(&state, &jar).await?;
    let without = gift_offers(&state, &base, None, &rules).await?;
    let item = CartItem {
        id: TEMP_KEY,
        product_id,
        store_id: None,
        quantity: 1.0,
        name: String::new(),
    };
    let with = gift_offers(&state, &base, Some((item, purchase)), &rules).await?;

    let had: HashSet<(i64, i64)> = resolve(&state.db, &without)
        .await?
        .into_iter()
        .flat_map(|(rule, _, ids)| ids.into_iter().map(move |id| (rule, id)))
        .collect();
    let list: Vec<Value> = resolve(&state.db, &with)
        .await?
        .into_iter()
        .filter_map(|(rule, kind, ids)| {
            let new: Vec<i64> = ids
                .into_iter()
                .filter(|id| !had.contains(&(rule, *id)))
                .collect();
            (!new.is_empty()).then(|| to_json(rule, kind, &new))
        })
        .collect();
    Ok(success(Value::Array(list)))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};

    async fn get(app: &axum::Router, path: &str, cookie: &str) -> (StatusCode, Value) {
        use tower::ServiceExt;
        let req = axum::http::Request::builder()
            .uri(path)
            .header(axum::http::header::COOKIE, cookie)
            .body(axum::body::Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// Условие «в корзине есть товар `product`».
    fn product_in_cart(product: i64) -> Value {
        json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
            {"CLASS_ID": "CondBsktProductGroup", "DATA": {"Found": "Found", "All": "AND"}, "CHILDREN": [
                {"CLASS_ID": "CondIBElement", "DATA": {"logic": "Equal", "value": [product]}}
            ]}
        ]})
    }

    #[sqlx::test]
    async fn catalog_gifts_for_product(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let x = fx.f.product_id;
        let rule: i64 = sqlx::query_scalar(
            "INSERT INTO discounts (name, conditions, actions) VALUES ('Подарок', $1, $2) RETURNING id",
        )
        .bind(product_in_cart(x))
        .bind(json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
            {"CLASS_ID": "GiftCondGroup", "DATA": {"All": "AND"}, "CHILDREN": [
                {"CLASS_ID": "GifterCondIBElement", "DATA": {"Type": "one", "Value": [fx.parent_id]}}
            ]}
        ]}))
        .fetch_one(&db)
        .await
        .unwrap();
        // Пустая корзина гостя (cookie без покупателя)
        let (status, body) = get(
            &app,
            &format!("/api/v1/catalog/gifts?productId={x}"),
            "CMS_BUYER=nobody",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["data"],
            json!([{"ruleId": rule, "type": "one", "productIds": [fx.parent_id]}])
        );
        let (_, body) = get(
            &app,
            &format!("/api/v1/catalog/gifts?productId={}", fx.offer_id),
            "CMS_BUYER=nobody",
        )
        .await;
        assert_eq!(body["data"], json!([]));
        // Нет товара и нет параметра
        let (_, body) = get(
            &app,
            "/api/v1/catalog/gifts?productId=999999",
            "CMS_BUYER=nobody",
        )
        .await;
        assert_eq!(body["data"], json!([]));
        let (_, body) = get(&app, "/api/v1/catalog/gifts", "CMS_BUYER=nobody").await;
        assert_eq!(body["data"], json!([]));
        // Товар X уже в корзине фикстуры: подарок есть и без него — разницы нет
        let (_, body) = get(
            &app,
            &format!("/api/v1/catalog/gifts?productId={x}"),
            &fx.cookie,
        )
        .await;
        assert_eq!(body["data"], json!([]));
    }

    #[sqlx::test]
    async fn catalog_gifts_without_gift_rules_is_empty(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        let (status, body) = get(
            &app,
            &format!("/api/v1/catalog/gifts?productId={}", fx.f.product_id),
            &fx.cookie,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "success");
        assert_eq!(body["data"], json!([]));
    }
}
