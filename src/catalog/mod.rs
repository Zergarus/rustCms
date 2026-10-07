//! Торговый каталог (аналог модуля `catalog`): цены, остатки и правила покупки товара.

use std::collections::{HashMap, HashSet};

use sqlx::{FromRow, PgConnection, PgPool};

/// Цена товара одного типа.
#[derive(Debug, Clone, PartialEq)]
pub struct Price {
    pub type_id: i64,
    pub type_name: String,
    pub is_base: bool,
    pub price: f64,
    pub currency: String,
    pub quantity_from: Option<i32>,
    pub quantity_to: Option<i32>,
}

/// Всё, что нужно, чтобы решить, можно ли купить товар и почём.
#[derive(Debug, Clone)]
pub struct PurchaseInfo {
    pub active: bool,
    /// Инфоблок товара — торговый каталог.
    pub is_catalog: bool,
    pub available: bool,
    pub quantity_trace: bool,
    pub can_buy_zero: bool,
    /// Цены по типам, в порядке сортировки типа.
    pub prices: Vec<Price>,
    /// Склад → остаток.
    pub amounts: HashMap<i64, f64>,
    /// Общий остаток (`catalog_products.quantity`).
    pub total: f64,
}

/// Основная цена: базового типа, иначе первая по сортировке типа.
pub fn main_price(prices: &[Price]) -> Option<&Price> {
    prices.iter().find(|p| p.is_base).or_else(|| prices.first())
}

/// Можно ли купить `quantity`: при учёте количества без покупки «в ноль» — не больше
/// остатка на складе (без склада — общего остатка).
pub fn check_quantity(
    info: &PurchaseInfo,
    store_id: Option<i64>,
    quantity: f64,
) -> Result<(), String> {
    if !info.quantity_trace || info.can_buy_zero {
        return Ok(());
    }
    let limit = match store_id {
        Some(store) => info.amounts.get(&store).copied().unwrap_or(0.0),
        None => info.total,
    };
    if quantity <= limit {
        Ok(())
    } else {
        let shown = if limit.fract() == 0.0 {
            format!("{}", limit as i64)
        } else {
            format!("{limit}")
        };
        Err(format!("Доступно {shown} шт."))
    }
}

#[derive(FromRow)]
struct PriceRow {
    element_id: i64,
    type_id: i64,
    type_name: String,
    is_base: bool,
    price: f64,
    currency: String,
    quantity_from: Option<i32>,
    quantity_to: Option<i32>,
}

/// Цены товаров: элемент → цены в порядке сортировки типа.
pub async fn load_prices(
    db: &PgPool,
    element_ids: &[i64],
) -> sqlx::Result<HashMap<i64, Vec<Price>>> {
    let mut conn = db.acquire().await?;
    load_prices_on(&mut conn, element_ids).await
}

async fn load_prices_on(
    db: &mut PgConnection,
    element_ids: &[i64],
) -> sqlx::Result<HashMap<i64, Vec<Price>>> {
    let rows: Vec<PriceRow> = sqlx::query_as(
        "SELECT p.element_id, p.price_type_id AS type_id, t.name AS type_name, t.is_base,
                p.price::float8 AS price, p.currency, p.quantity_from, p.quantity_to
         FROM catalog_prices p JOIN catalog_price_types t ON t.id = p.price_type_id
         WHERE p.element_id = ANY($1)
         ORDER BY p.element_id, t.sort, p.price_type_id, p.quantity_from NULLS FIRST",
    )
    .bind(element_ids)
    .fetch_all(db)
    .await?;
    let mut out: HashMap<i64, Vec<Price>> = HashMap::new();
    for r in rows {
        out.entry(r.element_id).or_default().push(Price {
            type_id: r.type_id,
            type_name: r.type_name,
            is_base: r.is_base,
            price: r.price,
            currency: r.currency,
            quantity_from: r.quantity_from,
            quantity_to: r.quantity_to,
        });
    }
    Ok(out)
}

#[derive(FromRow)]
struct ProductRow {
    element_id: i64,
    active: bool,
    is_catalog: bool,
    available: Option<bool>,
    quantity_trace: Option<bool>,
    can_buy_zero: Option<bool>,
    total: Option<f64>,
}

/// Флаг каталога по умолчанию из `options` (`Y`/`N`).
async fn default_flag(db: &mut PgConnection, name: &str, fallback: bool) -> sqlx::Result<bool> {
    let value: Option<String> =
        sqlx::query_scalar("SELECT value FROM options WHERE module = 'catalog' AND name = $1")
            .bind(name)
            .fetch_optional(db)
            .await?;
    Ok(value.map_or(fallback, |v| v == "Y"))
}

/// Данные покупки для товаров пачкой. Товара без строки в `catalog_products` —
/// флаги по умолчанию и общий остаток 0.
pub async fn load(db: &PgPool, element_ids: &[i64]) -> sqlx::Result<HashMap<i64, PurchaseInfo>> {
    let mut conn = db.acquire().await?;
    load_on(&mut conn, element_ids).await
}

/// То же, что [`load`], но в транзакции и с блокировкой строк товаров и остатков
/// (в порядке id — два оформления не заблокируют друг друга). Товару без строки в
/// `catalog_products` она создаётся, чтобы было что блокировать.
pub async fn load_locked(
    tx: &mut PgConnection,
    element_ids: &[i64],
) -> sqlx::Result<HashMap<i64, PurchaseInfo>> {
    sqlx::query(
        "INSERT INTO catalog_products (element_id)
         SELECT id FROM iblock_elements WHERE id = ANY($1) ON CONFLICT DO NOTHING",
    )
    .bind(element_ids)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "SELECT element_id FROM catalog_products WHERE element_id = ANY($1)
         ORDER BY element_id FOR UPDATE",
    )
    .bind(element_ids)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "SELECT element_id FROM catalog_store_amounts WHERE element_id = ANY($1)
         ORDER BY element_id, store_id FOR UPDATE",
    )
    .bind(element_ids)
    .execute(&mut *tx)
    .await?;
    load_on(tx, element_ids).await
}

async fn load_on(
    db: &mut PgConnection,
    element_ids: &[i64],
) -> sqlx::Result<HashMap<i64, PurchaseInfo>> {
    if element_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let default_trace = default_flag(db, "default_quantity_trace", true).await?;
    let default_zero = default_flag(db, "default_can_buy_zero", false).await?;
    let products: Vec<ProductRow> = sqlx::query_as(
        "SELECT e.id AS element_id, e.active, i.is_catalog,
                p.available, p.quantity_trace, p.can_buy_zero, p.quantity::float8 AS total
         FROM iblock_elements e
         JOIN iblocks i ON i.id = e.iblock_id
         LEFT JOIN catalog_products p ON p.element_id = e.id
         WHERE e.id = ANY($1)",
    )
    .bind(element_ids)
    .fetch_all(&mut *db)
    .await?;
    let mut prices = load_prices_on(&mut *db, element_ids).await?;
    let mut amounts: HashMap<i64, HashMap<i64, f64>> = HashMap::new();
    for (element, store, amount) in sqlx::query_as::<_, (i64, i64, f64)>(
        "SELECT element_id, store_id, amount::float8 FROM catalog_store_amounts WHERE element_id = ANY($1)",
    )
    .bind(element_ids)
    .fetch_all(db)
    .await?
    {
        amounts.entry(element).or_default().insert(store, amount);
    }
    Ok(products
        .into_iter()
        .map(|p| {
            let info = PurchaseInfo {
                active: p.active,
                is_catalog: p.is_catalog,
                available: p.available.unwrap_or(true),
                quantity_trace: p.quantity_trace.unwrap_or(default_trace),
                can_buy_zero: p.can_buy_zero.unwrap_or(default_zero),
                prices: prices.remove(&p.element_id).unwrap_or_default(),
                amounts: amounts.remove(&p.element_id).unwrap_or_default(),
                total: p.total.unwrap_or(0.0),
            };
            (p.element_id, info)
        })
        .collect())
}

/// Вкладка «Торговый каталог» товара: цены по типам (`None` — удалить цену),
/// остатки по складам, флаги (`None` — «по умолчанию»).
/// Типы цен с диапазонами по количеству (несколько строк или границы диапазона):
/// одной ценой в форме их не задать, вкладка товара их не меняет.
pub fn tiered_price_types(prices: &[Price]) -> HashSet<i64> {
    let mut rows: HashMap<i64, usize> = HashMap::new();
    let mut tiered = HashSet::new();
    for p in prices {
        *rows.entry(p.type_id).or_default() += 1;
        if p.quantity_from.is_some() || p.quantity_to.is_some() {
            tiered.insert(p.type_id);
        }
    }
    tiered.extend(rows.into_iter().filter(|(_, n)| *n > 1).map(|(t, _)| t));
    tiered
}

#[derive(Debug, Clone, PartialEq)]
pub struct PurchaseInput {
    pub prices: Vec<(i64, Option<f64>)>,
    pub amounts: Vec<(i64, f64)>,
    /// Общий остаток (`QUANTITY`); `None` — сумма остатков по складам.
    pub quantity: Option<f64>,
    pub available: bool,
    pub quantity_trace: Option<bool>,
    pub can_buy_zero: Option<bool>,
}

/// Флаги товара как сохранены: (доступен, учёт количества, покупка при нуле); `None` — нет записи.
pub async fn raw_flags(
    db: &PgPool,
    element_id: i64,
) -> sqlx::Result<Option<(bool, Option<bool>, Option<bool>)>> {
    sqlx::query_as("SELECT available, quantity_trace, can_buy_zero FROM catalog_products WHERE element_id = $1")
        .bind(element_id)
        .fetch_optional(db)
        .await
}

/// Сохраняет цены, остатки и флаги товара; общий остаток не задан — сумма по складам.
pub async fn save_purchase(
    db: &PgPool,
    element_id: i64,
    input: &PurchaseInput,
) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    let default_currency: String = sqlx::query_scalar(
        "SELECT COALESCE((SELECT code FROM currencies ORDER BY code = 'RUB' DESC, code LIMIT 1), 'RUB')",
    )
    .fetch_one(&mut *tx)
    .await?;
    for (type_id, value) in &input.prices {
        // Валюта прежней цены сохраняется
        let old: Option<String> = sqlx::query_scalar(
            "DELETE FROM catalog_prices WHERE element_id = $1 AND price_type_id = $2 RETURNING currency",
        )
        .bind(element_id)
        .bind(type_id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .next();
        if let Some(price) = value {
            sqlx::query(
                "INSERT INTO catalog_prices (element_id, price_type_id, price, currency) VALUES ($1, $2, $3, $4)",
            )
            .bind(element_id)
            .bind(type_id)
            .bind(price)
            .bind(old.unwrap_or_else(|| default_currency.clone()))
            .execute(&mut *tx)
            .await?;
        }
    }
    for (store_id, amount) in &input.amounts {
        sqlx::query(
            "INSERT INTO catalog_store_amounts (element_id, store_id, amount) VALUES ($1, $2, $3)
             ON CONFLICT (element_id, store_id) DO UPDATE SET amount = EXCLUDED.amount",
        )
        .bind(element_id)
        .bind(store_id)
        .bind(amount)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO catalog_products (element_id, quantity, available, quantity_trace, can_buy_zero)
         VALUES ($1, COALESCE($5, (SELECT COALESCE(sum(amount), 0) FROM catalog_store_amounts WHERE element_id = $1)), $2, $3, $4)
         ON CONFLICT (element_id) DO UPDATE SET quantity = EXCLUDED.quantity, available = EXCLUDED.available,
             quantity_trace = EXCLUDED.quantity_trace, can_buy_zero = EXCLUDED.can_buy_zero",
    )
    .bind(element_id)
    .bind(input.available)
    .bind(input.quantity_trace)
    .bind(input.can_buy_zero)
    .bind(input.quantity)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    fn price(type_id: i64, is_base: bool, value: f64) -> Price {
        Price {
            type_id,
            type_name: format!("Тип {type_id}"),
            is_base,
            price: value,
            currency: "RUB".into(),
            quantity_from: None,
            quantity_to: None,
        }
    }

    fn info(trace: bool, buy_zero: bool, amounts: &[(i64, f64)], total: f64) -> PurchaseInfo {
        PurchaseInfo {
            active: true,
            is_catalog: true,
            available: true,
            quantity_trace: trace,
            can_buy_zero: buy_zero,
            prices: vec![price(1, true, 100.0)],
            amounts: amounts.iter().copied().collect::<HashMap<_, _>>(),
            total,
        }
    }

    #[test]
    fn tiered_price_types_detects_ranges() {
        let mut tier = price(2, false, 90.0);
        tier.quantity_from = Some(10);
        let mut ranged = price(4, false, 70.0);
        ranged.quantity_to = Some(5);
        let prices = [
            price(1, true, 100.0),
            price(2, false, 95.0),
            tier,
            price(3, false, 80.0),
            ranged,
        ];
        assert_eq!(tiered_price_types(&prices), HashSet::from([2, 4]));
    }

    #[test]
    fn main_price_prefers_base() {
        let prices = [price(3, false, 597.4), price(1, true, 600.0)];
        assert_eq!(main_price(&prices).map(|p| p.type_id), Some(1));
    }

    #[test]
    fn main_price_first_when_no_base() {
        let prices = [price(3, false, 597.4), price(5, false, 600.0)];
        assert_eq!(main_price(&prices).map(|p| p.type_id), Some(3));
    }

    #[test]
    fn main_price_none_when_empty() {
        assert!(main_price(&[]).is_none());
    }

    #[test]
    fn quantity_limited_by_store() {
        let i = info(true, false, &[(5, 3.0)], 10.0);
        assert_eq!(
            check_quantity(&i, Some(5), 4.0),
            Err("Доступно 3 шт.".to_string())
        );
        assert_eq!(check_quantity(&i, Some(5), 3.0), Ok(()));
    }

    #[test]
    fn quantity_limited_by_total_without_store() {
        let i = info(true, false, &[], 2.0);
        assert_eq!(
            check_quantity(&i, None, 3.0),
            Err("Доступно 2 шт.".to_string())
        );
    }

    #[test]
    fn can_buy_zero_ignores_stock() {
        let i = info(true, true, &[(5, 0.0)], 0.0);
        assert_eq!(check_quantity(&i, Some(5), 7.0), Ok(()));
    }

    #[test]
    fn no_trace_ignores_stock() {
        let i = info(false, false, &[(5, 0.0)], 0.0);
        assert_eq!(check_quantity(&i, Some(5), 7.0), Ok(()));
    }
}
