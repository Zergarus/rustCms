//! Публичный JSON API для фронтенда (Vue и т.п.). Только чтение, без авторизации.
//! Отдаются только инфоблоки с `api_enabled` и только активные, опубликованные элементы.

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::{FromRow, Postgres, QueryBuilder, types::Json as SqlJson};

use crate::{
    collection::{Collection, is_valid_code, repo, sku},
    error::{AppError, AppResult},
    state::AppState,
};

const DEFAULT_PER_PAGE: i64 = 20;
const MAX_PER_PAGE: i64 = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/v1/iblocks", get(list_collections))
        .route("/v1/iblocks/{iblock}", get(get_collection))
        .route("/v1/iblocks/{iblock}/elements", get(list_items))
        .route("/v1/iblocks/{iblock}/elements/{element}", get(get_item))
}

async fn health(State(state): State<AppState>) -> AppResult<Json<Value>> {
    sqlx::query("SELECT 1").execute(&state.db).await?;
    Ok(Json(json!({ "status": "ok" })))
}

#[derive(Serialize)]
struct ApiIblock {
    code: String,
    name: String,
    description: String,
}

#[derive(Serialize)]
struct ApiProperty {
    code: String,
    name: String,
    kind: String,
    required: bool,
}

#[derive(Serialize, FromRow)]
struct ApiElement {
    id: i64,
    code: String,
    name: String,
    sort: i32,
    preview_text: String,
    detail_text: String,
    published_at: Option<DateTime<Utc>>,
    properties: SqlJson<Map<String, Value>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    /// Только у предложения: родительский товар.
    #[serde(skip_serializing_if = "Option::is_none")]
    product_id: Option<i64>,
    /// Только у записей коллекций с каталогом: тип товара.
    #[sqlx(rename = "catalog_type")]
    #[serde(
        rename = "catalog",
        serialize_with = "serialize_catalog",
        skip_serializing_if = "Option::is_none"
    )]
    catalog: Option<i16>,
    /// Только у записей коллекций с каталогом, цену которых меняет правило работы с корзиной:
    /// цена единицы основного типа после правил товара для гостя (`GetOptimalPrice`).
    #[sqlx(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    price_discount: Option<f64>,
}

/// `price_discount` записей коллекции с каталогом: правила читаются один раз на запрос.
async fn fill_price_discount(
    state: &AppState,
    collection: &Collection,
    items: &mut [ApiElement],
) -> AppResult<()> {
    if !collection.is_catalog || items.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = items.iter().map(|i| i.id).collect();
    let prices = crate::catalog::load_prices(&state.db, &ids).await?;
    let base: Vec<(i64, f64)> = ids
        .iter()
        .filter_map(|id| Some((*id, crate::catalog::main_price(prices.get(id)?)?.price)))
        .collect();
    let discounted = crate::discount::catalog::discount_prices(
        &state.db,
        state.project.discount_condition_hook.as_deref(),
        None,
        &base,
    )
    .await?;
    for item in items {
        item.price_discount = discounted.get(&item.id).copied();
    }
    Ok(())
}

fn serialize_catalog<S: serde::Serializer>(
    value: &Option<i16>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    json!({ "type": value }).serialize(serializer)
}

#[derive(Serialize)]
struct Pagination {
    page: i64,
    per_page: i64,
    total: i64,
    pages: i64,
}

async fn list_collections(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let items: Vec<ApiIblock> = repo::list_collections(&state.db)
        .await?
        .into_iter()
        .filter(|s| s.collection.api_enabled)
        .map(|s| ApiIblock {
            code: s.collection.code,
            name: s.collection.name,
            description: s.collection.description,
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

async fn public_iblock(state: &AppState, code: &str) -> AppResult<Collection> {
    repo::get_collection_by_code(&state.db, code)
        .await?
        .filter(|i| i.api_enabled)
        .ok_or(AppError::NotFound)
}

async fn get_collection(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> AppResult<Json<Value>> {
    let iblock = public_iblock(&state, &code).await?;
    let properties: Vec<ApiProperty> = repo::list_fields(&state.db, iblock.id)
        .await?
        .into_iter()
        .map(|p| ApiProperty {
            code: p.code,
            name: p.name,
            kind: p.kind,
            required: p.is_required,
        })
        .collect();
    Ok(Json(json!({
        "code": iblock.code,
        "name": iblock.name,
        "description": iblock.description,
        "properties": properties,
    })))
}

/// Параметры списка:
/// - `page`, `per_page` (≤ 100)
/// - `sort`: `sort` | `name` | `published_at` | `created_at` | `id`, префикс `-` — по убыванию
/// - `prop.<код>=<значение>` — фильтр по значению свойства (сравнение как строк)
async fn list_items(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Json<Value>> {
    let iblock = public_iblock(&state, &code).await?;

    let page = parse_positive(params.get("page"), 1).min(1_000_000);
    let per_page = parse_positive(params.get("per_page"), DEFAULT_PER_PAGE).min(MAX_PER_PAGE);
    let order_by = match params.get("sort").map(String::as_str).unwrap_or("sort") {
        "sort" => "sort ASC, id DESC",
        "-sort" => "sort DESC, id DESC",
        "name" => "name ASC, id ASC",
        "-name" => "name DESC, id DESC",
        "published_at" => "published_at ASC NULLS FIRST, id ASC",
        "-published_at" => "published_at DESC NULLS LAST, id DESC",
        "created_at" => "created_at ASC, id ASC",
        "-created_at" => "created_at DESC, id DESC",
        "id" => "id ASC",
        "-id" => "id DESC",
        other => {
            return Err(AppError::BadRequest(format!(
                "недопустимая сортировка: {other}"
            )));
        }
    };

    let mut prop_filters = Vec::new();
    for (key, value) in &params {
        if let Some(prop) = key.strip_prefix("prop.") {
            if !is_valid_code(prop) {
                return Err(AppError::BadRequest(format!(
                    "недопустимый код свойства: {prop}"
                )));
            }
            prop_filters.push((prop.to_string(), value.clone()));
        }
    }

    let push_where = |qb: &mut QueryBuilder<Postgres>| {
        qb.push(" FROM collection_items WHERE collection_id = ")
            .push_bind(iblock.id)
            .push(" AND active AND (published_at IS NULL OR published_at <= now())");
        for (prop, value) in &prop_filters {
            qb.push(" AND field_values ->> ")
                .push_bind(prop.clone())
                .push(" = ")
                .push_bind(value.clone());
        }
    };

    let mut count_qb = QueryBuilder::new("SELECT count(*)");
    push_where(&mut count_qb);
    let (total,): (i64,) = count_qb.build_query_as().fetch_one(&state.db).await?;

    let mut qb = QueryBuilder::new(format!(
        "SELECT id, code, name, sort, preview_text, detail_text, published_at, {} AS properties, \
         created_at, updated_at, product_id, {} AS catalog_type",
        sku::field_values_sql("collection_items"),
        catalog_type_sql(&iblock),
    ));
    push_where(&mut qb);
    qb.push(" ORDER BY ")
        .push(order_by)
        .push(" LIMIT ")
        .push_bind(per_page)
        .push(" OFFSET ")
        .push_bind((page - 1) * per_page);
    let mut items: Vec<ApiElement> = qb.build_query_as().fetch_all(&state.db).await?;
    fill_price_discount(&state, &iblock, &mut items).await?;

    let pagination = Pagination {
        page,
        per_page,
        total,
        pages: (total + per_page - 1) / per_page,
    };
    Ok(Json(json!({ "items": items, "pagination": pagination })))
}

async fn get_item(
    State(state): State<AppState>,
    Path((iblock_code, element_code)): Path<(String, String)>,
) -> AppResult<Json<ApiElement>> {
    let iblock = public_iblock(&state, &iblock_code).await?;
    let mut element: ApiElement = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, code, name, sort, preview_text, detail_text, published_at, {} AS properties,
                created_at, updated_at, product_id, {} AS catalog_type
         FROM collection_items
         WHERE collection_id = $1 AND code = $2 AND active
           AND (published_at IS NULL OR published_at <= now())",
        sku::field_values_sql("collection_items"),
        catalog_type_sql(&iblock),
    )))
    .bind(iblock.id)
    .bind(&element_code)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    fill_price_discount(&state, &iblock, std::slice::from_mut(&mut element)).await?;
    Ok(Json(element))
}

/// Тип товара записи (`catalog_products.type`) у коллекций с каталогом, иначе `NULL`.
fn catalog_type_sql(collection: &Collection) -> &'static str {
    if collection.is_catalog {
        "COALESCE((SELECT cp.type FROM catalog_products cp WHERE cp.item_id = collection_items.id),
                  CASE WHEN collection_items.product_id IS NULL THEN 1 ELSE 4 END)::smallint"
    } else {
        "NULL::smallint"
    }
}

fn parse_positive(raw: Option<&String>, default: i64) -> i64 {
    raw.and_then(|s| s.parse::<i64>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};

    async fn get(app: &axum::Router, path: &str) -> Value {
        let (status, _, body) = crate::test_support::get_raw(app, path, "").await;
        assert_eq!(status, StatusCode::OK, "{path}");
        serde_json::from_slice(&body).unwrap()
    }

    /// Каталог из `cart_fixture` открыт в API: товар `tovar` (1000), товар с предложениями
    /// `maslo` (своей цены нет), предложение `maslo-1l` (500) — в коллекции `offers`.
    async fn catalog(db: &sqlx::PgPool) -> crate::test_support::CartFixture {
        let fx = crate::test_support::cart_fixture(db).await;
        sqlx::query("UPDATE collections SET api_enabled = TRUE")
            .execute(db)
            .await
            .unwrap();
        fx
    }

    async fn insert_rule(db: &sqlx::PgPool, conditions: Value, actions: Value) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO discounts (name, conditions, actions) VALUES ('Скидка', $1, $2) RETURNING id",
        )
        .bind(conditions)
        .bind(json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": actions}))
        .fetch_one(db)
        .await
        .unwrap()
    }

    fn always() -> Value {
        json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": []})
    }

    fn perc(value: f64, filter: Value) -> Value {
        json!([{
            "CLASS_ID": "ActSaleBsktGrp",
            "DATA": {"Type": "Discount", "Value": value, "Unit": "Perc", "Max": 0, "All": "AND", "True": "True"},
            "CHILDREN": filter
        }])
    }

    fn by_code<'a>(list: &'a Value, code: &str) -> &'a Value {
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["code"] == code)
            .unwrap()
    }

    #[sqlx::test]
    async fn api_without_rules_unchanged(db: sqlx::PgPool) {
        catalog(&db).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        // Правило с условием корзины и правило по купону в каталоге не участвуют
        insert_rule(
            &db,
            json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
                {"CLASS_ID": "CondSaleOrderSumm", "DATA": {"logic": "Great", "value": 1}}
            ]}),
            perc(10.0, json!([])),
        )
        .await;
        let coupon = insert_rule(&db, always(), perc(20.0, json!([]))).await;
        sqlx::query("UPDATE discounts SET use_coupons = TRUE WHERE id = $1")
            .bind(coupon)
            .execute(&db)
            .await
            .unwrap();
        let list = get(&app, "/api/v1/iblocks/catalog/elements").await;
        let detail = get(&app, "/api/v1/iblocks/catalog/elements/tovar").await;
        let keys = |v: &Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        let want = [
            "catalog",
            "code",
            "created_at",
            "detail_text",
            "id",
            "name",
            "preview_text",
            "properties",
            "published_at",
            "sort",
            "updated_at",
        ];
        assert_eq!(keys(by_code(&list, "tovar")), want);
        assert_eq!(keys(&detail), want);
        assert!(!list.to_string().contains("price_discount"));
    }

    #[sqlx::test]
    async fn api_price_discount_only_with_rule(db: sqlx::PgPool) {
        let fx = catalog(&db).await;
        let app = crate::app(crate::test_support::test_state(db.clone()));
        // −10 % на `tovar`; −50 % для группы — гостю не положено
        insert_rule(
            &db,
            always(),
            perc(
                10.0,
                json!([{"CLASS_ID": "CondBsktFldProduct", "DATA": {"logic": "Equal", "value": fx.f.product_id}}]),
            ),
        )
        .await;
        let vip = insert_rule(&db, always(), perc(50.0, json!([]))).await;
        let group: i64 = sqlx::query_scalar(
            "INSERT INTO groups (code, name) VALUES ('vip', 'VIP') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO discount_groups (discount_id, group_id) VALUES ($1, $2)")
            .bind(vip)
            .bind(group)
            .execute(&db)
            .await
            .unwrap();

        let list = get(&app, "/api/v1/iblocks/catalog/elements").await;
        assert_eq!(by_code(&list, "tovar")["price_discount"], json!(900.0));
        // У товара с предложениями своей цены нет
        assert!(by_code(&list, "maslo").get("price_discount").is_none());
        let detail = get(&app, "/api/v1/iblocks/catalog/elements/tovar").await;
        assert_eq!(detail["price_discount"], json!(900.0));
        // Предложение: правило на `tovar` его не касается
        let offers = get(&app, "/api/v1/iblocks/offers/elements").await;
        assert!(by_code(&offers, "maslo-1l").get("price_discount").is_none());

        // Правило на всё — и предложению
        insert_rule(&db, always(), perc(20.0, json!([]))).await;
        let offers = get(&app, "/api/v1/iblocks/offers/elements").await;
        assert_eq!(by_code(&offers, "maslo-1l")["price_discount"], json!(400.0));
        let list = get(&app, "/api/v1/iblocks/catalog/elements").await;
        assert_eq!(by_code(&list, "tovar")["price_discount"], json!(720.0));
    }
}
