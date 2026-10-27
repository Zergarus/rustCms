//! Резервы позиций заказа по условию `sale.product_reserve_condition` (`Sale\Order`).
//!
//! Код только ставит, меняет и удаляет строки `basket_reservations`; счётчики товара и склада
//! ведёт триггер. Строки товаров блокируются (`catalog::load_locked`) до любых изменений.

use std::collections::HashMap;

use sqlx::PgConnection;

use super::{Condition, Settings, settings};
use crate::{catalog, sale::repo::traced_locked, sale::stock::StockLine};

/// Событие заказа, после которого пересчитываются резервы.
#[derive(Debug, Clone, Copy)]
pub enum Event {
    Created,
    Edited,
    PaidChanged,
    AllowDeliveryChanged,
    Canceled,
    Uncanceled,
}

/// Состояние заказа для условия резерва; доставка и отгрузка — по основной отгрузке.
#[derive(Debug, Clone, Default)]
pub struct OrderState {
    pub canceled: bool,
    pub paid: bool,
    pub any_payment_paid: bool,
    pub allow_delivery: bool,
    pub shipped: bool,
}

/// Выполнено ли условие резерва.
pub fn condition_met(c: Condition, s: &OrderState) -> bool {
    match c {
        Condition::OnCreate => true,
        Condition::OnPay => s.any_payment_paid,
        Condition::OnFullPay => s.paid,
        Condition::OnAllowDelivery => s.allow_delivery,
        Condition::OnShip => false,
    }
}

/// Склад резерва: со складским учётом — склад позиции, иначе склад по умолчанию; без учёта —
/// без склада.
pub fn reserve_store(
    store_control: bool,
    line_store: Option<i64>,
    default_store: Option<i64>,
) -> Option<i64> {
    if store_control {
        line_store.or(default_store)
    } else {
        None
    }
}

/// Сколько можно зарезервировать: «можно покупать при отсутствии» — всё, иначе не больше
/// доступного.
pub fn reservable(requested: f64, available: f64, can_buy_zero: bool) -> f64 {
    if can_buy_zero {
        requested
    } else {
        requested.min(available.max(0.0))
    }
}

/// Состояние заказа; `None` — заказа нет.
async fn order_state(tx: &mut PgConnection, order_id: i64) -> sqlx::Result<Option<OrderState>> {
    let row: Option<(bool, bool, bool, bool, bool)> = sqlx::query_as(
        "SELECT o.canceled, o.paid,
                EXISTS (SELECT 1 FROM payments WHERE order_id = o.id AND paid),
                COALESCE(s.allow_delivery, FALSE), COALESCE(s.deducted, FALSE)
         FROM orders o
         LEFT JOIN shipments s ON s.id = (SELECT min(id) FROM shipments WHERE order_id = o.id)
         WHERE o.id = $1",
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(row.map(
        |(canceled, paid, any_payment_paid, allow_delivery, shipped)| OrderState {
            canceled,
            paid,
            any_payment_paid,
            allow_delivery,
            shipped,
        },
    ))
}

/// Пересчитывает резервы заказа после события; возвращает предупреждения о частичном резерве
/// (они же пишутся в историю заказа с его текущим статусом).
pub async fn sync(tx: &mut PgConnection, order_id: i64, event: Event) -> sqlx::Result<Vec<String>> {
    let s = settings(&mut *tx).await?;
    if !s.reservation {
        return Ok(Vec::new());
    }
    let Some(state) = order_state(tx, order_id).await? else {
        return Ok(Vec::new());
    };
    // Отгруженный заказ: резерв уже перешёл в списание
    if state.shipped {
        return Ok(Vec::new());
    }
    let warnings = if matches!(event, Event::Canceled) || state.canceled {
        release_lines(tx, order_id).await?;
        Vec::new()
    } else if condition_met(s.condition, &state) {
        adjust(tx, order_id, None, &s, true).await?
    } else if matches!(
        s.condition,
        Condition::OnPay | Condition::OnFullPay | Condition::OnAllowDelivery
    ) {
        release_lines(tx, order_id).await?;
        Vec::new()
    } else {
        adjust(tx, order_id, None, &s, false).await?
    };
    for w in &warnings {
        sqlx::query(
            "INSERT INTO order_status_history (order_id, status, comment)
             SELECT id, status, $2 FROM orders WHERE id = $1",
        )
        .bind(order_id)
        .bind(w)
        .execute(&mut *tx)
        .await?;
    }
    Ok(warnings)
}

/// Резервирует позиции заказа до их количества (лишнее урезается); возвращает предупреждения
/// о частичном резерве. `user` — кто поставил резерв (`None` — система).
pub async fn reserve_lines(
    tx: &mut PgConnection,
    order_id: i64,
    user: Option<i64>,
) -> sqlx::Result<Vec<String>> {
    let s = settings(&mut *tx).await?;
    if !s.reservation {
        return Ok(Vec::new());
    }
    adjust(tx, order_id, user, &s, true).await
}

/// Снимает все резервы позиций заказа.
pub async fn release_lines(tx: &mut PgConnection, order_id: i64) -> sqlx::Result<()> {
    let items: Vec<i64> = sqlx::query_scalar(
        "SELECT r.item_id FROM basket_reservations r JOIN cart_items c ON c.id = r.cart_item_id
         WHERE c.order_id = $1",
    )
    .bind(order_id)
    .fetch_all(&mut *tx)
    .await?;
    if items.is_empty() {
        return Ok(());
    }
    catalog::load_locked(tx, &items).await?;
    sqlx::query(
        "DELETE FROM basket_reservations
         WHERE cart_item_id IN (SELECT id FROM cart_items WHERE order_id = $1)",
    )
    .bind(order_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Line {
    id: i64,
    item_id: i64,
    store_id: Option<i64>,
    quantity: f64,
    name: String,
}

#[derive(sqlx::FromRow)]
struct Reservation {
    id: i64,
    cart_item_id: i64,
    store_id: Option<i64>,
    quantity: f64,
}

const EPS: f64 = 1e-9;

fn shown(v: f64) -> String {
    let v = (v * 1000.0).round() / 1000.0;
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

async fn write_history(tx: &mut PgConnection, reservation: i64, quantity: f64) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO basket_reservation_history (reservation_id, date_reserve, quantity)
         VALUES ($1, now(), $2::float8::numeric)",
    )
    .bind(reservation)
    .bind(quantity)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

async fn set_quantity(tx: &mut PgConnection, r: &Reservation, quantity: f64) -> sqlx::Result<()> {
    if quantity <= EPS {
        sqlx::query("DELETE FROM basket_reservations WHERE id = $1")
            .bind(r.id)
            .execute(&mut *tx)
            .await?;
        return Ok(());
    }
    sqlx::query("UPDATE basket_reservations SET quantity = $2::float8::numeric WHERE id = $1")
        .bind(r.id)
        .bind(quantity)
        .execute(&mut *tx)
        .await?;
    write_history(tx, r.id, quantity).await
}

/// Урезает резервы позиций до их количества (и снимает резервы не на том складе); при `top_up`
/// дорезервирует недостающее в пределах доступного.
async fn adjust(
    tx: &mut PgConnection,
    order_id: i64,
    user: Option<i64>,
    s: &Settings,
    top_up: bool,
) -> sqlx::Result<Vec<String>> {
    let lines: Vec<Line> = sqlx::query_as(
        "SELECT id, item_id, store_id, quantity::float8 AS quantity, name FROM cart_items
         WHERE order_id = $1 ORDER BY id",
    )
    .bind(order_id)
    .fetch_all(&mut *tx)
    .await?;
    let ids: Vec<i64> = lines.iter().map(|l| l.id).collect();
    let reservations: Vec<Reservation> = sqlx::query_as(
        "SELECT id, cart_item_id, store_id, quantity::float8 AS quantity FROM basket_reservations
         WHERE cart_item_id = ANY($1) ORDER BY id",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    if lines.is_empty() || (!top_up && reservations.is_empty()) {
        return Ok(Vec::new());
    }
    let stock_lines: Vec<StockLine> = lines
        .iter()
        .map(|l| StockLine {
            product_id: l.item_id,
            store_id: l.store_id,
            quantity: l.quantity,
        })
        .collect();
    let traced = traced_locked(tx, &stock_lines).await?;
    let default_store: Option<i64> = if s.store_control {
        sqlx::query_scalar("SELECT id FROM catalog_stores WHERE is_default AND active LIMIT 1")
            .fetch_optional(&mut *tx)
            .await?
    } else {
        None
    };

    // Урезание: резервы не на складе позиции снимаются, лишнее — с последних
    let mut kept: HashMap<i64, Vec<Reservation>> = HashMap::new();
    for r in reservations {
        kept.entry(r.cart_item_id).or_default().push(r);
    }
    for line in &lines {
        let Some(list) = kept.get_mut(&line.id) else {
            continue;
        };
        let store = reserve_store(s.store_control, line.store_id, default_store);
        let mut left = line.quantity;
        let mut keep = Vec::new();
        for r in list.drain(..) {
            let target = if r.store_id == store {
                r.quantity.min(left.max(0.0))
            } else {
                0.0
            };
            if (target - r.quantity).abs() > EPS {
                set_quantity(tx, &r, target).await?;
            }
            left -= target;
            if target > EPS {
                keep.push(Reservation {
                    quantity: target,
                    ..r
                });
            }
        }
        *list = keep;
    }
    if !top_up {
        return Ok(Vec::new());
    }

    // Дорезервирование по доступному после урезания (строки уже заблокированы)
    let products: Vec<i64> = lines.iter().map(|l| l.item_id).collect();
    let mut info = catalog::load_on(tx, &products).await?;
    let end: Option<chrono::DateTime<chrono::Utc>> = (s.clear_period > 0)
        .then(|| chrono::Utc::now() + chrono::Duration::days(i64::from(s.clear_period)));
    let mut warnings = Vec::new();
    for line in &lines {
        if !traced.contains(&line.item_id) {
            continue;
        }
        let Some(product) = info.get_mut(&line.item_id) else {
            continue;
        };
        let have: f64 = kept
            .get(&line.id)
            .map_or(0.0, |l| l.iter().map(|r| r.quantity).sum());
        let need = line.quantity - have;
        if need <= EPS {
            continue;
        }
        let store = reserve_store(s.store_control, line.store_id, default_store);
        let available = match store {
            Some(st) => product.amounts.get(&st).copied().unwrap_or(0.0),
            None => product.total,
        };
        let put = reservable(need, available, product.can_buy_zero);
        if put > EPS {
            product.total -= put;
            if let Some(st) = store {
                *product.amounts.entry(st).or_insert(0.0) -= put;
            }
            match kept.get(&line.id).and_then(|l| l.first()) {
                Some(r) => set_quantity(tx, r, r.quantity + put).await?,
                None => {
                    let id: i64 = sqlx::query_scalar(
                        "INSERT INTO basket_reservations
                             (cart_item_id, item_id, store_id, quantity, date_reserve_end, reserved_by)
                         VALUES ($1, $2, $3, $4::float8::numeric, $5, $6) RETURNING id",
                    )
                    .bind(line.id)
                    .bind(line.item_id)
                    .bind(store)
                    .bind(put)
                    .bind(end)
                    .bind(user)
                    .fetch_one(&mut *tx)
                    .await?;
                    write_history(tx, id, put).await?;
                }
            }
        }
        let missing = need - put;
        if missing > EPS {
            warnings.push(format!(
                "Не удалось зарезервировать {} шт. товара «{}»",
                shown(missing),
                line.name
            ));
        }
    }
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stock::Condition;

    fn state(paid: bool, any_payment_paid: bool, allow_delivery: bool) -> OrderState {
        OrderState {
            paid,
            any_payment_paid,
            allow_delivery,
            ..OrderState::default()
        }
    }

    #[test]
    fn condition_table() {
        let none = state(false, false, false);
        let partial = state(false, true, false);
        let full = state(true, true, false);
        let delivery = state(false, false, true);
        for s in [&none, &partial, &full, &delivery] {
            assert!(condition_met(Condition::OnCreate, s));
            assert!(!condition_met(Condition::OnShip, s));
        }
        assert!(!condition_met(Condition::OnPay, &none));
        assert!(condition_met(Condition::OnPay, &partial));
        assert!(condition_met(Condition::OnPay, &full));
        assert!(!condition_met(Condition::OnPay, &delivery));
        assert!(!condition_met(Condition::OnFullPay, &none));
        assert!(!condition_met(Condition::OnFullPay, &partial));
        assert!(condition_met(Condition::OnFullPay, &full));
        assert!(!condition_met(Condition::OnFullPay, &delivery));
        assert!(!condition_met(Condition::OnAllowDelivery, &none));
        assert!(!condition_met(Condition::OnAllowDelivery, &full));
        assert!(condition_met(Condition::OnAllowDelivery, &delivery));
    }

    #[test]
    fn reserve_store_choice() {
        assert_eq!(reserve_store(true, Some(3), Some(1)), Some(3));
        assert_eq!(reserve_store(true, None, Some(1)), Some(1));
        assert_eq!(reserve_store(true, None, None), None);
        assert_eq!(reserve_store(false, Some(3), Some(1)), None);
    }

    #[test]
    fn reservable_partial() {
        assert_eq!(reservable(5.0, 3.0, false), 3.0);
        assert_eq!(reservable(5.0, -1.0, false), 0.0);
        assert_eq!(reservable(5.0, 0.0, true), 5.0);
    }

    // --- БД ---

    use crate::sale::repo;
    use crate::stock::StockError;
    use crate::test_support::{Fixture, order_fixture, place_order, place_order_with};
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

    async fn condition(db: &PgPool, code: &str) {
        set_option(db, "sale", "product_reserve_condition", code).await;
    }

    async fn order_with(db: &PgPool, f: &Fixture, store: Option<i64>, quantity: f64) -> i64 {
        place_order_with(db, f, None, store, quantity).await
    }

    /// Доступно, `amount` склада 1, резерв склада 1.
    async fn counters(db: &PgPool, item: i64) -> (f64, f64, f64) {
        sqlx::query_as(
            "SELECT p.quantity::float8, a.amount::float8, a.quantity_reserved::float8
             FROM catalog_products p
             JOIN catalog_store_amounts a ON a.item_id = p.item_id AND a.store_id = 1
             WHERE p.item_id = $1",
        )
        .bind(item)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn quantity(db: &PgPool, item: i64) -> f64 {
        counters(db, item).await.0
    }

    /// Резервы заказа: (склад, количество) по позициям.
    async fn reserves(db: &PgPool, order: i64) -> Vec<(Option<i64>, f64)> {
        sqlx::query_as(
            "SELECT r.store_id, r.quantity::float8 FROM basket_reservations r
             JOIN cart_items c ON c.id = r.cart_item_id WHERE c.order_id = $1 ORDER BY r.id",
        )
        .bind(order)
        .fetch_all(db)
        .await
        .unwrap()
    }

    async fn sync_now(db: &PgPool, order: i64, event: Event) -> Vec<String> {
        let mut tx = db.begin().await.unwrap();
        let out = sync(&mut tx, order, event).await.unwrap();
        tx.commit().await.unwrap();
        out
    }

    #[sqlx::test]
    async fn create_reserves_on_create(db: PgPool) {
        let f = order_fixture(&db).await;
        set_option(&db, "catalog", "default_use_store_control", "Y").await;
        // Позиция фикстуры на складе 1 занимает место в корзине
        sqlx::query("DELETE FROM cart_items WHERE id = $1")
            .bind(f.item_id)
            .execute(&db)
            .await
            .unwrap();
        let id = order_with(&db, &f, Some(1), 2.0).await;
        assert_eq!(counters(&db, f.product_id).await, (8.0, 5.0, 2.0));
        let rows: Vec<(f64, bool)> = sqlx::query_as(
            "SELECT quantity::float8, date_reserve_end IS NULL FROM basket_reservations",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(rows, [(2.0, true)]);
        let history: Vec<f64> =
            sqlx::query_scalar("SELECT quantity::float8 FROM basket_reservation_history")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(history, [2.0]);
        assert_eq!(reserves(&db, id).await, [(Some(1), 2.0)]);

        // Период 2 дня — срок резерва сегодня + 2 дня
        set_option(&db, "sale", "product_reserve_clear_period", "2").await;
        let other = order_with(&db, &f, Some(1), 1.0).await;
        let off: f64 = sqlx::query_scalar(
            "SELECT abs(extract(epoch FROM r.date_reserve_end - (now() + interval '2 days')))::float8
             FROM basket_reservations r JOIN cart_items c ON c.id = r.cart_item_id
             WHERE c.order_id = $1",
        )
        .bind(other)
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(off < 60.0, "{off}");
    }

    #[sqlx::test]
    async fn reservation_disabled_no_reserve(db: PgPool) {
        let f = order_fixture(&db).await;
        set_option(&db, "catalog", "enable_reservation", "N").await;
        let id = order_with(&db, &f, None, 2.0).await;
        assert_eq!(quantity(&db, f.product_id).await, 10.0);
        assert!(reserves(&db, id).await.is_empty());
    }

    #[sqlx::test]
    async fn on_pay_reserve(db: PgPool) {
        let f = order_fixture(&db).await;
        condition(&db, "R").await;
        let id = place_order(&db, &f, None).await;
        assert!(reserves(&db, id).await.is_empty());
        sqlx::query("INSERT INTO payments (order_id, name, sum) VALUES ($1, 'Вторая', 50)")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let pay = |paid: bool| {
            let db = db.clone();
            async move {
                sqlx::query(
                    "UPDATE payments SET paid = $2
                     WHERE id = (SELECT min(id) FROM payments WHERE order_id = $1)",
                )
                .bind(id)
                .bind(paid)
                .execute(&db)
                .await
                .unwrap();
                sync_now(&db, id, Event::PaidChanged).await;
            }
        };
        pay(true).await;
        assert_eq!(reserves(&db, id).await, [(None, 1.0)]);
        assert_eq!(quantity(&db, f.product_id).await, 9.0);
        pay(false).await;
        assert!(reserves(&db, id).await.is_empty());
        assert_eq!(quantity(&db, f.product_id).await, 10.0);
    }

    #[sqlx::test]
    async fn on_full_pay_reserve(db: PgPool) {
        let f = order_fixture(&db).await;
        condition(&db, "P").await;
        let id = place_order(&db, &f, None).await;
        assert!(reserves(&db, id).await.is_empty());
        let mut tx = db.begin().await.unwrap();
        assert!(repo::set_paid_in(&mut tx, id, true).await.unwrap());
        tx.commit().await.unwrap();
        assert_eq!(reserves(&db, id).await, [(None, 1.0)]);
        let mut tx = db.begin().await.unwrap();
        assert!(repo::set_paid_in(&mut tx, id, false).await.unwrap());
        tx.commit().await.unwrap();
        assert!(reserves(&db, id).await.is_empty());

        // Оплачена одна из двух оплат — заказ не оплачен полностью
        sqlx::query("INSERT INTO payments (order_id, name, sum) VALUES ($1, 'Вторая', 50)")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE payments SET paid = TRUE
             WHERE id = (SELECT min(id) FROM payments WHERE order_id = $1)",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        let mut tx = db.begin().await.unwrap();
        assert_eq!(repo::sync_paid_in(&mut tx, id).await.unwrap(), None);
        tx.commit().await.unwrap();
        assert!(reserves(&db, id).await.is_empty());
        assert_eq!(quantity(&db, f.product_id).await, 10.0);
    }

    #[sqlx::test]
    async fn on_allow_delivery_reserve(db: PgPool) {
        let f = order_fixture(&db).await;
        condition(&db, "D").await;
        let id = place_order(&db, &f, None).await;
        assert!(reserves(&db, id).await.is_empty());
        assert!(repo::set_shipment(&db, id, "", true).await.unwrap());
        assert_eq!(reserves(&db, id).await, [(None, 1.0)]);
        assert!(repo::set_shipment(&db, id, "", false).await.unwrap());
        assert!(reserves(&db, id).await.is_empty());
        assert_eq!(quantity(&db, f.product_id).await, 10.0);
    }

    #[sqlx::test]
    async fn on_ship_no_reserve_before_shipping(db: PgPool) {
        let f = order_fixture(&db).await;
        condition(&db, "S").await;
        let id = place_order(&db, &f, None).await;
        assert!(repo::set_paid(&db, id, true).await.unwrap());
        assert!(reserves(&db, id).await.is_empty());
        assert_eq!(quantity(&db, f.product_id).await, 10.0);
    }

    #[sqlx::test]
    async fn cancel_releases_and_uncancel_restores(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        assert_eq!(quantity(&db, f.product_id).await, 9.0);
        assert!(repo::set_canceled(&db, id, true, "тест").await.unwrap());
        assert_eq!(quantity(&db, f.product_id).await, 10.0);
        assert!(reserves(&db, id).await.is_empty());
        assert!(repo::set_canceled(&db, id, false, "").await.unwrap());
        assert_eq!(quantity(&db, f.product_id).await, 9.0);
        assert_eq!(reserves(&db, id).await, [(None, 1.0)]);
    }

    #[sqlx::test]
    async fn cancel_shipped_rejected(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        sqlx::query("UPDATE shipments SET deducted = TRUE WHERE order_id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let mut tx = db.begin().await.unwrap();
        let result = repo::set_canceled_in(&mut tx, id, true, "").await;
        assert!(
            matches!(&result, Err(StockError::Rejected(m)) if m == &["Некоторые товары уже отгружены"]),
            "{result:?}"
        );
        drop(tx);
        let canceled: bool = sqlx::query_scalar("SELECT canceled FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(!canceled);
    }

    #[sqlx::test]
    async fn partial_reserve_warns(db: PgPool) {
        let f = order_fixture(&db).await;
        sqlx::query("UPDATE catalog_products SET quantity = 5 WHERE item_id = $1")
            .bind(f.product_id)
            .execute(&db)
            .await
            .unwrap();
        let id = order_with(&db, &f, None, 7.0).await;
        assert_eq!(reserves(&db, id).await, [(None, 5.0)]);
        assert_eq!(quantity(&db, f.product_id).await, 0.0);
        let history: Vec<(String, String)> = sqlx::query_as(
            "SELECT status, comment FROM order_status_history WHERE order_id = $1 AND comment <> ''",
        )
        .bind(id)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            history,
            [(
                "N".to_string(),
                "Не удалось зарезервировать 2 шт. товара «Товар»".to_string()
            )]
        );
    }

    #[sqlx::test]
    async fn default_store_reserve(db: PgPool) {
        let f = order_fixture(&db).await;
        set_option(&db, "catalog", "default_use_store_control", "Y").await;
        sqlx::query(
            "INSERT INTO catalog_stores (id, name, is_default) VALUES (2, 'Склад 2', TRUE)",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO catalog_store_amounts (item_id, store_id, amount) VALUES ($1, 2, 5)",
        )
        .bind(f.product_id)
        .execute(&db)
        .await
        .unwrap();
        let id = place_order(&db, &f, None).await;
        assert_eq!(reserves(&db, id).await, [(Some(2), 1.0)]);
    }

    #[sqlx::test]
    async fn untraced_line_not_reserved(db: PgPool) {
        let f = order_fixture(&db).await;
        sqlx::query("UPDATE catalog_products SET quantity_trace = FALSE WHERE item_id = $1")
            .bind(f.product_id)
            .execute(&db)
            .await
            .unwrap();
        let id = place_order(&db, &f, None).await;
        assert!(reserves(&db, id).await.is_empty());
        assert_eq!(quantity(&db, f.product_id).await, 10.0);
    }
}
