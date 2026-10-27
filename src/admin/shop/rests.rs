//! Остатки по складам и резервы товара (аналог «Остатков» складского учёта Битрикса).

use axum::{
    Extension,
    extract::{Path, Query, State},
    response::Html,
};
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use super::require_shop;
use crate::{
    access::Access,
    admin::render,
    error::{AppError, AppResult},
    state::AppState,
    stock,
};

const PER_PAGE: i64 = 50;

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    store: String,
    #[serde(default)]
    section: String,
    page: Option<i64>,
}

#[derive(FromRow)]
struct RestRow {
    item_id: i64,
    name: String,
    store_name: String,
    amount: f64,
    reserved: f64,
}

#[derive(Serialize)]
struct RestView {
    item_id: i64,
    name: String,
    store_name: String,
    amount: String,
    reserved: String,
    available: String,
}

fn parse_id(raw: &str) -> Option<i64> {
    raw.trim().parse().ok()
}

/// Число без хвоста «.0».
fn fmt(v: f64) -> String {
    v.to_string()
}

/// Остаток для показа: при `catalog.allow_negative_amount = N` отрицательный виден как 0.
fn shown(v: f64, allow_negative: bool) -> f64 {
    if allow_negative { v } else { v.max(0.0) }
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<ListQuery>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let allow_negative = stock::settings(&mut *state.db.acquire().await?)
        .await?
        .allow_negative;
    let (store, section) = (parse_id(&q.store), parse_id(&q.section));
    let page = q.page.unwrap_or(1).max(1);
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM catalog_store_amounts a
         JOIN collection_items i ON i.id = a.item_id
         WHERE ($1::bigint IS NULL OR a.store_id = $1) AND ($2::bigint IS NULL OR i.section_id = $2)",
    )
    .bind(store)
    .bind(section)
    .fetch_one(&state.db)
    .await?;
    let rows: Vec<RestRow> = sqlx::query_as(
        "SELECT a.item_id, i.name, st.name AS store_name, a.amount::float8 AS amount,
                a.quantity_reserved::float8 AS reserved
         FROM catalog_store_amounts a
         JOIN collection_items i ON i.id = a.item_id
         JOIN catalog_stores st ON st.id = a.store_id
         WHERE ($1::bigint IS NULL OR a.store_id = $1) AND ($2::bigint IS NULL OR i.section_id = $2)
         ORDER BY i.name, i.id, st.sort, st.id LIMIT $3 OFFSET $4",
    )
    .bind(store)
    .bind(section)
    .bind(PER_PAGE)
    .bind((page - 1) * PER_PAGE)
    .fetch_all(&state.db)
    .await?;
    let items: Vec<RestView> = rows
        .into_iter()
        .map(|r| RestView {
            item_id: r.item_id,
            name: r.name,
            store_name: r.store_name,
            amount: fmt(shown(r.amount, allow_negative)),
            reserved: fmt(r.reserved),
            available: fmt(shown(r.amount - r.reserved, allow_negative)),
        })
        .collect();
    let stores: Vec<minijinja::Value> =
        sqlx::query_as::<_, (i64, String)>("SELECT id, name FROM catalog_stores ORDER BY sort, id")
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|(id, name)| context! { id, name })
            .collect();
    let sections: Vec<minijinja::Value> = sqlx::query_as::<_, (i64, String)>(
        "SELECT s.id, c.name || ' / ' || s.name FROM collection_sections s
         JOIN collections c ON c.id = s.collection_id
         WHERE c.is_catalog ORDER BY c.name, s.name, s.id",
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|(id, name)| context! { id, name })
    .collect();
    let pages = (total + PER_PAGE - 1) / PER_PAGE;
    let (filter_store, filter_section) = (store, section);
    let query = [("store", store), ("section", section)]
        .iter()
        .filter_map(|(k, v)| v.map(|v| format!("&{k}={v}")))
        .collect::<String>();
    render(
        &state,
        "shop/rests.html",
        context! {
            user, items, stores, sections, filter_store, filter_section, total, page, pages, query,
        },
    )
}

#[derive(FromRow, Serialize)]
struct ReserveRow {
    order_id: i64,
    order_number: String,
    cart_item_id: i64,
    position: String,
    store_name: String,
    quantity: String,
    reserve_end: String,
}

pub async fn reserves(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(item_id): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let name: String = sqlx::query_scalar("SELECT name FROM collection_items WHERE id = $1")
        .bind(item_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let items: Vec<ReserveRow> = sqlx::query_as(
        "SELECT o.id AS order_id, COALESCE(o.account_number, o.id::text) AS order_number,
                ci.id AS cart_item_id, ci.name AS position,
                COALESCE(st.name, '—') AS store_name, br.quantity::float8::text AS quantity,
                COALESCE(to_char(br.date_reserve_end, 'DD.MM.YYYY HH24:MI'), '—') AS reserve_end
         FROM basket_reservations br
         JOIN cart_items ci ON ci.id = br.cart_item_id
         JOIN orders o ON o.id = ci.order_id
         LEFT JOIN catalog_stores st ON st.id = br.store_id
         WHERE br.item_id = $1
         ORDER BY br.date_reserve DESC, br.id DESC",
    )
    .bind(item_id)
    .fetch_all(&state.db)
    .await?;
    render(
        &state,
        "shop/rest_reserves.html",
        context! { user, item_id, name, items },
    )
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use crate::{
        smoke_tests::get,
        test_support::{admin_cookie, order_fixture, place_order, test_state},
    };

    async fn two_stores(db: &PgPool, item: i64) {
        sqlx::query("INSERT INTO catalog_stores (id, name) VALUES (2, 'Второй')")
            .execute(db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO catalog_store_amounts (item_id, store_id, amount, quantity_reserved)
             VALUES ($1, 2, 9, 2)",
        )
        .bind(item)
        .execute(db)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn rests_table(db: PgPool) {
        let f = order_fixture(&db).await;
        two_stores(&db, f.product_id).await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s, html) = get(&app, "/admin/shop/rests", &cookie).await;
        assert!(s.is_success(), "{s}");
        for want in [
            "Остаток",
            "Резерв",
            "Доступно",
            "Товар",
            "Склад",
            "Второй",
            ">9<",
            ">7<",
            ">5<",
            ">2<",
        ] {
            assert!(html.contains(want), "{want}: {html}");
        }
        let (_, html) = get(&app, "/admin/shop/rests?store=2", &cookie).await;
        assert!(html.contains("Второй"), "{html}");
        assert!(!html.contains(">5<"), "склад 1 отфильтрован: {html}");
    }

    #[sqlx::test]
    async fn rest_reserves_list(db: PgPool) {
        let f = order_fixture(&db).await;
        let order = place_order(&db, &f, None).await;
        let cart_item: i64 = sqlx::query_scalar("SELECT id FROM cart_items WHERE order_id = $1")
            .bind(order)
            .fetch_one(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO basket_reservations (cart_item_id, item_id, store_id, quantity, date_reserve_end)
             VALUES ($1, $2, 1, 3, '2030-01-02 10:30+00')",
        )
        .bind(cart_item)
        .bind(f.product_id)
        .execute(&db)
        .await
        .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s, html) = get(
            &app,
            &format!("/admin/shop/rests/{}", f.product_id),
            &cookie,
        )
        .await;
        assert!(s.is_success(), "{s}");
        assert!(
            html.contains(&format!("/admin/shop/orders/{order}")),
            "{html}"
        );
        for want in ["Склад", ">3<", "02.01.2030"] {
            assert!(html.contains(want), "{want}: {html}");
        }
    }

    #[sqlx::test]
    async fn negative_amount_display(db: PgPool) {
        let f = order_fixture(&db).await;
        sqlx::query("UPDATE catalog_store_amounts SET amount = -2 WHERE item_id = $1")
            .bind(f.product_id)
            .execute(&db)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (_, html) = get(&app, "/admin/shop/rests", &cookie).await;
        assert!(!html.contains(">-2<"), "{html}");
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ('catalog', 'allow_negative_amount', 'Y')
             ON CONFLICT (module, name) DO UPDATE SET value = 'Y'",
        )
        .execute(&db)
        .await
        .unwrap();
        let (_, html) = get(&app, "/admin/shop/rests", &cookie).await;
        assert!(html.contains(">-2<"), "{html}");
    }
}
