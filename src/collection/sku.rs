//! Торговые предложения: связь предложения с товаром и тесты триггеров пересчёта типа
//! и доступности товара.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use serde_json::{Map, Value};
use sqlx::PgPool;

use super::{Collection, Field, fields, repo};

/// Код системного поля связи, которое создаёт `create_offer_collection`.
pub const LINK_CODE: &str = "cml2_link";

/// SQL-выражение `field_values` записи `alias` (алиас или имя таблицы `collection_items`).
/// У предложения (`product_id` не пуст) добавлен ключ кода поля `sku_field_id` её
/// коллекции со значением `product_id` — родительского товара, не позиции корзины.
/// В самой БД значение связи в JSONB не хранится.
pub fn field_values_sql(alias: &str) -> String {
    format!(
        "(CASE WHEN {alias}.product_id IS NULL THEN {alias}.field_values \
         ELSE {alias}.field_values || COALESCE((SELECT jsonb_build_object(f.code, {alias}.product_id) \
         FROM collections c JOIN collection_fields f ON f.id = c.sku_field_id \
         WHERE c.id = {alias}.collection_id), '{{}}'::jsonb) END)"
    )
}

/// Вынимает из `values` значение поля связи коллекции предложений и возвращает id
/// родительского товара (число или строка с числом, у множественного — первый).
pub fn take_link(
    collection: &Collection,
    fields: &[Field],
    values: &mut Map<String, Value>,
) -> Option<i64> {
    let id = collection.sku_field_id?;
    let code = &fields.iter().find(|f| f.id == id)?.code;
    let value = values.remove(code)?;
    let value = match value {
        Value::Array(items) => items.into_iter().next()?,
        other => other,
    };
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Больше стольких сочетаний за один запуск генератора не создаётся.
pub const MAX_COMBINATIONS: usize = 100;

/// Значение поля выбора предложения: вариант списка, запись другой коллекции или текст.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AxisValue {
    /// id варианта списка.
    Option(i64),
    /// id записи привязанной коллекции.
    Item(i64),
    Text(String),
}

impl AxisValue {
    /// Ключ значения для сравнения сочетаний: id или текст.
    fn key(&self) -> String {
        match self {
            Self::Option(id) | Self::Item(id) => id.to_string(),
            Self::Text(text) => text.clone(),
        }
    }

    /// Значение для `field_values`: число или строка.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Option(id) | Self::Item(id) => Value::from(*id),
            Self::Text(text) => Value::from(text.clone()),
        }
    }
}

/// Поле выбора и значения, отмеченные для генерации; подпись идёт в название предложения.
#[derive(Debug, Clone)]
pub struct Axis {
    pub code: String,
    pub values: Vec<(AxisValue, String)>,
}

/// Сочетание: (код поля, значение, подпись) по полям в порядке осей.
pub type Combo = Vec<(String, AxisValue, String)>;

/// Генератор не может построить сочетания: `TooMany(0)` — значения не выбраны ни у одного
/// поля, иначе — число сочетаний больше `MAX_COMBINATIONS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TooMany(pub usize);

impl fmt::Display for TooMany {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            write!(f, "Выберите значения хотя бы одного поля")
        } else {
            write!(
                f,
                "Сочетаний {}, больше {MAX_COMBINATIONS} за раз нельзя",
                self.0
            )
        }
    }
}

/// Декартово произведение осей, первая ось — внешняя. Пустые оси пропускаются.
pub fn combinations(axes: &[Axis]) -> Result<Vec<Combo>, TooMany> {
    let axes: Vec<&Axis> = axes.iter().filter(|a| !a.values.is_empty()).collect();
    if axes.is_empty() {
        return Err(TooMany(0));
    }
    let total = axes
        .iter()
        .try_fold(1usize, |acc, a| acc.checked_mul(a.values.len()))
        .unwrap_or(usize::MAX);
    if total > MAX_COMBINATIONS {
        return Err(TooMany(total));
    }
    let mut out: Vec<Combo> = vec![Vec::new()];
    for axis in axes {
        out = out
            .into_iter()
            .flat_map(|prefix| {
                axis.values.iter().map(move |(value, label)| {
                    let mut combo = prefix.clone();
                    combo.push((axis.code.clone(), value.clone(), label.clone()));
                    combo
                })
            })
            .collect();
    }
    Ok(out)
}

/// Название предложения по шаблону: `#PRODUCT_NAME#` — название товара, `#КОД#` — подпись
/// значения поля (код без учёта регистра); неизвестный `#X#` остаётся как есть.
pub fn offer_name(
    template: &str,
    product_name: &str,
    combo: &[(String, AxisValue, String)],
) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('#') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let replaced = tail.find('#').and_then(|end| {
            let token = &tail[..end];
            let text = if token == "PRODUCT_NAME" {
                Some(product_name)
            } else {
                combo
                    .iter()
                    .find(|(code, _, _)| code.to_uppercase() == token)
                    .map(|(_, _, label)| label.as_str())
            };
            text.map(|t| (t, end + 1))
        });
        match replaced {
            Some((text, used)) => {
                out.push_str(text);
                rest = &tail[used..];
            }
            None => {
                out.push('#');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Ключ сочетания без учёта порядка полей: код поля -> id или текст значения.
pub fn combo_key(combo: &[(String, AxisValue, String)]) -> BTreeMap<String, String> {
    combo
        .iter()
        .map(|(code, value, _)| (code.clone(), value.key()))
        .collect()
}

/// Ключи сочетаний уже существующих предложений товара `product_id` (запись коллекции
/// товаров) по полям `offer_fields` (поля выбора коллекции предложений). Поле без значения
/// в ключ не входит.
pub async fn existing_keys(
    db: &PgPool,
    product_id: i64,
    offer_fields: &[Field],
) -> sqlx::Result<HashSet<BTreeMap<String, String>>> {
    let Some(first) = offer_fields.first() else {
        return Ok(HashSet::new());
    };
    let rows = repo::list_offer_rows(db, first.collection_id, product_id).await?;
    Ok(rows
        .iter()
        .map(|row| {
            offer_fields
                .iter()
                .filter_map(|f| {
                    let value = row.field_values.0.get(&f.code)?;
                    let key = match f.kind.as_str() {
                        "list" | "element" => fields::ids(value)
                            .iter()
                            .map(i64::to_string)
                            .collect::<Vec<_>>()
                            .join(","),
                        _ => match value {
                            Value::String(s) => s.clone(),
                            Value::Array(a) => a
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(","),
                            _ => String::new(),
                        },
                    };
                    (!key.is_empty()).then(|| (f.code.clone(), key))
                })
                .collect()
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::{Axis, AxisValue, TooMany, combinations, offer_name};
    use crate::collection::repo;

    fn axis(code: &str, labels: &[&str]) -> Axis {
        Axis {
            code: code.into(),
            values: labels
                .iter()
                .enumerate()
                .map(|(i, l)| (AxisValue::Option(i as i64 + 1), l.to_string()))
                .collect(),
        }
    }

    #[test]
    fn combinations_cartesian_and_limit() {
        let all = combinations(&[axis("a", &["1", "2"]), axis("b", &["x", "y", "z"])]).unwrap();
        assert_eq!(all.len(), 6);
        // первая ось внешняя
        let labels: Vec<String> = all
            .iter()
            .map(|c| c.iter().map(|(_, _, l)| l.as_str()).collect::<String>())
            .collect();
        assert_eq!(labels, ["1x", "1y", "1z", "2x", "2y", "2z"]);
        assert_eq!(all[0][0].0, "a");
        assert_eq!(all[0][1].1, AxisValue::Option(1));

        // пустая ось пропускается
        let skipped = combinations(&[axis("a", &["1", "2"]), axis("b", &[])]).unwrap();
        assert_eq!(skipped.len(), 2);
        assert_eq!(skipped[0].len(), 1);

        let ten: Vec<&str> = vec!["v"; 10];
        let eleven: Vec<&str> = vec!["v"; 11];
        assert_eq!(
            combinations(&[axis("a", &eleven), axis("b", &ten)]),
            Err(TooMany(110))
        );
        assert_eq!(
            combinations(&[axis("a", &ten), axis("b", &ten)])
                .unwrap()
                .len(),
            100
        );
        // нет осей — ошибка с текстом про выбор значений
        let none = combinations(&[axis("a", &[])]).unwrap_err();
        assert_eq!(none.to_string(), "Выберите значения хотя бы одного поля");
        assert_eq!(
            TooMany(110).to_string(),
            "Сочетаний 110, больше 100 за раз нельзя"
        );
    }

    #[test]
    fn offer_name_template() {
        let combo = vec![
            (
                "volume".to_string(),
                AxisValue::Text("4 л".into()),
                "4 л".to_string(),
            ),
            (
                "color".to_string(),
                AxisValue::Option(3),
                "Красный".to_string(),
            ),
        ];
        assert_eq!(
            offer_name("#PRODUCT_NAME# (#VOLUME#, #COLOR#)", "Масло ATF", &combo),
            "Масло ATF (4 л, Красный)"
        );
        assert_eq!(
            offer_name("#PRODUCT_NAME# #UNKNOWN# # #COLOR#", "М", &combo),
            "М #UNKNOWN# # Красный"
        );
    }

    #[test]
    fn combo_key_ignores_order() {
        let a = vec![
            (
                "volume".to_string(),
                AxisValue::Text("4 л".into()),
                "4 л".to_string(),
            ),
            (
                "color".to_string(),
                AxisValue::Option(3),
                "Красный".to_string(),
            ),
        ];
        let b: Vec<_> = a.iter().rev().cloned().collect();
        assert_eq!(super::combo_key(&a), super::combo_key(&b));
        assert_eq!(super::combo_key(&a)["color"], "3");
        assert_eq!(super::combo_key(&a)["volume"], "4 л");
    }

    async fn item(db: &PgPool, collection: i64, code: &str, product: Option<i64>) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name, product_id)
             VALUES ($1, $2, $2, $3) RETURNING id",
        )
        .bind(collection)
        .bind(code)
        .bind(product)
        .fetch_one(db)
        .await
        .unwrap()
    }

    /// Коллекция товаров и коллекция предложений.
    async fn collections(db: &PgPool) -> (i64, i64) {
        let products: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name, is_catalog) VALUES ('catalog', 'Каталог', TRUE) RETURNING id",
        )
        .fetch_one(db)
        .await
        .unwrap();
        let offers: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name, is_catalog, product_collection_id)
             VALUES ('offers', 'Предложения', TRUE, $1) RETURNING id",
        )
        .bind(products)
        .fetch_one(db)
        .await
        .unwrap();
        (products, offers)
    }

    async fn stock(db: &PgPool, id: i64, quantity: i32, trace: bool, zero: bool) {
        sqlx::query(
            "INSERT INTO catalog_products (item_id, quantity, quantity_trace, can_buy_zero)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(quantity)
        .bind(trace)
        .bind(zero)
        .execute(db)
        .await
        .unwrap();
    }

    async fn state(db: &PgPool, id: i64) -> Option<(i16, bool)> {
        sqlx::query_as("SELECT type, available FROM catalog_products WHERE item_id = $1")
            .bind(id)
            .fetch_optional(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn type_follows_offers(db: PgPool) {
        let (products, offers) = collections(&db).await;
        let product = item(&db, products, "tovar", None).await;
        let offer = item(&db, offers, "offer", Some(product)).await;
        stock(&db, offer, 5, true, false).await;
        assert_eq!(state(&db, product).await, Some((3, true)));
        assert_eq!(state(&db, offer).await.unwrap().0, 4);

        sqlx::query("DELETE FROM collection_items WHERE id = $1")
            .bind(offer)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, product).await, Some((6, false)));
    }

    #[sqlx::test]
    async fn availability_follows_offer_stock(db: PgPool) {
        let (products, offers) = collections(&db).await;
        let product = item(&db, products, "tovar", None).await;
        let offer = item(&db, offers, "offer", Some(product)).await;
        stock(&db, offer, 5, true, false).await;
        assert_eq!(state(&db, product).await, Some((3, true)));

        sqlx::query("UPDATE catalog_products SET quantity = 0 WHERE item_id = $1")
            .bind(offer)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, product).await, Some((3, false)));

        sqlx::query("UPDATE catalog_products SET can_buy_zero = TRUE WHERE item_id = $1")
            .bind(offer)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, product).await, Some((3, true)));

        sqlx::query("UPDATE collection_items SET active = FALSE WHERE id = $1")
            .bind(offer)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, product).await, Some((3, false)));
    }

    #[sqlx::test]
    async fn moving_offer_refreshes_both_products(db: PgPool) {
        let (products, offers) = collections(&db).await;
        let a = item(&db, products, "a", None).await;
        let b = item(&db, products, "b", None).await;
        let offer = item(&db, offers, "offer", Some(a)).await;
        stock(&db, offer, 5, true, false).await;
        assert_eq!(state(&db, a).await, Some((3, true)));

        sqlx::query("UPDATE collection_items SET product_id = $1 WHERE id = $2")
            .bind(b)
            .bind(offer)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, a).await, Some((6, false)));
        assert_eq!(state(&db, b).await, Some((3, true)));
        assert_eq!(state(&db, offer).await.unwrap().0, 4);
    }

    #[sqlx::test]
    async fn offer_type_follows_link(db: PgPool) {
        let (products, offers) = collections(&db).await;
        let product = item(&db, products, "tovar", None).await;
        let rec = item(&db, offers, "rec", None).await;
        stock(&db, rec, 5, true, false).await;
        assert_eq!(state(&db, rec).await.unwrap().0, 1);

        sqlx::query("UPDATE collection_items SET product_id = $1 WHERE id = $2")
            .bind(product)
            .bind(rec)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, rec).await.unwrap().0, 4);
        assert_eq!(state(&db, product).await, Some((3, true)));

        sqlx::query("UPDATE collection_items SET product_id = NULL WHERE id = $1")
            .bind(rec)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(state(&db, rec).await.unwrap().0, 1);
        assert_eq!(state(&db, product).await, Some((6, false)));
    }

    #[sqlx::test]
    async fn product_delete_cascades_offers(db: PgPool) {
        let (products, offers) = collections(&db).await;
        let product = item(&db, products, "tovar", None).await;
        let offer = item(&db, offers, "offer", Some(product)).await;
        stock(&db, offer, 5, true, false).await;

        sqlx::query("DELETE FROM collection_items WHERE id = $1")
            .bind(product)
            .execute(&db)
            .await
            .unwrap();
        let left: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM collection_items WHERE id IN ($1, $2))
                  + (SELECT count(*) FROM catalog_products WHERE item_id IN ($1, $2))",
        )
        .bind(product)
        .bind(offer)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(left, 0);
    }

    /// Каталог с коллекцией предложений, созданной через `create_offer_collection`.
    async fn linked(db: &PgPool) -> (i64, crate::collection::Collection, i64, i64) {
        let products: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name, is_catalog) VALUES ('catalog', 'Каталог', TRUE) RETURNING id",
        )
        .fetch_one(db)
        .await
        .unwrap();
        let product = item(db, products, "tovar", None).await;
        let parent = repo::get_collection(db, products).await.unwrap().unwrap();
        let offers = repo::create_offer_collection(db, &parent).await.unwrap();
        let offer = item(db, offers.id, "offer", Some(product)).await;
        (products, offers, product, offer)
    }

    #[sqlx::test]
    async fn link_reads_as_field(db: PgPool) {
        let (products, offers, product, offer) = linked(&db).await;
        assert_eq!(offers.code, "catalog_offers");
        assert_eq!(offers.product_collection_id, Some(products));
        assert!(offers.is_catalog);
        let fields = repo::list_fields(&db, offers.id).await.unwrap();
        let link = fields.iter().find(|f| f.code == "cml2_link").unwrap();
        assert_eq!(link.kind, "element");
        assert_eq!(link.link_collection_id, Some(products));
        assert_eq!(offers.sku_field_id, Some(link.id));

        let read = repo::get_item(&db, offer).await.unwrap().unwrap();
        assert_eq!(read.field_values["cml2_link"], json!(product));
        assert_eq!(read.product_id, Some(product));
        let (listed, _) = repo::list_items(&db, offers.id, None, 10, 0).await.unwrap();
        assert_eq!(listed[0].field_values["cml2_link"], json!(product));
        // у товара ключа нет, в самой БД значение не дублируется
        let plain = repo::get_item(&db, product).await.unwrap().unwrap();
        assert!(plain.field_values.get("cml2_link").is_none());
        let stored: bool = sqlx::query_scalar(
            "SELECT field_values ? 'cml2_link' FROM collection_items WHERE id = $1",
        )
        .bind(offer)
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(!stored);
    }

    #[sqlx::test]
    async fn take_link_removes_value(db: PgPool) {
        let (_, offers, product, _) = linked(&db).await;
        let fields = repo::list_fields(&db, offers.id).await.unwrap();
        let mut values = serde_json::Map::new();
        values.insert("cml2_link".into(), json!(product));
        values.insert("other".into(), json!(1));
        assert_eq!(
            super::take_link(&offers, &fields, &mut values),
            Some(product)
        );
        assert!(!values.contains_key("cml2_link"));
        assert!(values.contains_key("other"));
        let mut text = serde_json::Map::new();
        text.insert("cml2_link".into(), json!(product.to_string()));
        assert_eq!(super::take_link(&offers, &fields, &mut text), Some(product));
        assert_eq!(super::take_link(&offers, &fields, &mut text), None);
    }

    #[sqlx::test]
    async fn create_and_update_write_product_id(db: PgPool) {
        let (_, offers, product, _) = linked(&db).await;
        let other = item(&db, offers.product_collection_id.unwrap(), "drugoy", None).await;
        let mut input = crate::collection::ItemInput {
            section_id: None,
            code: "novoe".into(),
            xml_id: String::new(),
            name: "Новое".into(),
            active: true,
            sort: 500,
            preview_text: String::new(),
            detail_text: String::new(),
            preview_picture_id: None,
            detail_picture_id: None,
            published_at: None,
            field_values: serde_json::Map::new(),
            product_id: Some(product),
        };
        let id = repo::create_item(&db, offers.id, &input).await.unwrap();
        assert_eq!(
            repo::get_item(&db, id).await.unwrap().unwrap().product_id,
            Some(product)
        );
        input.product_id = Some(other);
        repo::update_item(&db, id, &input).await.unwrap();
        assert_eq!(
            repo::get_item(&db, id).await.unwrap().unwrap().product_id,
            Some(other)
        );
    }

    #[sqlx::test]
    async fn link_and_unlink_offers(db: PgPool) {
        let (products, offers) = collections(&db).await;
        // существующая пара из collections() без поля связи: link_offers его создаёт
        repo::link_offers(&db, products, offers).await.unwrap();
        let c = repo::get_collection(&db, offers).await.unwrap().unwrap();
        assert!(c.sku_field_id.is_some());
        repo::link_offers(&db, products, offers).await.unwrap();
        let fields = repo::list_fields(&db, offers).await.unwrap();
        assert_eq!(fields.iter().filter(|f| f.code == "cml2_link").count(), 1);

        let product = item(&db, products, "tovar", None).await;
        item(&db, offers, "offer", Some(product)).await;
        assert!(matches!(
            repo::unlink_offers(&db, products).await,
            Err(repo::UnlinkError::HasOffers(1))
        ));
        sqlx::query("DELETE FROM collection_items WHERE collection_id = $1")
            .bind(offers)
            .execute(&db)
            .await
            .unwrap();
        repo::unlink_offers(&db, products).await.unwrap();
        let c = repo::get_collection(&db, offers).await.unwrap().unwrap();
        assert_eq!((c.product_collection_id, c.sku_field_id), (None, None));
        assert!(repo::list_fields(&db, offers).await.unwrap().is_empty());
    }
}
