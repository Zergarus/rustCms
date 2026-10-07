//! Снимок корзины для API: чистый расчёт без БД.

use std::collections::HashMap;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::{CartItem, ProductView, SnapshotConfig, StoreInfo};
use crate::catalog::{PurchaseInfo, main_price};

/// Число в JSON: целое без дробной части, иначе с копейками.
fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9e15 {
        Value::from(n as i64)
    } else {
        Value::from((n * 100.0).round() / 100.0)
    }
}

fn hash40(data: &str) -> String {
    let mut hex = hex::encode(Sha256::digest(data.as_bytes()));
    hex.truncate(40);
    hex
}

/// Отпечаток позиции: меняется при изменении товара, количества, цены или склада.
pub fn item_hash(
    id: i64,
    product_id: i64,
    quantity: f64,
    price: f64,
    store_id: Option<i64>,
) -> String {
    let store = store_id.map(|s| s.to_string()).unwrap_or_default();
    hash40(&format!("{id}|{product_id}|{quantity}|{price}|{store}"))
}

/// Объект склада: id, название, поля карточки и UF.
fn store_json(store: &StoreInfo) -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert("id".into(), json!(store.id));
    obj.insert("name".into(), json!(store.name));
    for (k, v) in &store.fields {
        obj.insert(k.clone(), v.clone());
    }
    obj
}

/// Снимок корзины в формате bxapi (`BasketSnapshot`).
pub fn build_snapshot(
    items: &[CartItem],
    info: &HashMap<i64, PurchaseInfo>,
    views: &HashMap<i64, ProductView>,
    stores: &[StoreInfo],
    config: &SnapshotConfig,
) -> Value {
    let mut sorted: Vec<&CartItem> = items.iter().collect();
    sorted.sort_by_key(|i| i.id);

    let mut out_items = Vec::with_capacity(sorted.len());
    let mut hashes = String::new();
    let mut total = 0.0;
    let mut count = 0.0;
    let mut can_checkout = !sorted.is_empty();
    let mut currency: Option<String> = None;

    for item in sorted {
        let product = info.get(&item.element_id);
        let price = product.and_then(|p| main_price(&p.prices));
        let available =
            product.is_some_and(|p| p.active && p.is_catalog && p.available) && price.is_some();
        let unit = price.map_or(0.0, |p| p.price);
        if currency.is_none() {
            currency = price.map(|p| p.currency.clone());
        }

        // Склады: активные, у которых есть запись остатка товара
        let mut store_list = Vec::new();
        let mut selected = Value::Null;
        for st in stores.iter().filter(|s| s.active) {
            let Some(amount) = product.and_then(|p| p.amounts.get(&st.id)) else {
                continue;
            };
            let is_selected = item.store_id == Some(st.id);
            if is_selected {
                selected = Value::Object(store_json(st));
            }
            let mut obj = store_json(st);
            obj.insert("amount".into(), number(*amount));
            obj.insert("isAvailable".into(), json!(*amount > 0.0));
            obj.insert("selected".into(), json!(is_selected));
            store_list.push(Value::Object(obj));
        }

        let view = views.get(&item.element_id).cloned().unwrap_or_default();
        let hash = item_hash(item.id, item.element_id, item.quantity, unit, item.store_id);
        hashes.push_str(&hash);
        let sum = unit * item.quantity;
        total += sum;
        count += item.quantity;
        if !available || (config.required_store && selected.is_null()) {
            can_checkout = false;
        }
        out_items.push(json!({
            "id": item.id,
            "productId": item.element_id,
            "name": if view.name.is_empty() { item.name.clone() } else { view.name },
            "slug": view.slug,
            "article": view.article,
            "image": view.image,
            "store": selected,
            "stores": store_list,
            "quantity": number(item.quantity),
            "price": number(unit),
            "sum": number(sum),
            "currency": price.map_or_else(|| config.currency.clone(), |p| p.currency.clone()),
            "isAvailable": available,
            "properties": view.properties,
            "snapshot": hash,
        }));
    }

    let items_count = if config.items_count_positions {
        out_items.len() as f64
    } else {
        count
    };
    json!({
        "items": out_items,
        "totalPrice": number(total),
        "itemsCount": number(items_count),
        "canCheckout": can_checkout,
        "currency": currency.unwrap_or_else(|| config.currency.clone()),
        "empty": out_items.is_empty(),
        "snapshot": hash40(&hashes),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::{Map, json};

    use super::*;
    use crate::{
        cart::{CartItem, ProductView, SnapshotConfig, StoreInfo},
        catalog::{Price, PurchaseInfo},
    };

    fn config() -> SnapshotConfig {
        SnapshotConfig {
            items_count_positions: false,
            required_store: false,
            currency: "RUB".into(),
        }
    }

    fn info(id: i64, price: Option<f64>, amounts: &[(i64, f64)]) -> (i64, PurchaseInfo) {
        (
            id,
            PurchaseInfo {
                active: true,
                is_catalog: true,
                available: true,
                quantity_trace: true,
                can_buy_zero: false,
                prices: price
                    .map(|p| Price {
                        type_id: 1,
                        type_name: "Розница".into(),
                        is_base: true,
                        price: p,
                        currency: "RUB".into(),
                        quantity_from: None,
                        quantity_to: None,
                    })
                    .into_iter()
                    .collect(),
                amounts: amounts.iter().copied().collect(),
                total: amounts.iter().map(|(_, a)| a).sum(),
            },
        )
    }

    fn item(id: i64, element: i64, store: Option<i64>, qty: f64) -> CartItem {
        CartItem {
            id,
            element_id: element,
            store_id: store,
            quantity: qty,
            name: format!("Товар {element}"),
        }
    }

    fn store(id: i64, active: bool) -> StoreInfo {
        let mut fields = Map::new();
        fields.insert("address".into(), json!(format!("Адрес {id}")));
        StoreInfo {
            id,
            name: format!("Склад {id}"),
            active,
            fields,
        }
    }

    fn run(
        items: &[CartItem],
        infos: Vec<(i64, PurchaseInfo)>,
        stores: &[StoreInfo],
        cfg: &SnapshotConfig,
    ) -> serde_json::Value {
        let infos: HashMap<i64, PurchaseInfo> = infos.into_iter().collect();
        let views: HashMap<i64, ProductView> = HashMap::new();
        build_snapshot(items, &infos, &views, stores, cfg)
    }

    #[test]
    fn empty_cart() {
        let s = run(&[], vec![], &[], &config());
        assert_eq!(s["empty"], json!(true));
        assert_eq!(s["canCheckout"], json!(false));
        assert_eq!(s["items"], json!([]));
        assert_eq!(s["totalPrice"], json!(0));
        assert_eq!(s["currency"], json!("RUB"));
    }

    #[test]
    fn totals_and_sum() {
        let items = [item(1, 10, None, 2.0), item(2, 20, None, 1.0)];
        let s = run(
            &items,
            vec![info(10, Some(1000.0), &[]), info(20, Some(500.0), &[])],
            &[],
            &config(),
        );
        assert_eq!(s["totalPrice"], json!(2500));
        assert_eq!(s["itemsCount"], json!(3));
        assert_eq!(s["items"][0]["sum"], json!(2000));
        let positions = SnapshotConfig {
            items_count_positions: true,
            ..config()
        };
        let s = run(
            &items,
            vec![info(10, Some(1000.0), &[]), info(20, Some(500.0), &[])],
            &[],
            &positions,
        );
        assert_eq!(s["itemsCount"], json!(2));
    }

    #[test]
    fn snapshot_is_stable() {
        let items = [item(1, 10, Some(5), 2.0)];
        let a = run(
            &items,
            vec![info(10, Some(100.0), &[(5, 9.0)])],
            &[store(5, true)],
            &config(),
        );
        let b = run(
            &items,
            vec![info(10, Some(100.0), &[(5, 9.0)])],
            &[store(5, true)],
            &config(),
        );
        assert_eq!(a["snapshot"], b["snapshot"]);
        assert_eq!(a["snapshot"].as_str().map(str::len), Some(40));
    }

    #[test]
    fn snapshot_changes_on_quantity_price_store() {
        let base = item_hash(1, 10, 2.0, 100.0, Some(5));
        assert_ne!(base, item_hash(1, 10, 3.0, 100.0, Some(5)));
        assert_ne!(base, item_hash(1, 10, 2.0, 101.0, Some(5)));
        assert_ne!(base, item_hash(1, 10, 2.0, 100.0, Some(6)));
        assert_ne!(base, item_hash(1, 10, 2.0, 100.0, None));
    }

    #[test]
    fn stores_list() {
        let items = [item(1, 10, Some(5), 1.0)];
        let stores = [
            store(5, true),
            store(6, true),
            store(7, false),
            store(8, true),
        ];
        let s = run(
            &items,
            vec![info(10, Some(100.0), &[(5, 3.0), (6, 0.0), (7, 4.0)])],
            &stores,
            &config(),
        );
        let list = s["items"][0]["stores"].as_array().unwrap();
        let ids: Vec<i64> = list.iter().map(|st| st["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, [5, 6], "неактивный и без записи остатка исключены");
        assert_eq!(list[0]["selected"], json!(true));
        assert_eq!(list[0]["amount"], json!(3));
        assert_eq!(list[0]["address"], json!("Адрес 5"));
        assert_eq!(list[1]["isAvailable"], json!(false));
        assert_eq!(s["items"][0]["store"]["id"], json!(5));
        assert!(s["items"][0]["store"].get("amount").is_none());
    }

    #[test]
    fn deactivated_store_is_null() {
        let items = [item(1, 10, Some(7), 1.0)];
        let cfg = SnapshotConfig {
            required_store: true,
            ..config()
        };
        let s = run(
            &items,
            vec![info(10, Some(100.0), &[(7, 4.0)])],
            &[store(7, false)],
            &cfg,
        );
        assert_eq!(s["items"][0]["store"], json!(null));
        assert_eq!(s["canCheckout"], json!(false));
    }

    #[test]
    fn unavailable_item_blocks_checkout() {
        let items = [item(1, 10, None, 1.0)];
        let mut inactive = info(10, Some(100.0), &[]);
        inactive.1.active = false;
        let mut not_catalog = info(10, Some(100.0), &[]);
        not_catalog.1.is_catalog = false;
        for i in [inactive, not_catalog, info(10, None, &[])] {
            let s = run(&items, vec![i], &[], &config());
            assert_eq!(s["items"][0]["isAvailable"], json!(false));
            assert_eq!(s["canCheckout"], json!(false));
        }
        let ok = run(&items, vec![info(10, Some(100.0), &[])], &[], &config());
        assert_eq!(ok["canCheckout"], json!(true));
    }
}
