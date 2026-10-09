//! Хранилище корзины: покупатели (`buyers`) и позиции (`cart_items`).

use std::collections::HashSet;

use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};

use super::CartItem;

/// Владелец корзины: вошедший пользователь или гость по токену из cookie.
#[derive(Debug, Clone)]
pub enum Owner {
    User(i64),
    Guest(String),
}

/// Действие объединения корзины гостя с корзиной пользователя.
#[derive(Debug, Clone, PartialEq)]
pub enum MergeOp {
    /// Прибавить количество к позиции пользователя (тот же товар на том же складе).
    AddTo { user_item: i64, quantity: f64 },
    /// Перенести позицию гостя пользователю как есть.
    Move { guest_item: i64 },
}

pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[derive(FromRow)]
struct ItemRow {
    id: i64,
    element_id: i64,
    store_id: Option<i64>,
    quantity: f64,
    name: String,
}

impl From<ItemRow> for CartItem {
    fn from(r: ItemRow) -> Self {
        CartItem {
            id: r.id,
            element_id: r.element_id,
            store_id: r.store_id,
            quantity: r.quantity,
            name: r.name,
        }
    }
}

const ITEM_COLS: &str = "id, item_id AS element_id, store_id, quantity::float8 AS quantity, name";

pub async fn find_buyer(db: &PgPool, owner: &Owner) -> sqlx::Result<Option<i64>> {
    match owner {
        Owner::User(user_id) => {
            sqlx::query_scalar("SELECT id FROM buyers WHERE user_id = $1")
                .bind(user_id)
                .fetch_optional(db)
                .await
        }
        Owner::Guest(token) => {
            sqlx::query_scalar("SELECT id FROM buyers WHERE token_hash = $1")
                .bind(token_hash(token))
                .fetch_optional(db)
                .await
        }
    }
}

pub async fn ensure_buyer(db: &PgPool, owner: &Owner) -> sqlx::Result<i64> {
    match owner {
        Owner::User(user_id) => {
            sqlx::query_scalar(
                "INSERT INTO buyers (user_id) VALUES ($1)
                 ON CONFLICT (user_id) DO UPDATE SET updated_at = now() RETURNING id",
            )
            .bind(user_id)
            .fetch_one(db)
            .await
        }
        Owner::Guest(token) => {
            sqlx::query_scalar(
                "INSERT INTO buyers (token_hash) VALUES ($1)
                 ON CONFLICT (token_hash) DO UPDATE SET updated_at = now() RETURNING id",
            )
            .bind(token_hash(token))
            .fetch_one(db)
            .await
        }
    }
}

async fn touch(db: &PgPool, buyer_id: i64) -> sqlx::Result<()> {
    sqlx::query("UPDATE buyers SET updated_at = now() WHERE id = $1")
        .bind(buyer_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Позиции корзины (без уже оформленных), по порядку добавления.
pub async fn items(db: &PgPool, buyer_id: i64) -> sqlx::Result<Vec<CartItem>> {
    let rows: Vec<ItemRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLS} FROM cart_items WHERE buyer_id = $1 AND order_id IS NULL ORDER BY id"
    )))
    .bind(buyer_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(CartItem::from).collect())
}

pub async fn item(db: &PgPool, buyer_id: i64, item_id: i64) -> sqlx::Result<Option<CartItem>> {
    let row: Option<ItemRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLS} FROM cart_items WHERE buyer_id = $1 AND id = $2 AND order_id IS NULL"
    )))
    .bind(buyer_id)
    .bind(item_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(CartItem::from))
}

/// Добавляет товар; тот же товар на том же складе — увеличивает количество.
pub async fn add(
    db: &PgPool,
    buyer_id: i64,
    element_id: i64,
    store_id: Option<i64>,
    quantity: f64,
    name: &str,
) -> sqlx::Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO cart_items (buyer_id, item_id, store_id, quantity, name)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (buyer_id, item_id, store_id) WHERE order_id IS NULL
         DO UPDATE SET quantity = cart_items.quantity + EXCLUDED.quantity, updated_at = now()
         RETURNING id",
    )
    .bind(buyer_id)
    .bind(element_id)
    .bind(store_id)
    .bind(quantity)
    .bind(name)
    .fetch_one(db)
    .await?;
    touch(db, buyer_id).await?;
    Ok(id)
}

pub async fn set_quantity(
    db: &PgPool,
    buyer_id: i64,
    item_id: i64,
    quantity: f64,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE cart_items SET quantity = $3, updated_at = now()
         WHERE buyer_id = $1 AND id = $2 AND order_id IS NULL",
    )
    .bind(buyer_id)
    .bind(item_id)
    .bind(quantity)
    .execute(db)
    .await?;
    touch(db, buyer_id).await
}

/// Меняет склад позиции. Если у покупателя уже есть этот товар на новом складе —
/// количества складываются в ту позицию, а эта удаляется.
pub async fn set_store(
    db: &PgPool,
    buyer_id: i64,
    item_id: i64,
    store_id: i64,
) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    let twin: Option<i64> = sqlx::query_scalar(
        "SELECT t.id FROM cart_items t JOIN cart_items s ON s.id = $2
         WHERE t.buyer_id = $1 AND t.order_id IS NULL AND t.id <> s.id
           AND t.item_id = s.item_id AND t.store_id = $3",
    )
    .bind(buyer_id)
    .bind(item_id)
    .bind(store_id)
    .fetch_optional(&mut *tx)
    .await?;
    match twin {
        Some(twin) => {
            sqlx::query(
                "UPDATE cart_items t SET quantity = t.quantity + s.quantity, updated_at = now()
                 FROM cart_items s WHERE t.id = $1 AND s.id = $2",
            )
            .bind(twin)
            .bind(item_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM cart_items WHERE id = $1 AND buyer_id = $2")
                .bind(item_id)
                .bind(buyer_id)
                .execute(&mut *tx)
                .await?;
        }
        None => {
            sqlx::query(
                "UPDATE cart_items SET store_id = $3, updated_at = now()
                 WHERE buyer_id = $1 AND id = $2 AND order_id IS NULL",
            )
            .bind(buyer_id)
            .bind(item_id)
            .bind(store_id)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    touch(db, buyer_id).await
}

pub async fn remove(db: &PgPool, buyer_id: i64, item_id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM cart_items WHERE buyer_id = $1 AND id = $2 AND order_id IS NULL")
        .bind(buyer_id)
        .bind(item_id)
        .execute(db)
        .await?;
    touch(db, buyer_id).await
}

pub async fn clear(db: &PgPool, buyer_id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM cart_items WHERE buyer_id = $1 AND order_id IS NULL")
        .bind(buyer_id)
        .execute(db)
        .await?;
    touch(db, buyer_id).await
}

/// Куда прибавить добавляемый товар (контракт bxapi, 5.2).
#[derive(Debug, Clone, PartialEq)]
pub enum AddTarget {
    /// К существующей позиции; `set_store` — проставить ей склад.
    Existing {
        item_id: i64,
        set_store: Option<i64>,
    },
    New,
}

/// Без склада — к позиции товара с любым складом; со складом — к позиции с этим
/// складом, иначе к позиции без склада (склад деактивирован — тоже «без склада»).
pub fn add_target(
    items: &[CartItem],
    element_id: i64,
    store_id: Option<i64>,
    active_stores: &HashSet<i64>,
) -> AddTarget {
    let mut rows = items.iter().filter(|i| i.element_id == element_id);
    let found = match store_id {
        None => rows.next().map(|i| (i.id, None)),
        Some(store) => {
            let rows: Vec<&CartItem> = rows.collect();
            rows.iter()
                .find(|i| i.store_id == Some(store))
                .map(|i| (i.id, None))
                .or_else(|| {
                    rows.iter()
                        .find(|i| !i.store_id.is_some_and(|s| active_stores.contains(&s)))
                        .map(|i| (i.id, Some(store)))
                })
        }
    };
    match found {
        Some((item_id, set_store)) => AddTarget::Existing { item_id, set_store },
        None => AddTarget::New,
    }
}

/// Прибавляет количество к позиции и, если задан, проставляет ей склад.
pub async fn add_to_item(
    db: &PgPool,
    buyer_id: i64,
    item_id: i64,
    quantity: f64,
    set_store: Option<i64>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE cart_items SET quantity = quantity + $3, store_id = COALESCE($4, store_id), updated_at = now()
         WHERE buyer_id = $1 AND id = $2 AND order_id IS NULL",
    )
    .bind(buyer_id)
    .bind(item_id)
    .bind(quantity)
    .bind(set_store)
    .execute(db)
    .await?;
    touch(db, buyer_id).await
}

/// План объединения: тот же товар на том же складе — количество прибавляется к позиции
/// пользователя, остальное переносится. Остатки не проверяются (их проверит заказ).
pub fn merge_plan(guest: &[CartItem], user: &[CartItem]) -> Vec<MergeOp> {
    guest
        .iter()
        .map(|g| {
            match user
                .iter()
                .find(|u| u.element_id == g.element_id && u.store_id == g.store_id)
            {
                Some(u) => MergeOp::AddTo {
                    user_item: u.id,
                    quantity: g.quantity,
                },
                None => MergeOp::Move { guest_item: g.id },
            }
        })
        .collect()
}

/// Переносит корзину гостя в корзину пользователя и удаляет покупателя-гостя.
pub async fn merge_guest_into_user(
    db: &PgPool,
    guest_token: &str,
    user_id: i64,
) -> sqlx::Result<()> {
    let Some(guest_id) = find_buyer(db, &Owner::Guest(guest_token.to_string())).await? else {
        return Ok(());
    };
    let user_buyer = ensure_buyer(db, &Owner::User(user_id)).await?;
    if user_buyer == guest_id {
        return Ok(());
    }
    let guest_items = items(db, guest_id).await?;
    let user_items = items(db, user_buyer).await?;
    let mut tx = db.begin().await?;
    for op in merge_plan(&guest_items, &user_items) {
        match op {
            MergeOp::AddTo {
                user_item,
                quantity,
            } => {
                sqlx::query("UPDATE cart_items SET quantity = quantity + $2, updated_at = now() WHERE id = $1")
                    .bind(user_item)
                    .bind(quantity)
                    .execute(&mut *tx)
                    .await?;
            }
            MergeOp::Move { guest_item } => {
                sqlx::query(
                    "UPDATE cart_items SET buyer_id = $2, updated_at = now() WHERE id = $1",
                )
                .bind(guest_item)
                .bind(user_buyer)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    // Позиции оформленных гостем заказов переходят к покупателю-пользователю
    sqlx::query("UPDATE cart_items SET buyer_id = $2 WHERE buyer_id = $1 AND order_id IS NOT NULL")
        .bind(guest_id)
        .bind(user_buyer)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM buyers WHERE id = $1")
        .bind(guest_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE buyers SET updated_at = now() WHERE id = $1")
        .bind(user_buyer)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cart::CartItem;

    fn item(id: i64, element: i64, store: Option<i64>, qty: f64) -> CartItem {
        CartItem {
            id,
            element_id: element,
            store_id: store,
            quantity: qty,
            name: String::new(),
        }
    }

    #[test]
    fn add_without_store_goes_to_any_row_of_product() {
        let items = [item(1, 7, Some(5), 1.0)];
        assert_eq!(
            add_target(&items, 7, None, &HashSet::from([5])),
            AddTarget::Existing {
                item_id: 1,
                set_store: None
            }
        );
        assert_eq!(
            add_target(&items, 8, None, &HashSet::from([5])),
            AddTarget::New
        );
    }

    #[test]
    fn add_with_store_prefers_same_store_then_storeless() {
        let active = HashSet::from([5, 6]);
        let items = [item(1, 7, None, 1.0), item(2, 7, Some(5), 1.0)];
        assert_eq!(
            add_target(&items, 7, Some(5), &active),
            AddTarget::Existing {
                item_id: 2,
                set_store: None
            }
        );
        assert_eq!(
            add_target(&items, 7, Some(6), &active),
            AddTarget::Existing {
                item_id: 1,
                set_store: Some(6)
            }
        );
        assert_eq!(
            add_target(&[item(2, 7, Some(5), 1.0)], 7, Some(6), &active),
            AddTarget::New
        );
    }

    #[test]
    fn add_treats_inactive_store_row_as_storeless() {
        let items = [item(3, 7, Some(9), 1.0)];
        assert_eq!(
            add_target(&items, 7, Some(5), &HashSet::from([5])),
            AddTarget::Existing {
                item_id: 3,
                set_store: Some(5)
            }
        );
    }

    async fn ordered_guest(db: &sqlx::PgPool) -> (crate::test_support::Fixture, i64) {
        let f = crate::test_support::order_fixture(db).await;
        sqlx::query("UPDATE buyers SET token_hash = $2 WHERE id = $1")
            .bind(f.buyer_id)
            .bind(token_hash("guest-token"))
            .execute(db)
            .await
            .unwrap();
        let order: i64 = sqlx::query_scalar(
            "INSERT INTO orders (person_type_id, status) VALUES ($1, 'N') RETURNING id",
        )
        .bind(f.person_type_id)
        .fetch_one(db)
        .await
        .unwrap();
        sqlx::query("UPDATE cart_items SET order_id = $2 WHERE id = $1")
            .bind(f.item_id)
            .bind(order)
            .execute(db)
            .await
            .unwrap();
        let user: i64 = sqlx::query_scalar(
            "INSERT INTO users (login, password_hash) VALUES ('buyer', 'x') RETURNING id",
        )
        .fetch_one(db)
        .await
        .unwrap();
        (f, user)
    }

    async fn order_items(db: &sqlx::PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM cart_items WHERE order_id IS NOT NULL")
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn guest_order_items_survive_login_merge(db: sqlx::PgPool) {
        let (_, user) = ordered_guest(&db).await;
        merge_guest_into_user(&db, "guest-token", user)
            .await
            .unwrap();
        assert_eq!(order_items(&db).await, 1);
    }

    #[sqlx::test]
    async fn order_items_survive_user_deletion(db: sqlx::PgPool) {
        let (f, user) = ordered_guest(&db).await;
        sqlx::query("UPDATE buyers SET user_id = $2, token_hash = NULL WHERE id = $1")
            .bind(f.buyer_id)
            .bind(user)
            .execute(&db)
            .await
            .unwrap();
        // открытая позиция корзины того же покупателя — должна удалиться
        sqlx::query("INSERT INTO cart_items (buyer_id, item_id, quantity) VALUES ($1, $2, 1)")
            .bind(f.buyer_id)
            .bind(f.element_id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(order_items(&db).await, 1);
        let open: i64 =
            sqlx::query_scalar("SELECT count(*) FROM cart_items WHERE order_id IS NULL")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(open, 0);
    }

    #[test]
    fn merge_sums_same_product_and_store() {
        let plan = merge_plan(&[item(1, 1, Some(5), 2.0)], &[item(9, 1, Some(5), 3.0)]);
        assert_eq!(
            plan,
            vec![MergeOp::AddTo {
                user_item: 9,
                quantity: 2.0
            }]
        );
    }

    #[test]
    fn merge_moves_other_items() {
        let plan = merge_plan(
            &[
                item(1, 1, Some(6), 2.0),
                item(2, 2, Some(5), 1.0),
                item(3, 1, None, 1.0),
            ],
            &[item(9, 1, Some(5), 3.0)],
        );
        assert_eq!(
            plan,
            vec![
                MergeOp::Move { guest_item: 1 },
                MergeOp::Move { guest_item: 2 },
                MergeOp::Move { guest_item: 3 }
            ]
        );
    }

    #[test]
    fn merge_sums_even_over_stock() {
        // Остатки при объединении не проверяются — их проверит оформление заказа
        let plan = merge_plan(&[item(1, 1, Some(5), 500.0)], &[item(9, 1, Some(5), 700.0)]);
        assert_eq!(
            plan,
            vec![MergeOp::AddTo {
                user_item: 9,
                quantity: 500.0
            }]
        );
    }
}
