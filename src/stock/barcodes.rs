//! Штрихкоды товара: хранение (`catalog_barcodes`) и поиск товара по коду.

use std::collections::HashMap;

use sqlx::PgConnection;

use super::StockError;
use super::documents::{DocType, Line};

/// Штрихкод товара: общий (`store_id`/`order_id` пусты) или единица на складе/в заказе.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct BarcodeRow {
    pub barcode: String,
    pub store_id: Option<i64>,
    pub order_id: Option<i64>,
}

fn reject(message: impl Into<String>) -> StockError {
    StockError::Rejected(vec![message.into()])
}

/// Заменяет общие штрихкоды товара. Пустые и повторы отбрасываются; код другого товара — отказ.
pub async fn set_common(
    tx: &mut PgConnection,
    item_id: i64,
    codes: &[String],
    user: Option<i64>,
) -> Result<(), StockError> {
    let mut clean: Vec<&str> = Vec::new();
    for c in codes {
        let c = c.trim();
        if !c.is_empty() && !clean.contains(&c) {
            clean.push(c);
        }
    }
    let name: String = sqlx::query_scalar("SELECT name FROM collection_items WHERE id = $1")
        .bind(item_id)
        .fetch_one(&mut *tx)
        .await?;
    for code in &clean {
        let other: Option<i64> = sqlx::query_scalar(
            "SELECT item_id FROM catalog_barcodes WHERE barcode = $1 AND item_id <> $2",
        )
        .bind(code)
        .bind(item_id)
        .fetch_optional(&mut *tx)
        .await?;
        if other.is_some() {
            return Err(reject(format!(
                "Штрихкод {code}, введенный для товара {name}, уже существует."
            )));
        }
    }
    sqlx::query(
        "DELETE FROM catalog_barcodes
         WHERE item_id = $1 AND store_id IS NULL AND order_id IS NULL AND barcode <> ALL($2)",
    )
    .bind(item_id)
    .bind(&clean)
    .execute(&mut *tx)
    .await?;
    for code in &clean {
        // Код мог уже быть у этого товара единицей на складе — его не трогаем
        sqlx::query(
            "INSERT INTO catalog_barcodes (item_id, barcode, created_by, modified_by)
             VALUES ($1, $2, $3, $3) ON CONFLICT (barcode) DO NOTHING",
        )
        .bind(item_id)
        .bind(code)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Флажок «поштучный учёт штрихкодов»; снять нельзя, пока есть штрихкоды единиц на складах.
pub async fn set_multi(tx: &mut PgConnection, item_id: i64, on: bool) -> Result<(), StockError> {
    if !on {
        let has: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM catalog_barcodes WHERE item_id = $1 AND store_id IS NOT NULL)",
        )
        .bind(item_id)
        .fetch_one(&mut *tx)
        .await?;
        if has {
            return Err(reject("У товара есть штрихкоды единиц на складах"));
        }
    }
    sqlx::query("UPDATE catalog_products SET barcode_multi = $2 WHERE item_id = $1")
        .bind(item_id)
        .bind(on)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Штрихкоды товара по возрастанию кода.
pub async fn list(tx: &mut PgConnection, item_id: i64) -> sqlx::Result<Vec<BarcodeRow>> {
    sqlx::query_as(
        "SELECT barcode, store_id, order_id FROM catalog_barcodes
         WHERE item_id = $1 ORDER BY barcode",
    )
    .bind(item_id)
    .fetch_all(tx)
    .await
}

/// Товар по штрихкоду: `catalog_barcodes`, затем поле `cml2_bar_code`; при нескольких — меньший id.
pub async fn find_item(db: &mut PgConnection, code: &str) -> sqlx::Result<Option<i64>> {
    let code = code.trim();
    if code.is_empty() {
        return Ok(None);
    }
    let found: Option<i64> =
        sqlx::query_scalar("SELECT min(item_id) FROM catalog_barcodes WHERE barcode = $1")
            .bind(code)
            .fetch_one(&mut *db)
            .await?;
    if found.is_some() {
        return Ok(found);
    }
    sqlx::query_scalar(
        "SELECT min(id) FROM collection_items WHERE field_values ->> 'cml2_bar_code' = $1",
    )
    .bind(code)
    .fetch_one(db)
    .await
}

/// Похоже на штрихкод: 8–14 цифр (EAN-8…GTIN-14).
pub fn looks_like_barcode(q: &str) -> bool {
    let q = q.trim();
    (8..=14).contains(&q.len()) && q.chars().all(|c| c.is_ascii_digit())
}

/// Что делает строка документа со штрихкодами единиц поштучного товара.
#[derive(Clone, Copy)]
enum Action {
    /// Привязать к складу (приход, оприходование, возврат; отмена списания).
    Bind(i64),
    /// Убрать со склада (отмена прихода, оприходования, возврата).
    Unbind(i64),
    /// Код должен лежать на складе, затем запись удаляется (списание).
    Remove(i64),
    /// Код должен лежать на первом складе, затем переходит на второй (перемещение).
    Move(i64, i64),
}

fn action(doc_type: DocType, line: &Line, cancel: bool) -> Option<Action> {
    Some(match (doc_type, cancel) {
        (DocType::Arrival | DocType::Adjustment | DocType::Return, false) => {
            Action::Bind(line.store_to?)
        }
        (DocType::Arrival | DocType::Adjustment | DocType::Return, true) => {
            Action::Unbind(line.store_to?)
        }
        (DocType::Deduct, false) => Action::Remove(line.store_from?),
        (DocType::Deduct, true) => Action::Bind(line.store_from?),
        (DocType::Moving, false) => Action::Move(line.store_from?, line.store_to?),
        (DocType::Moving, true) => Action::Move(line.store_to?, line.store_from?),
    })
}

fn clean_codes(line: &Line) -> Vec<&str> {
    line.barcodes
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .collect()
}

/// Штрихкоды строк при проведении и отмене документа склада. Сначала всё проверяется (ошибки
/// собираются списком), и только потом пишется: отказ ничего не меняет.
pub(super) async fn apply_document(
    tx: &mut PgConnection,
    doc_type: DocType,
    lines: &[Line],
    names: &HashMap<i64, String>,
    stores: &HashMap<i64, (String, bool)>,
    cancel: bool,
    user: Option<i64>,
) -> Result<(), StockError> {
    let mut item_ids: Vec<i64> = lines.iter().filter_map(|l| l.item_id).collect();
    item_ids.sort_unstable();
    item_ids.dedup();
    let multi: Vec<i64> = sqlx::query_scalar(
        "SELECT item_id FROM catalog_products WHERE item_id = ANY($1) AND barcode_multi",
    )
    .bind(&item_ids)
    .fetch_all(&mut *tx)
    .await?;
    let name_of = |id: i64| names.get(&id).map(String::as_str).unwrap_or_default();
    let store_name = |id: i64| stores.get(&id).map(|s| s.0.as_str()).unwrap_or_default();
    let bound_msg = |code: &str, id: i64, other: i64| {
        format!(
            "Штрихкод {code} товара #{id} ({}) привязан к товару {other}",
            name_of(id)
        )
    };

    let mut errors: Vec<String> = Vec::new();
    // Общие коды обычных товаров к добавлению
    let mut common: Vec<(i64, &str)> = Vec::new();
    // Коды обычных товаров, занятые в этом документе: код -> товар
    let mut claimed: HashMap<&str, i64> = HashMap::new();
    // Проверка количества — только при проведении
    if !cancel {
        for &id in &multi {
            let mine: Vec<&Line> = lines.iter().filter(|l| l.item_id == Some(id)).collect();
            let sum_codes: usize = mine.iter().map(|l| clean_codes(l).len()).sum();
            let sum_amount: f64 = mine.iter().map(|l| l.amount).sum();
            if (sum_codes as f64 - sum_amount).abs() > 1e-9 {
                errors.push(format!(
                    "В документе складского учета число штрихкодов товара #{id} ({}) не совпадает с количеством самого товара",
                    name_of(id)
                ));
            } else if mine
                .iter()
                .any(|l| (clean_codes(l).len() as f64 - l.amount).abs() > 1e-9)
            {
                errors.push(format!(
                    "Не совпадает количество товара #{id} ({}) и число штрихкодов в строке документа складского учета",
                    name_of(id)
                ));
            }
        }
    }
    if !errors.is_empty() {
        return Err(StockError::Rejected(errors));
    }

    let mut seen: Vec<&str> = Vec::new();
    let mut plan: Vec<(i64, &str, Action)> = Vec::new();
    for line in lines {
        let Some(id) = line.item_id else { continue };
        let codes = clean_codes(line);
        if codes.is_empty() {
            continue;
        }
        if !multi.contains(&id) {
            if cancel {
                continue;
            }
            for code in codes {
                let owner: Option<i64> =
                    sqlx::query_scalar("SELECT item_id FROM catalog_barcodes WHERE barcode = $1")
                        .bind(code)
                        .fetch_optional(&mut *tx)
                        .await?;
                let owner = owner.or(claimed.get(code).copied());
                match owner {
                    Some(o) if o != id => errors.push(bound_msg(code, id, o)),
                    Some(_) => {}
                    None => {
                        claimed.insert(code, id);
                        common.push((id, code));
                    }
                }
            }
            continue;
        }
        let Some(act) = action(doc_type, line, cancel) else {
            continue;
        };
        for code in codes {
            if seen.contains(&code) {
                errors.push(format!(
                    "Штрихкод {code}, введенный для товара {}, уже существует.",
                    name_of(id)
                ));
                continue;
            }
            seen.push(code);
            let row: Option<(i64, Option<i64>)> =
                sqlx::query_as("SELECT item_id, store_id FROM catalog_barcodes WHERE barcode = $1")
                    .bind(code)
                    .fetch_optional(&mut *tx)
                    .await?;
            if let Some((owner, _)) = row
                && owner != id
            {
                errors.push(bound_msg(code, id, owner));
                continue;
            }
            let store_of = row.and_then(|r| r.1);
            match act {
                Action::Bind(_) if store_of.is_some() => errors.push(format!(
                    "Штрихкод {code}, введенный для товара {}, уже существует.",
                    name_of(id)
                )),
                Action::Remove(s) | Action::Move(s, _) if store_of != Some(s) => {
                    errors.push(format!(
                        "На складе {} нет товара {} со штрихкодом {code}",
                        store_name(s),
                        name_of(id)
                    ))
                }
                _ => {}
            }
            plan.push((id, code, act));
        }
    }
    if !errors.is_empty() {
        return Err(StockError::Rejected(errors));
    }

    for (id, code) in common {
        sqlx::query(
            "INSERT INTO catalog_barcodes (item_id, barcode, created_by, modified_by)
             VALUES ($1, $2, $3, $3) ON CONFLICT (barcode) DO NOTHING",
        )
        .bind(id)
        .bind(code)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    }
    for (id, code, act) in plan {
        match act {
            Action::Bind(store) => {
                sqlx::query(
                    "INSERT INTO catalog_barcodes (item_id, barcode, store_id, created_by, modified_by)
                     VALUES ($1, $2, $3, $4, $4)
                     ON CONFLICT (barcode) DO UPDATE
                     SET store_id = EXCLUDED.store_id, order_id = NULL,
                         modified_by = EXCLUDED.modified_by, updated_at = now()",
                )
                .bind(id)
                .bind(code)
                .bind(store)
                .bind(user)
                .execute(&mut *tx)
                .await?;
            }
            Action::Unbind(store) | Action::Remove(store) => {
                sqlx::query(
                    "DELETE FROM catalog_barcodes
                     WHERE item_id = $1 AND barcode = $2 AND store_id = $3",
                )
                .bind(id)
                .bind(code)
                .bind(store)
                .execute(&mut *tx)
                .await?;
            }
            Action::Move(_, to) => {
                sqlx::query(
                    "UPDATE catalog_barcodes SET store_id = $3, modified_by = $4, updated_at = now()
                     WHERE item_id = $1 AND barcode = $2",
                )
                .bind(id)
                .bind(code)
                .bind(to)
                .bind(user)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::order_fixture;
    use sqlx::PgPool;

    async fn second_item(db: &PgPool, first: i64) -> i64 {
        let collection: i64 =
            sqlx::query_scalar("SELECT collection_id FROM collection_items WHERE id = $1")
                .bind(first)
                .fetch_one(db)
                .await
                .unwrap();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name)
             VALUES ($1, 'vtoroy', 'Второй') RETURNING id",
        )
        .bind(collection)
        .fetch_one(db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO catalog_products (item_id) VALUES ($1)")
            .bind(id)
            .execute(db)
            .await
            .unwrap();
        id
    }

    fn codes(rows: &[BarcodeRow]) -> Vec<&str> {
        rows.iter().map(|r| r.barcode.as_str()).collect()
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[sqlx::test]
    async fn set_common_replaces(db: PgPool) {
        let f = order_fixture(&db).await;
        let mut c = db.acquire().await.unwrap();
        set_common(&mut c, f.product_id, &strings(&["111", "222"]), None)
            .await
            .unwrap();
        set_common(
            &mut c,
            f.product_id,
            &strings(&[" 222 ", "333", "", "333"]),
            None,
        )
        .await
        .unwrap();
        let rows = list(&mut c, f.product_id).await.unwrap();
        assert_eq!(codes(&rows), ["222", "333"]);
    }

    #[sqlx::test]
    async fn set_common_rejects_foreign(db: PgPool) {
        let f = order_fixture(&db).await;
        let other = second_item(&db, f.product_id).await;
        let mut c = db.acquire().await.unwrap();
        set_common(&mut c, other, &strings(&["999"]), None)
            .await
            .unwrap();
        match set_common(&mut c, f.product_id, &strings(&["999"]), None).await {
            Err(StockError::Rejected(m)) => assert_eq!(
                m,
                ["Штрихкод 999, введенный для товара Товар, уже существует."]
            ),
            other => panic!("ожидался отказ, получено {other:?}"),
        }
    }

    #[sqlx::test]
    async fn set_multi_blocked_by_store_barcodes(db: PgPool) {
        let f = order_fixture(&db).await;
        let mut c = db.acquire().await.unwrap();
        set_multi(&mut c, f.product_id, true).await.unwrap();
        sqlx::query(
            "INSERT INTO catalog_barcodes (item_id, barcode, store_id) VALUES ($1, 'U1', 1)",
        )
        .bind(f.product_id)
        .execute(&mut *c)
        .await
        .unwrap();
        match set_multi(&mut c, f.product_id, false).await {
            Err(StockError::Rejected(m)) => {
                assert_eq!(m, ["У товара есть штрихкоды единиц на складах"])
            }
            other => panic!("ожидался отказ, получено {other:?}"),
        }
        let on: bool =
            sqlx::query_scalar("SELECT barcode_multi FROM catalog_products WHERE item_id = $1")
                .bind(f.product_id)
                .fetch_one(&mut *c)
                .await
                .unwrap();
        assert!(on);
    }

    #[sqlx::test]
    async fn find_item_by_barcode_then_field(db: PgPool) {
        let f = order_fixture(&db).await;
        let other = second_item(&db, f.product_id).await;
        let mut c = db.acquire().await.unwrap();
        sqlx::query("UPDATE collection_items SET field_values = $2 WHERE id = $1")
            .bind(f.product_id)
            .bind(serde_json::json!({"cml2_bar_code": "4600000000017"}))
            .execute(&mut *c)
            .await
            .unwrap();
        assert_eq!(
            find_item(&mut c, " 4600000000017 ").await.unwrap(),
            Some(f.product_id)
        );
        assert_eq!(find_item(&mut c, "0000").await.unwrap(), None);
        // Код в обоих местах у разных товаров — берётся из catalog_barcodes
        set_common(&mut c, other, &strings(&["4600000000017"]), None)
            .await
            .unwrap();
        assert_eq!(
            find_item(&mut c, "4600000000017").await.unwrap(),
            Some(other)
        );
    }

    #[test]
    fn looks_like_barcode_cases() {
        assert!(looks_like_barcode("4601234567890"));
        assert!(!looks_like_barcode("1044"));
        assert!(looks_like_barcode("12345678"));
        assert!(!looks_like_barcode("abc12345678"));
    }
}
