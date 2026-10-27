//! Складской учёт: настройки, резервы и пересчёт доступного количества.
//!
//! Счётчики резервов (`quantity_reserved` товара и склада, `quantity` товара) ведёт триггер
//! `basket_reservations_counters`; здесь — пересчёт доступного по остаткам складов.

pub mod documents;
pub mod reserve;

use std::collections::HashMap;

use sqlx::PgConnection;

/// Когда ставится резерв (`sale.product_reserve_condition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // имена — по условиям Битрикса («при оформлении», «при оплате»…)
pub enum Condition {
    /// `O` — при оформлении заказа
    OnCreate,
    /// `R` — при оплате (частичной)
    OnPay,
    /// `P` — при полной оплате
    OnFullPay,
    /// `D` — при разрешении доставки
    OnAllowDelivery,
    /// `S` — при отгрузке
    OnShip,
}

impl Condition {
    pub fn from_code(code: &str) -> Condition {
        match code {
            "R" => Condition::OnPay,
            "P" => Condition::OnFullPay,
            "D" => Condition::OnAllowDelivery,
            "S" => Condition::OnShip,
            _ => Condition::OnCreate,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Condition::OnCreate => "O",
            Condition::OnPay => "R",
            Condition::OnFullPay => "P",
            Condition::OnAllowDelivery => "D",
            Condition::OnShip => "S",
        }
    }
}

/// Настройки складского учёта и резервирования (умолчания — как в `default_option.php`).
#[derive(Debug, Clone)]
pub struct Settings {
    /// `catalog.default_use_store_control`
    pub store_control: bool,
    /// `catalog.enable_reservation`
    pub reservation: bool,
    /// `catalog.allow_negative_amount`
    pub allow_negative: bool,
    /// `sale.product_reserve_condition`
    pub condition: Condition,
    /// `sale.product_reserve_clear_period` — дни, 0 — не снимать
    pub clear_period: i32,
    /// `sale.allow_deduction_on_delivery`
    pub deduct_on_delivery: bool,
}

pub async fn settings(db: &mut PgConnection) -> sqlx::Result<Settings> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT module, name, value FROM options
         WHERE (module = 'catalog' AND name IN ('default_use_store_control', 'enable_reservation',
                                                'allow_negative_amount'))
            OR (module = 'sale' AND name IN ('product_reserve_condition',
                                             'product_reserve_clear_period',
                                             'allow_deduction_on_delivery'))",
    )
    .fetch_all(db)
    .await?;
    let values: HashMap<(String, String), String> = rows
        .into_iter()
        .map(|(module, name, value)| ((module, name), value))
        .collect();
    let get = |module: &str, name: &str| values.get(&(module.to_string(), name.to_string()));
    let flag =
        |module: &str, name: &str, default: bool| get(module, name).map_or(default, |v| v == "Y");
    Ok(Settings {
        store_control: flag("catalog", "default_use_store_control", false),
        reservation: flag("catalog", "enable_reservation", true),
        allow_negative: flag("catalog", "allow_negative_amount", false),
        condition: get("sale", "product_reserve_condition")
            .map_or(Condition::OnCreate, |v| Condition::from_code(v)),
        clear_period: get("sale", "product_reserve_clear_period")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0),
        deduct_on_delivery: flag("sale", "allow_deduction_on_delivery", false),
    })
}

/// Доступное количество по инварианту складского учёта (`updateProductQuantity`):
/// Σ `amount` по активным складам − резерв товара. Без складского учёта ничего не делает.
pub async fn recalc_quantity(tx: &mut PgConnection, item_ids: &[i64]) -> sqlx::Result<()> {
    if item_ids.is_empty() || !settings(&mut *tx).await?.store_control {
        return Ok(());
    }
    recalc_where(tx, Some(item_ids)).await
}

/// То же для всех товаров с записями остатков.
pub async fn recalc_all(tx: &mut PgConnection) -> sqlx::Result<()> {
    if !settings(&mut *tx).await?.store_control {
        return Ok(());
    }
    recalc_where(tx, None).await
}

/// `item_ids` = `None` — все товары с записями остатков.
async fn recalc_where(tx: &mut PgConnection, item_ids: Option<&[i64]>) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE catalog_products p SET quantity = s.total - p.quantity_reserved
         FROM (
             SELECT p.item_id, COALESCE(sum(a.amount) FILTER (WHERE st.active), 0) AS total
             FROM catalog_products p
             LEFT JOIN catalog_store_amounts a ON a.item_id = p.item_id
             LEFT JOIN catalog_stores st ON st.id = a.store_id
             WHERE ($1::bigint[] IS NULL AND a.item_id IS NOT NULL) OR p.item_id = ANY($1)
             GROUP BY p.item_id
         ) s
         WHERE p.item_id = s.item_id AND p.quantity <> s.total - p.quantity_reserved",
    )
    .bind(item_ids)
    .execute(tx)
    .await?;
    Ok(())
}

/// Включает или выключает складской учёт; при включении пересчитывает доступное.
pub async fn set_store_control(tx: &mut PgConnection, on: bool) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO options (module, name, value) VALUES ('catalog', 'default_use_store_control', $1)
         ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(if on { "Y" } else { "N" })
    .execute(&mut *tx)
    .await?;
    if on {
        recalc_all(tx).await?;
    }
    Ok(())
}

/// Название склада (пустое — склада нет).
pub async fn store_name(tx: &mut PgConnection, id: i64) -> sqlx::Result<String> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM catalog_stores WHERE id = $1")
        .bind(id)
        .fetch_optional(tx)
        .await?;
    Ok(name.unwrap_or_default())
}

/// Ошибка складской операции: БД или отказ с текстами для пользователя.
#[derive(Debug)]
pub enum StockError {
    Db(sqlx::Error),
    Rejected(Vec<String>),
}

impl From<sqlx::Error> for StockError {
    fn from(e: sqlx::Error) -> Self {
        StockError::Db(e)
    }
}

impl std::fmt::Display for StockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StockError::Db(e) => write!(f, "{e}"),
            StockError::Rejected(messages) => write!(f, "{}", messages.join("; ")),
        }
    }
}

impl std::error::Error for StockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StockError::Db(e) => Some(e),
            StockError::Rejected(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{order_fixture, place_order};
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

    async fn reserve(db: &PgPool, cart_item: i64, item: i64, store: Option<i64>, q: f64) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO basket_reservations (cart_item_id, item_id, store_id, quantity)
             VALUES ($1, $2, $3, $4::float8::numeric) RETURNING id",
        )
        .bind(cart_item)
        .bind(item)
        .bind(store)
        .bind(q)
        .fetch_one(db)
        .await
        .unwrap()
    }

    /// Доступно, резерв товара, резерв склада 1, остаток склада 1.
    async fn counters(db: &PgPool, item: i64) -> (f64, f64, f64, f64) {
        sqlx::query_as(
            "SELECT p.quantity::float8, p.quantity_reserved::float8,
                    a.quantity_reserved::float8, a.amount::float8
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
        sqlx::query_scalar("SELECT quantity::float8 FROM catalog_products WHERE item_id = $1")
            .bind(item)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn add_store(db: &PgPool, id: i64, active: bool, item: i64, amount: f64) {
        sqlx::query("INSERT INTO catalog_stores (id, name, active) VALUES ($1, 'Склад 2', $2)")
            .bind(id)
            .bind(active)
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

    #[sqlx::test]
    async fn settings_defaults_and_options(db: PgPool) {
        let mut conn = db.acquire().await.unwrap();
        let s = settings(&mut conn).await.unwrap();
        assert!(!s.store_control);
        assert!(s.reservation);
        assert!(!s.allow_negative);
        assert_eq!(s.condition, Condition::OnCreate);
        assert_eq!(s.clear_period, 0);
        assert!(!s.deduct_on_delivery);

        for (module, name, value) in [
            ("catalog", "default_use_store_control", "Y"),
            ("catalog", "enable_reservation", "Y"),
            ("catalog", "allow_negative_amount", "Y"),
            ("sale", "product_reserve_condition", "P"),
            ("sale", "product_reserve_clear_period", "2"),
            ("sale", "allow_deduction_on_delivery", "Y"),
        ] {
            set_option(&db, module, name, value).await;
        }
        let s = settings(&mut conn).await.unwrap();
        assert!(s.store_control);
        assert!(s.reservation);
        assert!(s.allow_negative);
        assert_eq!(s.condition, Condition::OnFullPay);
        assert_eq!(s.clear_period, 2);
        assert!(s.deduct_on_delivery);

        set_option(&db, "catalog", "enable_reservation", "N").await;
        assert!(!settings(&mut conn).await.unwrap().reservation);
    }

    #[test]
    fn condition_codes() {
        for (code, c) in [
            ("O", Condition::OnCreate),
            ("R", Condition::OnPay),
            ("P", Condition::OnFullPay),
            ("D", Condition::OnAllowDelivery),
            ("S", Condition::OnShip),
        ] {
            assert_eq!(Condition::from_code(code), c);
            assert_eq!(c.code(), code);
        }
        assert_eq!(Condition::from_code("X"), Condition::OnCreate);
    }

    #[test]
    fn stock_error_display() {
        let e = StockError::Rejected(vec!["Первая".into(), "Вторая".into()]);
        assert_eq!(e.to_string(), "Первая; Вторая");
        let any: anyhow::Error = e.into();
        assert_eq!(any.to_string(), "Первая; Вторая");
    }

    #[sqlx::test]
    async fn reservation_trigger_counts(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = reserve(&db, f.item_id, f.product_id, Some(1), 2.0).await;
        assert_eq!(counters(&db, f.product_id).await, (8.0, 2.0, 2.0, 5.0));

        sqlx::query("UPDATE basket_reservations SET quantity = 1 WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(counters(&db, f.product_id).await, (9.0, 1.0, 1.0, 5.0));

        sqlx::query("UPDATE basket_reservations SET store_id = NULL WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(counters(&db, f.product_id).await, (9.0, 1.0, 0.0, 5.0));

        sqlx::query("DELETE FROM basket_reservations WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(counters(&db, f.product_id).await, (10.0, 0.0, 0.0, 5.0));
    }

    #[sqlx::test]
    async fn reservation_on_store_without_amount_row(db: PgPool) {
        let f = order_fixture(&db).await;
        sqlx::query("INSERT INTO catalog_stores (id, name) VALUES (2, 'Склад 2')")
            .execute(&db)
            .await
            .unwrap();
        reserve(&db, f.item_id, f.product_id, Some(2), 3.0).await;
        let row: (f64, f64) = sqlx::query_as(
            "SELECT amount::float8, quantity_reserved::float8 FROM catalog_store_amounts
             WHERE item_id = $1 AND store_id = 2",
        )
        .bind(f.product_id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(row, (0.0, 3.0));
    }

    #[sqlx::test]
    async fn deleting_reserved_order_releases(db: PgPool) {
        let f = order_fixture(&db).await;
        // Оформление ставит резерв 1 шт.
        let order = place_order(&db, &f, None).await;
        assert_eq!(quantity(&db, f.product_id).await, 9.0);
        sqlx::query("DELETE FROM orders WHERE id = $1")
            .bind(order)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(counters(&db, f.product_id).await, (10.0, 0.0, 0.0, 5.0));
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM basket_reservations")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    #[sqlx::test]
    async fn recalc_quantity_store_control(db: PgPool) {
        let f = order_fixture(&db).await;
        add_store(&db, 2, false, f.product_id, 7.0).await;
        reserve(&db, f.item_id, f.product_id, None, 2.0).await;
        sqlx::query("UPDATE catalog_products SET quantity = 50 WHERE item_id = $1")
            .bind(f.product_id)
            .execute(&db)
            .await
            .unwrap();

        let mut tx = db.begin().await.unwrap();
        recalc_quantity(&mut tx, &[f.product_id]).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(quantity(&db, f.product_id).await, 50.0);

        set_option(&db, "catalog", "default_use_store_control", "Y").await;
        let mut tx = db.begin().await.unwrap();
        recalc_quantity(&mut tx, &[f.product_id]).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(quantity(&db, f.product_id).await, 3.0);
    }

    #[sqlx::test]
    async fn enabling_store_control_recalculates(db: PgPool) {
        let f = order_fixture(&db).await;
        sqlx::query("UPDATE catalog_products SET quantity = 100 WHERE item_id = $1")
            .bind(f.product_id)
            .execute(&db)
            .await
            .unwrap();
        let mut tx = db.begin().await.unwrap();
        set_store_control(&mut tx, true).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(quantity(&db, f.product_id).await, 5.0);
        let value: String = sqlx::query_scalar(
            "SELECT value FROM options WHERE module = 'catalog' AND name = 'default_use_store_control'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(value, "Y");

        let mut tx = db.begin().await.unwrap();
        set_store_control(&mut tx, false).await.unwrap();
        tx.commit().await.unwrap();
        let mut conn = db.acquire().await.unwrap();
        assert!(!settings(&mut conn).await.unwrap().store_control);
    }

    #[sqlx::test]
    async fn deactivating_store_recalculates(db: PgPool) {
        let f = order_fixture(&db).await;
        set_option(&db, "catalog", "default_use_store_control", "Y").await;
        add_store(&db, 2, true, f.product_id, 3.0).await;
        let mut tx = db.begin().await.unwrap();
        recalc_quantity(&mut tx, &[f.product_id]).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(quantity(&db, f.product_id).await, 8.0);

        sqlx::query("UPDATE catalog_stores SET active = FALSE WHERE id = 2")
            .execute(&db)
            .await
            .unwrap();
        let mut tx = db.begin().await.unwrap();
        recalc_quantity(&mut tx, &[f.product_id]).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(quantity(&db, f.product_id).await, 5.0);
    }

    #[sqlx::test]
    async fn purchase_limit_excludes_store_reserve(db: PgPool) {
        let f = order_fixture(&db).await;
        reserve(&db, f.item_id, f.product_id, Some(1), 2.0).await;
        let mut conn = db.acquire().await.unwrap();
        let info = crate::catalog::load_on(&mut conn, &[f.product_id])
            .await
            .unwrap();
        let product = &info[&f.product_id];
        assert_eq!(
            crate::catalog::check_quantity(product, Some(1), 4.0),
            Err("Доступно 3 шт.".to_string())
        );
        assert!(crate::catalog::check_quantity(product, Some(1), 3.0).is_ok());
    }

    #[sqlx::test]
    async fn store_name_by_id(db: PgPool) {
        order_fixture(&db).await;
        let mut conn = db.acquire().await.unwrap();
        assert_eq!(store_name(&mut conn, 1).await.unwrap(), "Склад");
    }

    /// Миграция 0023 переводит списанные неотгруженные заказы в резервы.
    #[sqlx::test(migrations = false)]
    async fn migration_converts_deducted_orders(db: PgPool) {
        let full = sqlx::migrate!();
        full.run_to(22, &db).await.unwrap();
        for sql in [
            "INSERT INTO collections (id, code, name, is_catalog) VALUES (1, 'cat', 'Каталог', TRUE)",
            "INSERT INTO collection_items (id, collection_id, code, name) VALUES (1, 1, 't', 'Товар')",
            "INSERT INTO catalog_products (item_id, quantity, quantity_trace) VALUES (1, 8, TRUE)",
            "INSERT INTO catalog_stores (id, name) VALUES (1, 'Склад')",
            "INSERT INTO catalog_store_amounts (item_id, store_id, amount) VALUES (1, 1, 3)",
            "INSERT INTO person_types (id, name) VALUES (1, 'Клиент')",
            "INSERT INTO order_statuses (code, name) VALUES ('N', 'Принят')",
            // Списан, не отгружен — переводится
            "INSERT INTO orders (id, person_type_id, status, stock_deducted) VALUES (1, 1, 'N', TRUE)",
            "INSERT INTO shipments (order_id, base_price) VALUES (1, 0)",
            "INSERT INTO cart_items (order_id, item_id, store_id, quantity) VALUES (1, 1, 1, 2)",
            // Отгружен — считается списанным
            "INSERT INTO orders (id, person_type_id, status, stock_deducted) VALUES (2, 1, 'N', TRUE)",
            "INSERT INTO shipments (order_id, base_price, deducted) VALUES (2, 0, TRUE)",
            "INSERT INTO cart_items (order_id, item_id, store_id, quantity) VALUES (2, 1, 1, 1)",
            // Отменён — остатки уже возвращены
            "INSERT INTO orders (id, person_type_id, status, stock_deducted, canceled) VALUES (3, 1, 'N', FALSE, TRUE)",
            "INSERT INTO cart_items (order_id, item_id, store_id, quantity) VALUES (3, 1, 1, 4)",
        ] {
            sqlx::query(sql).execute(&db).await.unwrap();
        }
        full.run(&db).await.unwrap();

        let row: (f64, f64, f64, f64) = sqlx::query_as(
            "SELECT p.quantity::float8, p.quantity_reserved::float8,
                    a.amount::float8, a.quantity_reserved::float8
             FROM catalog_products p
             JOIN catalog_store_amounts a ON a.item_id = p.item_id AND a.store_id = 1
             WHERE p.item_id = 1",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(row, (8.0, 2.0, 5.0, 2.0));
        let reservations: Vec<(i64, Option<i64>, f64, bool)> = sqlx::query_as(
            "SELECT r.item_id, r.store_id, r.quantity::float8, r.date_reserve_end IS NULL
             FROM basket_reservations r JOIN cart_items c ON c.id = r.cart_item_id
             WHERE c.order_id = 1",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(reservations, [(1, Some(1), 2.0, true)]);
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM basket_reservations")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(total, 1);
    }
}
