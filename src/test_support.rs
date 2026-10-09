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
        "INSERT INTO collections (code, name, is_catalog) VALUES ('catalog', 'Каталог', TRUE) RETURNING id",
    )
    .fetch_one(db)
    .await
    .unwrap();
    let element_id: i64 = sqlx::query_scalar(
        "INSERT INTO collection_items (collection_id, code, name) VALUES ($1, 'tovar', 'Товар') RETURNING id",
    )
    .bind(iblock)
    .fetch_one(db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO catalog_products (item_id, quantity, quantity_trace, can_buy_zero) VALUES ($1, 10, TRUE, FALSE)")
        .bind(element_id)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_stores (id, name) VALUES (1, 'Склад')")
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_store_amounts (item_id, store_id, amount) VALUES ($1, 1, 5)")
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
        "INSERT INTO cart_items (buyer_id, item_id, store_id, quantity, name) VALUES ($1, $2, 1, 1, 'Товар') RETURNING id",
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
        "SELECT (SELECT quantity::float8 FROM catalog_products WHERE item_id = $1),
                (SELECT amount::float8 FROM catalog_store_amounts WHERE item_id = $1 AND store_id = 1)",
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
        "INSERT INTO cart_items (buyer_id, item_id, store_id, quantity, name) VALUES ($1, $2, NULL, 1, 'Товар') RETURNING id",
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

/// Состояние приложения для тестов через роутер: шаблоны из `templates/admin`,
/// файлы и письма — во временном каталоге, проект bxapi по умолчанию.
pub fn test_state(db: PgPool) -> crate::state::AppState {
    use std::sync::Arc;
    let tmp = std::env::temp_dir().join(format!("cms-test-{}", rand::random::<u64>()));
    let config = crate::config::Config {
        database_url: String::new(),
        bind_addr: "127.0.0.1:3000".into(),
        cors_origins: vec!["*".into()],
        templates_dir: "templates".into(),
        static_dir: "static".into(),
        upload_dir: tmp.join("upload"),
        upload_origin_url: None,
        mail_smtp_url: None,
        mail_dir: tmp.join("mail"),
        cookie_secure: false,
    };
    let mut env = minijinja::Environment::new();
    env.set_loader(minijinja::path_loader(config.templates_dir.join("admin")));
    crate::state::AppState {
        db,
        templates: Arc::new(env),
        config: Arc::new(config),
        registry: Arc::default(),
        project: Arc::new(crate::bxapi::project::Project::default()),
    }
}

/// Создаёт пользователя (`admin` — суперпользователь) и возвращает заголовок Cookie его сессии.
pub async fn user_cookie(db: &PgPool, login: &str, admin: bool) -> (i64, String) {
    let user = crate::auth::create_user(db, login, "password123".into(), admin)
        .await
        .unwrap();
    let token = crate::auth::create_session(db, user.id).await.unwrap();
    (user.id, format!("{}={token}", crate::auth::SESSION_COOKIE))
}

pub async fn admin_cookie(db: &PgPool) -> String {
    user_cookie(db, "smoke", true).await.1
}

/// Контент для проверки страниц: коллекция `news` с разделом, полями (список с вариантом,
/// привязка к себе), записью и группой с доступом на запись.
pub struct Content {
    pub collection_id: i64,
    pub section_id: i64,
    pub field_id: i64,
    pub link_field_id: i64,
    pub option_id: i64,
    pub item_id: i64,
    pub group_id: i64,
}

pub async fn content_fixture(db: &PgPool) -> Content {
    let one = |sql: &'static str| sqlx::query_scalar::<_, i64>(sql);
    let collection_id =
        one("INSERT INTO collections (code, name) VALUES ('news', 'Новости') RETURNING id")
            .fetch_one(db)
            .await
            .unwrap();
    let section_id = one(
        "INSERT INTO collection_sections (collection_id, code, name) VALUES ($1, 'razdel-a', 'Раздел А') RETURNING id",
    )
    .bind(collection_id)
    .fetch_one(db)
    .await
    .unwrap();
    let field_id = one(
        "INSERT INTO collection_fields (collection_id, code, name, kind) VALUES ($1, 'color', 'Цвет', 'list') RETURNING id",
    )
    .bind(collection_id)
    .fetch_one(db)
    .await
    .unwrap();
    let link_field_id = one(
        "INSERT INTO collection_fields (collection_id, code, name, kind, link_collection_id)
         VALUES ($1, 'link', 'Связь', 'element', $1) RETURNING id",
    )
    .bind(collection_id)
    .fetch_one(db)
    .await
    .unwrap();
    let option_id = one(
        "INSERT INTO collection_field_options (field_id, value, xml_id) VALUES ($1, 'Красный', 'red') RETURNING id",
    )
    .bind(field_id)
    .fetch_one(db)
    .await
    .unwrap();
    let item_id = one(
        "INSERT INTO collection_items (collection_id, section_id, code, name, published_at, created_at, updated_at, field_values)
         VALUES ($1, $2, 'pervaya', 'Первая новость', '2026-01-02T03:04:05Z', '2026-01-02T03:04:05Z',
                 '2026-01-02T03:04:05Z', jsonb_build_object('color', $3))
         RETURNING id",
    )
    .bind(collection_id)
    .bind(section_id)
    .bind(option_id)
    .fetch_one(db)
    .await
    .unwrap();
    sqlx::query("UPDATE collection_items SET field_values = field_values || jsonb_build_object('link', id) WHERE id = $1")
        .bind(item_id)
        .execute(db)
        .await
        .unwrap();
    let group_id =
        one("INSERT INTO groups (code, name) VALUES ('editors', 'Редакторы') RETURNING id")
            .fetch_one(db)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO collection_access (collection_id, group_id, level) VALUES ($1, $2, 'write')",
    )
    .bind(collection_id)
    .bind(group_id)
    .execute(db)
    .await
    .unwrap();
    Content {
        collection_id,
        section_id,
        field_id,
        link_field_id,
        option_id,
        item_id,
        group_id,
    }
}
