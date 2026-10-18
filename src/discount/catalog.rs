//! Цена со скидкой в каталоге (`/api/v1`, bxapi): правила товара, применённые к одной единице.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::model::Rule;
use super::{context, engine, repo};
use crate::bxapi::project::DiscountConditionHook;

/// Действующие правила товара (`product_rules`), прочитанные один раз на запрос.
pub struct CatalogRules {
    rules: Vec<Rule>,
    now: DateTime<Utc>,
}

impl CatalogRules {
    /// `None` — правил товара нет: цены и данные записей читать не нужно.
    pub async fn load(db: &PgPool) -> sqlx::Result<Option<CatalogRules>> {
        let now = Utc::now();
        let all = repo::active_rules(db, now).await?;
        let rules: Vec<Rule> = engine::product_rules(&all).into_iter().cloned().collect();
        Ok((!rules.is_empty()).then_some(CatalogRules { rules, now }))
    }

    /// Цены со скидкой записей: (запись, цена единицы до скидок) → цена после правил для
    /// посетителя `user_id` (гость — `None`, без групп). Только записи, цену которых правила
    /// изменили.
    pub async fn prices(
        &self,
        db: &PgPool,
        hook: Option<&dyn DiscountConditionHook>,
        user_id: Option<i64>,
        products: &[(i64, f64)],
    ) -> sqlx::Result<HashMap<i64, f64>> {
        if products.is_empty() {
            return Ok(HashMap::new());
        }
        let rules: Vec<&Rule> = self.rules.iter().collect();
        let mut conn = db.acquire().await?;
        let group_ids = context::user_group_ids(&mut conn, user_id).await?;
        let items = context::for_products(&mut conn, products).await?;
        Ok(items
            .iter()
            .filter_map(|it| {
                let price =
                    engine::catalog_price_with(&rules, it, user_id, &group_ids, self.now, hook)?;
                Some((it.key, price))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[sqlx::test]
    async fn load_is_none_without_product_rules(db: PgPool) {
        assert!(CatalogRules::load(&db).await.unwrap().is_none());
        // Условие корзины, купон, неактивное — не правила товара
        for sql in [
            "INSERT INTO discounts (name, use_coupons) VALUES ('Купон', TRUE)",
            "INSERT INTO discounts (name, active) VALUES ('Выкл', FALSE)",
        ] {
            sqlx::query(sql).execute(&db).await.unwrap();
        }
        sqlx::query("INSERT INTO discounts (name, conditions) VALUES ('Корзина', $1)")
            .bind(json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
                {"CLASS_ID": "CondSaleOrderSumm", "DATA": {"logic": "Great", "value": 1}}
            ]}))
            .execute(&db)
            .await
            .unwrap();
        assert!(CatalogRules::load(&db).await.unwrap().is_none());
        sqlx::query("INSERT INTO discounts (name) VALUES ('Товар')")
            .execute(&db)
            .await
            .unwrap();
        assert!(CatalogRules::load(&db).await.unwrap().is_some());
    }
}
