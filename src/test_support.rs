//! Наполнение тестовой БД (`#[sqlx::test]`): каталог, покупатель, корзина, справочники заказа.

use sqlx::PgPool;

pub struct Fixture {
    pub product_id: i64,
    pub buyer_id: i64,
    pub item_id: i64,
    pub person_type_id: i64,
}

/// Товар с остатком 10 (склад 1 — 5), гость-покупатель с позицией 1 шт., тип плательщика, статус N.
pub async fn order_fixture(db: &PgPool) -> Fixture {
    let collection: i64 = sqlx::query_scalar(
        "INSERT INTO collections (code, name, is_catalog) VALUES ('catalog', 'Каталог', TRUE) RETURNING id",
    )
    .fetch_one(db)
    .await
    .unwrap();
    let product_id: i64 = sqlx::query_scalar(
        "INSERT INTO collection_items (collection_id, code, name) VALUES ($1, 'tovar', 'Товар') RETURNING id",
    )
    .bind(collection)
    .fetch_one(db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO catalog_products (item_id, quantity, quantity_trace, can_buy_zero) VALUES ($1, 10, TRUE, FALSE)")
        .bind(product_id)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_stores (id, name) VALUES (1, 'Склад')")
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_store_amounts (item_id, store_id, amount) VALUES ($1, 1, 5)")
        .bind(product_id)
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
    .bind(product_id)
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
        product_id,
        buyer_id,
        item_id,
        person_type_id,
    }
}

pub async fn amounts(db: &PgPool, product_id: i64) -> (f64, f64) {
    sqlx::query_as(
        "SELECT (SELECT quantity::float8 FROM catalog_products WHERE item_id = $1),
                (SELECT amount::float8 FROM catalog_store_amounts WHERE item_id = $1 AND store_id = 1)",
    )
    .bind(product_id)
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
    .bind(f.product_id)
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
        delivery_base_price: 0.0,
        items: vec![crate::sale::repo::NewItem::plain(
            item_id,
            1.0,
            100.0,
            "Товар",
        )],
        stock: Vec::new(),
        discounts: Vec::new(),
        coupons: Vec::new(),
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

/// GET без перекодирования тела: статус, заголовки и байты ответа.
pub async fn get_raw(
    app: &axum::Router,
    path: &str,
    cookie: &str,
) -> (axum::http::StatusCode, axum::http::HeaderMap, Vec<u8>) {
    use tower::ServiceExt;
    let req = axum::http::Request::get(path)
        .header(axum::http::header::COOKIE, cookie)
        .body(axum::body::Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, body.to_vec())
}

/// Обычный пользователь с правами `permissions` (через группу); возвращает Cookie сессии.
pub async fn permitted_cookie(db: &PgPool, login: &str, permissions: &[&str]) -> String {
    let (uid, cookie) = user_cookie(db, login, false).await;
    let group: i64 =
        sqlx::query_scalar("INSERT INTO groups (code, name) VALUES ($1, $1) RETURNING id")
            .bind(format!("g_{login}"))
            .fetch_one(db)
            .await
            .unwrap();
    for perm in permissions.iter().chain(&["admin.access"]) {
        sqlx::query("INSERT INTO group_permissions (group_id, permission) VALUES ($1, $2)")
            .bind(group)
            .bind(perm)
            .execute(db)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO user_groups (user_id, group_id) VALUES ($1, $2)")
        .bind(uid)
        .bind(group)
        .execute(db)
        .await
        .unwrap();
    cookie
}

/// Корзина для проверки скидок через bxapi: фикстура заказа, базовая цена товара 1000,
/// позиция 2 шт. на складе 1, второй товар — предложение (цена 500, 1 шт.) с родителем в
/// разделе, доставка 1 (300) и платёжка 1. Гость с cookie `CMS_BUYER=tok`.
pub struct CartFixture {
    pub f: Fixture,
    pub cookie: String,
    pub offer_id: i64,
    pub offer_item_id: i64,
    pub parent_id: i64,
    pub section_id: i64,
}

pub async fn cart_fixture(db: &PgPool) -> CartFixture {
    let f = order_fixture(db).await;
    let exec = |sql: &'static str| sqlx::query(sql);
    sqlx::query("UPDATE buyers SET token_hash = $2 WHERE id = $1")
        .bind(f.buyer_id)
        .bind(crate::cart::repo::token_hash("tok"))
        .execute(db)
        .await
        .unwrap();
    sqlx::query("UPDATE cart_items SET quantity = 2 WHERE id = $1")
        .bind(f.item_id)
        .execute(db)
        .await
        .unwrap();
    exec("INSERT INTO catalog_price_types (id, code, name, is_base, sort) VALUES (1, 'BASE', 'Розница', TRUE, 100)")
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_prices (item_id, price_type_id, price, currency) VALUES ($1, 1, 1000, 'RUB')")
        .bind(f.product_id)
        .execute(db)
        .await
        .unwrap();
    // Коллекция предложений: товар-родитель в разделе, предложение — в корзине
    let offers: i64 = sqlx::query_scalar(
        "INSERT INTO collections (code, name, is_catalog) VALUES ('offers', 'Предложения', TRUE) RETURNING id",
    )
    .fetch_one(db)
    .await
    .unwrap();
    let catalog: i64 =
        sqlx::query_scalar("SELECT collection_id FROM collection_items WHERE id = $1")
            .bind(f.product_id)
            .fetch_one(db)
            .await
            .unwrap();
    sqlx::query("UPDATE collections SET product_collection_id = $2 WHERE id = $1")
        .bind(offers)
        .bind(catalog)
        .execute(db)
        .await
        .unwrap();
    let section_id: i64 = sqlx::query_scalar(
        "INSERT INTO collection_sections (collection_id, code, name) VALUES ($1, 'oils', 'Масла') RETURNING id",
    )
    .bind(catalog)
    .fetch_one(db)
    .await
    .unwrap();
    let parent_id: i64 = sqlx::query_scalar(
        "INSERT INTO collection_items (collection_id, code, name, section_id, xml_id)
         VALUES ($1, 'maslo', 'Масло', $2, 'P-1') RETURNING id",
    )
    .bind(catalog)
    .bind(section_id)
    .fetch_one(db)
    .await
    .unwrap();
    let offer_id: i64 = sqlx::query_scalar(
        "INSERT INTO collection_items (collection_id, code, name, product_id)
         VALUES ($1, 'maslo-1l', 'Масло 1 л', $2) RETURNING id",
    )
    .bind(offers)
    .bind(parent_id)
    .fetch_one(db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO catalog_products (item_id, quantity, quantity_trace, can_buy_zero, type) VALUES ($1, 10, TRUE, FALSE, 3) ON CONFLICT (item_id) DO NOTHING")
        .bind(parent_id)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_products (item_id, quantity, quantity_trace, can_buy_zero, type) VALUES ($1, 10, TRUE, FALSE, 4)
                 ON CONFLICT (item_id) DO UPDATE SET quantity = 10, type = 4")
        .bind(offer_id)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_prices (item_id, price_type_id, price, currency) VALUES ($1, 1, 500, 'RUB')")
        .bind(offer_id)
        .execute(db)
        .await
        .unwrap();
    let offer_item_id: i64 = sqlx::query_scalar(
        "INSERT INTO cart_items (buyer_id, item_id, quantity, name) VALUES ($1, $2, 1, 'Масло 1 л') RETURNING id",
    )
    .bind(f.buyer_id)
    .bind(offer_id)
    .fetch_one(db)
    .await
    .unwrap();
    exec("INSERT INTO deliveries (id, name, sort, price) VALUES (1, 'Курьер', 10, 300)")
        .execute(db)
        .await
        .unwrap();
    exec("INSERT INTO pay_systems (id, name, sort, api_type) VALUES (1, 'Счёт', 10, 'document')")
        .execute(db)
        .await
        .unwrap();
    CartFixture {
        f,
        cookie: "CMS_BUYER=tok".into(),
        offer_id,
        offer_item_id,
        parent_id,
        section_id,
    }
}

/// Валюты `RUB` (базовая) и `USD`; курс USD → RUB на сегодня — `rate` (нет — без курса).
pub async fn seed_usd(db: &PgPool, rate: Option<f64>) {
    sqlx::query(
        "INSERT INTO currencies (code, base) VALUES ('RUB', TRUE), ('USD', FALSE)
         ON CONFLICT (code) DO NOTHING",
    )
    .execute(db)
    .await
    .unwrap();
    if let Some(rate) = rate {
        add_usd_rate(db, crate::currency::today(), rate).await;
    }
}

/// Курс USD → RUB на дату.
pub async fn add_usd_rate(db: &PgPool, date: chrono::NaiveDate, rate: f64) {
    sqlx::query(
        "INSERT INTO currency_rates (currency, base_currency, date_rate, rate_cnt, rate)
         VALUES ('USD', 'RUB', $1, 1, $2::float8::numeric)",
    )
    .bind(date)
    .bind(rate)
    .execute(db)
    .await
    .unwrap();
}
