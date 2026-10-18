//! Раздел «Скидки»: правила работы с корзиной (как «Правила работы с корзиной» Битрикса) и их
//! купоны. Дерево условий и действий собирает `static/admin/discount-tree.js` в скрытые поля
//! `conditions` и `actions`; сервер разбирает их в [`Node`] и проверяет (`validate_with`).

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::{
    Extension, Form, Json,
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{DateTime, NaiveDateTime, Utc};
use minijinja::context;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::render;
use crate::{
    access::{Access, DISCOUNTS_MANAGE},
    discount::model::{
        ACTION_CLASSES, CONDITION_CLASSES, CouponKind, Node, Refs, allowed_in, is_group,
        unsupported_classes_with, validate_with,
    },
    error::{AppError, AppResult, is_unique_violation},
    state::AppState,
};

type FormFields = Vec<(String, String)>;

const DATETIME_FORMAT: &str = "%Y-%m-%dT%H:%M";
/// Длина сгенерированного кода после префикса.
const CODE_LEN: usize = 12;
const CODE_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
/// Код купона — не длиннее, чем `COUPON` в Битриксе.
const MAX_CODE_LEN: usize = 32;
const MAX_PREFIX_LEN: usize = MAX_CODE_LEN - CODE_LEN;
const MAX_GENERATE: i64 = 10_000;
/// Сколько купонов показывать на странице правила.
const COUPONS_SHOWN: i64 = 500;

const DEFAULT_CONDITIONS: &str =
    r#"{"CLASS_ID":"CondGroup","DATA":{"All":"AND","True":"True"},"CHILDREN":[]}"#;
const DEFAULT_ACTIONS: &str = r#"{"CLASS_ID":"CondGroup","DATA":{"All":"AND"},"CHILDREN":[]}"#;

fn field<'a>(fields: &'a FormFields, key: &str) -> &'a str {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

fn checked(fields: &FormFields, key: &str) -> bool {
    fields.iter().any(|(k, _)| k == key)
}

fn input_date(dt: Option<DateTime<Utc>>) -> String {
    dt.map(|d| d.format(DATETIME_FORMAT).to_string())
        .unwrap_or_default()
}

fn shown_date(dt: Option<DateTime<Utc>>) -> String {
    dt.map(|d| d.format("%d.%m.%Y %H:%M").to_string())
        .unwrap_or_default()
}

fn parse_date(raw: &str, label: &str, errors: &mut Vec<String>) -> Option<DateTime<Utc>> {
    match raw.trim() {
        "" => None,
        raw => match NaiveDateTime::parse_from_str(raw, DATETIME_FORMAT) {
            Ok(dt) => Some(dt.and_utc()),
            Err(_) => {
                errors.push(format!("{label}: неверная дата"));
                None
            }
        },
    }
}

fn parse_int(raw: &str, default: i32, label: &str, errors: &mut Vec<String>) -> i32 {
    match raw.trim() {
        "" => default,
        raw => raw.parse().unwrap_or_else(|_| {
            errors.push(format!("{label}: целое число"));
            default
        }),
    }
}

fn hook_classes(state: &AppState) -> &[&'static str] {
    state
        .project
        .discount_condition_hook
        .as_deref()
        .map(|h| h.class_ids())
        .unwrap_or(&[])
}

// ---------- Конструктор: классы, подписи (lang/ru Битрикса), вложенность ----------

/// Поле `DATA` узла в конструкторе.
#[derive(Serialize)]
struct FieldDef {
    key: &'static str,
    /// Текст перед полем.
    label: &'static str,
    /// `choice` — один из вариантов, `multi` — несколько (id), `number`, `text`,
    /// `ids` — id через запятую с подсказкой названий (`lookup`), `json` — весь `DATA`.
    kind: &'static str,
    options: Vec<(String, String)>,
    default: Value,
    lookup: Option<&'static str>,
}

impl FieldDef {
    fn new(key: &'static str, label: &'static str, kind: &'static str, default: Value) -> Self {
        FieldDef {
            key,
            label,
            kind,
            options: Vec::new(),
            default,
            lookup: None,
        }
    }

    fn choice(key: &'static str, label: &'static str, options: &[(&str, &str)]) -> Self {
        FieldDef {
            options: options
                .iter()
                .map(|(v, l)| (v.to_string(), l.to_string()))
                .collect(),
            default: Value::from(options[0].0),
            ..FieldDef::new(key, label, "choice", Value::Null)
        }
    }

    fn multi(options: Vec<(String, String)>) -> Self {
        FieldDef {
            options,
            ..FieldDef::new("value", "", "multi", json!([]))
        }
    }

    fn ids(lookup: &'static str) -> Self {
        FieldDef {
            lookup: Some(lookup),
            ..FieldDef::new("value", "", "ids", json!([]))
        }
    }
}

/// Класс узла в конструкторе.
#[derive(Serialize)]
struct ClassDef {
    id: String,
    label: String,
    /// Группа в списке выбора (как группы контролов Битрикса).
    group: String,
    fields: Vec<FieldDef>,
}

const LOGIC_EQ: &[&str] = &["Equal", "Not"];
const LOGIC_NUM: &[&str] = &["Equal", "Not", "Great", "Less", "EqGr", "EqLs"];
const LOGIC_STR: &[&str] = &["Equal", "Not", "Contain", "NotCont"];

fn logic(set: &[&str]) -> FieldDef {
    let label = |l: &str| match l {
        "Equal" => "равно",
        "Not" => "не равно",
        "Great" => "больше",
        "Less" => "меньше",
        "EqGr" => "больше либо равно",
        "EqLs" => "меньше либо равно",
        "Contain" => "содержит",
        _ => "не содержит",
    };
    let options: Vec<(&str, &str)> = set.iter().map(|l| (*l, label(l))).collect();
    FieldDef::choice("logic", "", &options)
}

fn all(label: &'static str, all: &str, any: &str) -> FieldDef {
    FieldDef::choice("All", label, &[("AND", all), ("OR", any)])
}

fn truth() -> FieldDef {
    FieldDef::choice(
        "True",
        "",
        &[("True", "выполнено(ы)"), ("False", "не выполнено(ы)")],
    )
}

fn number(key: &'static str, label: &'static str) -> FieldDef {
    FieldDef::new(key, label, "number", json!(0))
}

fn text() -> FieldDef {
    FieldDef::new("value", "", "text", json!(""))
}

fn yes_no() -> FieldDef {
    FieldDef::choice("value", "", &[("Y", "да"), ("N", "нет")])
}

/// Справочники для выпадающих списков конструктора.
#[derive(Default)]
struct Dicts {
    collections: Vec<(i64, String)>,
    /// (id, коллекция, название, тип)
    fields: Vec<(i64, i64, String, String)>,
    /// (id, поле, значение)
    options: Vec<(i64, i64, String)>,
    pay_systems: Vec<(i64, String)>,
    deliveries: Vec<(i64, String)>,
    person_types: Vec<(i64, String)>,
}

impl Dicts {
    async fn load(db: &PgPool) -> sqlx::Result<Dicts> {
        let pairs = |sql: &'static str| sqlx::query_as::<_, (i64, String)>(sql).fetch_all(db);
        Ok(Dicts {
            collections: pairs("SELECT id, name FROM collections ORDER BY sort, id").await?,
            fields: sqlx::query_as(
                "SELECT id, collection_id, name, kind FROM collection_fields ORDER BY sort, id",
            )
            .fetch_all(db)
            .await?,
            options: sqlx::query_as(
                "SELECT id, field_id, value FROM collection_field_options ORDER BY sort, id",
            )
            .fetch_all(db)
            .await?,
            pay_systems: pairs("SELECT id, name FROM pay_systems ORDER BY sort, id").await?,
            deliveries: pairs("SELECT id, name FROM deliveries ORDER BY sort, id").await?,
            person_types: pairs("SELECT id, name FROM person_types ORDER BY sort, id").await?,
        })
    }
}

fn labeled(list: &[(i64, String)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(id, name)| (id.to_string(), format!("{name} [{id}]")))
        .collect()
}

const G_BASKET: &str = "Корзина";
const G_BASKET_FIELDS: &str = "Параметры в корзине";
const G_PRODUCT: &str = "Поля и характеристики товара";
const G_ORDER: &str = "Параметры заказа";
const G_COMMON: &str = "Общие условия";
const G_ACTIONS: &str = "Действия";
const G_PROJECT: &str = "Условия проекта";
const ROW_ALL: (&str, &str) = ("всем условиям", "любому из условий");

/// Все классы конструктора: подписи — из `sale_cond.php`, `sale_act.php`, `catalog_cond.php`
/// (инфоблок — наша коллекция), свойства — по полям коллекций.
fn class_defs(d: &Dicts, hooks: &[&str]) -> Vec<ClassDef> {
    let def = |id: &str, label: &str, group: &str, fields: Vec<FieldDef>| ClassDef {
        id: id.to_string(),
        label: label.to_string(),
        group: group.to_string(),
        fields,
    };
    let counted = |id: &str, label: &str| {
        def(
            id,
            label,
            G_BASKET,
            vec![
                logic(LOGIC_NUM),
                number("Value", ""),
                all("товары, удовлетворяющие", ROW_ALL.0, ROW_ALL.1),
            ],
        )
    };
    let leaf = |id: &str, label: &str, group: &str, logic_set: &[&str], value: FieldDef| {
        def(id, label, group, vec![logic(logic_set), value])
    };
    let days: Vec<(String, String)> = [
        "Понедельник",
        "Вторник",
        "Среда",
        "Четверг",
        "Пятница",
        "Суббота",
        "Воскресенье",
    ]
    .iter()
    .enumerate()
    .map(|(i, n)| ((i + 1).to_string(), n.to_string()))
    .collect();

    let mut out = vec![
        def(
            "CondGroup",
            "Группа условий",
            G_COMMON,
            vec![all("Если", "все условия", "любое из условий"), truth()],
        ),
        counted("CondBsktCntGroup", "Количество товаров"),
        counted("CondBsktAmtGroup", "Общая стоимость товаров"),
        counted("CondBsktAmtBaseGroup", "Общая исходная стоимость товаров"),
        def(
            "CondBsktProductGroup",
            "Товары",
            G_BASKET,
            vec![
                FieldDef::choice(
                    "Found",
                    "в корзине/заказе",
                    &[("Found", "есть"), ("NoFound", "отсутствуют")],
                ),
                all("товары, удовлетворяющие", ROW_ALL.0, ROW_ALL.1),
            ],
        ),
        counted("CondBsktRowGroup", "Количество позиций"),
        def(
            "CondBsktSubGroup",
            "Группа условий",
            G_BASKET,
            vec![all("", "все условия", "любое из условий"), truth()],
        ),
        leaf(
            "CondBsktFldProduct",
            "Товар в корзине",
            G_BASKET_FIELDS,
            LOGIC_EQ,
            FieldDef::ids("items"),
        ),
        leaf(
            "CondBsktFldName",
            "Название товара в корзине",
            G_BASKET_FIELDS,
            LOGIC_STR,
            text(),
        ),
        leaf(
            "CondBsktFldPrice",
            "Цена товара",
            G_BASKET_FIELDS,
            LOGIC_NUM,
            number("value", ""),
        ),
        leaf(
            "CondBsktFldQuantity",
            "Количество товара",
            G_BASKET_FIELDS,
            LOGIC_NUM,
            number("value", ""),
        ),
        leaf(
            "CondBsktFldSumm",
            "Стоимость позиции",
            G_BASKET_FIELDS,
            LOGIC_NUM,
            number("value", ""),
        ),
        leaf(
            "CondBsktFldWeight",
            "Вес товара",
            G_BASKET_FIELDS,
            LOGIC_NUM,
            number("value", "грамм"),
        ),
        def(
            "CondBsktAppliedDiscount",
            "Были применены правила корзины",
            G_BASKET_FIELDS,
            vec![yes_no()],
        ),
        leaf(
            "CondIBElement",
            "Товар",
            G_PRODUCT,
            LOGIC_EQ,
            FieldDef::ids("items"),
        ),
        leaf(
            "CondIBIBlock",
            "Коллекция",
            G_PRODUCT,
            LOGIC_EQ,
            FieldDef::multi(labeled(&d.collections)),
        ),
        leaf(
            "CondIBSection",
            "Раздел",
            G_PRODUCT,
            LOGIC_EQ,
            FieldDef::ids("sections"),
        ),
        leaf("CondIBXmlID", "Внешний код", G_PRODUCT, LOGIC_STR, text()),
        leaf("CondIBName", "Название", G_PRODUCT, LOGIC_STR, text()),
        leaf("CondIBCode", "Символьный код", G_PRODUCT, LOGIC_STR, text()),
        leaf(
            "CondCatQuantity",
            "Количество товара на складе",
            G_PRODUCT,
            LOGIC_NUM,
            number("value", ""),
        ),
        leaf(
            "CondCatWeight",
            "Вес товара",
            G_PRODUCT,
            LOGIC_NUM,
            number("value", "грамм"),
        ),
        leaf(
            "CondSaleOrderSumm",
            "Общая сумма заказа c доставкой и налогами",
            G_ORDER,
            LOGIC_NUM,
            number("value", ""),
        ),
        leaf(
            "CondSaleOrderWeight",
            "Общий вес заказа",
            G_ORDER,
            LOGIC_NUM,
            number("value", "грамм"),
        ),
        leaf(
            "CondSalePersonType",
            "Тип плательщика",
            G_ORDER,
            LOGIC_EQ,
            FieldDef::multi(labeled(&d.person_types)),
        ),
        leaf(
            "CondSalePaySystem",
            "Платежная система",
            G_ORDER,
            LOGIC_EQ,
            FieldDef::multi(labeled(&d.pay_systems)),
        ),
        leaf(
            "CondSaleDelivery",
            "Служба доставки",
            G_ORDER,
            LOGIC_EQ,
            FieldDef::multi(labeled(&d.deliveries)),
        ),
        leaf(
            "CondSaleCmnDayOfWeek",
            "День недели",
            G_COMMON,
            LOGIC_EQ,
            FieldDef::multi(days),
        ),
        def(
            "ActSaleBsktGrp",
            "Изменить стоимость товаров в корзине",
            G_ACTIONS,
            vec![
                FieldDef::choice(
                    "Type",
                    "применить",
                    &[
                        ("Discount", "скидку"),
                        ("Extra", "наценку"),
                        ("Closeout", "фиксированную цену распродажи"),
                    ],
                ),
                number("Value", ""),
                FieldDef::choice(
                    "Unit",
                    "",
                    &[
                        ("Perc", "%"),
                        ("CurEach", "руб. на каждый товар"),
                        ("CurAll", "руб. на общую сумму товаров"),
                    ],
                ),
                number("Max", "но не более"),
                all("к товарам, для которых", "все условия", "любое из условий"),
                truth(),
            ],
        ),
        def(
            "ActSaleSubGrp",
            "Группа условий",
            G_ACTIONS,
            vec![all("", "все условия", "любое из условий"), truth()],
        ),
        def(
            "ActSaleDelivery",
            "Изменить стоимость доставки",
            G_ACTIONS,
            vec![
                FieldDef::choice(
                    "Type",
                    "применить к стоимости доставки",
                    &[
                        ("Discount", "скидку"),
                        ("DiscountZero", "скидку не более"),
                        ("Extra", "наценку"),
                    ],
                ),
                number("Value", ""),
                FieldDef::choice("Unit", "", &[("Perc", "%"), ("Cur", "валюта сайта")]),
            ],
        ),
    ];

    // Свойства: «Свойства инфоблока #NAME# [#ID#]» / «Свойство #NAME#»
    for (cid, cname) in &d.collections {
        for (fid, _, fname, kind) in d.fields.iter().filter(|f| f.1 == *cid) {
            let (logic_set, value) = match kind.as_str() {
                "number" => (LOGIC_NUM, number("value", "")),
                "date" => (LOGIC_NUM, text()),
                "string" | "text" => (LOGIC_STR, text()),
                "list" => (
                    LOGIC_EQ,
                    FieldDef::multi(
                        d.options
                            .iter()
                            .filter(|o| o.1 == *fid)
                            .map(|(id, _, v)| (id.to_string(), v.clone()))
                            .collect(),
                    ),
                ),
                "element" => (LOGIC_EQ, FieldDef::ids("items")),
                "boolean" => (LOGIC_EQ, yes_no()),
                _ => continue,
            };
            out.push(ClassDef {
                id: format!("CondIBProp:{cid}:{fid}"),
                label: format!("Свойство {fname}"),
                group: format!("Свойства коллекции {cname} [{cid}]"),
                fields: vec![logic(logic_set), value],
            });
        }
    }
    for hook in hooks {
        out.push(def(
            hook,
            hook,
            G_PROJECT,
            vec![FieldDef::new("*", "DATA", "json", json!({}))],
        ));
    }
    out
}

/// Какие классы можно добавить внутрь группы, по деревьям (`#root` — корень дерева).
/// Как `GetShowIn` контролов Битрикса; классы проекта — в любую группу условий.
fn children_map(classes: &[ClassDef], hooks: &[&str]) -> Value {
    let conditions: Vec<&str> = classes
        .iter()
        .map(|c| c.id.as_str())
        .filter(|id| !ACTION_CLASSES.contains(id) && !hooks.contains(id))
        .collect();
    let allowed = |parent: &str, root: bool, extra: &[&str]| -> Vec<String> {
        extra
            .iter()
            .copied()
            .chain(
                conditions
                    .iter()
                    .copied()
                    .filter(|c| allowed_in(parent, c, root)),
            )
            .chain(hooks.iter().copied())
            .map(str::to_string)
            .collect()
    };
    let mut cond = BTreeMap::new();
    cond.insert("#root".to_string(), allowed("CondGroup", true, &[]));
    for parent in CONDITION_CLASSES.iter().filter(|c| is_group(c)) {
        cond.insert(parent.to_string(), allowed(parent, false, &[]));
    }
    let mut act = BTreeMap::new();
    act.insert(
        "#root".to_string(),
        vec!["ActSaleBsktGrp".to_string(), "ActSaleDelivery".to_string()],
    );
    for parent in ["ActSaleBsktGrp", "ActSaleSubGrp"] {
        act.insert(
            parent.to_string(),
            allowed(parent, false, &["ActSaleSubGrp"]),
        );
    }
    json!({ "conditions": cond, "actions": act })
}

/// Id из `value` узла: число, числовая строка или список.
fn value_ids(v: Option<&Value>) -> Vec<i64> {
    let one = |v: &Value| match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(one).collect(),
        Some(v) => one(v).into_iter().collect(),
        None => Vec::new(),
    }
}

/// Названия записей и разделов, упомянутых в деревьях (подсказки у полей id).
async fn tree_names(db: &PgPool, classes: &[ClassDef], trees: &[&Node]) -> sqlx::Result<Value> {
    let lookups: HashMap<&str, &str> = classes
        .iter()
        .filter_map(|c| {
            c.fields
                .iter()
                .find_map(|f| f.lookup)
                .map(|l| (c.id.as_str(), l))
        })
        .collect();
    fn walk(node: &Node, lookups: &HashMap<&str, &str>, out: &mut HashMap<String, Vec<i64>>) {
        if let Some(l) = lookups.get(node.class_id.as_str()) {
            out.entry(l.to_string())
                .or_default()
                .extend(value_ids(node.data.get("value")));
        }
        for c in &node.children {
            walk(c, lookups, out);
        }
    }
    let mut ids = HashMap::new();
    for t in trees {
        walk(t, &lookups, &mut ids);
    }
    names(
        db,
        ids.remove("items").unwrap_or_default(),
        ids.remove("sections").unwrap_or_default(),
    )
    .await
}

async fn names(db: &PgPool, items: Vec<i64>, sections: Vec<i64>) -> sqlx::Result<Value> {
    let load = |sql: &'static str, ids: Vec<i64>| async move {
        let rows: Vec<(i64, String)> = sqlx::query_as(sql).bind(ids).fetch_all(db).await?;
        Ok::<_, sqlx::Error>(
            rows.into_iter()
                .map(|(id, name)| (id.to_string(), Value::from(name)))
                .collect::<serde_json::Map<_, _>>(),
        )
    };
    Ok(json!({
        "items": load("SELECT id, name FROM collection_items WHERE id = ANY($1)", items).await?,
        "sections": load("SELECT id, name FROM collection_sections WHERE id = ANY($1)", sections).await?,
    }))
}

/// `GET /admin/discounts/names?items=1,2&sections=3` — названия для подсказок конструктора.
pub async fn names_lookup(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(q): Query<HashMap<String, String>>,
) -> AppResult<Json<Value>> {
    user.require(DISCOUNTS_MANAGE)?;
    let ids = |key: &str| -> Vec<i64> {
        q.get(key)
            .map(|s| s.split(',').filter_map(|p| p.trim().parse().ok()).collect())
            .unwrap_or_default()
    };
    Ok(Json(names(&state.db, ids("items"), ids("sections")).await?))
}

// ---------- Список правил ----------

#[derive(sqlx::FromRow)]
struct ListRow {
    id: i64,
    name: String,
    active: bool,
    active_from: Option<DateTime<Utc>>,
    active_to: Option<DateTime<Utc>>,
    priority: i32,
    sort: i32,
    use_coupons: bool,
    unsupported: String,
    coupons: i64,
    groups: Vec<String>,
}

#[derive(Serialize)]
struct ListView {
    id: i64,
    name: String,
    active: bool,
    active_from: String,
    active_to: String,
    priority: i32,
    sort: i32,
    use_coupons: bool,
    unsupported: String,
    coupons: i64,
    groups: Vec<String>,
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require(DISCOUNTS_MANAGE)?;
    let rows: Vec<ListRow> = sqlx::query_as(
        "SELECT d.id, d.name, d.active, d.active_from, d.active_to, d.priority, d.sort,
                d.use_coupons, d.unsupported,
                (SELECT count(*) FROM discount_coupons c WHERE c.discount_id = d.id) AS coupons,
                ARRAY(SELECT g.name FROM discount_groups dg JOIN groups g ON g.id = dg.group_id
                      WHERE dg.discount_id = d.id ORDER BY g.sort, g.name) AS groups
         FROM discounts d ORDER BY d.priority DESC, d.sort, d.id",
    )
    .fetch_all(&state.db)
    .await?;
    let items: Vec<ListView> = rows
        .into_iter()
        .map(|r| ListView {
            id: r.id,
            name: r.name,
            active: r.active,
            active_from: shown_date(r.active_from),
            active_to: shown_date(r.active_to),
            priority: r.priority,
            sort: r.sort,
            use_coupons: r.use_coupons,
            unsupported: r.unsupported,
            coupons: r.coupons,
            groups: r.groups,
        })
        .collect();
    render(&state, "discounts.html", context! { user, items })
}

/// `POST /admin/discounts/{id}/active` с `active=true|false`.
pub async fn set_active(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(fields): Form<FormFields>,
) -> AppResult<Redirect> {
    user.require(DISCOUNTS_MANAGE)?;
    let active = field(&fields, "active") == "true";
    let done = sqlx::query("UPDATE discounts SET active = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(active)
        .execute(&state.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(Redirect::to("/admin/discounts"))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    user.require(DISCOUNTS_MANAGE)?;
    sqlx::query("DELETE FROM discounts WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to("/admin/discounts"))
}

// ---------- Форма правила ----------

#[derive(Serialize)]
struct RuleForm {
    name: String,
    active: bool,
    active_from: String,
    active_to: String,
    priority: String,
    sort: String,
    last_discount: bool,
    last_level_discount: bool,
    use_coupons: bool,
    groups: Vec<i64>,
    /// Деревья — как пришли (JSON).
    conditions: String,
    actions: String,
}

impl Default for RuleForm {
    fn default() -> Self {
        RuleForm {
            name: String::new(),
            active: true,
            active_from: String::new(),
            active_to: String::new(),
            priority: "1".into(),
            sort: "100".into(),
            last_discount: false,
            last_level_discount: false,
            use_coupons: false,
            groups: Vec::new(),
            conditions: DEFAULT_CONDITIONS.into(),
            actions: DEFAULT_ACTIONS.into(),
        }
    }
}

impl RuleForm {
    fn from_fields(fields: &FormFields) -> RuleForm {
        let tree = |key: &str, default: &str| match field(fields, key).trim() {
            "" => default.to_string(),
            raw => raw.to_string(),
        };
        RuleForm {
            name: field(fields, "name").trim().to_string(),
            active: checked(fields, "active"),
            active_from: field(fields, "active_from").trim().to_string(),
            active_to: field(fields, "active_to").trim().to_string(),
            priority: field(fields, "priority").trim().to_string(),
            sort: field(fields, "sort").trim().to_string(),
            last_discount: checked(fields, "last_discount"),
            last_level_discount: checked(fields, "last_level_discount"),
            use_coupons: checked(fields, "use_coupons"),
            groups: fields
                .iter()
                .filter(|(k, _)| k == "group")
                .filter_map(|(_, v)| v.parse().ok())
                .collect(),
            conditions: tree("conditions", DEFAULT_CONDITIONS),
            actions: tree("actions", DEFAULT_ACTIONS),
        }
    }
}

/// Ошибка у узла дерева.
#[derive(Serialize)]
struct TreeError {
    /// `conditions` | `actions`
    tree: &'static str,
    path: String,
    message: String,
}

struct RuleInput {
    name: String,
    active: bool,
    active_from: Option<DateTime<Utc>>,
    active_to: Option<DateTime<Utc>>,
    priority: i32,
    sort: i32,
    last_discount: bool,
    last_level_discount: bool,
    use_coupons: bool,
    groups: Vec<i64>,
    conditions: Node,
    actions: Node,
    unsupported: Vec<String>,
}

/// Классы не своего дерева: действия в условиях, условия в корне действий.
fn scope_errors(conditions: &Node, actions: &Node, errs: &mut Vec<TreeError>) {
    fn walk(node: &Node, path: &str, errs: &mut Vec<TreeError>) {
        if ACTION_CLASSES.contains(&node.class_id.as_str()) {
            errs.push(TreeError {
                tree: "conditions",
                path: path.to_string(),
                message: format!("«{}» нельзя использовать в условиях", node.class_id),
            });
        }
        for (i, c) in node.children.iter().enumerate() {
            walk(c, &format!("{path}.{i}"), errs);
        }
    }
    walk(conditions, "0", errs);
    for (i, c) in actions.children.iter().enumerate() {
        let class = c.class_id.as_str();
        let known = CONDITION_CLASSES.contains(&class)
            || ACTION_CLASSES.contains(&class)
            || class.starts_with("CondIBProp:");
        if known && !matches!(class, "ActSaleBsktGrp" | "ActSaleDelivery") {
            errs.push(TreeError {
                tree: "actions",
                path: format!("0.{i}"),
                message: format!("«{class}» нельзя использовать в действиях"),
            });
        }
    }
}

/// Проверяет форму. Неизвестные классы (перенесённые из Битрикса) не мешают сохранению —
/// правило сохраняется с `unsupported` и не применяется.
async fn check(
    state: &AppState,
    form: &RuleForm,
) -> AppResult<Result<RuleInput, (Vec<String>, Vec<TreeError>)>> {
    let mut errors = Vec::new();
    if form.name.is_empty() {
        errors.push("Укажите название".to_string());
    }
    let active_from = parse_date(&form.active_from, "Начало активности", &mut errors);
    let active_to = parse_date(&form.active_to, "Окончание активности", &mut errors);
    let priority = parse_int(&form.priority, 1, "Приоритет", &mut errors);
    let sort = parse_int(&form.sort, 100, "Сортировка", &mut errors);
    let conditions = serde_json::from_str::<Node>(&form.conditions);
    let actions = serde_json::from_str::<Node>(&form.actions);
    if conditions.is_err() {
        errors.push("Условия: неверный формат дерева".to_string());
    }
    if actions.is_err() {
        errors.push("Действия: неверный формат дерева".to_string());
    }
    let (Ok(conditions), Ok(actions)) = (conditions, actions) else {
        return Ok(Err((errors, Vec::new())));
    };
    let hooks = hook_classes(state);
    let unsupported = unsupported_classes_with(&conditions, &actions, hooks);
    let refs = Refs::load(&state.db, &[&conditions, &actions]).await?;
    let known_error = |(path, message): (String, String)| {
        let ignorable = unsupported
            .iter()
            .any(|c| message == format!("неизвестный элемент «{c}»"));
        (!ignorable).then_some((path, message))
    };
    let mut tree_errors: Vec<TreeError> = Vec::new();
    for (tree, node) in [("conditions", &conditions), ("actions", &actions)] {
        tree_errors.extend(
            validate_with(node, &refs, hooks)
                .into_iter()
                .filter_map(known_error)
                .map(|(path, message)| TreeError {
                    tree,
                    path,
                    message,
                }),
        );
    }
    scope_errors(&conditions, &actions, &mut tree_errors);
    let groups: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM groups WHERE id = ANY($1) ORDER BY id")
            .bind(&form.groups)
            .fetch_all(&state.db)
            .await?;
    if !errors.is_empty() || !tree_errors.is_empty() {
        return Ok(Err((errors, tree_errors)));
    }
    Ok(Ok(RuleInput {
        name: form.name.clone(),
        active: form.active,
        active_from,
        active_to,
        priority,
        sort,
        last_discount: form.last_discount,
        last_level_discount: form.last_level_discount,
        use_coupons: form.use_coupons,
        groups,
        conditions,
        actions,
        unsupported,
    }))
}

async fn save_rule(db: &PgPool, id: Option<i64>, r: &RuleInput) -> sqlx::Result<i64> {
    let mut tx = db.begin().await?;
    let conditions = serde_json::to_value(&r.conditions).unwrap_or_default();
    let actions = serde_json::to_value(&r.actions).unwrap_or_default();
    let q = match id {
        None => sqlx::query_scalar::<_, i64>(
            "INSERT INTO discounts (name, active, active_from, active_to, priority, sort,
                 last_discount, last_level_discount, use_coupons, conditions, actions, unsupported)
             VALUES ($2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING id",
        ),
        Some(_) => sqlx::query_scalar::<_, i64>(
            "UPDATE discounts SET name = $2, active = $3, active_from = $4, active_to = $5,
                 priority = $6, sort = $7, last_discount = $8, last_level_discount = $9,
                 use_coupons = $10, conditions = $11, actions = $12, unsupported = $13,
                 updated_at = now()
             WHERE id = $1 RETURNING id",
        ),
    };
    let id = q
        .bind(id.unwrap_or(0))
        .bind(&r.name)
        .bind(r.active)
        .bind(r.active_from)
        .bind(r.active_to)
        .bind(r.priority)
        .bind(r.sort)
        .bind(r.last_discount)
        .bind(r.last_level_discount)
        .bind(r.use_coupons)
        .bind(conditions)
        .bind(actions)
        .bind(r.unsupported.join(","))
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM discount_groups WHERE discount_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO discount_groups (discount_id, group_id) SELECT $1, unnest($2::bigint[])",
    )
    .bind(id)
    .bind(&r.groups)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

async fn render_form(
    state: &AppState,
    user: Access,
    rule_id: Option<i64>,
    form: RuleForm,
    errors: Vec<String>,
    tree_errors: Vec<TreeError>,
) -> AppResult<Html<String>> {
    let db = &state.db;
    let hooks = hook_classes(state);
    let classes = class_defs(&Dicts::load(db).await?, hooks);
    let trees: Vec<Node> = [&form.conditions, &form.actions]
        .iter()
        .filter_map(|t| serde_json::from_str(t).ok())
        .collect();
    let unsupported = match &trees[..] {
        [c, a] => unsupported_classes_with(c, a, hooks),
        _ => Vec::new(),
    };
    let names = tree_names(db, &classes, &trees.iter().collect::<Vec<_>>()).await?;
    let mut by_path: HashMap<&str, BTreeMap<&str, Vec<&str>>> = HashMap::new();
    for e in &tree_errors {
        by_path
            .entry(e.tree)
            .or_default()
            .entry(&e.path)
            .or_default()
            .push(&e.message);
    }
    let editor = json!({
        "classes": classes,
        "children": children_map(&classes, hooks),
        "errors": by_path,
        "names": names,
    })
    .to_string();
    let groups: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM groups ORDER BY sort, name")
            .fetch_all(db)
            .await?;
    let coupons: i64 = match rule_id {
        Some(id) => {
            sqlx::query_scalar("SELECT count(*) FROM discount_coupons WHERE discount_id = $1")
                .bind(id)
                .fetch_one(db)
                .await?
        }
        None => 0,
    };
    render(
        state,
        "discount_form.html",
        context! { user, rule_id, form, errors, tree_errors, unsupported, groups, coupons, editor },
    )
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require(DISCOUNTS_MANAGE)?;
    render_form(&state, user, None, RuleForm::default(), vec![], vec![]).await
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    name: String,
    active: bool,
    active_from: Option<DateTime<Utc>>,
    active_to: Option<DateTime<Utc>>,
    priority: i32,
    sort: i32,
    last_discount: bool,
    last_level_discount: bool,
    use_coupons: bool,
    conditions: sqlx::types::Json<Value>,
    actions: sqlx::types::Json<Value>,
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    user.require(DISCOUNTS_MANAGE)?;
    let r: RuleRow = sqlx::query_as(
        "SELECT name, active, active_from, active_to, priority, sort, last_discount,
                last_level_discount, use_coupons, conditions, actions
         FROM discounts WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let groups: Vec<i64> = sqlx::query_scalar(
        "SELECT group_id FROM discount_groups WHERE discount_id = $1 ORDER BY group_id",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let form = RuleForm {
        name: r.name,
        active: r.active,
        active_from: input_date(r.active_from),
        active_to: input_date(r.active_to),
        priority: r.priority.to_string(),
        sort: r.sort.to_string(),
        last_discount: r.last_discount,
        last_level_discount: r.last_level_discount,
        use_coupons: r.use_coupons,
        groups,
        conditions: r.conditions.0.to_string(),
        actions: r.actions.0.to_string(),
    };
    render_form(&state, user, Some(id), form, vec![], vec![]).await
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(fields): Form<FormFields>,
) -> AppResult<Response> {
    user.require(DISCOUNTS_MANAGE)?;
    save(state, user, None, fields).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(fields): Form<FormFields>,
) -> AppResult<Response> {
    user.require(DISCOUNTS_MANAGE)?;
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM discounts WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    exists.ok_or(AppError::NotFound)?;
    save(state, user, Some(id), fields).await
}

async fn save(
    state: AppState,
    user: Access,
    id: Option<i64>,
    fields: FormFields,
) -> AppResult<Response> {
    let form = RuleForm::from_fields(&fields);
    match check(&state, &form).await? {
        Ok(input) => {
            save_rule(&state.db, id, &input).await?;
            Ok(Redirect::to("/admin/discounts").into_response())
        }
        Err((errors, tree_errors)) => Ok(render_form(&state, user, id, form, errors, tree_errors)
            .await?
            .into_response()),
    }
}

// ---------- Купоны ----------

#[derive(sqlx::FromRow)]
struct CouponRow {
    id: i64,
    coupon: String,
    active: bool,
    active_from: Option<DateTime<Utc>>,
    active_to: Option<DateTime<Utc>>,
    kind: String,
    max_use: i32,
    use_count: i32,
    user_id: Option<i64>,
    login: Option<String>,
    description: String,
}

#[derive(Serialize)]
struct CouponView {
    id: i64,
    coupon: String,
    active: bool,
    active_from: String,
    active_to: String,
    kind: String,
    max_use: i32,
    use_count: i32,
    user_id: Option<i64>,
    login: Option<String>,
    description: String,
}

/// Поля добавления купона и генерации пачки (введённое сохраняется при ошибке).
#[derive(Serialize)]
struct CouponForm {
    coupon: String,
    count: String,
    prefix: String,
    kind: String,
    max_use: String,
    active_from: String,
    active_to: String,
    user_id: String,
    description: String,
}

impl CouponForm {
    fn empty() -> CouponForm {
        CouponForm {
            coupon: String::new(),
            count: "10".into(),
            prefix: String::new(),
            kind: CouponKind::Order.as_str().into(),
            max_use: "0".into(),
            active_from: String::new(),
            active_to: String::new(),
            user_id: String::new(),
            description: String::new(),
        }
    }

    fn from_fields(fields: &FormFields) -> CouponForm {
        let get = |k: &str| field(fields, k).trim().to_string();
        CouponForm {
            coupon: get("coupon"),
            count: get("count"),
            prefix: get("prefix"),
            kind: match get("kind").as_str() {
                "" => CouponKind::Order.as_str().into(),
                k => k.to_string(),
            },
            max_use: get("max_use"),
            active_from: get("active_from"),
            active_to: get("active_to"),
            user_id: get("user_id"),
            description: get("description"),
        }
    }
}

/// Общие для добавления и генерации поля купона.
struct CouponCommon {
    kind: CouponKind,
    max_use: i32,
    active_from: Option<DateTime<Utc>>,
    active_to: Option<DateTime<Utc>>,
}

fn parse_common(f: &CouponForm, errors: &mut Vec<String>) -> Option<CouponCommon> {
    let kind = CouponKind::parse(&f.kind);
    if kind.is_none() {
        errors.push("Неизвестный тип купона".into());
    }
    let max_use = match f.max_use.as_str() {
        "" => 0,
        raw => match raw.parse::<i32>() {
            Ok(n) if n >= 0 => n,
            _ => {
                errors.push("Лимит использований: целое число не меньше 0".into());
                0
            }
        },
    };
    let active_from = parse_date(&f.active_from, "Начало активности", errors);
    let active_to = parse_date(&f.active_to, "Окончание активности", errors);
    let kind = kind?;
    Some(CouponCommon {
        kind,
        // Лимит — только у многоразовых, как в Битриксе
        max_use: if kind == CouponKind::Multi {
            max_use
        } else {
            0
        },
        active_from,
        active_to,
    })
}

fn random_code() -> String {
    let mut code = String::with_capacity(CODE_LEN);
    while code.len() < CODE_LEN {
        // 252 = 36 × 7: без перекоса в пользу первых символов
        for b in rand::random::<[u8; 16]>() {
            if b < 252 && code.len() < CODE_LEN {
                code.push(CODE_CHARS[(b % 36) as usize] as char);
            }
        }
    }
    code
}

fn valid_code_chars(s: &str) -> bool {
    !s.chars().any(char::is_whitespace)
}

struct RuleHead {
    id: i64,
    name: String,
}

async fn rule_head(db: &PgPool, id: i64) -> AppResult<RuleHead> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM discounts WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(RuleHead {
        id,
        name: name.ok_or(AppError::NotFound)?,
    })
}

#[derive(Default)]
struct CouponErrors {
    add: Option<String>,
    generate: Option<String>,
    page: Option<String>,
}

async fn render_coupons(
    state: &AppState,
    user: Access,
    rule: RuleHead,
    add: CouponForm,
    generate: CouponForm,
    errors: CouponErrors,
) -> AppResult<Html<String>> {
    let rows: Vec<CouponRow> = sqlx::query_as(
        "SELECT c.id, c.coupon, c.active, c.active_from, c.active_to, c.kind, c.max_use,
                c.use_count, c.user_id, u.login, c.description
         FROM discount_coupons c LEFT JOIN users u ON u.id = c.user_id
         WHERE c.discount_id = $1 ORDER BY c.id DESC LIMIT $2",
    )
    .bind(rule.id)
    .bind(COUPONS_SHOWN)
    .fetch_all(&state.db)
    .await?;
    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM discount_coupons WHERE discount_id = $1")
            .bind(rule.id)
            .fetch_one(&state.db)
            .await?;
    let coupons: Vec<CouponView> = rows
        .into_iter()
        .map(|r| CouponView {
            id: r.id,
            coupon: r.coupon,
            active: r.active,
            active_from: shown_date(r.active_from),
            active_to: shown_date(r.active_to),
            kind: r.kind,
            max_use: r.max_use,
            use_count: r.use_count,
            user_id: r.user_id,
            login: r.login,
            description: r.description,
        })
        .collect();
    render(
        state,
        "discount_coupons.html",
        context! {
            user,
            rule_id => rule.id,
            rule_name => rule.name,
            coupons,
            total,
            add,
            generate,
            add_error => errors.add,
            generate_error => errors.generate,
            error => errors.page,
            kinds => vec![
                ("order", "на один заказ"),
                ("basket", "на одну позицию заказа"),
                ("multi", "многоразовый"),
            ],
        },
    )
}

pub async fn coupons(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    user.require(DISCOUNTS_MANAGE)?;
    let rule = rule_head(&state.db, id).await?;
    render_coupons(
        &state,
        user,
        rule,
        CouponForm::empty(),
        CouponForm::empty(),
        CouponErrors::default(),
    )
    .await
}

fn coupons_url(id: i64) -> String {
    format!("/admin/discounts/{id}/coupons")
}

/// Добавить один купон; пустой код — сгенерированный.
pub async fn add_coupon(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(fields): Form<FormFields>,
) -> AppResult<Response> {
    user.require(DISCOUNTS_MANAGE)?;
    let rule = rule_head(&state.db, id).await?;
    let form = CouponForm::from_fields(&fields);
    let mut errors = Vec::new();
    let common = parse_common(&form, &mut errors);
    let code = match form.coupon.as_str() {
        "" => random_code(),
        c => c.to_string(),
    };
    if code.chars().count() > MAX_CODE_LEN || !valid_code_chars(&code) {
        errors.push(format!("Код: до {MAX_CODE_LEN} символов, без пробелов"));
    }
    let user_id = match form.user_id.as_str() {
        "" => None,
        raw => match raw.parse::<i64>() {
            Ok(uid) => {
                let found: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
                    .bind(uid)
                    .fetch_optional(&state.db)
                    .await?;
                if found.is_none() {
                    errors.push(format!("Пользователь {uid} не найден"));
                }
                Some(uid)
            }
            Err(_) => {
                errors.push("ID пользователя: целое число".into());
                None
            }
        },
    };
    if let (Some(c), true) = (&common, errors.is_empty()) {
        let res = sqlx::query(
            "INSERT INTO discount_coupons
                 (discount_id, coupon, kind, max_use, active_from, active_to, user_id, description)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(&code)
        .bind(c.kind.as_str())
        .bind(c.max_use)
        .bind(c.active_from)
        .bind(c.active_to)
        .bind(user_id)
        .bind(&form.description)
        .execute(&state.db)
        .await;
        match res {
            Ok(_) => return Ok(Redirect::to(&coupons_url(id)).into_response()),
            Err(e) if is_unique_violation(&e) => errors.push("Купон с таким кодом уже есть".into()),
            Err(e) => return Err(e.into()),
        }
    }
    let errors = CouponErrors {
        add: Some(errors.join("; ")),
        ..Default::default()
    };
    Ok(
        render_coupons(&state, user, rule, form, CouponForm::empty(), errors)
            .await?
            .into_response(),
    )
}

/// Сгенерировать пачку купонов: префикс + 12 символов `A-Z0-9`, уникальные без учёта регистра.
pub async fn generate_coupons(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(fields): Form<FormFields>,
) -> AppResult<Response> {
    user.require(DISCOUNTS_MANAGE)?;
    let rule = rule_head(&state.db, id).await?;
    let form = CouponForm::from_fields(&fields);
    let mut errors = Vec::new();
    let count = match form.count.parse::<i64>() {
        Ok(n) if (1..=MAX_GENERATE).contains(&n) => n,
        _ => {
            errors.push(format!("Количество: от 1 до {MAX_GENERATE}"));
            0
        }
    };
    if form.prefix.chars().count() > MAX_PREFIX_LEN || !valid_code_chars(&form.prefix) {
        errors.push(format!(
            "Префикс: до {MAX_PREFIX_LEN} символов, без пробелов"
        ));
    }
    let common = parse_common(&form, &mut errors);
    if let (Some(c), true) = (common, errors.is_empty()) {
        insert_generated(&state.db, id, count, &form.prefix, &c).await?;
        return Ok(Redirect::to(&coupons_url(id)).into_response());
    }
    let errors = CouponErrors {
        generate: Some(errors.join("; ")),
        ..Default::default()
    };
    Ok(
        render_coupons(&state, user, rule, CouponForm::empty(), form, errors)
            .await?
            .into_response(),
    )
}

async fn insert_generated(
    db: &PgPool,
    discount_id: i64,
    count: i64,
    prefix: &str,
    c: &CouponCommon,
) -> AppResult<()> {
    let mut tx = db.begin().await?;
    let mut left = count as u64;
    // Совпадения с существующими кодами пропускаются и догенерируются
    for _ in 0..10 {
        let codes: HashSet<String> = (0..left)
            .map(|_| format!("{prefix}{}", random_code()))
            .collect();
        let codes: Vec<String> = codes.into_iter().collect();
        let done = sqlx::query(
            "INSERT INTO discount_coupons (discount_id, coupon, kind, max_use, active_from, active_to)
             SELECT $1, code, $3, $4, $5, $6 FROM unnest($2::text[]) AS code
             ON CONFLICT ((lower(coupon))) DO NOTHING",
        )
        .bind(discount_id)
        .bind(&codes)
        .bind(c.kind.as_str())
        .bind(c.max_use)
        .bind(c.active_from)
        .bind(c.active_to)
        .execute(&mut *tx)
        .await?;
        left -= done.rows_affected().min(left);
        if left == 0 {
            tx.commit().await?;
            return Ok(());
        }
    }
    Err(anyhow::anyhow!("не удалось подобрать уникальные коды купонов").into())
}

/// `POST /admin/discounts/{id}/coupons/{coupon}/active` с `active=true|false`.
pub async fn set_coupon_active(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path((id, coupon)): Path<(i64, i64)>,
    Form(fields): Form<FormFields>,
) -> AppResult<Redirect> {
    user.require(DISCOUNTS_MANAGE)?;
    let done =
        sqlx::query("UPDATE discount_coupons SET active = $3 WHERE id = $1 AND discount_id = $2")
            .bind(coupon)
            .bind(id)
            .bind(field(&fields, "active") == "true")
            .execute(&state.db)
            .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(Redirect::to(&coupons_url(id)))
}

/// Удалить купон — только неиспользованный (как в Битриксе: история заказов ссылается на код).
pub async fn delete_coupon(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path((id, coupon)): Path<(i64, i64)>,
) -> AppResult<Response> {
    user.require(DISCOUNTS_MANAGE)?;
    let use_count: i32 = sqlx::query_scalar(
        "SELECT use_count FROM discount_coupons WHERE id = $1 AND discount_id = $2",
    )
    .bind(coupon)
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    if use_count > 0 {
        let rule = rule_head(&state.db, id).await?;
        let errors = CouponErrors {
            page: Some("Использованный купон удалить нельзя — его можно выключить".into()),
            ..Default::default()
        };
        return Ok(render_coupons(
            &state,
            user,
            rule,
            CouponForm::empty(),
            CouponForm::empty(),
            errors,
        )
        .await?
        .into_response());
    }
    sqlx::query("DELETE FROM discount_coupons WHERE id = $1 AND use_count = 0")
        .bind(coupon)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to(&coupons_url(id)).into_response())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use crate::smoke_tests::{get, post_form};
    use crate::test_support::{admin_cookie, content_fixture, permitted_cookie, test_state};

    fn app(db: &PgPool) -> axum::Router {
        crate::app(test_state(db.clone()))
    }

    /// Кодирование значения формы (application/x-www-form-urlencoded).
    fn enc(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    (b as char).to_string()
                }
                b => format!("%{b:02X}"),
            })
            .collect()
    }

    fn body(pairs: &[(&str, &str)]) -> String {
        pairs
            .iter()
            .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
            .collect::<Vec<_>>()
            .join("&")
    }

    fn conditions() -> Value {
        json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
            {"CLASS_ID": "CondBsktAmtGroup", "DATA": {"logic": "EqGr", "Value": 1000, "All": "AND"}, "CHILDREN": []}
        ]})
    }

    fn actions(collection: i64, field: i64, option: i64) -> Value {
        json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": [{
            "CLASS_ID": "ActSaleBsktGrp",
            "DATA": {"Type": "Discount", "Value": 10, "Unit": "Perc", "Max": 0, "All": "AND", "True": "True"},
            "CHILDREN": [{"CLASS_ID": format!("CondIBProp:{collection}:{field}"), "DATA": {"logic": "Equal", "value": [option]}, "CHILDREN": []}]
        }]})
    }

    async fn rule(db: &PgPool, name: &str) -> i64 {
        sqlx::query_scalar("INSERT INTO discounts (name) VALUES ($1) RETURNING id")
            .bind(name)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn coupon(db: &PgPool, discount: i64, code: &str, use_count: i32) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO discount_coupons (discount_id, coupon, kind, use_count) VALUES ($1, $2, 'multi', $3) RETURNING id",
        )
        .bind(discount)
        .bind(code)
        .bind(use_count)
        .fetch_one(db)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn requires_discounts_permission(db: PgPool) {
        let app = app(&db);
        let id = rule(&db, "Правило").await;
        let cid = coupon(&db, id, "FREE", 0).await;
        let none = permitted_cookie(&db, "nobody", &["shop.manage"]).await;
        let manager = permitted_cookie(&db, "manager", &["discounts.manage"]).await;

        let gets = [
            "/admin/discounts".to_string(),
            "/admin/discounts/new".to_string(),
            format!("/admin/discounts/{id}"),
            format!("/admin/discounts/{id}/coupons"),
            "/admin/discounts/names?items=1".to_string(),
        ];
        for path in &gets {
            let (status, _) = get(&app, path, &none).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
            let (status, html) = get(&app, path, &manager).await;
            assert_eq!(status, StatusCode::OK, "{path}: {html}");
        }
        let posts = [
            ("/admin/discounts".to_string(), "name=x".to_string()),
            (format!("/admin/discounts/{id}"), "name=x".to_string()),
            (
                format!("/admin/discounts/{id}/active"),
                "active=false".to_string(),
            ),
            (
                format!("/admin/discounts/{id}/coupons"),
                "coupon=X1".to_string(),
            ),
            (
                format!("/admin/discounts/{id}/coupons/generate"),
                "count=1".to_string(),
            ),
            (
                format!("/admin/discounts/{id}/coupons/{cid}/active"),
                "active=false".to_string(),
            ),
            (
                format!("/admin/discounts/{id}/coupons/{cid}/delete"),
                String::new(),
            ),
            (format!("/admin/discounts/{id}/delete"), String::new()),
        ];
        for (path, form) in &posts {
            let (status, _) = post_form(&app, path, &none, form).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        }
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM discount_coupons")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 1);

        // пункт меню — только с правом
        let (_, home) = get(&app, "/admin", &manager).await;
        let item = "&#x2f;admin&#x2f;discounts";
        assert!(home.contains(item), "{home}");
        let (_, home) = get(&app, "/admin", &none).await;
        assert!(!home.contains(item));
    }

    #[sqlx::test]
    async fn creates_and_edits_rule_with_tree(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let acts = actions(c.collection_id, c.field_id, c.option_id);
        let group = c.group_id.to_string();
        let form = body(&[
            ("name", "Скидка 10%"),
            ("active", "on"),
            ("active_from", "2026-01-01T00:00"),
            ("active_to", ""),
            ("priority", "5"),
            ("sort", "200"),
            ("last_discount", "on"),
            ("use_coupons", "on"),
            ("group", &group),
            ("conditions", &conditions().to_string()),
            ("actions", &acts.to_string()),
        ]);
        let (status, html) = post_form(&app, "/admin/discounts", &cookie, &form).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
        let row: (
            i64,
            String,
            bool,
            i32,
            i32,
            bool,
            bool,
            bool,
            Value,
            Value,
            String,
        ) = sqlx::query_as(
            "SELECT id, name, active, priority, sort, last_discount, last_level_discount,
                        use_coupons, conditions, actions, unsupported FROM discounts",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let id = row.0;
        assert_eq!(
            (&row.1[..], row.2, row.3, row.4, row.5, row.6, row.7),
            ("Скидка 10%", true, 5, 200, true, false, true)
        );
        let parsed: crate::discount::model::Node = serde_json::from_value(row.9.clone()).unwrap();
        assert_eq!(
            parsed,
            serde_json::from_value::<crate::discount::model::Node>(acts.clone()).unwrap()
        );
        assert_eq!(row.8["CHILDREN"][0]["CLASS_ID"], "CondBsktAmtGroup");
        assert_eq!(row.10, "");
        let groups: Vec<i64> =
            sqlx::query_scalar("SELECT group_id FROM discount_groups WHERE discount_id = $1")
                .bind(id)
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(groups, vec![c.group_id]);
        let from: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT active_from FROM discounts WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(from.unwrap().to_rfc3339(), "2026-01-01T00:00:00+00:00");

        // форма правки показывает поля и дерево
        let (status, html) = get(&app, &format!("/admin/discounts/{id}"), &cookie).await;
        assert_eq!(status, StatusCode::OK);
        for s in [
            r#"value="Скидка 10%""#,
            r#"value="2026-01-01T00:00""#,
            r#"name="last_discount" checked"#,
            r#"name="use_coupons" checked"#,
            &format!(r#"value="{}" checked"#, c.group_id),
            &format!("CondIBProp:{}:{}", c.collection_id, c.field_id),
            "Изменить стоимость товаров в корзине",
            "Цвет",
            "/admin/static/discount-tree.js",
        ] {
            assert!(html.contains(s), "нет «{s}»");
        }
        // список
        let (_, list) = get(&app, "/admin/discounts", &cookie).await;
        assert!(
            list.contains("Скидка 10%") && list.contains("Редакторы"),
            "{list}"
        );

        // правка: без групп, другие условия
        let form = body(&[
            ("name", "Скидка 15%"),
            ("priority", "1"),
            ("sort", "100"),
            (
                "conditions",
                r#"{"CLASS_ID":"CondGroup","DATA":{"All":"OR","True":"True"},"CHILDREN":[]}"#,
            ),
            ("actions", &acts.to_string()),
        ]);
        let (status, html) =
            post_form(&app, &format!("/admin/discounts/{id}"), &cookie, &form).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
        let (name, active, all, groups): (String, bool, String, i64) = sqlx::query_as(
            "SELECT name, active, conditions->'DATA'->>'All',
                    (SELECT count(*) FROM discount_groups WHERE discount_id = d.id)
             FROM discounts d WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(
            (&name[..], active, &all[..], groups),
            ("Скидка 15%", false, "OR", 0)
        );
    }

    #[sqlx::test]
    async fn node_error_keeps_input(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let acts = actions(c.collection_id, c.field_id, 999_999);
        let form = body(&[
            ("name", "Ошибочное"),
            ("priority", "3"),
            ("conditions", &conditions().to_string()),
            ("actions", &acts.to_string()),
        ]);
        let (status, html) = post_form(&app, "/admin/discounts", &cookie, &form).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(r#"class="alert""#), "{html}");
        assert!(html.contains("0.0.0"), "{html}");
        assert!(html.contains("вариант 999999 не найден"), "{html}");
        assert!(html.contains(r#"value="Ошибочное""#));
        assert!(
            html.contains(r#"name="priority" type="number" value="3""#),
            "{html}"
        );
        // дерево возвращается как прислано (экранированным в атрибуте)
        assert!(html.contains("999999"));
        assert!(html.contains("&quot;CondBsktAmtGroup&quot;"), "{html}");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM discounts")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);

        // испорченный JSON — ошибка, введённое на месте
        let form = body(&[
            ("name", "Ошибочное"),
            ("conditions", "{not json"),
            ("actions", &acts.to_string()),
        ]);
        let (status, html) = post_form(&app, "/admin/discounts", &cookie, &form).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Условия: неверный формат"), "{html}");
        assert!(html.contains(r#"value="{not json""#), "{html}");

        // действие в условиях и условие в корне действий
        let form = body(&[
            ("name", "Ошибочное"),
            (
                "conditions",
                &json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
                    {"CLASS_ID": "ActSaleDelivery", "DATA": {"Type": "Discount", "Value": 1, "Unit": "Perc"}}
                ]})
                .to_string(),
            ),
            (
                "actions",
                &json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": [
                    {"CLASS_ID": "CondSaleOrderSumm", "DATA": {"logic": "Great", "value": 1}}
                ]})
                .to_string(),
            ),
        ]);
        let (status, html) = post_form(&app, "/admin/discounts", &cookie, &form).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("«ActSaleDelivery» нельзя использовать в условиях"),
            "{html}"
        );
        assert!(
            html.contains("«CondSaleOrderSumm» нельзя использовать в действиях"),
            "{html}"
        );
    }

    #[sqlx::test]
    async fn unsupported_is_saved_and_warned(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let conds = json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
            {"CLASS_ID": "CondCumulativeGroup", "DATA": {"logic": "EqGr", "Value": 1000}, "CHILDREN": []}
        ]});
        let acts = json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": [
            {"CLASS_ID": "ActSaleDelivery", "DATA": {"Type": "DiscountZero", "Value": 0, "Unit": "Perc"}}
        ]});
        let form = body(&[
            ("name", "Накопительная"),
            ("active", "on"),
            ("conditions", &conds.to_string()),
            ("actions", &acts.to_string()),
        ]);
        let (status, html) = post_form(&app, "/admin/discounts", &cookie, &form).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
        let (id, unsupported, stored): (i64, String, Value) =
            sqlx::query_as("SELECT id, unsupported, conditions FROM discounts")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(unsupported, "CondCumulativeGroup");
        assert_eq!(stored["CHILDREN"][0]["DATA"]["Value"], 1000);
        let (_, list) = get(&app, "/admin/discounts", &cookie).await;
        assert!(list.contains("не применяется"), "{list}");
        assert!(list.contains("CondCumulativeGroup"), "{list}");
        let (_, html) = get(&app, &format!("/admin/discounts/{id}"), &cookie).await;
        assert!(html.contains("не применяется") && html.contains("CondCumulativeGroup"));
    }

    #[sqlx::test]
    async fn toggles_activity_from_list(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let id = rule(&db, "Правило").await;
        let active = || async {
            sqlx::query_scalar::<_, bool>("SELECT active FROM discounts WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap()
        };
        let path = format!("/admin/discounts/{id}/active");
        let (status, _) = post_form(&app, &path, &cookie, "active=false").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(!active().await);
        let (_, list) = get(&app, "/admin/discounts", &cookie).await;
        assert!(list.contains(&path), "{list}");
        let (status, _) = post_form(&app, &path, &cookie, "active=true").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(active().await);
        let (status, _) = post_form(
            &app,
            "/admin/discounts/999999/active",
            &cookie,
            "active=true",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn generates_coupons(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let id = rule(&db, "По купону").await;
        let path = format!("/admin/discounts/{id}/coupons/generate");
        let (status, html) = post_form(
            &app,
            &path,
            &cookie,
            "count=300&prefix=SALE-&kind=multi&max_use=3&active_to=2027-01-01T00:00",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
        let rows: Vec<(String, String, i32, Option<chrono::DateTime<chrono::Utc>>)> =
            sqlx::query_as(
                "SELECT coupon, kind, max_use, active_to FROM discount_coupons WHERE discount_id = $1",
            )
            .bind(id)
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(rows.len(), 300);
        let mut codes = std::collections::HashSet::new();
        for (code, kind, max_use, to) in &rows {
            let tail = code.strip_prefix("SALE-").expect(code);
            assert_eq!(tail.len(), 12, "{code}");
            assert!(
                tail.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
                "{code}"
            );
            assert_eq!((&kind[..], *max_use), ("multi", 3));
            assert!(to.is_some());
            assert!(codes.insert(code.to_lowercase()), "повтор {code}");
        }
        // одноразовые — без лимита
        let (status, _) = post_form(&app, &path, &cookie, "count=2&kind=order&max_use=5").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let (n, max): (i64, i32) = sqlx::query_as(
            "SELECT count(*), max(max_use) FROM discount_coupons WHERE kind = 'order'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!((n, max), (2, 0));

        for bad in [
            "count=0",
            "count=10001",
            "count=abc",
            "count=1&kind=bogus",
            "count=1&prefix=A%20B",
        ] {
            let (status, html) =
                post_form(&app, &path, &cookie, &format!("{bad}&prefix_keep=1")).await;
            assert_eq!(status, StatusCode::OK, "{bad}");
            assert!(html.contains(r#"class="alert""#), "{bad}: {html}");
        }
        let (status, html) = post_form(&app, &path, &cookie, "count=0&prefix=KEEP").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(r#"value="KEEP""#), "{html}");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM discount_coupons")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 302);

        // список купонов
        let (status, html) = get(&app, &format!("/admin/discounts/{id}/coupons"), &cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(&rows[0].0), "{html}");
    }

    #[sqlx::test]
    async fn adds_coupon_unique_case_insensitive(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let id = rule(&db, "По купону").await;
        let path = format!("/admin/discounts/{id}/coupons");
        let (status, html) = post_form(
            &app,
            &path,
            &cookie,
            "coupon=Summer2026&kind=order&description=%D0%9B%D0%B5%D1%82%D0%BE",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
        let (status, html) = post_form(&app, &path, &cookie, "coupon=SUMMER2026&kind=order").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Купон с таким кодом уже есть"), "{html}");
        assert!(html.contains(r#"value="SUMMER2026""#), "{html}");
        // пустой код — сгенерированный
        let (status, _) = post_form(&app, &path, &cookie, "coupon=&kind=basket").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let codes: Vec<(String, String)> =
            sqlx::query_as("SELECT coupon, description FROM discount_coupons ORDER BY id")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(codes.len(), 2);
        assert_eq!(codes[0], ("Summer2026".into(), "Лето".into()));
        assert_eq!(codes[1].0.len(), 12);
    }

    #[sqlx::test]
    async fn used_coupon_cannot_be_deleted(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let id = rule(&db, "По купону").await;
        let used = coupon(&db, id, "USED", 1).await;
        let fresh = coupon(&db, id, "FRESH", 0).await;
        let other = rule(&db, "Другое").await;

        let (status, html) = post_form(
            &app,
            &format!("/admin/discounts/{id}/coupons/{used}/delete"),
            &cookie,
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Использованный купон удалить нельзя"),
            "{html}"
        );
        // чужое правило в пути — не найдено
        let (status, _) = post_form(
            &app,
            &format!("/admin/discounts/{other}/coupons/{fresh}/delete"),
            &cookie,
            "",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = post_form(
            &app,
            &format!("/admin/discounts/{id}/coupons/{fresh}/delete"),
            &cookie,
            "",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let left: Vec<String> = sqlx::query_scalar("SELECT coupon FROM discount_coupons")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(left, vec!["USED"]);

        // выключение
        let (status, _) = post_form(
            &app,
            &format!("/admin/discounts/{id}/coupons/{used}/active"),
            &cookie,
            "active=false",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let active: bool = sqlx::query_scalar("SELECT active FROM discount_coupons WHERE id = $1")
            .bind(used)
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(!active);
        let (_, html) = get(&app, &format!("/admin/discounts/{id}/coupons"), &cookie).await;
        assert!(!html.contains(&format!("/coupons/{used}/delete")), "{html}");
    }

    #[sqlx::test]
    async fn names_are_escaped(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let id = rule(&db, "<script>alert(1)</script>").await;
        coupon(&db, id, "<b>X</b>", 0).await;
        for path in [
            "/admin/discounts".to_string(),
            format!("/admin/discounts/{id}"),
            format!("/admin/discounts/{id}/coupons"),
        ] {
            let (status, html) = get(&app, &path, &cookie).await;
            assert_eq!(status, StatusCode::OK);
            assert!(!html.contains("<script>alert(1)"), "{path}: {html}");
            assert!(
                html.contains("&lt;script&gt;alert(1)&lt;"),
                "{path}: {html}"
            );
            assert!(!html.contains("<b>X</b>"), "{path}");
        }
    }

    #[sqlx::test]
    async fn names_lookup_for_items_and_sections(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (status, body) = get(
            &app,
            &format!(
                "/admin/discounts/names?items={},999&sections={}",
                c.item_id, c.section_id
            ),
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["items"][c.item_id.to_string()], "Первая новость");
        assert!(v["items"].get("999").is_none());
        assert_eq!(v["sections"][c.section_id.to_string()], "Раздел А");
    }
}
