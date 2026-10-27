//! Правка состава заказа: позиции, склад, цена, доставка, оплата; остатки — на разницу.

use std::collections::{HashMap, HashSet};

use axum::{
    Extension,
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::Form;
use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::PgPool;

use super::orders::{card, require_orders};
use crate::{
    access::Access,
    cart::repo::{Owner, ensure_buyer},
    catalog::{self, main_price},
    error::{AppError, AppResult},
    sale::{
        repo::stock_lines,
        stock::{self, StockChange, StockLine},
    },
    state::AppState,
};

/// Позиция в форме состава; `item_id` нет — добавленный товар.
#[derive(Debug, Clone, PartialEq)]
pub struct EditLine {
    pub item_id: Option<i64>,
    pub product_id: i64,
    pub store_id: Option<i64>,
    pub quantity: f64,
    pub price: f64,
    pub custom_price: bool,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Composition {
    pub lines: Vec<EditLine>,
    pub delivery_id: i64,
    pub delivery_price: f64,
    pub pay_system_id: i64,
}

fn number(raw: Option<&String>) -> Option<f64> {
    raw.map(|s| s.trim().replace(',', ".").replace(' ', ""))
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

fn id(raw: Option<&String>) -> Option<i64> {
    raw.and_then(|s| s.trim().parse().ok())
        .filter(|v: &i64| *v > 0)
}

/// Состав из формы: поля `item_<id>_quantity|store|price|remove`, `add_element`,
/// `add_quantity`, `delivery_id`, `delivery_price`, `pay_system_id`.
pub fn parse_composition(
    form: &HashMap<String, String>,
    current: &[EditLine],
) -> Result<Composition, String> {
    let get = |k: String| form.get(&k);
    let mut lines = Vec::new();
    for cur in current {
        let Some(item) = cur.item_id else { continue };
        if get(format!("item_{item}_remove")).is_some() {
            continue;
        }
        let quantity = number(get(format!("item_{item}_quantity")))
            .filter(|q| *q > 0.0)
            .ok_or("Количество — число больше нуля")?;
        let price = match number(get(format!("item_{item}_price"))) {
            Some(p) if p >= 0.0 => p,
            Some(_) => return Err("Цена — число не меньше нуля".into()),
            None => cur.price,
        };
        lines.push(EditLine {
            item_id: Some(item),
            product_id: cur.product_id,
            store_id: id(get(format!("item_{item}_store"))),
            quantity,
            custom_price: cur.custom_price || (price - cur.price).abs() > 0.005,
            price,
            name: cur.name.clone(),
        });
    }
    if let Some(element) = id(form.get("add_element")) {
        let quantity = match form.get("add_quantity").filter(|s| !s.trim().is_empty()) {
            None => 1.0,
            q => number(q)
                .filter(|q| *q > 0.0)
                .ok_or("Количество — число больше нуля")?,
        };
        lines.push(EditLine {
            item_id: None,
            product_id: element,
            store_id: id(form.get("add_store")),
            quantity,
            price: 0.0,
            custom_price: false,
            name: String::new(),
        });
    }
    if lines.is_empty() {
        return Err("В заказе должна остаться хотя бы одна позиция".into());
    }
    let delivery_price = number(form.get("delivery_price"))
        .filter(|p| *p >= 0.0)
        .ok_or("Стоимость доставки — число не меньше нуля")?;
    Ok(Composition {
        lines,
        delivery_id: id(form.get("delivery_id")).ok_or("Выберите доставку")?,
        delivery_price,
        pay_system_id: id(form.get("pay_system_id")).ok_or("Выберите платёжную систему")?,
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// (товары, доставка, итого)
pub fn totals(c: &Composition) -> (f64, f64, f64) {
    let goods = round2(c.lines.iter().map(|l| l.price * l.quantity).sum());
    (goods, c.delivery_price, round2(goods + c.delivery_price))
}

/// Изменения остатков при правке; у отменённого заказа (остатки не списаны) — никаких.
pub fn stock_changes_for_edit(
    canceled: bool,
    before: &[StockLine],
    after: &[StockLine],
    traced: &HashSet<i64>,
) -> Vec<StockChange> {
    if canceled {
        Vec::new()
    } else {
        stock::plan(before, after, traced)
    }
}

/// Что проверять по остаткам при правке: только товары на складах, где количество
/// выросло (нетронутые позиции, в том числе удалённых из каталога товаров, не проверяются).
pub fn lines_to_check(before: &[StockLine], after: &[StockLine]) -> Vec<StockLine> {
    let sum = |lines: &[StockLine]| {
        let mut out: std::collections::BTreeMap<(i64, Option<i64>), f64> = Default::default();
        for l in lines {
            *out.entry((l.product_id, l.store_id)).or_insert(0.0) += l.quantity;
        }
        out
    };
    let was = sum(before);
    let mut out: Vec<StockLine> = sum(after)
        .into_iter()
        .filter(|(key, q)| *q > was.get(key).copied().unwrap_or(0.0) + 1e-9)
        .map(|((product_id, store_id), quantity)| StockLine {
            product_id,
            store_id,
            quantity,
        })
        .collect();
    // В порядке позиций формы
    out.sort_by_key(|l| {
        after
            .iter()
            .position(|a| a.product_id == l.product_id && a.store_id == l.store_id)
    });
    out
}

pub fn version(updated_at: &DateTime<Utc>) -> String {
    updated_at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Сохраняет состав одной транзакцией; `version` — `updated_at` заказа при открытии формы.
pub async fn save_composition(
    db: &PgPool,
    order_id: i64,
    c: &Composition,
    version: &str,
) -> AppResult<Result<(), String>> {
    let user: Option<i64> = sqlx::query_scalar("SELECT user_id FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)?;
    let existing_buyer: Option<i64> = sqlx::query_scalar(
        "SELECT buyer_id FROM cart_items WHERE order_id = $1 AND buyer_id IS NOT NULL LIMIT 1",
    )
    .bind(order_id)
    .fetch_optional(db)
    .await?;
    let buyer = match (existing_buyer, user) {
        (Some(b), _) => b,
        (None, Some(u)) => ensure_buyer(db, &Owner::User(u)).await?,
        (None, None) => return Ok(Err("У заказа нет покупателя".into())),
    };

    let mut tx = db.begin().await?;
    let (updated_at, canceled, deducted, currency): (DateTime<Utc>, bool, bool, String) = sqlx::query_as(
        "SELECT updated_at, canceled, stock_deducted, currency FROM orders WHERE id = $1 FOR UPDATE",
    )
    .bind(order_id)
    .fetch_one(&mut *tx)
    .await?;
    if self::version(&updated_at) != version {
        return Ok(Err("Заказ изменён, обновите страницу".into()));
    }
    let delivery: Option<String> = sqlx::query_scalar("SELECT name FROM deliveries WHERE id = $1")
        .bind(c.delivery_id)
        .fetch_optional(&mut *tx)
        .await?;
    let payment: Option<String> = sqlx::query_scalar("SELECT name FROM pay_systems WHERE id = $1")
        .bind(c.pay_system_id)
        .fetch_optional(&mut *tx)
        .await?;
    let (Some(delivery), Some(payment)) = (delivery, payment) else {
        return Ok(Err("Нет такой доставки или платёжной системы".into()));
    };

    let before = stock_lines(&mut tx, order_id).await?;
    let mut lines = c.lines.clone();
    let all: Vec<i64> = before
        .iter()
        .map(|l| l.product_id)
        .chain(lines.iter().map(|l| l.product_id))
        .collect();
    let mut info = catalog::load_locked(&mut tx, &all).await?;
    // Цены новых позиций — в валюте заказа
    crate::currency::convert_info(&mut tx, &currency, &mut info).await?;
    let traced: HashSet<i64> = info
        .iter()
        .filter(|(_, p)| p.quantity_trace)
        .map(|(id, _)| *id)
        .collect();
    // Новые позиции: название и текущая основная цена
    for line in lines.iter_mut().filter(|l| l.item_id.is_none()) {
        let Some(product) = info.get(&line.product_id) else {
            return Ok(Err(format!("Товар {} не найден", line.product_id)));
        };
        if !catalog::sellable(product) {
            return Ok(Err(format!(
                "Товар {} нельзя добавить в заказ",
                line.product_id
            )));
        }
        line.price = main_price(&product.prices).map_or(0.0, |p| p.price);
        line.name = sqlx::query_scalar("SELECT name FROM collection_items WHERE id = $1")
            .bind(line.product_id)
            .fetch_one(&mut *tx)
            .await?;
    }
    let after: Vec<StockLine> = lines
        .iter()
        .map(|l| StockLine {
            product_id: l.product_id,
            store_id: l.store_id,
            quantity: l.quantity,
        })
        .collect();
    let keeps_stock = canceled || !deducted;
    if !keeps_stock {
        // Проверка против остатка с учётом того, что уже списано по заказу
        for l in &before {
            if let Some(p) = info.get_mut(&l.product_id) {
                p.total += l.quantity;
                if let Some(store) = l.store_id {
                    *p.amounts.entry(store).or_insert(0.0) += l.quantity;
                }
            }
        }
        if let Err(message) = stock::check(&lines_to_check(&before, &after), &info) {
            return Ok(Err(message));
        }
    }

    let kept: Vec<i64> = lines.iter().filter_map(|l| l.item_id).collect();
    sqlx::query("DELETE FROM cart_items WHERE order_id = $1 AND NOT (id = ANY($2))")
        .bind(order_id)
        .bind(&kept)
        .execute(&mut *tx)
        .await?;
    for l in &lines {
        match l.item_id {
            Some(item) => {
                sqlx::query(
                    // Своя цена — без скидки; прочие позиции скидки сохраняют (правка их не
                    // пересчитывает)
                    "UPDATE cart_items SET quantity = $3, store_id = $4, price = $5, custom_price = $6,
                         base_price = CASE WHEN $6 THEN $5 ELSE COALESCE(base_price, $5) END,
                         discount_price = CASE WHEN $6 THEN 0 ELSE discount_price END,
                         updated_at = now()
                     WHERE id = $1 AND order_id = $2",
                )
                .bind(item)
                .bind(order_id)
                .bind(l.quantity)
                .bind(l.store_id)
                .bind(l.price)
                .bind(l.custom_price)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                let props = crate::collection::sku::basket_props(db, l.product_id).await?;
                sqlx::query(
                    "INSERT INTO cart_items (buyer_id, item_id, store_id, quantity, name, price,
                                             base_price, currency, order_id, custom_price, props)
                     VALUES ($1, $2, $3, $4, $5, $6, $6, $7, $8, FALSE, $9)",
                )
                .bind(buyer)
                .bind(l.product_id)
                .bind(l.store_id)
                .bind(l.quantity)
                .bind(&l.name)
                .bind(l.price)
                .bind(&currency)
                .bind(order_id)
                .bind(sqlx::types::Json(&props))
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    let composition = Composition { lines, ..c.clone() };
    let (goods, delivery_price, total) = totals(&composition);
    sqlx::query(
        // Доставка или её цена изменены вручную — без скидки
        "UPDATE shipments SET delivery_id = $2, delivery_name = $3, price = $4,
             base_price = CASE WHEN price IS DISTINCT FROM $4 OR delivery_id IS DISTINCT FROM $2
                               THEN $4 ELSE base_price END
         WHERE order_id = $1",
    )
    .bind(order_id)
    .bind(c.delivery_id)
    .bind(&delivery)
    .bind(delivery_price)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE payments SET pay_system_id = $2, name = $3, sum = $4 WHERE order_id = $1")
        .bind(order_id)
        .bind(c.pay_system_id)
        .bind(&payment)
        .bind(total)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE orders SET goods_price = $2, delivery_price = $3, price = $4, updated_at = now() WHERE id = $1",
    )
    .bind(order_id)
    .bind(goods)
    .bind(delivery_price)
    .bind(total)
    .execute(&mut *tx)
    .await?;
    stock::apply(
        &mut tx,
        &stock_changes_for_edit(keeps_stock, &before, &after, &traced),
    )
    .await?;
    tx.commit().await?;
    Ok(Ok(()))
}

/// Текущие позиции заказа в виде строк формы.
pub fn current_lines(items: &[crate::sale::repo::OrderItem]) -> Vec<EditLine> {
    items
        .iter()
        .map(|i| EditLine {
            item_id: Some(i.id),
            product_id: i.product_id,
            store_id: i.store_id,
            quantity: i.quantity,
            price: i.price,
            custom_price: i.custom_price,
            name: i.name.clone(),
        })
        .collect()
}

pub async fn save_items(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    require_orders(&user)?;
    let order = crate::sale::repo::load(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let result = match parse_composition(&form, &current_lines(&order.items)) {
        Ok(c) => {
            let version = form.get("version").map(String::as_str).unwrap_or_default();
            save_composition(&state.db, id, &c, version).await?
        }
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => Ok(Redirect::to(&format!("/admin/shop/orders/{id}")).into_response()),
        Err(e) => Ok(card(&state, user, id, Some(e), None).await?.into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sale::stock::plan;

    fn edit(id: i64, quantity: f64, price: f64) -> EditLine {
        EditLine {
            item_id: Some(id),
            product_id: id,
            store_id: None,
            quantity,
            price,
            custom_price: false,
            name: format!("Товар {id}"),
        }
    }

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn current() -> Vec<EditLine> {
        vec![
            EditLine {
                product_id: 1,
                ..edit(7, 1.0, 100.0)
            },
            EditLine {
                product_id: 2,
                ..edit(8, 1.0, 50.0)
            },
        ]
    }

    const BASE: &[(&str, &str)] = &[
        ("item_7_quantity", "2"),
        ("item_7_price", "90"),
        ("item_7_store", "5"),
        ("item_8_quantity", "1"),
        ("item_8_price", "50"),
        ("delivery_id", "8"),
        ("delivery_price", "300"),
        ("pay_system_id", "7"),
    ];

    #[test]
    fn composition_totals() {
        let c = Composition {
            lines: vec![edit(1, 2.0, 100.0), edit(2, 1.0, 594.22)],
            delivery_id: 8,
            delivery_price: 300.0,
            pay_system_id: 7,
        };
        assert_eq!(totals(&c), (794.22, 300.0, 1094.22));
    }

    #[test]
    fn parse_marks_custom_price_and_removal() {
        let mut pairs = BASE.to_vec();
        pairs.extend([
            ("item_8_remove", "on"),
            ("add_element", "59958"),
            ("add_quantity", "2"),
        ]);
        let c = parse_composition(&form(&pairs), &current()).unwrap();
        assert_eq!(c.lines.len(), 2);
        let first = &c.lines[0];
        assert_eq!(
            (
                first.item_id,
                first.quantity,
                first.price,
                first.custom_price,
                first.store_id
            ),
            (Some(7), 2.0, 90.0, true, Some(5))
        );
        assert_eq!(
            (
                c.lines[1].item_id,
                c.lines[1].product_id,
                c.lines[1].quantity
            ),
            (None, 59958, 2.0)
        );
        assert_eq!(
            (c.delivery_id, c.delivery_price, c.pay_system_id),
            (8, 300.0, 7)
        );

        let same = parse_composition(&form(BASE), &current()).unwrap();
        assert!(!same.lines[1].custom_price);

        let mut zero = BASE.to_vec();
        zero[0] = ("item_7_quantity", "0");
        assert_eq!(
            parse_composition(&form(&zero), &current()).unwrap_err(),
            "Количество — число больше нуля"
        );
        let mut empty = BASE.to_vec();
        empty.extend([("item_7_remove", "on"), ("item_8_remove", "on")]);
        assert_eq!(
            parse_composition(&form(&empty), &current()).unwrap_err(),
            "В заказе должна остаться хотя бы одна позиция"
        );
    }

    #[test]
    fn only_increases_are_checked() {
        let l = |e: i64, s: Option<i64>, q: f64| StockLine {
            product_id: e,
            store_id: s,
            quantity: q,
        };
        let before = [l(1, Some(5), 2.0), l(9, None, 1.0)];
        // товар 9 (удалён из каталога) не тронут, товар 1: 2 → 3 и новый склад для товара 2
        let after = [l(1, Some(5), 3.0), l(9, None, 1.0), l(2, Some(6), 1.0)];
        assert_eq!(
            lines_to_check(&before, &after),
            vec![l(1, Some(5), 3.0), l(2, Some(6), 1.0)]
        );
        assert!(lines_to_check(&after, &before).is_empty());
    }

    #[test]
    fn edit_canceled_keeps_stock() {
        let traced = HashSet::from([1]);
        let before = [StockLine {
            product_id: 1,
            store_id: Some(5),
            quantity: 2.0,
        }];
        let after = [StockLine {
            product_id: 1,
            store_id: Some(5),
            quantity: 3.0,
        }];
        assert!(stock_changes_for_edit(true, &before, &after, &traced).is_empty());
        assert_eq!(
            stock_changes_for_edit(false, &before, &after, &traced),
            plan(&before, &after, &traced)
        );
    }

    /// Правка в админке скидки не пересчитывает: своя цена — без скидки, только количество —
    /// скидка остаётся, ручная доставка — без скидки.
    #[sqlx::test]
    async fn admin_edit_keeps_base_prices(db: sqlx::PgPool) {
        use crate::smoke_tests::post_form;
        use crate::test_support::{admin_cookie, order_fixture, place_order, test_state};
        let f = order_fixture(&db).await;
        for sql in [
            "INSERT INTO deliveries (id, name) VALUES (1, 'Самовывоз'), (2, 'Курьер')",
            "INSERT INTO pay_systems (id, name) VALUES (1, 'QR')",
        ] {
            sqlx::query(sql).execute(&db).await.unwrap();
        }
        let id = place_order(&db, &f, None).await;
        let line: i64 = sqlx::query_scalar("SELECT id FROM cart_items WHERE order_id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        // Заказ со скидками: позиция 120 → 100, доставка 300 → 200
        sqlx::query("UPDATE cart_items SET base_price = 120, discount_price = 20 WHERE id = $1")
            .bind(line)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE shipments SET base_price = 300, price = 200 WHERE order_id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE orders SET delivery_price = 200 WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let path = format!("/admin/shop/orders/{id}/items");
        let edit = |fields: String| {
            let (app, db, cookie, path) = (app.clone(), db.clone(), cookie.clone(), path.clone());
            async move {
                let at: DateTime<Utc> =
                    sqlx::query_scalar("SELECT updated_at FROM orders WHERE id = $1")
                        .bind(id)
                        .fetch_one(&db)
                        .await
                        .unwrap();
                let v = version(&at).replace(':', "%3A").replace('+', "%2B");
                let (status, html) = post_form(
                    &app,
                    &path,
                    &cookie,
                    &format!("version={v}&pay_system_id=1&{fields}"),
                )
                .await;
                assert!(status.is_redirection(), "{status} {html}");
            }
        };
        let state = || async {
            sqlx::query_as::<_, (f64, f64, f64, f64, f64)>(
                "SELECT c.base_price::float8, c.discount_price::float8, c.price::float8,
                        s.base_price::float8, s.price::float8
                 FROM cart_items c JOIN shipments s ON s.order_id = c.order_id WHERE c.id = $1",
            )
            .bind(line)
            .fetch_one(&db)
            .await
            .unwrap()
        };

        edit(format!(
            "item_{line}_quantity=2&item_{line}_price=100&delivery_id=1&delivery_price=200"
        ))
        .await;
        assert_eq!(
            state().await,
            (120.0, 20.0, 100.0, 300.0, 200.0),
            "только количество"
        );

        edit(format!(
            "item_{line}_quantity=2&item_{line}_price=80&delivery_id=1&delivery_price=200"
        ))
        .await;
        assert_eq!(state().await, (80.0, 0.0, 80.0, 300.0, 200.0), "своя цена");

        edit(format!(
            "item_{line}_quantity=2&item_{line}_price=80&delivery_id=2&delivery_price=350"
        ))
        .await;
        assert_eq!(
            state().await,
            (80.0, 0.0, 80.0, 350.0, 350.0),
            "другая доставка"
        );
    }

    /// Новая позиция в заказе в рублях: цена товара в USD пересчитывается в валюту заказа.
    #[sqlx::test]
    async fn order_edit_converts_to_order_currency(db: sqlx::PgPool) {
        use crate::test_support::{cart_fixture, place_order, seed_usd};
        let fx = cart_fixture(&db).await;
        seed_usd(&db, Some(80.0)).await;
        sqlx::query("UPDATE catalog_prices SET price = 12.5, currency = 'USD' WHERE item_id = $1")
            .bind(fx.f.product_id)
            .execute(&db)
            .await
            .unwrap();
        let id = place_order(&db, &fx.f, None).await;
        let at: DateTime<Utc> = sqlx::query_scalar("SELECT updated_at FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        let c = Composition {
            lines: vec![EditLine {
                item_id: None,
                product_id: fx.f.product_id,
                store_id: None,
                quantity: 1.0,
                price: 0.0,
                custom_price: false,
                name: String::new(),
            }],
            delivery_id: 1,
            delivery_price: 0.0,
            pay_system_id: 1,
        };
        let result = save_composition(&db, id, &c, &version(&at)).await.unwrap();
        assert_eq!(result, Ok(()));
        let price: f64 = sqlx::query_scalar(
            "SELECT price::float8 FROM cart_items WHERE order_id = $1 AND price <> 100",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(price, 1000.0);
    }
}
