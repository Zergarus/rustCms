//! Данные для проверки условий и расчёта скидок — без БД (как `orderData` у `Sale\DiscountBase`
//! и данные каталога, которые `Catalog\Discount\DiscountManager::extendOrderData` дописывает
//! в позиции корзины).

use std::collections::HashMap;

use chrono::{DateTime, Datelike, FixedOffset, Utc};
use serde_json::{Map, Value};

use super::model::Coupon;

/// Родительский товар предложения: поля, которые условия товара проверяют и у родителя
/// (`PARENT_*` у Битрикса).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CtxParent {
    /// id товара.
    pub id: i64,
    pub collection_id: i64,
    pub xml_id: String,
    pub name: String,
    pub code: String,
}

/// Позиция корзины с данными товара.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CtxItem {
    /// id позиции.
    pub key: i64,
    /// Запись коллекции (у предложения — само предложение).
    pub product_id: i64,
    pub collection_id: i64,
    /// Товар предложения — единственный источник данных о родителе; построитель контекста
    /// заполняет его для предложений, у товаров — `None`. Разделы и поля предложения
    /// (`section_ids`, `fields`, `field_ids`) уже включают родительские.
    pub parent: Option<CtxParent>,
    /// Разделы записи и всех их предков; у предложения — и родителя.
    pub section_ids: Vec<i64>,
    pub xml_id: String,
    pub name: String,
    pub code: String,
    /// `field_values` записи; у предложения — объединение с родителем, как Битрикс.
    pub fields: Map<String, Value>,
    /// id поля → код: поля коллекции записи (у предложения — и коллекции родителя).
    pub field_ids: HashMap<i64, String>,
    pub quantity: f64,
    /// Цена единицы до скидок.
    pub base_price: f64,
    pub custom_price: bool,
    /// Вес единицы.
    pub weight: f64,
    /// Остаток на складе.
    pub stock_quantity: f64,
}

/// Данные заказа; в корзине до оформления — пусто.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OrderCtx {
    pub person_type_id: Option<i64>,
    pub pay_system_id: Option<i64>,
    pub delivery_id: Option<i64>,
    /// Цена доставки до скидок.
    pub delivery_price: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Context {
    pub items: Vec<CtxItem>,
    pub user_id: Option<i64>,
    pub group_ids: Vec<i64>,
    pub coupons: Vec<Coupon>,
    pub order: OrderCtx,
    pub now: DateTime<Utc>,
}

impl Context {
    /// День недели по МСК (как время bxapi): 1 — понедельник … 7 — воскресенье (`date('N')`).
    pub fn day_of_week(&self) -> u32 {
        let msk = FixedOffset::east_opt(3 * 3600).expect("valid offset");
        self.now.with_timezone(&msk).weekday().number_from_monday()
    }
}

#[derive(sqlx::FromRow)]
struct RecordRow {
    id: i64,
    collection_id: i64,
    /// Товар предложения.
    product_id: Option<i64>,
    section_id: Option<i64>,
    xml_id: String,
    name: String,
    code: String,
    field_values: sqlx::types::Json<Map<String, Value>>,
    weight: f64,
}

/// Контекст корзины: позиции, которые можно купить (`CAN_BUY`, как `loadBasket` Битрикса),
/// с данными товара и родителя предложения; группы пользователя; купоны покупателя.
/// Позиции с `custom_price` видны условиям по своей цене, действия их не меняют.
pub async fn for_cart(
    db: &sqlx::PgPool,
    items: &[crate::cart::CartItem],
    info: &HashMap<i64, crate::catalog::PurchaseInfo>,
    user_id: Option<i64>,
    coupons: Vec<Coupon>,
    order: OrderCtx,
) -> sqlx::Result<Context> {
    use crate::catalog::{main_price, sellable};

    let group_ids: Vec<i64> = match user_id {
        Some(u) => {
            sqlx::query_scalar(
                "SELECT group_id FROM user_groups WHERE user_id = $1 ORDER BY group_id",
            )
            .bind(u)
            .fetch_all(db)
            .await?
        }
        None => Vec::new(),
    };
    let mut ctx = Context {
        items: Vec::new(),
        user_id,
        group_ids,
        coupons,
        order,
        now: Utc::now(),
    };

    let mut buyable: Vec<(&crate::cart::CartItem, f64, f64)> = items
        .iter()
        .filter_map(|it| {
            let p = info.get(&it.product_id)?;
            if !(sellable(p) && p.available) {
                return None;
            }
            Some((it, main_price(&p.prices)?.price, p.total))
        })
        .collect();
    buyable.sort_by_key(|(it, _, _)| it.id);
    if buyable.is_empty() {
        return Ok(ctx);
    }

    let keys: Vec<i64> = buyable.iter().map(|(it, _, _)| it.id).collect();
    let custom: HashMap<i64, Option<f64>> = sqlx::query_as::<_, (i64, Option<f64>)>(
        "SELECT id, price::float8 FROM cart_items WHERE id = ANY($1) AND custom_price",
    )
    .bind(&keys)
    .fetch_all(db)
    .await?
    .into_iter()
    .collect();

    const RECORD: &str =
        "SELECT i.id, i.collection_id, i.product_id, i.section_id, i.xml_id, i.name,
                i.code, i.field_values, COALESCE(p.weight, 0)::float8 AS weight
         FROM collection_items i LEFT JOIN catalog_products p ON p.item_id = i.id
         WHERE i.id = ANY($1)";
    let product_ids: Vec<i64> = buyable.iter().map(|(it, _, _)| it.product_id).collect();
    let mut records: HashMap<i64, RecordRow> = sqlx::query_as::<_, RecordRow>(RECORD)
        .bind(&product_ids)
        .fetch_all(db)
        .await?
        .into_iter()
        .map(|r| (r.id, r))
        .collect();
    let parent_ids: Vec<i64> = records.values().filter_map(|r| r.product_id).collect();
    if !parent_ids.is_empty() {
        for r in sqlx::query_as::<_, RecordRow>(RECORD)
            .bind(&parent_ids)
            .fetch_all(db)
            .await?
        {
            records.entry(r.id).or_insert(r);
        }
    }

    // Раздел записи → он и все предки
    let section_ids: Vec<i64> = records.values().filter_map(|r| r.section_id).collect();
    let mut chains: HashMap<i64, Vec<i64>> = HashMap::new();
    for (start, id) in sqlx::query_as::<_, (i64, i64)>(
        "WITH RECURSIVE t(start, id, parent_id, depth) AS (
             SELECT id, id, parent_id, 0 FROM collection_sections WHERE id = ANY($1)
             UNION ALL
             SELECT t.start, s.id, s.parent_id, t.depth + 1
             FROM collection_sections s JOIN t ON s.id = t.parent_id WHERE t.depth < 100
         )
         SELECT start, id FROM t ORDER BY start, depth",
    )
    .bind(&section_ids)
    .fetch_all(db)
    .await?
    {
        chains.entry(start).or_default().push(id);
    }

    let collections: Vec<i64> = records.values().map(|r| r.collection_id).collect();
    let mut fields_of: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
    for (id, collection, code) in sqlx::query_as::<_, (i64, i64, String)>(
        "SELECT id, collection_id, code FROM collection_fields WHERE collection_id = ANY($1)
         ORDER BY id",
    )
    .bind(&collections)
    .fetch_all(db)
    .await?
    {
        fields_of.entry(collection).or_default().push((id, code));
    }

    let sections = |r: &RecordRow| -> Vec<i64> {
        r.section_id
            .and_then(|s| chains.get(&s).cloned())
            .unwrap_or_default()
    };
    for (it, catalog_price, stock) in buyable {
        let Some(rec) = records.get(&it.product_id) else {
            continue;
        };
        let own_fields = fields_of
            .get(&rec.collection_id)
            .cloned()
            .unwrap_or_default();
        let mut field_ids: HashMap<i64, String> = own_fields.iter().cloned().collect();
        let mut fields = rec.field_values.0.clone();
        let mut section_ids = sections(rec);
        let parent = rec.product_id.and_then(|p| records.get(&p));
        if let Some(parent) = parent {
            for s in sections(parent) {
                if !section_ids.contains(&s) {
                    section_ids.push(s);
                }
            }
            // Поле родителя с тем же кодом, что у поля предложения, — под отдельным ключом
            for (id, code) in fields_of.get(&parent.collection_id).into_iter().flatten() {
                let key = if own_fields.iter().any(|(_, c)| c == code) {
                    format!("parent:{code}")
                } else {
                    code.clone()
                };
                if let Some(v) = parent.field_values.0.get(code) {
                    fields.entry(key.clone()).or_insert_with(|| v.clone());
                }
                field_ids.insert(*id, key);
            }
        }
        let custom_price = custom.get(&it.id);
        ctx.items.push(CtxItem {
            key: it.id,
            product_id: it.product_id,
            collection_id: rec.collection_id,
            parent: parent.map(|p| CtxParent {
                id: p.id,
                collection_id: p.collection_id,
                xml_id: p.xml_id.clone(),
                name: p.name.clone(),
                code: p.code.clone(),
            }),
            section_ids,
            xml_id: rec.xml_id.clone(),
            name: if rec.name.is_empty() {
                it.name.clone()
            } else {
                rec.name.clone()
            },
            code: rec.code.clone(),
            fields,
            field_ids,
            quantity: it.quantity,
            base_price: custom_price.copied().flatten().unwrap_or(catalog_price),
            custom_price: custom_price.is_some(),
            weight: rec.weight,
            stock_quantity: stock,
        });
    }
    Ok(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn for_cart_fills_offer_parent_and_sections(db: sqlx::PgPool) {
        let fx = crate::test_support::cart_fixture(&db).await;
        // Раздел родителя вложен в корневой — условие по корневому тоже срабатывает
        let root: i64 = sqlx::query_scalar(
            "INSERT INTO collection_sections (collection_id, code, name)
             SELECT collection_id, 'auto', 'Авто' FROM collection_sections WHERE id = $1 RETURNING id",
        )
        .bind(fx.section_id)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("UPDATE collection_sections SET parent_id = $2 WHERE id = $1")
            .bind(fx.section_id)
            .bind(root)
            .execute(&db)
            .await
            .unwrap();
        let field: i64 = sqlx::query_scalar(
            "INSERT INTO collection_fields (collection_id, code, name, kind)
             SELECT collection_id, 'brand', 'Бренд', 'string' FROM collection_items WHERE id = $1 RETURNING id",
        )
        .bind(fx.parent_id)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE collection_items SET field_values = '{\"brand\": \"Shell\"}' WHERE id = $1",
        )
        .bind(fx.parent_id)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("UPDATE catalog_products SET weight = 900 WHERE item_id = $1")
            .bind(fx.offer_id)
            .execute(&db)
            .await
            .unwrap();
        let items = crate::cart::repo::items(&db, fx.f.buyer_id).await.unwrap();
        let ids: Vec<i64> = items.iter().map(|i| i.product_id).collect();
        let info = crate::catalog::load(&db, &ids).await.unwrap();
        let ctx = for_cart(&db, &items, &info, None, Vec::new(), OrderCtx::default())
            .await
            .unwrap();
        assert_eq!(ctx.items.len(), 2);
        let simple = &ctx.items[0];
        assert_eq!(
            (simple.key, simple.base_price, simple.quantity),
            (fx.f.item_id, 1000.0, 2.0)
        );
        assert_eq!(simple.parent, None);
        assert_eq!(simple.stock_quantity, 10.0);
        let offer = &ctx.items[1];
        assert_eq!(offer.key, fx.offer_item_id);
        assert_eq!(offer.product_id, fx.offer_id);
        assert_eq!(offer.base_price, 500.0);
        assert_eq!(offer.weight, 900.0);
        let parent = offer.parent.as_ref().unwrap();
        assert_eq!(parent.id, fx.parent_id);
        assert_eq!(parent.xml_id, "P-1");
        assert_eq!(parent.name, "Масло");
        assert_eq!(parent.code, "maslo");
        let mut sections = offer.section_ids.clone();
        sections.sort();
        assert_eq!(sections, [fx.section_id, root]);
        assert_eq!(
            offer.field_ids.get(&field).map(String::as_str),
            Some("brand")
        );
        assert_eq!(offer.fields["brand"], "Shell");

        // Непокупаемая позиция в контекст не попадает
        sqlx::query("UPDATE collection_items SET active = FALSE WHERE id = $1")
            .bind(fx.f.product_id)
            .execute(&db)
            .await
            .unwrap();
        let info = crate::catalog::load(&db, &ids).await.unwrap();
        let ctx = for_cart(&db, &items, &info, None, Vec::new(), OrderCtx::default())
            .await
            .unwrap();
        let keys: Vec<i64> = ctx.items.iter().map(|i| i.key).collect();
        assert_eq!(keys, [fx.offer_item_id]);
    }
}
