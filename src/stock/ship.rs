//! Отгрузка: списание остатков основной отгрузки заказа (`shipQuantityWithStoreControl` /
//! `shipQuantityWithoutStoreControl` каталога Битрикса).

use std::collections::{HashMap, HashSet};

use sqlx::PgConnection;

use super::reserve::{self, Event};
use super::{Condition, Settings, StockError, recalc_quantity, settings, store_name};
use super::{barcodes, batches};
use crate::{catalog, sale::repo::traced_locked, sale::stock::StockLine};

const EPS: f64 = 1e-9;

/// Склад списания позиции (только со складским учётом): выбранный (`shipment_item_stores`
/// или форма), иначе склад позиции, иначе единственный активный склад с `amount` > 0.
/// `amounts` — `(склад, amount)` активных складов товара.
pub fn pick_store(
    chosen: Option<i64>,
    line_store: Option<i64>,
    amounts: &[(i64, f64)],
) -> Option<i64> {
    if let Some(store) = chosen.or(line_store) {
        return Some(store);
    }
    let mut positive = amounts.iter().filter(|(_, a)| *a > EPS);
    match (positive.next(), positive.next()) {
        (Some((store, _)), None) => Some(*store),
        _ => None,
    }
}

/// Откуда списывается количество позиции.
#[derive(Debug, Clone, PartialEq)]
pub enum Take {
    /// Со складским учётом: склад знает вызывающий.
    Store {
        from_reserve: f64,
        from_available: f64,
    },
    Product {
        from_reserve: f64,
        from_available: f64,
    },
}

/// Чего не хватило для отгрузки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortOf {
    Store,
    Product,
}

/// Списание позиции: `reserved` — её резерв, `store_amount` — остаток склада списания,
/// `available` — доступное количество товара.
pub fn take(
    store_control: bool,
    quantity: f64,
    reserved: f64,
    store_amount: f64,
    available: f64,
    can_buy_zero: bool,
) -> Result<Take, ShortOf> {
    let from_reserve = reserved.max(0.0).min(quantity);
    let from_available = quantity - from_reserve;
    if store_control {
        if store_amount + EPS < quantity {
            return Err(ShortOf::Store);
        }
        if from_available > available + EPS {
            return Err(ShortOf::Product);
        }
        return Ok(Take::Store {
            from_reserve,
            from_available,
        });
    }
    if from_available > available + EPS && !can_buy_zero {
        return Err(ShortOf::Product);
    }
    Ok(Take::Product {
        from_reserve,
        from_available,
    })
}

#[derive(sqlx::FromRow)]
struct Line {
    id: i64,
    item_id: i64,
    store_id: Option<i64>,
    quantity: f64,
    name: String,
}

/// Списание позиции после всех проверок.
struct Move {
    line: i64,
    item_id: i64,
    store: Option<i64>,
    quantity: f64,
}

/// Ставит или снимает «Отгружено» (`deducted`, `deducted_at`); `false` — уже так (или
/// отгрузки нет). `stores` — выбранный склад позиций `(cart_item_id, store_id)`.
///
/// Остатки движутся только по основной отгрузке; ошибка любой позиции отменяет отгрузку
/// целиком. Снятие возвращает товар на те же склады и восстанавливает резерв по условию.
pub async fn set_deducted(
    tx: &mut PgConnection,
    shipment_id: i64,
    deducted: bool,
    stores: &[(i64, i64)],
    user: Option<i64>,
) -> Result<bool, StockError> {
    let Some(order_id): Option<i64> =
        sqlx::query_scalar("SELECT order_id FROM shipments WHERE id = $1")
            .bind(shipment_id)
            .fetch_optional(&mut *tx)
            .await?
    else {
        return Ok(false);
    };
    // Заказ — раньше отгрузки и товаров
    let canceled: bool = sqlx::query_scalar("SELECT canceled FROM orders WHERE id = $1 FOR UPDATE")
        .bind(order_id)
        .fetch_one(&mut *tx)
        .await?;
    let (was, main): (bool, bool) = sqlx::query_as(
        "SELECT deducted, id = (SELECT min(id) FROM shipments WHERE order_id = $2)
         FROM shipments WHERE id = $1 FOR UPDATE",
    )
    .bind(shipment_id)
    .bind(order_id)
    .fetch_one(&mut *tx)
    .await?;
    if was == deducted {
        return Ok(false);
    }
    if deducted && canceled {
        return Err(StockError::Rejected(vec![
            "Невозможно отгрузить отмененный заказ".into(),
        ]));
    }
    if main {
        let s = settings(&mut *tx).await?;
        if deducted {
            ship(tx, order_id, shipment_id, stores, user, &s).await?;
        } else {
            unship(tx, order_id, shipment_id, &s).await?;
        }
    }
    sqlx::query(
        "UPDATE shipments SET deducted = $2, deducted_at = CASE WHEN $2 THEN now() END
         WHERE id = $1",
    )
    .bind(shipment_id)
    .bind(deducted)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE orders SET updated_at = now() WHERE id = $1")
        .bind(order_id)
        .execute(&mut *tx)
        .await?;
    if main && !deducted {
        reserve::sync(tx, order_id, Event::Uncanceled).await?;
    }
    Ok(true)
}

/// Сохраняет выбранные склады позиций отгрузки `(позиция, склад)`; `None` — выбор снят.
/// Склады отгруженной отгрузки не меняются; позиции чужого заказа пропускаются.
pub async fn choose_stores(
    tx: &mut PgConnection,
    shipment_id: i64,
    choices: &[(i64, Option<i64>)],
) -> sqlx::Result<()> {
    let Some(order_id): Option<i64> =
        sqlx::query_scalar("SELECT order_id FROM shipments WHERE id = $1")
            .bind(shipment_id)
            .fetch_optional(&mut *tx)
            .await?
    else {
        return Ok(());
    };
    sqlx::query("SELECT id FROM orders WHERE id = $1 FOR UPDATE")
        .bind(order_id)
        .execute(&mut *tx)
        .await?;
    let deducted: bool =
        sqlx::query_scalar("SELECT deducted FROM shipments WHERE id = $1 FOR UPDATE")
            .bind(shipment_id)
            .fetch_one(&mut *tx)
            .await?;
    if deducted {
        return Ok(());
    }
    for (line, store) in choices {
        sqlx::query(
            "DELETE FROM shipment_item_stores WHERE shipment_id = $1 AND cart_item_id = $2",
        )
        .bind(shipment_id)
        .bind(line)
        .execute(&mut *tx)
        .await?;
        if let Some(store) = store {
            sqlx::query(
                "INSERT INTO shipment_item_stores (shipment_id, cart_item_id, store_id, quantity)
                 SELECT $1, id, $3, quantity FROM cart_items WHERE id = $2 AND order_id = $4",
            )
            .bind(shipment_id)
            .bind(line)
            .bind(store)
            .bind(order_id)
            .execute(&mut *tx)
            .await?;
        }
    }
    Ok(())
}

async fn order_lines(tx: &mut PgConnection, order_id: i64) -> sqlx::Result<Vec<Line>> {
    sqlx::query_as(
        "SELECT id, item_id, store_id, quantity::float8 AS quantity, name FROM cart_items
         WHERE order_id = $1 ORDER BY id",
    )
    .bind(order_id)
    .fetch_all(tx)
    .await
}

/// Товары с учётом количества среди позиций; строки товаров и остатков блокируются.
async fn lock_traced(tx: &mut PgConnection, lines: &[Line]) -> sqlx::Result<HashSet<i64>> {
    let stock_lines: Vec<StockLine> = lines
        .iter()
        .map(|l| StockLine {
            product_id: l.item_id,
            store_id: l.store_id,
            quantity: l.quantity,
        })
        .collect();
    traced_locked(tx, &stock_lines).await
}

async fn ship(
    tx: &mut PgConnection,
    order_id: i64,
    shipment_id: i64,
    chosen: &[(i64, i64)],
    user: Option<i64>,
    s: &Settings,
) -> Result<(), StockError> {
    let lines = order_lines(tx, order_id).await?;
    let traced = lock_traced(tx, &lines).await?;
    let lines: Vec<Line> = lines
        .into_iter()
        .filter(|l| traced.contains(&l.item_id) && l.quantity > EPS)
        .collect();
    if lines.is_empty() {
        return Ok(());
    }
    // Условие «при отгрузке»: сначала резерв, затем списание из него
    if s.condition == Condition::OnShip {
        reserve::reserve_lines(tx, order_id, user).await?;
    }
    let ids: Vec<i64> = lines.iter().map(|l| l.id).collect();
    let items: Vec<i64> = lines.iter().map(|l| l.item_id).collect();
    let reserved: HashMap<i64, f64> = sqlx::query_as::<_, (i64, f64)>(
        "SELECT cart_item_id, sum(quantity)::float8 FROM basket_reservations
         WHERE cart_item_id = ANY($1) GROUP BY cart_item_id",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let info = catalog::load_on(tx, &items).await?;
    let mut available: HashMap<i64, f64> = info.iter().map(|(id, i)| (*id, i.total)).collect();
    // Физический остаток (товар, склад) и активные склады товара
    let mut amounts: HashMap<(i64, i64), f64> = HashMap::new();
    let mut active: HashMap<i64, Vec<(i64, f64)>> = HashMap::new();
    if s.store_control {
        for (item, store, amount, is_active) in sqlx::query_as::<_, (i64, i64, f64, bool)>(
            "SELECT a.item_id, a.store_id, a.amount::float8, COALESCE(st.active, FALSE)
             FROM catalog_store_amounts a LEFT JOIN catalog_stores st ON st.id = a.store_id
             WHERE a.item_id = ANY($1) ORDER BY a.item_id, a.store_id",
        )
        .bind(&items)
        .fetch_all(&mut *tx)
        .await?
        {
            amounts.insert((item, store), amount);
            if is_active {
                active.entry(item).or_default().push((store, amount));
            }
        }
    }
    let saved: HashMap<i64, i64> = sqlx::query_as::<_, (i64, i64)>(
        "SELECT cart_item_id, min(store_id) FROM shipment_item_stores
         WHERE shipment_id = $1 GROUP BY cart_item_id",
    )
    .bind(shipment_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let chosen: HashMap<i64, i64> = chosen.iter().copied().collect();

    // Поштучные товары: их штрихкоды проверяются и привязываются к заказу
    let multi: HashSet<i64> = if s.store_control {
        sqlx::query_scalar(
            "SELECT item_id FROM catalog_products WHERE item_id = ANY($1) AND barcode_multi",
        )
        .bind(&items)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect()
    } else {
        HashSet::new()
    };
    let mut barcode_lines: Vec<(i64, i64, String, i64)> = Vec::new();
    let mut errors = Vec::new();
    let mut moves = Vec::new();
    for line in &lines {
        let can_buy_zero = info.get(&line.item_id).is_some_and(|i| i.can_buy_zero);
        let store = if s.store_control {
            let pick = pick_store(
                chosen.get(&line.id).or(saved.get(&line.id)).copied(),
                line.store_id,
                active.get(&line.item_id).map_or(&[][..], Vec::as_slice),
            );
            let Some(store) = pick else {
                errors.push(format!(
                    "Не указаны склады для отгрузки товара {} (#{})",
                    line.name, line.item_id
                ));
                continue;
            };
            if multi.contains(&line.item_id) {
                barcode_lines.push((line.id, line.item_id, line.name.clone(), store));
            }
            Some(store)
        } else {
            None
        };
        let store_amount = store.map_or(0.0, |st| {
            amounts.get(&(line.item_id, st)).copied().unwrap_or(0.0)
        });
        let avail = available.get(&line.item_id).copied().unwrap_or(0.0);
        let line_reserved = reserved.get(&line.id).copied().unwrap_or(0.0);
        match take(
            s.store_control,
            line.quantity,
            line_reserved,
            store_amount,
            avail,
            can_buy_zero,
        ) {
            Ok(Take::Store { from_available, .. } | Take::Product { from_available, .. }) => {
                // Лишний резерв позиции (если он больше количества) вернётся в доступное
                let extra = (line_reserved - line.quantity).max(0.0);
                *available.entry(line.item_id).or_insert(0.0) += extra - from_available;
                if let Some(st) = store {
                    *amounts.entry((line.item_id, st)).or_insert(0.0) -= line.quantity;
                }
                moves.push(Move {
                    line: line.id,
                    item_id: line.item_id,
                    store,
                    quantity: line.quantity,
                });
            }
            Err(ShortOf::Store) => {
                let st = store.unwrap_or_default();
                errors.push(format!(
                    "Недостаточное количество товара \"{}\" (#{}) на складе \"{}\" (#{st}) для отгрузки.",
                    line.name,
                    line.item_id,
                    store_name(tx, st).await?
                ));
            }
            Err(ShortOf::Product) => errors.push(format!(
                "Недостаточное количество товара {} (#{}) для отгрузки",
                line.name, line.item_id
            )),
        }
    }
    // Ошибки штрихкодов — в тот же отказ: отгрузка отменяется целиком
    if !barcode_lines.is_empty()
        && let Err(e) = barcodes::ship_out(tx, shipment_id, &barcode_lines).await
    {
        match e {
            StockError::Rejected(m) => errors.extend(m),
            other => return Err(other),
        }
    }
    if !errors.is_empty() {
        return Err(StockError::Rejected(errors));
    }

    for m in &moves {
        // Резерв позиции снимается: триггер возвращает его в доступное
        sqlx::query("DELETE FROM basket_reservations WHERE cart_item_id = $1")
            .bind(m.line)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM shipment_item_stores WHERE shipment_id = $1 AND cart_item_id = $2",
        )
        .bind(shipment_id)
        .bind(m.line)
        .execute(&mut *tx)
        .await?;
        match m.store {
            Some(store) => {
                sqlx::query(
                    "UPDATE catalog_store_amounts SET amount = amount - $3::float8::numeric
                     WHERE item_id = $1 AND store_id = $2",
                )
                .bind(m.item_id)
                .bind(store)
                .bind(m.quantity)
                .execute(&mut *tx)
                .await?;
                let row: i64 = sqlx::query_scalar(
                    "INSERT INTO shipment_item_stores (shipment_id, cart_item_id, store_id, quantity)
                     VALUES ($1, $2, $3, $4::float8::numeric)
                     RETURNING id",
                )
                .bind(shipment_id)
                .bind(m.line)
                .bind(store)
                .bind(m.quantity)
                .fetch_one(&mut *tx)
                .await?;
                if let Some(method) = s.method {
                    batches::ship_out(tx, method, row, m.item_id, store, m.quantity).await?;
                }
            }
            None => {
                sqlx::query(
                    "UPDATE catalog_products SET quantity = quantity - $2::float8::numeric
                     WHERE item_id = $1",
                )
                .bind(m.item_id)
                .bind(m.quantity)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    recalc_quantity(tx, &items).await?;
    Ok(())
}

async fn unship(
    tx: &mut PgConnection,
    order_id: i64,
    shipment_id: i64,
    s: &Settings,
) -> Result<(), StockError> {
    let lines = order_lines(tx, order_id).await?;
    let traced = lock_traced(tx, &lines).await?;
    let rows: Vec<(i64, i64, f64)> = sqlx::query_as(
        "SELECT cart_item_id, store_id, quantity::float8 FROM shipment_item_stores
         WHERE shipment_id = $1",
    )
    .bind(shipment_id)
    .fetch_all(&mut *tx)
    .await?;
    if s.store_control {
        barcodes::ship_back(tx, shipment_id).await?;
    }
    if let Some(method) = s.method {
        batches::ship_back(tx, method, shipment_id).await?;
    }
    let mut items = Vec::new();
    for line in lines.iter().filter(|l| traced.contains(&l.item_id)) {
        items.push(line.item_id);
        // Товар возвращается на те же склады; выбор склада остаётся для следующей отгрузки
        for (_, store, quantity) in rows.iter().filter(|r| r.0 == line.id) {
            sqlx::query(
                "INSERT INTO catalog_store_amounts (item_id, store_id, amount)
                 VALUES ($1, $2, $3::float8::numeric)
                 ON CONFLICT (item_id, store_id) DO UPDATE
                 SET amount = catalog_store_amounts.amount + EXCLUDED.amount",
            )
            .bind(line.item_id)
            .bind(store)
            .bind(quantity)
            .execute(&mut *tx)
            .await?;
        }
        if !s.store_control {
            sqlx::query(
                "UPDATE catalog_products SET quantity = quantity + $2::float8::numeric
                 WHERE item_id = $1",
            )
            .bind(line.item_id)
            .bind(line.quantity)
            .execute(&mut *tx)
            .await?;
        }
    }
    recalc_quantity(tx, &items).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_store_steps() {
        assert_eq!(pick_store(Some(4), Some(1), &[]), Some(4));
        assert_eq!(pick_store(None, Some(1), &[]), Some(1));
        assert_eq!(pick_store(None, None, &[(2, 0.0), (3, 5.0)]), Some(3));
        assert_eq!(pick_store(None, None, &[(2, 1.0), (3, 5.0)]), None);
        assert_eq!(pick_store(None, None, &[]), None);
    }

    #[test]
    fn take_with_store_control() {
        assert_eq!(
            take(true, 3.0, 2.0, 5.0, 10.0, false),
            Ok(Take::Store {
                from_reserve: 2.0,
                from_available: 1.0
            })
        );
        assert_eq!(take(true, 6.0, 2.0, 5.0, 10.0, false), Err(ShortOf::Store));
        assert_eq!(take(true, 3.0, 0.0, 5.0, 1.0, false), Err(ShortOf::Product));
    }

    #[test]
    fn take_without_store_control() {
        assert_eq!(
            take(false, 3.0, 1.0, 0.0, 1.0, false),
            Err(ShortOf::Product)
        );
        assert_eq!(
            take(false, 3.0, 1.0, 0.0, 1.0, true),
            Ok(Take::Product {
                from_reserve: 1.0,
                from_available: 2.0
            })
        );
        assert_eq!(
            take(false, 3.0, 3.0, 0.0, 0.0, false),
            Ok(Take::Product {
                from_reserve: 3.0,
                from_available: 0.0
            })
        );
    }

    // --- БД ---

    use crate::stock::{StockError, barcodes};
    use crate::test_support::{Fixture, order_fixture, place_order_with};
    use sqlx::PgPool;

    async fn set_option(db: &PgPool, module: &str, name: &str, value: &str) {
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ($1, $2, $3)
             ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(module)
        .bind(name)
        .bind(value)
        .execute(db)
        .await
        .unwrap();
    }

    async fn exec(db: &PgPool, sql: &str, id: i64) {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .execute(db)
            .await
            .unwrap();
    }

    /// Складской учёт включён, позиция фикстуры из корзины убрана.
    async fn setup(db: &PgPool, store_control: bool) -> Fixture {
        let f = order_fixture(db).await;
        if store_control {
            set_option(db, "catalog", "default_use_store_control", "Y").await;
        }
        exec(db, "DELETE FROM cart_items WHERE id = $1", f.item_id).await;
        f
    }

    async fn add_store(db: &PgPool, id: i64, item: i64, amount: f64) {
        sqlx::query("INSERT INTO catalog_stores (id, name) VALUES ($1, 'Склад ' || $1)")
            .bind(id)
            .execute(db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO catalog_store_amounts (item_id, store_id, amount)
             VALUES ($1, $2, $3::float8::numeric)",
        )
        .bind(item)
        .bind(id)
        .bind(amount)
        .execute(db)
        .await
        .unwrap();
    }

    /// Основная отгрузка и позиция заказа.
    async fn parts(db: &PgPool, order: i64) -> (i64, i64) {
        sqlx::query_as(
            "SELECT (SELECT min(id) FROM shipments WHERE order_id = $1),
                    (SELECT min(id) FROM cart_items WHERE order_id = $1)",
        )
        .bind(order)
        .fetch_one(db)
        .await
        .unwrap()
    }

    /// Доступно, резерв товара, `amount` и резерв склада 1.
    async fn counters(db: &PgPool, item: i64) -> (f64, f64, f64, f64) {
        sqlx::query_as(
            "SELECT p.quantity::float8, p.quantity_reserved::float8,
                    a.amount::float8, a.quantity_reserved::float8
             FROM catalog_products p
             JOIN catalog_store_amounts a ON a.item_id = p.item_id AND a.store_id = 1
             WHERE p.item_id = $1",
        )
        .bind(item)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn amount(db: &PgPool, item: i64, store: i64) -> f64 {
        sqlx::query_scalar(
            "SELECT amount::float8 FROM catalog_store_amounts WHERE item_id = $1 AND store_id = $2",
        )
        .bind(item)
        .bind(store)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn deducted(db: &PgPool, shipment: i64) -> (bool, bool) {
        sqlx::query_as("SELECT deducted, deducted_at IS NOT NULL FROM shipments WHERE id = $1")
            .bind(shipment)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn ship_rows(db: &PgPool, shipment: i64) -> Vec<(i64, i64, f64)> {
        sqlx::query_as(
            "SELECT cart_item_id, store_id, quantity::float8 FROM shipment_item_stores
             WHERE shipment_id = $1 ORDER BY cart_item_id, store_id",
        )
        .bind(shipment)
        .fetch_all(db)
        .await
        .unwrap()
    }

    async fn ship(
        db: &PgPool,
        shipment: i64,
        on: bool,
        stores: &[(i64, i64)],
    ) -> Result<bool, StockError> {
        let mut tx = db.begin().await.unwrap();
        let out = set_deducted(&mut tx, shipment, on, stores, None).await;
        if out.is_ok() {
            tx.commit().await.unwrap();
        }
        out
    }

    fn rejected(r: Result<bool, StockError>) -> Vec<String> {
        match r {
            Err(StockError::Rejected(m)) => m,
            other => panic!("ожидался отказ: {other:?}"),
        }
    }

    #[sqlx::test]
    async fn ship_deducts_store_and_reserve(db: PgPool) {
        let f = setup(&db, true).await;
        let order = place_order_with(&db, &f, None, Some(1), 2.0).await;
        let (sh, line) = parts(&db, order).await;
        assert_eq!(counters(&db, f.product_id).await, (8.0, 2.0, 5.0, 2.0));

        assert!(ship(&db, sh, true, &[]).await.unwrap());
        assert_eq!(counters(&db, f.product_id).await, (3.0, 0.0, 3.0, 0.0));
        assert_eq!(ship_rows(&db, sh).await, [(line, 1, 2.0)]);
        assert_eq!(deducted(&db, sh).await, (true, true));
        // Повторно — уже так
        assert!(!ship(&db, sh, true, &[]).await.unwrap());
        assert_eq!(counters(&db, f.product_id).await, (3.0, 0.0, 3.0, 0.0));
    }

    #[sqlx::test]
    async fn unship_restores(db: PgPool) {
        let f = setup(&db, true).await;
        let order = place_order_with(&db, &f, None, Some(1), 2.0).await;
        let (sh, line) = parts(&db, order).await;
        assert!(ship(&db, sh, true, &[]).await.unwrap());
        assert!(ship(&db, sh, false, &[]).await.unwrap());
        assert_eq!(counters(&db, f.product_id).await, (3.0, 2.0, 5.0, 2.0));
        assert_eq!(deducted(&db, sh).await, (false, false));
        // Выбор склада остаётся
        assert_eq!(ship_rows(&db, sh).await, [(line, 1, 2.0)]);
    }

    async fn choose(db: &PgPool, shipment: i64, choices: &[(i64, Option<i64>)]) {
        let mut tx = db.begin().await.unwrap();
        choose_stores(&mut tx, shipment, choices).await.unwrap();
        tx.commit().await.unwrap();
    }

    #[sqlx::test]
    async fn chosen_store_persists(db: PgPool) {
        let f = setup(&db, true).await;
        add_store(&db, 2, f.product_id, 5.0).await;
        let order = place_order_with(&db, &f, None, None, 2.0).await;
        let (sh, line) = parts(&db, order).await;

        // Выбор без отгрузки сохраняется; пустой — снимается
        choose(&db, sh, &[(line, Some(1))]).await;
        assert_eq!(ship_rows(&db, sh).await, [(line, 1, 2.0)]);
        choose(&db, sh, &[(line, None)]).await;
        assert!(ship_rows(&db, sh).await.is_empty());
        choose(&db, sh, &[(line, Some(2))]).await;
        assert_eq!(ship_rows(&db, sh).await, [(line, 2, 2.0)]);

        assert!(ship(&db, sh, true, &[]).await.unwrap());
        assert_eq!(amount(&db, f.product_id, 2).await, 3.0);
        assert_eq!(amount(&db, f.product_id, 1).await, 5.0);
        // Отгруженной склады не меняются
        choose(&db, sh, &[(line, Some(1))]).await;
        assert_eq!(ship_rows(&db, sh).await, [(line, 2, 2.0)]);

        // Снятие и повторная отгрузка — с того же склада
        assert!(ship(&db, sh, false, &[]).await.unwrap());
        assert_eq!(amount(&db, f.product_id, 2).await, 5.0);
        assert_eq!(ship_rows(&db, sh).await, [(line, 2, 2.0)]);
        assert!(ship(&db, sh, true, &[]).await.unwrap());
        assert_eq!(amount(&db, f.product_id, 2).await, 3.0);
        assert_eq!(amount(&db, f.product_id, 1).await, 5.0);
        assert_eq!(ship_rows(&db, sh).await, [(line, 2, 2.0)]);
    }

    #[sqlx::test]
    async fn allow_delivery_ships_from_chosen_store(db: PgPool) {
        let f = setup(&db, true).await;
        set_option(&db, "sale", "allow_deduction_on_delivery", "Y").await;
        add_store(&db, 2, f.product_id, 5.0).await;
        let order = place_order_with(&db, &f, None, None, 2.0).await;
        let (sh, line) = parts(&db, order).await;
        choose(&db, sh, &[(line, Some(2))]).await;
        assert!(
            crate::sale::repo::set_shipment(&db, order, "", true)
                .await
                .unwrap()
        );
        assert_eq!(deducted(&db, sh).await, (true, true));
        assert_eq!(amount(&db, f.product_id, 2).await, 3.0);
        assert_eq!(amount(&db, f.product_id, 1).await, 5.0);
    }

    #[sqlx::test]
    async fn ship_without_store_errors(db: PgPool) {
        let f = setup(&db, true).await;
        add_store(&db, 2, f.product_id, 5.0).await;
        let order = place_order_with(&db, &f, None, None, 2.0).await;
        let (sh, line) = parts(&db, order).await;
        let before = counters(&db, f.product_id).await;

        assert_eq!(
            rejected(ship(&db, sh, true, &[]).await),
            [format!(
                "Не указаны склады для отгрузки товара Товар (#{})",
                f.product_id
            )]
        );
        assert_eq!(counters(&db, f.product_id).await, before);
        assert_eq!(amount(&db, f.product_id, 2).await, 5.0);
        assert_eq!(deducted(&db, sh).await, (false, false));
        assert!(ship_rows(&db, sh).await.is_empty());

        assert!(ship(&db, sh, true, &[(line, 2)]).await.unwrap());
        assert_eq!(amount(&db, f.product_id, 2).await, 3.0);
        assert_eq!(amount(&db, f.product_id, 1).await, 5.0);
        assert_eq!(ship_rows(&db, sh).await, [(line, 2, 2.0)]);
        // Доступное: 5 + 3 − резервов нет
        assert_eq!(counters(&db, f.product_id).await.0, 8.0);
    }

    #[sqlx::test]
    async fn ship_auto_single_store(db: PgPool) {
        let f = setup(&db, true).await;
        exec(
            &db,
            "UPDATE catalog_store_amounts SET amount = 0 WHERE item_id = $1",
            f.product_id,
        )
        .await;
        add_store(&db, 2, f.product_id, 5.0).await;
        let order = place_order_with(&db, &f, None, None, 2.0).await;
        let (sh, line) = parts(&db, order).await;
        assert!(ship(&db, sh, true, &[]).await.unwrap());
        assert_eq!(amount(&db, f.product_id, 2).await, 3.0);
        assert_eq!(ship_rows(&db, sh).await, [(line, 2, 2.0)]);
        assert_eq!(counters(&db, f.product_id).await.0, 3.0);
    }

    #[sqlx::test]
    async fn ship_short_store(db: PgPool) {
        let f = setup(&db, true).await;
        exec(
            &db,
            "UPDATE catalog_store_amounts SET amount = 1 WHERE item_id = $1",
            f.product_id,
        )
        .await;
        let order = place_order_with(&db, &f, None, Some(1), 2.0).await;
        let (sh, _) = parts(&db, order).await;
        let before = counters(&db, f.product_id).await;
        assert_eq!(
            rejected(ship(&db, sh, true, &[]).await),
            [format!(
                "Недостаточное количество товара \"Товар\" (#{}) на складе \"Склад\" (#1) для отгрузки.",
                f.product_id
            )]
        );
        assert_eq!(counters(&db, f.product_id).await, before);
        assert_eq!(deducted(&db, sh).await, (false, false));
    }

    #[sqlx::test]
    async fn ship_short_product(db: PgPool) {
        // Без учёта: резерва нет, доступного 1 из 2
        let f = setup(&db, false).await;
        set_option(&db, "catalog", "enable_reservation", "N").await;
        exec(
            &db,
            "UPDATE catalog_products SET quantity = 1 WHERE item_id = $1",
            f.product_id,
        )
        .await;
        let order = place_order_with(&db, &f, None, None, 2.0).await;
        let (sh, _) = parts(&db, order).await;
        assert_eq!(
            rejected(ship(&db, sh, true, &[]).await),
            [format!(
                "Недостаточное количество товара Товар (#{}) для отгрузки",
                f.product_id
            )]
        );
        exec(
            &db,
            "UPDATE catalog_products SET can_buy_zero = TRUE WHERE item_id = $1",
            f.product_id,
        )
        .await;
        assert!(ship(&db, sh, true, &[]).await.unwrap());
        assert_eq!(counters(&db, f.product_id).await.0, -1.0);
    }

    #[sqlx::test]
    async fn ship_without_store_control(db: PgPool) {
        let f = setup(&db, false).await;
        let order = place_order_with(&db, &f, None, None, 2.0).await;
        let (sh, _) = parts(&db, order).await;
        assert_eq!(counters(&db, f.product_id).await, (8.0, 2.0, 5.0, 0.0));
        assert!(ship(&db, sh, true, &[]).await.unwrap());
        assert_eq!(counters(&db, f.product_id).await, (8.0, 0.0, 5.0, 0.0));
        assert!(ship_rows(&db, sh).await.is_empty());

        // Снятие: товар возвращается в доступное и снова резервируется
        assert!(ship(&db, sh, false, &[]).await.unwrap());
        assert_eq!(counters(&db, f.product_id).await, (8.0, 2.0, 5.0, 0.0));
    }

    #[sqlx::test]
    async fn ship_on_allow_delivery(db: PgPool) {
        let f = setup(&db, true).await;
        set_option(&db, "sale", "allow_deduction_on_delivery", "Y").await;
        let order = place_order_with(&db, &f, None, Some(1), 2.0).await;
        let (sh, _) = parts(&db, order).await;
        assert!(
            crate::sale::repo::set_shipment(&db, order, "", true)
                .await
                .unwrap()
        );
        assert_eq!(deducted(&db, sh).await, (true, true));
        assert_eq!(counters(&db, f.product_id).await, (3.0, 0.0, 3.0, 0.0));

        // Без опции разрешение доставки не отгружает
        set_option(&db, "sale", "allow_deduction_on_delivery", "N").await;
        let other = place_order_with(&db, &f, None, Some(1), 1.0).await;
        let (osh, _) = parts(&db, other).await;
        assert!(
            crate::sale::repo::set_shipment(&db, other, "", true)
                .await
                .unwrap()
        );
        assert_eq!(deducted(&db, osh).await, (false, false));
    }

    #[sqlx::test]
    async fn ship_reserve_on_ship_condition(db: PgPool) {
        let f = setup(&db, false).await;
        set_option(&db, "sale", "product_reserve_condition", "S").await;
        let order = place_order_with(&db, &f, None, None, 2.0).await;
        let (sh, _) = parts(&db, order).await;
        assert_eq!(counters(&db, f.product_id).await.0, 10.0);
        assert!(ship(&db, sh, true, &[]).await.unwrap());
        assert_eq!(counters(&db, f.product_id).await, (8.0, 0.0, 5.0, 0.0));
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM basket_reservations")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(left, 0);

        // Со складским учётом — то же, доступное по складам
        set_option(&db, "catalog", "default_use_store_control", "Y").await;
        let other = place_order_with(&db, &f, None, Some(1), 1.0).await;
        let (osh, _) = parts(&db, other).await;
        assert!(ship(&db, osh, true, &[]).await.unwrap());
        assert_eq!(counters(&db, f.product_id).await, (4.0, 0.0, 4.0, 0.0));
    }

    #[sqlx::test]
    async fn untraced_line_not_deducted(db: PgPool) {
        let f = setup(&db, true).await;
        exec(
            &db,
            "UPDATE catalog_products SET quantity_trace = FALSE WHERE item_id = $1",
            f.product_id,
        )
        .await;
        let order = place_order_with(&db, &f, None, Some(1), 2.0).await;
        let (sh, _) = parts(&db, order).await;
        assert!(ship(&db, sh, true, &[]).await.unwrap());
        assert_eq!(counters(&db, f.product_id).await, (10.0, 0.0, 5.0, 0.0));
        assert_eq!(deducted(&db, sh).await, (true, true));
        assert!(ship_rows(&db, sh).await.is_empty());
        assert!(ship(&db, sh, false, &[]).await.unwrap());
        assert_eq!(counters(&db, f.product_id).await, (10.0, 0.0, 5.0, 0.0));
    }

    #[sqlx::test]
    async fn canceled_and_secondary_shipment(db: PgPool) {
        let f = setup(&db, true).await;
        let order = place_order_with(&db, &f, None, Some(1), 2.0).await;
        let (sh, _) = parts(&db, order).await;
        let second: i64 = sqlx::query_scalar(
            "INSERT INTO shipments (order_id, base_price) VALUES ($1, 0) RETURNING id",
        )
        .bind(order)
        .fetch_one(&db)
        .await
        .unwrap();
        // Не основная — только флаг
        assert!(ship(&db, second, true, &[]).await.unwrap());
        assert_eq!(deducted(&db, second).await, (true, true));
        assert_eq!(counters(&db, f.product_id).await, (8.0, 2.0, 5.0, 2.0));

        assert!(
            crate::sale::repo::set_canceled(&db, order, true, "")
                .await
                .unwrap()
        );
        assert_eq!(
            rejected(ship(&db, sh, true, &[]).await),
            ["Невозможно отгрузить отмененный заказ"]
        );
        assert_eq!(deducted(&db, sh).await, (false, false));
    }

    async fn make_multi(db: &PgPool, item: i64, codes: &[&str]) {
        sqlx::query("UPDATE catalog_products SET barcode_multi = TRUE WHERE item_id = $1")
            .bind(item)
            .execute(db)
            .await
            .unwrap();
        for c in codes {
            sqlx::query(
                "INSERT INTO catalog_barcodes (item_id, barcode, store_id) VALUES ($1, $2, 1)",
            )
            .bind(item)
            .bind(c)
            .execute(db)
            .await
            .unwrap();
        }
    }

    async fn choose_codes(db: &PgPool, shipment: i64, line: i64, codes: &[&str]) {
        let mut tx = db.begin().await.unwrap();
        let list = codes.iter().map(|c| c.to_string()).collect();
        barcodes::choose_barcodes(&mut tx, shipment, &[(line, 1, list)])
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    async fn code_state(db: &PgPool, code: &str) -> (Option<i64>, Option<i64>) {
        sqlx::query_as("SELECT store_id, order_id FROM catalog_barcodes WHERE barcode = $1")
            .bind(code)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn multi_ship_requires_barcodes(db: PgPool) {
        let f = setup(&db, true).await;
        make_multi(&db, f.product_id, &["A1", "A2"]).await;
        let order = place_order_with(&db, &f, None, Some(1), 2.0).await;
        let (sh, line) = parts(&db, order).await;
        choose_codes(&db, sh, line, &["A1"]).await;
        let msgs = rejected(ship(&db, sh, true, &[]).await);
        assert_eq!(msgs, ["У товара \"Товар\" не указан штрихкод."]);
        assert_eq!(deducted(&db, sh).await, (false, false));
        assert_eq!(code_state(&db, "A1").await, (Some(1), None));
        assert_eq!(amount(&db, f.product_id, 1).await, 5.0);
    }

    #[sqlx::test]
    async fn multi_ship_unknown_barcode_rejected(db: PgPool) {
        let f = setup(&db, true).await;
        make_multi(&db, f.product_id, &["A1"]).await;
        let order = place_order_with(&db, &f, None, Some(1), 2.0).await;
        let (sh, line) = parts(&db, order).await;
        choose_codes(&db, sh, line, &["A1", "ZZ"]).await;
        let msgs = rejected(ship(&db, sh, true, &[]).await);
        assert_eq!(
            msgs,
            [format!(
                "Для товара Товар (#{}) не найден штрихкод \"ZZ\".",
                f.product_id
            )]
        );
        assert_eq!(code_state(&db, "A1").await, (Some(1), None));
    }

    #[sqlx::test]
    async fn multi_ship_binds_order_and_unship_returns(db: PgPool) {
        let f = setup(&db, true).await;
        make_multi(&db, f.product_id, &["A1", "A2", "A3"]).await;
        let order = place_order_with(&db, &f, None, Some(1), 2.0).await;
        let (sh, line) = parts(&db, order).await;
        choose_codes(&db, sh, line, &["A1", "A2"]).await;
        assert!(ship(&db, sh, true, &[]).await.unwrap());
        assert_eq!(code_state(&db, "A1").await, (None, Some(order)));
        assert_eq!(code_state(&db, "A2").await, (None, Some(order)));
        assert_eq!(code_state(&db, "A3").await, (Some(1), None));
        assert_eq!(amount(&db, f.product_id, 1).await, 3.0);

        assert!(ship(&db, sh, false, &[]).await.unwrap());
        assert_eq!(code_state(&db, "A1").await, (Some(1), None));
        assert_eq!(code_state(&db, "A2").await, (Some(1), None));
        assert_eq!(amount(&db, f.product_id, 1).await, 5.0);
    }

    #[sqlx::test]
    async fn ordinary_ship_unchanged(db: PgPool) {
        let f = setup(&db, true).await;
        let order = place_order_with(&db, &f, None, Some(1), 2.0).await;
        let (sh, line) = parts(&db, order).await;
        assert!(ship(&db, sh, true, &[]).await.unwrap());
        assert_eq!(counters(&db, f.product_id).await, (3.0, 0.0, 3.0, 0.0));
        assert_eq!(ship_rows(&db, sh).await, [(line, 1, 2.0)]);
        assert!(ship(&db, sh, false, &[]).await.unwrap());
        assert_eq!(counters(&db, f.product_id).await, (3.0, 2.0, 5.0, 2.0));
    }

    #[sqlx::test]
    async fn barcode_error_cancels_whole_shipment(db: PgPool) {
        let f = setup(&db, true).await;
        // Вторая позиция — поштучный товар без кодов; первая — обычный товар
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name)
             SELECT collection_id, 'multi', 'Поштучный' FROM collection_items WHERE id = $1
             RETURNING id",
        )
        .bind(f.product_id)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO catalog_products (item_id, quantity, quantity_trace, can_buy_zero, barcode_multi)
             VALUES ($1, 10, TRUE, FALSE, TRUE)",
        )
        .bind(other)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO catalog_store_amounts (item_id, store_id, amount) VALUES ($1, 1, 5)",
        )
        .bind(other)
        .execute(&db)
        .await
        .unwrap();
        let order = place_order_with(&db, &f, None, Some(1), 1.0).await;
        let (sh, _) = parts(&db, order).await;
        sqlx::query(
            "INSERT INTO cart_items (order_id, item_id, store_id, quantity, name)
             VALUES ($1, $2, 1, 1, 'Поштучный')",
        )
        .bind(order)
        .bind(other)
        .execute(&db)
        .await
        .unwrap();
        let msgs = rejected(ship(&db, sh, true, &[]).await);
        assert_eq!(msgs, ["У товара \"Поштучный\" не указан штрихкод."]);
        assert_eq!(deducted(&db, sh).await, (false, false));
        assert_eq!(amount(&db, f.product_id, 1).await, 5.0);
        assert_eq!(amount(&db, other, 1).await, 5.0);
    }
}
