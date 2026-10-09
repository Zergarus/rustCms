//! Генератор торговых предложений: сочетания значений полей выбора товара.

use axum::{
    Extension, Form,
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use minijinja::context;
use serde::Serialize;
use serde_json::Map;

use super::{
    items::{FormValues, purchase_input_from_form, require_parent_write},
    render,
};
use crate::{
    access::{Access, Level},
    catalog,
    collection::{
        Collection, Field, Item, ItemInput, repo,
        sku::{self, Axis, AxisValue},
        slugify,
    },
    error::{AppError, AppResult},
    state::AppState,
};

/// Сколько записей привязанной коллекции показывается для выбора.
const LINKED_LIMIT: i64 = 500;

/// Товар, коллекция его предложений и всё, что нужно форме генератора.
struct Context {
    product: Item,
    product_collection: Collection,
    offers: Collection,
    /// Поля выбора предложения, которые умеет перебирать генератор, по порядку.
    tree: Vec<Field>,
    price_types: Vec<(i64, String)>,
    stores: Vec<(i64, String)>,
}

/// Товар `product_id` (запись коллекции товаров). Нужна запись в обеих коллекциях,
/// как для остальных страниц предложений.
async fn load(state: &AppState, user: &Access, product_id: i64) -> AppResult<Context> {
    let product = repo::get_item(&state.db, product_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let product_collection = repo::get_collection(&state.db, product.collection_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let offers = repo::offers_collection(&state.db, product.collection_id)
        .await?
        .ok_or(AppError::NotFound)?;
    user.require_collection(offers.id, Level::Write)?;
    require_parent_write(user, &offers)?;
    let tree = repo::list_fields(&state.db, offers.id)
        .await?
        .into_iter()
        .filter(|f| f.offer_tree && matches!(f.kind.as_str(), "list" | "element" | "string"))
        .collect();
    Ok(Context {
        product,
        product_collection,
        offers,
        tree,
        price_types: sqlx::query_as("SELECT id, name FROM catalog_price_types ORDER BY sort, id")
            .fetch_all(&state.db)
            .await?,
        stores: sqlx::query_as(
            "SELECT id, name FROM catalog_stores WHERE active ORDER BY sort, id",
        )
        .fetch_all(&state.db)
        .await?,
    })
}

#[derive(Serialize)]
struct Choice {
    value: String,
    label: String,
    checked: bool,
}

#[derive(Serialize)]
struct AxisView {
    code: String,
    name: String,
    /// `true` — значения вводятся строкой через запятую, иначе отмечаются галочками.
    text: bool,
    typed: String,
    choices: Vec<Choice>,
}

/// Варианты выбора каждого поля; отмечены значения из `form`.
async fn axis_views(
    state: &AppState,
    ctx: &Context,
    form: &FormValues,
) -> AppResult<Vec<AxisView>> {
    let options = repo::list_collection_options(&state.db, ctx.offers.id).await?;
    let mut views = Vec::new();
    for f in &ctx.tree {
        let key = format!("axis_{}", f.code);
        let picked = form.all(&key);
        let checked = |id: i64| picked.iter().any(|p| p.trim() == id.to_string());
        let choices: Vec<(i64, String)> = match f.kind.as_str() {
            "list" => options
                .iter()
                .filter(|o| o.field_id == f.id)
                .map(|o| (o.id, o.value.clone()))
                .collect(),
            "element" => match f.link_collection_id {
                Some(link) => repo::list_items(&state.db, link, None, LINKED_LIMIT, 0)
                    .await?
                    .0
                    .into_iter()
                    .map(|i| (i.id, i.name))
                    .collect(),
                None => Vec::new(),
            },
            _ => Vec::new(),
        };
        views.push(AxisView {
            code: f.code.clone(),
            name: f.name.clone(),
            text: f.kind == "string",
            typed: form.get(&key).to_string(),
            choices: choices
                .into_iter()
                .map(|(id, label)| Choice {
                    value: id.to_string(),
                    label,
                    checked: checked(id),
                })
                .collect(),
        });
    }
    Ok(views)
}

async fn render_form(
    state: &AppState,
    user: Access,
    ctx: &Context,
    form: &FormValues,
    error: Option<String>,
) -> AppResult<Response> {
    let axes = axis_views(state, ctx, form).await?;
    let html = render(
        state,
        "offer_generator.html",
        context! {
            user, error, axes,
            product => &ctx.product,
            product_collection => &ctx.product_collection,
            price_types => &ctx.price_types,
            stores => &ctx.stores,
            form => form.first_values(),
        },
    )?;
    Ok(html.into_response())
}

pub async fn generate_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(product_id): Path<i64>,
) -> AppResult<Response> {
    let ctx = load(&state, &user, product_id).await?;
    let mut form = FormValues::default();
    form.set("active", "on");
    render_form(&state, user, &ctx, &form, None).await
}

/// Оси из отмеченных значений формы: `axis_<код>` — id вариантов / записей (только
/// существующие) или строка через запятую для строковых полей.
async fn axes_from_form(
    state: &AppState,
    ctx: &Context,
    form: &FormValues,
) -> AppResult<Vec<(Axis, Field)>> {
    let options = repo::list_collection_options(&state.db, ctx.offers.id).await?;
    let mut axes = Vec::new();
    for f in &ctx.tree {
        let key = format!("axis_{}", f.code);
        let values: Vec<(AxisValue, String)> = match f.kind.as_str() {
            "string" => {
                let mut texts: Vec<String> = Vec::new();
                for text in form.all(&key).iter().flat_map(|raw| raw.split(',')) {
                    let text = text.trim();
                    if !text.is_empty() && !texts.iter().any(|t| t == text) {
                        texts.push(text.to_string());
                    }
                }
                texts
                    .into_iter()
                    .map(|t| (AxisValue::Text(t.clone()), t))
                    .collect()
            }
            kind => {
                let mut ids: Vec<i64> = Vec::new();
                for id in form.all(&key).iter().filter_map(|s| s.trim().parse().ok()) {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                if kind == "list" {
                    ids.into_iter()
                        .filter_map(|id| {
                            options
                                .iter()
                                .find(|o| o.id == id && o.field_id == f.id)
                                .map(|o| (AxisValue::Option(id), o.value.clone()))
                        })
                        .collect()
                } else {
                    let names = repo::item_names(&state.db, &ids, f.link_collection_id).await?;
                    ids.into_iter()
                        .filter_map(|id| {
                            names
                                .iter()
                                .find(|(n, _)| *n == id)
                                .map(|(_, name)| (AxisValue::Item(id), name.clone()))
                        })
                        .collect()
                }
            }
        };
        if !values.is_empty() {
            axes.push((
                Axis {
                    code: f.code.clone(),
                    values,
                },
                f.clone(),
            ));
        }
    }
    Ok(axes)
}

pub async fn generate(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(product_id): Path<i64>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let ctx = load(&state, &user, product_id).await?;
    let mut form = FormValues::from_pairs(pairs);

    let axes = axes_from_form(&state, &ctx, &form).await?;
    let only_axes: Vec<Axis> = axes.iter().map(|(a, _)| a.clone()).collect();
    let combos = match sku::combinations(&only_axes) {
        Ok(combos) => combos,
        Err(e) => return render_form(&state, user, &ctx, &form, Some(e.to_string())).await,
    };
    // Как у обычной формы цен и остатков; доступность считается триггерами по остаткам
    form.set("available", "on");
    let price_types: Vec<i64> = ctx.price_types.iter().map(|(id, _)| *id).collect();
    let stores: Vec<i64> = ctx.stores.iter().map(|(id, _)| *id).collect();
    let purchase = match purchase_input_from_form(&form, &price_types, &stores) {
        Ok(p) => p,
        Err(e) => return render_form(&state, user, &ctx, &form, Some(e)).await,
    };

    let used: Vec<Field> = axes.into_iter().map(|(_, f)| f).collect();
    let existing = sku::existing_keys(&state.db, product_id, &used).await?;
    let template = match form.get("template") {
        "" => format!(
            "#PRODUCT_NAME# ({})",
            used.iter()
                .map(|f| format!("#{}#", f.code.to_uppercase()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        t => t.to_string(),
    };
    let active = !form.all("active").is_empty();

    let mut tx = state.db.begin().await?;
    let (mut created, mut skipped) = (0usize, 0usize);
    for combo in &combos {
        if existing.contains(&sku::combo_key(combo)) {
            skipped += 1;
            continue;
        }
        let name = sku::offer_name(&template, &ctx.product.name, combo);
        let mut field_values = Map::new();
        for (code, value, _) in combo {
            let multiple = used.iter().any(|f| f.code == *code && f.multiple);
            let json = value.to_json();
            field_values.insert(
                code.clone(),
                if multiple {
                    serde_json::Value::Array(vec![json])
                } else {
                    json
                },
            );
        }
        let code = free_code(&mut tx, ctx.offers.id, &name).await?;
        let input = ItemInput {
            section_id: None,
            code,
            xml_id: uuid::Uuid::new_v4().to_string(),
            name,
            active,
            sort: 500,
            preview_text: String::new(),
            detail_text: String::new(),
            preview_picture_id: None,
            detail_picture_id: None,
            published_at: None,
            field_values,
            product_id: Some(product_id),
        };
        let id = repo::create_item(&mut *tx, ctx.offers.id, &input).await?;
        catalog::save_purchase_in(&mut tx, id, &purchase).await?;
        created += 1;
    }
    tx.commit().await?;
    Ok(Redirect::to(&format!(
        "/admin/items/{product_id}?generated={created}&skipped={skipped}#offers"
    ))
    .into_response())
}

/// Свободный символьный код в коллекции из названия: `name`, `name-2`, ...
async fn free_code(
    tx: &mut sqlx::PgConnection,
    collection_id: i64,
    name: &str,
) -> sqlx::Result<String> {
    let base = match slugify(name) {
        s if s.is_empty() => "offer".to_string(),
        s => s,
    };
    let mut code = base.clone();
    let mut n = 1;
    while sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM collection_items WHERE collection_id = $1 AND code = $2)",
    )
    .bind(collection_id)
    .bind(&code)
    .fetch_one(&mut *tx)
    .await?
    {
        n += 1;
        code = format!("{base}-{n}");
    }
    Ok(code)
}
