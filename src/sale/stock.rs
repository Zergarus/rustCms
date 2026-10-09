//! Остатки при оформлении, отмене и правке заказа: план изменений — чистая функция,
//! применение — в транзакции под блокировкой строк (`catalog::load_locked`).

use std::collections::{BTreeMap, HashMap, HashSet};

use sqlx::PgConnection;

use crate::catalog::{PurchaseInfo, check_quantity};

/// Количество товара в позиции заказа (склад — если выбран).
#[derive(Debug, Clone, PartialEq)]
pub struct StockLine {
    pub product_id: i64,
    pub store_id: Option<i64>,
    pub quantity: f64,
}

/// Изменение остатка: отрицательное — списание, положительное — возврат.
#[derive(Debug, Clone, PartialEq)]
pub struct StockChange {
    pub product_id: i64,
    pub store_id: Option<i64>,
    pub delta: f64,
}

fn totals(lines: &[StockLine], traced: &HashSet<i64>) -> BTreeMap<(i64, Option<i64>), f64> {
    let mut out = BTreeMap::new();
    for l in lines.iter().filter(|l| traced.contains(&l.product_id)) {
        *out.entry((l.product_id, l.store_id)).or_insert(0.0) += l.quantity;
    }
    out
}

/// Что сделать с остатками при переходе от позиций `before` к `after` (оформление —
/// `before` пуст, отмена — `after` пуст). Учитываются только товары из `traced`.
pub fn plan(before: &[StockLine], after: &[StockLine], traced: &HashSet<i64>) -> Vec<StockChange> {
    let was = totals(before, traced);
    let now = totals(after, traced);
    let keys: std::collections::BTreeSet<_> = was.keys().chain(now.keys()).copied().collect();
    keys.into_iter()
        .filter_map(|key| {
            let delta =
                was.get(&key).copied().unwrap_or(0.0) - now.get(&key).copied().unwrap_or(0.0);
            (delta.abs() > 1e-9).then_some(StockChange {
                product_id: key.0,
                store_id: key.1,
                delta,
            })
        })
        .collect()
}

/// Хватает ли остатков на позиции (одинаковый товар на одном складе суммируется).
pub fn check(lines: &[StockLine], info: &HashMap<i64, PurchaseInfo>) -> Result<(), String> {
    let all: HashSet<i64> = lines.iter().map(|l| l.product_id).collect();
    for ((element, store), quantity) in totals(lines, &all) {
        let product = info
            .get(&element)
            .ok_or_else(|| "Товар недоступен для покупки".to_string())?;
        check_quantity(product, store, quantity)?;
    }
    Ok(())
}

/// Применяет изменения: общий остаток товара и остаток склада (если запись есть).
pub async fn apply(tx: &mut PgConnection, changes: &[StockChange]) -> sqlx::Result<()> {
    let mut per_product: BTreeMap<i64, f64> = BTreeMap::new();
    for c in changes {
        *per_product.entry(c.product_id).or_insert(0.0) += c.delta;
        if let Some(store) = c.store_id {
            sqlx::query(
                "UPDATE catalog_store_amounts SET amount = amount + $3
                 WHERE item_id = $1 AND store_id = $2",
            )
            .bind(c.product_id)
            .bind(store)
            .bind(c.delta)
            .execute(&mut *tx)
            .await?;
        }
    }
    for (element, delta) in per_product {
        sqlx::query("UPDATE catalog_products SET quantity = quantity + $2 WHERE item_id = $1")
            .bind(element)
            .bind(delta)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Price;

    fn line(e: i64, s: Option<i64>, q: f64) -> StockLine {
        StockLine {
            product_id: e,
            store_id: s,
            quantity: q,
        }
    }

    fn info() -> HashMap<i64, PurchaseInfo> {
        HashMap::from([(
            1,
            PurchaseInfo {
                active: true,
                is_catalog: true,
                available: true,
                quantity_trace: true,
                can_buy_zero: false,
                prices: Vec::<Price>::new(),
                amounts: HashMap::from([(5, 3.0)]),
                total: 10.0,
            },
        )])
    }

    #[test]
    fn plan_checkout_and_cancel() {
        let traced = HashSet::from([1, 2]);
        let lines = [
            line(1, Some(5), 2.0),
            line(2, None, 1.0),
            line(1, Some(5), 1.0),
        ];
        assert_eq!(
            plan(&[], &lines, &traced),
            vec![
                StockChange {
                    product_id: 1,
                    store_id: Some(5),
                    delta: -3.0
                },
                StockChange {
                    product_id: 2,
                    store_id: None,
                    delta: -1.0
                },
            ]
        );
        assert_eq!(plan(&lines, &[], &traced)[0].delta, 3.0);
        assert!(plan(&lines, &lines, &traced).is_empty());
    }

    #[test]
    fn plan_edit_store_and_quantity() {
        let traced = HashSet::from([1]);
        let changes = plan(&[line(1, Some(5), 2.0)], &[line(1, Some(6), 3.0)], &traced);
        assert_eq!(
            changes,
            vec![
                StockChange {
                    product_id: 1,
                    store_id: Some(5),
                    delta: 2.0
                },
                StockChange {
                    product_id: 1,
                    store_id: Some(6),
                    delta: -3.0
                },
            ]
        );
    }

    #[test]
    fn plan_ignores_untraced() {
        assert!(plan(&[], &[line(3, Some(5), 2.0)], &HashSet::from([1])).is_empty());
    }

    #[test]
    fn check_sums_same_product_and_store() {
        assert!(check(&[line(1, Some(5), 2.0), line(1, Some(5), 1.0)], &info()).is_ok());
        assert_eq!(
            check(&[line(1, Some(5), 2.0), line(1, Some(5), 2.0)], &info()).unwrap_err(),
            "Доступно 3 шт."
        );
        assert_eq!(
            check(&[line(1, None, 11.0)], &info()).unwrap_err(),
            "Доступно 10 шт."
        );
        assert!(check(&[line(7, None, 1.0)], &info()).is_err()); // нет данных о товаре
    }
}
