//! Себестоимость позиций заказа, маржа и стоимость остатков по партиям.
//!
//! Себестоимость единицы = (Σ количество × цена партии + непокрытое × закупочная цена товара) /
//! количество, в валюте заказа. Стоимость остатков — в валюте сайта.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use serde::Serialize;
use sqlx::PgConnection;

use super::batches::{self, Batch};
use crate::currency::{Converter, site_currency};

/// Погрешность сравнения количеств.
const EPS: f64 = 1e-9;

/// Как получена себестоимость позиции.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum CostKind {
    /// По движениям партий отгрузки
    Actual,
    /// «оценка» — отгрузки ещё не было, по текущим партиям склада
    Estimate,
    /// «по закупочной цене» — метод не выбран
    ByPurchasePrice,
}

/// Себестоимость единицы позиции заказа; `None` — ни партий, ни закупочной цены
/// (или нет курса для пересчёта в валюту заказа).
#[derive(Debug, Clone, Serialize)]
pub struct LineCost {
    pub cart_item_id: i64,
    pub unit_cost: Option<f64>,
    pub kind: CostKind,
}

/// Строка позиции: `(id, товар, склад, количество, закупочная цена, её валюта)`.
type LineRow = (i64, i64, Option<i64>, f64, Option<f64>, Option<String>);

/// Позиция заказа с закупочной ценой товара.
struct OrderLine {
    id: i64,
    item_id: i64,
    store_id: Option<i64>,
    quantity: f64,
    purchase: Option<(f64, String)>,
}

/// Покрытая партиями часть позиции и непокрытое количество.
#[derive(Default)]
struct Spent {
    /// `(количество, цена, валюта)`
    parts: Vec<(f64, f64, String)>,
    uncovered: f64,
}

/// Себестоимость позиций заказа в валюте заказа, в порядке позиций.
pub async fn order_costs(db: &mut PgConnection, order_id: i64) -> sqlx::Result<Vec<LineCost>> {
    let Some(currency): Option<String> =
        sqlx::query_scalar("SELECT currency FROM orders WHERE id = $1")
            .bind(order_id)
            .fetch_optional(&mut *db)
            .await?
    else {
        return Ok(Vec::new());
    };
    let site = site_currency(&mut *db).await?;
    let or_site = |c: String| if c.is_empty() { site.clone() } else { c };

    let rows: Vec<LineRow> = sqlx::query_as(
        "SELECT c.id, c.item_id, c.store_id, c.quantity::float8,
                p.purchasing_price::float8, p.purchasing_currency
         FROM cart_items c LEFT JOIN catalog_products p ON p.item_id = c.item_id
         WHERE c.order_id = $1 ORDER BY c.id",
    )
    .bind(order_id)
    .fetch_all(&mut *db)
    .await?;
    let lines: Vec<OrderLine> = rows
        .into_iter()
        .map(
            |(id, item_id, store_id, quantity, price, purchase_currency)| OrderLine {
                id,
                item_id,
                store_id,
                quantity,
                purchase: price.map(|p| (p, or_site(purchase_currency.unwrap_or_default()))),
            },
        )
        .collect();

    let method = batches::method(&mut *db).await?;
    let main: Option<(i64, bool)> = sqlx::query_as(
        "SELECT id, deducted FROM shipments WHERE order_id = $1 ORDER BY id LIMIT 1",
    )
    .bind(order_id)
    .fetch_optional(&mut *db)
    .await?;

    let (kind, spent) = match (method, main) {
        (None, _) => (
            CostKind::ByPurchasePrice,
            lines
                .iter()
                .map(|l| Spent {
                    parts: Vec::new(),
                    uncovered: l.quantity,
                })
                .collect(),
        ),
        (Some(_), Some((shipment, true))) => {
            (CostKind::Actual, actual(&mut *db, shipment, &lines).await?)
        }
        (Some(_), main) => (
            CostKind::Estimate,
            estimate(&mut *db, main.map(|m| m.0), &lines).await?,
        ),
    };

    let mut from: Vec<String> = Vec::new();
    for (line, s) in lines.iter().zip(&spent) {
        from.extend(s.parts.iter().map(|p| or_site(p.2.clone())));
        from.extend(line.purchase.iter().map(|p| p.1.clone()));
    }
    let from_refs: Vec<&str> = from.iter().map(String::as_str).collect();
    let conv = Converter::new(&mut *db, &currency, &from_refs).await?;

    Ok(lines
        .iter()
        .zip(spent)
        .map(|(line, s)| LineCost {
            cart_item_id: line.id,
            unit_cost: unit_cost(&conv, line, &s, &or_site),
            kind: kind.clone(),
        })
        .collect())
}

/// Себестоимость единицы в валюте конвертера; нет закупочной цены для непокрытого или
/// курса — `None`.
fn unit_cost(
    conv: &Converter,
    line: &OrderLine,
    spent: &Spent,
    or_site: &impl Fn(String) -> String,
) -> Option<f64> {
    if line.quantity <= EPS {
        return None;
    }
    let mut total = 0.0;
    for (amount, price, currency) in &spent.parts {
        total += conv.convert(amount * price, &or_site(currency.clone()))?;
    }
    if spent.uncovered > EPS {
        let (price, currency) = line.purchase.as_ref()?;
        total += conv.convert(spent.uncovered * price, currency)?;
    }
    Some(total / line.quantity)
}

/// По движениям основной отгрузки: расход партий и непокрытый остаток количества позиции.
async fn actual(
    db: &mut PgConnection,
    shipment: i64,
    lines: &[OrderLine],
) -> sqlx::Result<Vec<Spent>> {
    let moves: Vec<(i64, f64, f64, String)> = sqlx::query_as(
        "SELECT s.cart_item_id, -m.amount::float8, m.price::float8, m.currency
         FROM store_batch_moves m JOIN shipment_item_stores s ON s.id = m.shipment_store_id
         WHERE s.shipment_id = $1 AND m.amount < 0 ORDER BY m.id",
    )
    .bind(shipment)
    .fetch_all(db)
    .await?;
    Ok(lines
        .iter()
        .map(|line| {
            let parts: Vec<(f64, f64, String)> = moves
                .iter()
                .filter(|m| m.0 == line.id)
                .map(|m| (m.1, m.2, m.3.clone()))
                .collect();
            let covered: f64 = parts.iter().map(|p| p.0).sum();
            Spent {
                parts,
                uncovered: (line.quantity - covered).max(0.0),
            }
        })
        .collect())
}

/// Оценка до отгрузки: списание по текущим партиям склада позиции без записи. Склады —
/// из `shipment_item_stores` основной отгрузки, иначе склад позиции. Позиции одного товара
/// на одном складе расходуют партии по очереди.
async fn estimate(
    db: &mut PgConnection,
    shipment: Option<i64>,
    lines: &[OrderLine],
) -> sqlx::Result<Vec<Spent>> {
    let chosen: Vec<(i64, i64, f64)> = match shipment {
        Some(shipment) => {
            sqlx::query_as(
                "SELECT cart_item_id, store_id, quantity::float8 FROM shipment_item_stores
                 WHERE shipment_id = $1 ORDER BY id",
            )
            .bind(shipment)
            .fetch_all(&mut *db)
            .await?
        }
        None => Vec::new(),
    };
    let mut stock: HashMap<(i64, i64), Vec<Batch>> = HashMap::new();
    let mut result = Vec::new();
    for line in lines {
        let mut stores: Vec<(i64, f64)> = chosen
            .iter()
            .filter(|c| c.0 == line.id)
            .map(|c| (c.1, c.2))
            .collect();
        if stores.is_empty() {
            stores.extend(line.store_id.map(|s| (s, line.quantity)));
        }
        let mut spent = Spent::default();
        let mut left = line.quantity;
        for (store, amount) in stores {
            let amount = amount.min(left);
            if amount <= EPS {
                continue;
            }
            left -= amount;
            let key = (line.item_id, store);
            let list = match stock.entry(key) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(load_batches(&mut *db, line.item_id, store).await?),
            };
            spent.uncovered += take(list, amount, &mut spent.parts);
        }
        spent.uncovered += left.max(0.0);
        result.push(spent);
    }
    Ok(result)
}

/// Списывает `amount` из партий в памяти; возвращает непокрытое количество. У средней партия
/// на складе одна, правило то же.
fn take(list: &mut [Batch], amount: f64, out: &mut Vec<(f64, f64, String)>) -> f64 {
    let (parts, uncovered) = batches::write_off(list, amount);
    for part in parts {
        if let Some(batch) = list.iter_mut().find(|b| b.id == part.batch_id) {
            batch.available -= part.amount;
            out.push((part.amount, batch.price, batch.currency.clone()));
        }
    }
    uncovered
}

/// Партии товара на складе, без блокировки.
async fn load_batches(db: &mut PgConnection, item: i64, store: i64) -> sqlx::Result<Vec<Batch>> {
    let rows: Vec<(i64, f64, f64, String)> = sqlx::query_as(
        "SELECT id, available::float8, price::float8, currency FROM catalog_store_batches
         WHERE item_id = $1 AND store_id = $2 ORDER BY id",
    )
    .bind(item)
    .bind(store)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, available, price, currency)| Batch {
            id,
            available,
            price,
            currency,
        })
        .collect())
}

/// «Маржа заказа»: выручка позиций с себестоимостью минус их себестоимость.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Margin {
    pub sum: f64,
    /// Процент от выручки позиций с себестоимостью; `None` при нулевой выручке
    pub percent: Option<f64>,
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Маржа по позициям `(количество, цена единицы, себестоимость единицы)`. Позиции без
/// себестоимости не входят; ни у одной нет — `None`. Доставка не входит.
pub fn margin(lines: &[(f64, f64, Option<f64>)]) -> Option<Margin> {
    let mut any = false;
    let mut revenue = 0.0;
    let mut cost = 0.0;
    for &(quantity, price, unit_cost) in lines {
        if let Some(unit_cost) = unit_cost {
            any = true;
            revenue += quantity * price;
            cost += quantity * unit_cost;
        }
    }
    if !any {
        return None;
    }
    let sum = revenue - cost;
    Some(Margin {
        sum: round2(sum),
        percent: (revenue.abs() > EPS).then(|| round2(sum / revenue * 100.0)),
    })
}

/// Стоимость остатков партий по `(товар, склад)`: Σ `available × price` в валюте сайта и
/// Σ `available`. Партия, цену которой нельзя пересчитать, не входит ни в одну сумму.
pub async fn store_values(
    db: &mut PgConnection,
    item_ids: &[i64],
) -> sqlx::Result<HashMap<(i64, i64), (f64, f64)>> {
    let mut result = HashMap::new();
    if item_ids.is_empty() {
        return Ok(result);
    }
    let rows: Vec<(i64, i64, f64, f64, String)> = sqlx::query_as(
        "SELECT item_id, store_id, available::float8, price::float8, currency
         FROM catalog_store_batches WHERE item_id = ANY($1) AND available > 0 ORDER BY id",
    )
    .bind(item_ids)
    .fetch_all(&mut *db)
    .await?;
    let site = site_currency(&mut *db).await?;
    let from: Vec<&str> = rows.iter().map(|r| r.4.as_str()).collect();
    let conv = Converter::new(&mut *db, &site, &from).await?;
    for (item, store, available, price, currency) in rows {
        let currency = if currency.is_empty() {
            &site
        } else {
            &currency
        };
        let Some(value) = conv.convert(available * price, currency) else {
            continue;
        };
        let entry: &mut (f64, f64) = result.entry((item, store)).or_default();
        entry.0 = round2(entry.0 + value);
        entry.1 += available;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn margin_example() {
        let m = margin(&[(2.0, 200.0, Some(125.0)), (1.0, 50.0, None)]).unwrap();
        assert_eq!(m.sum, 150.0);
        assert_eq!(m.percent, Some(37.5));
    }

    #[test]
    fn margin_none_without_costs() {
        assert_eq!(margin(&[(2.0, 200.0, None), (1.0, 50.0, None)]), None);
        assert_eq!(margin(&[]), None);
    }

    #[test]
    fn margin_zero_revenue_no_percent() {
        let m = margin(&[(1.0, 0.0, Some(10.0))]).unwrap();
        assert_eq!(m.sum, -10.0);
        assert_eq!(m.percent, None);
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::stock::batches::Method;
    use crate::stock::batches::db_tests::{arrival, doc, line, order, set_shipped, setup};
    use crate::test_support::seed_usd;
    use sqlx::PgPool;

    async fn order_of(db: &PgPool, shipment: i64) -> i64 {
        sqlx::query_scalar("SELECT order_id FROM shipments WHERE id = $1")
            .bind(shipment)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn set_purchase(db: &PgPool, item: i64, price: Option<f64>, currency: Option<&str>) {
        sqlx::query(
            "UPDATE catalog_products
             SET purchasing_price = $2::float8::numeric, purchasing_currency = $3
             WHERE item_id = $1",
        )
        .bind(item)
        .bind(price)
        .bind(currency)
        .execute(db)
        .await
        .unwrap();
    }

    async fn costs(db: &PgPool, order: i64) -> Vec<LineCost> {
        let mut c = db.acquire().await.unwrap();
        order_costs(&mut c, order).await.unwrap()
    }

    /// Единственная позиция: себестоимость единицы и вид.
    async fn single(db: &PgPool, shipment: i64) -> (Option<f64>, CostKind) {
        let lines = costs(db, order_of(db, shipment).await).await;
        assert_eq!(lines.len(), 1);
        (lines[0].unit_cost, lines[0].kind.clone())
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 0.005)
    }

    async fn batch_moves(db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM store_batch_moves")
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn batch_state(db: &PgPool) -> Vec<(i64, f64)> {
        sqlx::query_as("SELECT id, available::float8 FROM catalog_store_batches ORDER BY id")
            .fetch_all(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn actual_cost_fifo(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        arrival(&db, f.product_id, 1, 10.0, 150.0).await;
        let sh = order(&db, &f, 12.0).await;
        set_shipped(&db, sh, true).await;
        let (cost, kind) = single(&db, sh).await;
        assert_eq!(kind, CostKind::Actual);
        assert!(close(cost, 108.33), "{cost:?}");
    }

    #[sqlx::test]
    async fn actual_cost_with_uncovered(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        arrival(&db, f.product_id, 1, 5.0, 100.0).await;
        set_purchase(&db, f.product_id, Some(120.0), None).await;
        let sh = order(&db, &f, 8.0).await;
        set_shipped(&db, sh, true).await;
        let (cost, kind) = single(&db, sh).await;
        assert_eq!(kind, CostKind::Actual);
        assert!(close(cost, 107.5), "{cost:?}");
        // Без закупочной цены непокрытое не оценить
        set_purchase(&db, f.product_id, None, None).await;
        assert_eq!(single(&db, sh).await, (None, CostKind::Actual));
    }

    #[sqlx::test]
    async fn estimate_before_ship(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        arrival(&db, f.product_id, 1, 10.0, 150.0).await;
        let before = (batch_state(&db).await, batch_moves(&db).await);
        let sh = order(&db, &f, 12.0).await;
        let (cost, kind) = single(&db, sh).await;
        assert_eq!(kind, CostKind::Estimate);
        assert!(close(cost, 108.33), "{cost:?}");
        // Оценка ничего не пишет
        assert_eq!((batch_state(&db).await, batch_moves(&db).await), before);
        // Партий не хватает — непокрытое по закупочной
        let sh2 = order(&db, &f, 25.0).await;
        set_purchase(&db, f.product_id, Some(200.0), None).await;
        let (cost, kind) = single(&db, sh2).await;
        assert_eq!(kind, CostKind::Estimate);
        assert!(
            close(cost, (1000.0 + 1500.0 + 5.0 * 200.0) / 25.0),
            "{cost:?}"
        );
        // Отгрузка — себестоимость по движениям
        set_shipped(&db, sh, true).await;
        let (cost, kind) = single(&db, sh).await;
        assert_eq!(kind, CostKind::Actual);
        assert!(close(cost, 108.33), "{cost:?}");
    }

    #[sqlx::test]
    async fn estimate_average(db: PgPool) {
        let f = setup(&db, Some(Method::Average)).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        arrival(&db, f.product_id, 1, 10.0, 150.0).await;
        let sh = order(&db, &f, 4.0).await;
        assert_eq!(single(&db, sh).await, (Some(125.0), CostKind::Estimate));
    }

    #[sqlx::test]
    async fn by_purchase_price_without_method(db: PgPool) {
        let f = setup(&db, None).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        set_purchase(&db, f.product_id, Some(90.0), None).await;
        let sh = order(&db, &f, 3.0).await;
        assert_eq!(
            single(&db, sh).await,
            (Some(90.0), CostKind::ByPurchasePrice)
        );
        set_shipped(&db, sh, true).await;
        assert_eq!(
            single(&db, sh).await,
            (Some(90.0), CostKind::ByPurchasePrice)
        );
        set_purchase(&db, f.product_id, None, None).await;
        assert_eq!(single(&db, sh).await, (None, CostKind::ByPurchasePrice));
    }

    #[sqlx::test]
    async fn cost_in_order_currency(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        seed_usd(&db, Some(90.0)).await;
        doc(
            &db,
            "A",
            "USD",
            vec![line(f.product_id, None, Some(1), 10.0, 2.0)],
        )
        .await;
        let sh = order(&db, &f, 4.0).await;
        assert_eq!(single(&db, sh).await, (Some(180.0), CostKind::Estimate));
        set_shipped(&db, sh, true).await;
        assert_eq!(single(&db, sh).await, (Some(180.0), CostKind::Actual));
        // Закупочная цена в USD для непокрытого
        let sh2 = order(&db, &f, 8.0).await;
        set_purchase(&db, f.product_id, Some(3.0), Some("USD")).await;
        let (cost, _) = single(&db, sh2).await;
        assert!(close(cost, (6.0 * 180.0 + 2.0 * 270.0) / 8.0), "{cost:?}");
        // Курса нет (валюта не заведена) — себестоимость неизвестна
        for sql in [
            "UPDATE store_batch_moves SET currency = 'EUR'",
            "UPDATE catalog_store_batches SET currency = 'EUR'",
        ] {
            sqlx::query(sql).execute(&db).await.unwrap();
        }
        assert_eq!(single(&db, sh).await, (None, CostKind::Actual));
        assert_eq!(single(&db, sh2).await, (None, CostKind::Estimate));
        assert_eq!(single(&db, sh2).await, (None, CostKind::Estimate));
    }

    #[sqlx::test]
    async fn store_values_sum(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        seed_usd(&db, Some(90.0)).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        arrival(&db, f.product_id, 1, 4.0, 150.0).await;
        doc(
            &db,
            "A",
            "USD",
            vec![line(f.product_id, None, Some(2), 3.0, 2.0)],
        )
        .await;
        // Израсходованная партия не входит
        let sh = order(&db, &f, 10.0).await;
        set_shipped(&db, sh, true).await;
        let mut c = db.acquire().await.unwrap();
        let values = store_values(&mut c, &[f.product_id]).await.unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[&(f.product_id, 1)], (600.0, 4.0));
        assert_eq!(values[&(f.product_id, 2)], (540.0, 3.0));
        assert!(store_values(&mut c, &[]).await.unwrap().is_empty());
    }
}
