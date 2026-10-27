//! Проверка наличия при оформлении и правке заказа; резервы — `crate::stock::reserve`.

use std::collections::{BTreeMap, HashMap};

use crate::catalog::{PurchaseInfo, check_quantity};

/// Количество товара в позиции заказа (склад — если выбран).
#[derive(Debug, Clone, PartialEq)]
pub struct StockLine {
    pub product_id: i64,
    pub store_id: Option<i64>,
    pub quantity: f64,
}

fn totals(lines: &[StockLine]) -> BTreeMap<(i64, Option<i64>), f64> {
    let mut out = BTreeMap::new();
    for l in lines {
        *out.entry((l.product_id, l.store_id)).or_insert(0.0) += l.quantity;
    }
    out
}

/// Хватает ли остатков на позиции (одинаковый товар на одном складе суммируется).
pub fn check(lines: &[StockLine], info: &HashMap<i64, PurchaseInfo>) -> Result<(), String> {
    for ((element, store), quantity) in totals(lines) {
        let product = info
            .get(&element)
            .ok_or_else(|| "Товар недоступен для покупки".to_string())?;
        check_quantity(product, store, quantity)?;
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
                product_type: crate::catalog::TYPE_SIMPLE,
                parent_id: None,
                parent_active: true,
                quantity_trace: true,
                can_buy_zero: false,
                prices: Vec::<Price>::new(),
                amounts: HashMap::from([(5, 3.0)]),
                total: 10.0,
            },
        )])
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
