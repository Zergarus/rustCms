//! Хранилище корзины: покупатели (`buyers`) и позиции (`cart_items`).

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

const ITEM_COLS: &str = "id, element_id, store_id, quantity::float8 AS quantity, name";

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
        "INSERT INTO cart_items (buyer_id, element_id, store_id, quantity, name)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (buyer_id, element_id, store_id) WHERE order_id IS NULL
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
           AND t.element_id = s.element_id AND t.store_id = $3",
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
