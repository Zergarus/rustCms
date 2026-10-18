//! Цена со скидкой в каталоге (`/api/v1`, bxapi): правила товара, применённые к одной единице.

use std::collections::HashMap;

use chrono::Utc;
use sqlx::PgPool;

use super::{context, engine, repo};
use crate::bxapi::project::DiscountConditionHook;

/// Цены со скидкой записей каталога: (запись, цена единицы до скидок) → цена после правил
/// товара для посетителя `user_id` (гость — `None`). Правила и группы читаются один раз на
/// вызов; без правил товара запросов к данным записей нет. В ответе — только записи, цену
/// которых правила изменили.
pub async fn discount_prices(
    db: &PgPool,
    hook: Option<&dyn DiscountConditionHook>,
    user_id: Option<i64>,
    products: &[(i64, f64)],
) -> sqlx::Result<HashMap<i64, f64>> {
    if products.is_empty() {
        return Ok(HashMap::new());
    }
    let now = Utc::now();
    let rules = repo::active_rules(db, now).await?;
    let rules = engine::product_rules(&rules);
    if rules.is_empty() {
        return Ok(HashMap::new());
    }
    let mut conn = db.acquire().await?;
    let group_ids = context::user_group_ids(&mut conn, user_id).await?;
    let items = context::for_products(&mut conn, products).await?;
    Ok(items
        .iter()
        .filter_map(|it| {
            let price = engine::catalog_price_with(&rules, it, &group_ids, now, hook)?;
            Some((it.key, price))
        })
        .collect())
}
