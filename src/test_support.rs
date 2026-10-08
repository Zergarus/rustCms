//! Наполнение тестовой БД (`#[sqlx::test]`): каталог, покупатель, корзина, справочники заказа.

use sqlx::PgPool;

pub struct Fixture {
    pub element_id: i64,
    pub buyer_id: i64,
    pub item_id: i64,
    pub person_type_id: i64,
}

/// Товар с остатком 10 (склад 1 — 5), гость-покупатель с позицией 1 шт., тип плательщика, статус N.
pub async fn order_fixture(db: &PgPool) -> Fixture {
    let iblock: i64 = sqlx::query_scalar(
        "INSERT INTO iblocks (code, name, is_catalog) VALUES ('catalog', 'Каталог', TRUE) RETURNING id",
    )
    .fetch_one(db)
    .await
    .unwrap();
    let element_id: i64 = sqlx::query_scalar(
        "INSERT INTO iblock_elements (iblock_id, code, name) VALUES ($1, 'tovar', 'Товар') RETURNING id",
    )
    .bind(iblock)
    .fetch_one(db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO catalog_products (element_id, quantity, quantity_trace, can_buy_zero) VALUES ($1, 10, TRUE, FALSE)")
        .bind(element_id)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_stores (id, name) VALUES (1, 'Склад')")
        .execute(db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO catalog_store_amounts (element_id, store_id, amount) VALUES ($1, 1, 5)",
    )
    .bind(element_id)
    .execute(db)
    .await
    .unwrap();
    let buyer_id: i64 =
        sqlx::query_scalar("INSERT INTO buyers (token_hash) VALUES ('guest') RETURNING id")
            .fetch_one(db)
            .await
            .unwrap();
    let item_id: i64 = sqlx::query_scalar(
        "INSERT INTO cart_items (buyer_id, element_id, store_id, quantity, name) VALUES ($1, $2, 1, 1, 'Товар') RETURNING id",
    )
    .bind(buyer_id)
    .bind(element_id)
    .fetch_one(db)
    .await
    .unwrap();
    let person_type_id: i64 =
        sqlx::query_scalar("INSERT INTO person_types (name) VALUES ('Клиент') RETURNING id")
            .fetch_one(db)
            .await
            .unwrap();
    sqlx::query("INSERT INTO order_statuses (code, name) VALUES ('N', 'Принят')")
        .execute(db)
        .await
        .unwrap();
    Fixture {
        element_id,
        buyer_id,
        item_id,
        person_type_id,
    }
}

pub async fn amounts(db: &PgPool, element_id: i64) -> (f64, f64) {
    sqlx::query_as(
        "SELECT (SELECT quantity::float8 FROM catalog_products WHERE element_id = $1),
                (SELECT amount::float8 FROM catalog_store_amounts WHERE element_id = $1 AND store_id = 1)",
    )
    .bind(element_id)
    .fetch_one(db)
    .await
    .unwrap()
}

/// Заказ из новой позиции (1 шт., без склада — позиция фикстуры на складе 1 остаётся в корзине)
/// покупателя фикстуры: самовывоз 1, платёжка 1,
/// без списания остатков.
pub async fn place_order(db: &PgPool, f: &Fixture, user_id: Option<i64>) -> i64 {
    use crate::sale::repo::{NewOrder, create};
    let item_id: i64 = sqlx::query_scalar(
        "INSERT INTO cart_items (buyer_id, element_id, store_id, quantity, name) VALUES ($1, $2, NULL, 1, 'Товар') RETURNING id",
    )
    .bind(f.buyer_id)
    .bind(f.element_id)
    .fetch_one(db)
    .await
    .unwrap();
    let order = NewOrder {
        user_id,
        buyer_id: f.buyer_id,
        person_type_id: f.person_type_id,
        status: "N".into(),
        currency: "RUB".into(),
        goods_price: 100.0,
        delivery_price: 0.0,
        user_comment: String::new(),
        properties: Vec::new(),
        delivery: (1, "Самовывоз".into(), 0.0, Some(1)),
        payment: (1, "QR".into()),
        items: vec![(item_id, 1.0, 100.0, "Товар".into())],
        stock: Vec::new(),
    };
    let mut tx = db.begin().await.unwrap();
    let id = create(&mut tx, &order).await.unwrap();
    tx.commit().await.unwrap();
    id
}
