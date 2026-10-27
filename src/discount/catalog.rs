//! Цена со скидкой в каталоге (`/api/v1`, bxapi): правила товара, применённые к одной единице.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::model::Rule;
use super::{context, engine, repo};
use crate::bxapi::project::DiscountConditionHook;
use crate::catalog::rounding::{self, Rounding};
use crate::currency;

/// Запись каталога для расчёта цены со скидкой: основная цена в её валюте.
pub struct CatalogProduct {
    pub id: i64,
    pub price: f64,
    pub price_type_id: Option<i64>,
    pub currency: String,
}

/// Действующие правила товара (`product_rules`), прочитанные один раз на запрос.
pub struct CatalogRules {
    rules: Vec<Rule>,
    /// Правила округления цен: цена со скидкой округляется по типу цены записи.
    rounding: Option<Rounding>,
    now: DateTime<Utc>,
}

impl CatalogRules {
    /// `None` — правил товара нет: цены и данные записей читать не нужно.
    pub async fn load(db: &PgPool) -> sqlx::Result<Option<CatalogRules>> {
        let Some(mut rules) = Self::load_rules(db).await? else {
            return Ok(None);
        };
        rules.rounding = rounding::load(&mut *db.acquire().await?).await?;
        Ok(Some(rules))
    }

    /// Только правила, без округления: вызывающий, читавший округление сам (bxapi — один раз
    /// на запрос), задаёт его через [`CatalogRules::with_rounding`].
    pub async fn load_rules(db: &PgPool) -> sqlx::Result<Option<CatalogRules>> {
        let now = Utc::now();
        let all = repo::active_rules(db, now).await?;
        let rules: Vec<Rule> = engine::product_rules(&all).into_iter().cloned().collect();
        Ok((!rules.is_empty()).then_some(CatalogRules {
            rules,
            rounding: None,
            now,
        }))
    }

    pub fn with_rounding(mut self, rounding: Option<Rounding>) -> Self {
        self.rounding = rounding;
        self
    }

    /// Цены со скидкой записей: (запись, цена единицы до скидок, тип цены, валюта) → цена после
    /// правил для посетителя `user_id` (гость — `None`, без групп). Только записи, цену которых
    /// правила изменили. Цена в чужой валюте пересчитывается в валюту сайта, правила
    /// применяются там, результат возвращается в валюте цены; нет курса — записи в ответе нет.
    pub async fn prices(
        &self,
        db: &PgPool,
        hook: Option<&dyn DiscountConditionHook>,
        user_id: Option<i64>,
        products: &[CatalogProduct],
    ) -> sqlx::Result<HashMap<i64, f64>> {
        if products.is_empty() {
            return Ok(HashMap::new());
        }
        let rules: Vec<&Rule> = self.rules.iter().collect();
        let mut conn = db.acquire().await?;
        let group_ids = context::user_group_ids(&mut conn, user_id).await?;
        let site = currency::site_currency(&mut conn).await?;
        // Курсы читаются один раз и только при цене не в валюте сайта
        let rates = if products.iter().any(|p| p.currency != site) {
            Some(currency::load_rates(&mut conn).await?)
        } else {
            None
        };
        let today = currency::today();
        let mut in_site = Vec::with_capacity(products.len());
        let mut currencies: HashMap<i64, &str> = HashMap::new();
        for p in products {
            let price = match &rates {
                Some(r) if p.currency != site => {
                    if r.factor(&p.currency, &site, today) == 0.0 {
                        continue;
                    }
                    r.convert(p.price, &p.currency, &site, today)
                }
                _ => p.price,
            };
            currencies.insert(p.id, &p.currency);
            in_site.push((p.id, price, p.price_type_id));
        }
        let items = context::for_products(&mut conn, &in_site).await?;
        Ok(items
            .iter()
            .filter_map(|it| {
                let price = engine::catalog_price_with(
                    &rules,
                    it,
                    user_id,
                    &group_ids,
                    self.now,
                    hook,
                    self.rounding.as_ref(),
                )?;
                let cur = currencies.get(&it.key).copied().unwrap_or(&site);
                let price = match &rates {
                    Some(r) if cur != site => {
                        if r.factor(&site, cur, today) == 0.0 {
                            return None;
                        }
                        r.convert(price, &site, cur, today)
                    }
                    _ => price,
                };
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

    #[sqlx::test]
    async fn catalog_discount_for_foreign_price(db: PgPool) {
        crate::test_support::seed_usd(&db, Some(80.0)).await;
        sqlx::query("INSERT INTO discounts (name, actions) VALUES ('Товар', $1)")
            .bind(json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": [{
                "CLASS_ID": "ActSaleBsktGrp",
                "DATA": {"Type": "Discount", "Value": 100, "Unit": "CurEach", "Max": 0, "All": "AND", "True": "True"},
                "CHILDREN": []
            }]}))
            .execute(&db)
            .await
            .unwrap();
        let rules = CatalogRules::load(&db).await.unwrap().unwrap();
        let product = |id, price, currency: &str| CatalogProduct {
            id,
            price,
            price_type_id: None,
            currency: currency.to_string(),
        };
        let got = rules
            .prices(
                &db,
                None,
                None,
                &[
                    product(1, 12.5, "USD"),
                    product(2, 1000.0, "RUB"),
                    product(3, 5.0, "ZZZ"),
                ],
            )
            .await
            .unwrap();
        assert_eq!(got.get(&1), Some(&11.25));
        assert_eq!(got.get(&2), Some(&900.0));
        assert!(!got.contains_key(&3));
    }
}
