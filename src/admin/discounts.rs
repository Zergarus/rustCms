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
    discount::{
        model::{
            ACTION_CLASSES, CONDITION_CLASSES, CouponKind, Node, Refs, allowed_in, is_group,
            unsupported_classes_with, validate_with,
        },
        msk,
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

/// Даты в форме и списках — по Москве, как в Битриксе и движке скидок.
fn input_date(dt: Option<DateTime<Utc>>) -> String {
    dt.map(|d| d.with_timezone(&msk()).format(DATETIME_FORMAT).to_string())
        .unwrap_or_default()
}

fn shown_date(dt: Option<DateTime<Utc>>) -> String {
    dt.map(|d| d.with_timezone(&msk()).format("%d.%m.%Y %H:%M").to_string())
        .unwrap_or_default()
}

fn parse_date(raw: &str, label: &str, errors: &mut Vec<String>) -> Option<DateTime<Utc>> {
    match raw.trim() {
        "" => None,
        raw => match NaiveDateTime::parse_from_str(raw, DATETIME_FORMAT) {
            Ok(dt) => dt
                .and_local_timezone(msk())
                .single()
                .map(|d| d.with_timezone(&Utc)),
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
    /// `ids` — id через запятую с подсказкой названий (`lookup`), `json` — весь `DATA`,
    /// `datetime` — дата и время по Москве в unix-секундах (пусто — ключа нет),
    /// `ranges` — таблица порогов `[{sum, value, type}]`, `const` — значение без поля.
    /// Ключ с точкой — вложенный: `sum_period_data.discount_sum_period_type`.
    kind: &'static str,
    options: Vec<(String, String)>,
    /// `null` — ключ в новый узел не пишется.
    default: Value,
    lookup: Option<&'static str>,
    /// У `ids`: одно значение вместо списка (раздел-подарок).
    single: bool,
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
            single: false,
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
        FieldDef::ids_at("value", lookup)
    }

    fn ids_at(key: &'static str, lookup: &'static str) -> Self {
        FieldDef {
            lookup: Some(lookup),
            ..FieldDef::new(key, "", "ids", json!([]))
        }
    }

    /// Одно id (`Value` раздела-подарка).
    fn id_at(key: &'static str, lookup: &'static str) -> Self {
        FieldDef {
            single: true,
            default: json!(""),
            ..FieldDef::ids_at(key, lookup)
        }
    }

    fn datetime(key: &'static str, label: &'static str) -> Self {
        FieldDef::new(key, label, "datetime", Value::Null)
    }

    fn fixed(key: &'static str, value: &str) -> Self {
        FieldDef::new(key, "", "const", Value::from(value))
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
const G_CUMULATIVE: &str = "Накопительные скидки";
const G_GIFTS: &str = "Подарки";
const G_PROJECT: &str = "Условия проекта";
const ROW_ALL: (&str, &str) = ("всем условиям", "любому из условий");
const PERIOD_UNITS: &[(&str, &str)] = &[("D", "Дней"), ("M", "Месяцев"), ("Y", "Лет")];
const GIFT_TYPES: &[(&str, &str)] = &[("one", "один из"), ("all", "все")];

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
        // `CSaleCondCumulativeCtrl`: сумма оплаченных заказов, период — вложенным узлом
        def(
            "CondCumulativeGroup",
            "Накопительная сумма заказов",
            G_ORDER,
            vec![logic(LOGIC_NUM), number("Value", "")],
        ),
        def(
            "Period",
            "Период: с … по …",
            G_ORDER,
            vec![
                FieldDef::datetime("ValueStart", "с"),
                FieldDef::datetime("ValueEnd", "по"),
            ],
        ),
        def(
            "PeriodRelative",
            "Период: последние N …",
            G_ORDER,
            vec![
                number("Value", "последние"),
                FieldDef::choice("TypeRelativePeriod", "", PERIOD_UNITS),
            ],
        ),
        // `CSaleCumulativeAction` — как пресет «Накопительная скидка»
        def(
            "CumulativeAction",
            "Накопительная скидка",
            G_CUMULATIVE,
            vec![
                FieldDef::new("ranges", "пороги", "ranges", json!([])),
                FieldDef::choice(
                    "type_sum_period",
                    "сумма заказов",
                    &[
                        ("all_time", "за всё время"),
                        ("interval", "за период"),
                        ("relative", "за последние"),
                    ],
                ),
                FieldDef::datetime("sum_period_data.discount_sum_order_start", "от"),
                FieldDef::datetime("sum_period_data.discount_sum_order_end", "до"),
                FieldDef::new(
                    "sum_period_data.discount_sum_period_value",
                    "последние",
                    "number",
                    json!(""),
                ),
                FieldDef::choice("sum_period_data.discount_sum_period_type", "", PERIOD_UNITS),
                FieldDef::choice(
                    "apply_if_more_profitable",
                    "только если выгоднее",
                    &[("N", "нет"), ("Y", "да")],
                ),
                all("к товарам, для которых", "все условия", "любое из условий"),
                truth(),
            ],
        ),
        // `CSaleActionGiftCtrlGroup` с одним `CCatalogGifterProduct`
        def(
            "GiftCondGroup",
            "Подарок",
            G_GIFTS,
            vec![FieldDef::fixed("All", "AND")],
        ),
        def(
            "GifterCondIBElement",
            "Товары",
            G_GIFTS,
            vec![
                FieldDef::choice("Type", "", GIFT_TYPES),
                FieldDef::ids_at("Value", "items"),
            ],
        ),
        def(
            "GifterCondIBSection",
            "Раздел",
            G_GIFTS,
            vec![
                FieldDef::choice("Type", "", GIFT_TYPES),
                FieldDef::id_at("Value", "sections"),
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

/// Действия корня дерева действий.
const ROOT_ACTIONS: &[&str] = &[
    "ActSaleBsktGrp",
    "ActSaleDelivery",
    "CumulativeAction",
    "GiftCondGroup",
];

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
        ROOT_ACTIONS.iter().map(|c| c.to_string()).collect(),
    );
    for parent in ["ActSaleBsktGrp", "ActSaleSubGrp"] {
        act.insert(
            parent.to_string(),
            allowed(parent, false, &["ActSaleSubGrp"]),
        );
    }
    act.insert(
        "CumulativeAction".to_string(),
        allowed("CumulativeAction", false, &[]),
    );
    act.insert(
        "GiftCondGroup".to_string(),
        allowed(
            "GiftCondGroup",
            false,
            &["GifterCondIBElement", "GifterCondIBSection"],
        ),
    );
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
    let lookups: HashMap<&str, (&str, &str)> = classes
        .iter()
        .filter_map(|c| {
            c.fields
                .iter()
                .find_map(|f| f.lookup.map(|l| (f.key, l)))
                .map(|kl| (c.id.as_str(), kl))
        })
        .collect();
    fn walk(
        node: &Node,
        lookups: &HashMap<&str, (&str, &str)>,
        out: &mut HashMap<String, Vec<i64>>,
    ) {
        if let Some((key, l)) = lookups.get(node.class_id.as_str()) {
            out.entry(l.to_string())
                .or_default()
                .extend(value_ids(node.data.get(*key)));
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
    let deleted = sqlx::query("DELETE FROM discounts WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
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
        if known && !ROOT_ACTIONS.contains(&class) {
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

async fn save_rule(db: &PgPool, id: Option<i64>, r: &RuleInput) -> AppResult<i64> {
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
        .fetch_optional(&mut *tx)
        .await?
        // Правило удалили, пока его правили
        .ok_or(AppError::NotFound)?;
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

// ---------- Пресет «Накопительная скидка» ----------

/// Строка порога, как введена.
#[derive(Serialize, Clone)]
struct RangeRow {
    sum: String,
    value: String,
    /// `P` | `F`
    kind: String,
}

impl RangeRow {
    fn empty() -> RangeRow {
        RangeRow {
            sum: String::new(),
            value: String::new(),
            kind: "P".into(),
        }
    }
}

/// Форма пресета (`Handlers\DiscountPreset\Cumulative`): введённое сохраняется при ошибке.
#[derive(Serialize)]
struct CumulativeForm {
    name: String,
    active: bool,
    groups: Vec<i64>,
    ranges: Vec<RangeRow>,
    /// `all_time` | `interval` | `relative`
    type_sum_period: String,
    sum_order_start: String,
    sum_order_end: String,
    sum_period_value: String,
    sum_period_type: String,
    apply_if_more_profitable: bool,
    skip_discounted: bool,
}

/// Пустых строк порогов в новой форме (у Битрикса — четыре).
const EMPTY_RANGE_ROWS: usize = 4;

impl Default for CumulativeForm {
    fn default() -> Self {
        CumulativeForm {
            name: String::new(),
            active: true,
            groups: Vec::new(),
            ranges: vec![RangeRow::empty(); EMPTY_RANGE_ROWS],
            type_sum_period: "all_time".into(),
            sum_order_start: String::new(),
            sum_order_end: String::new(),
            sum_period_value: String::new(),
            sum_period_type: "Y".into(),
            apply_if_more_profitable: false,
            skip_discounted: false,
        }
    }
}

impl CumulativeForm {
    fn from_fields(fields: &FormFields) -> CumulativeForm {
        let all = |key: &str| -> Vec<&str> {
            fields
                .iter()
                .filter(|(k, _)| k == key)
                .map(|(_, v)| v.trim())
                .collect()
        };
        let (sums, values, kinds) = (all("range_sum"), all("range_value"), all("range_type"));
        let mut ranges: Vec<RangeRow> = sums
            .iter()
            .enumerate()
            .map(|(i, sum)| RangeRow {
                sum: sum.to_string(),
                value: values.get(i).copied().unwrap_or("").to_string(),
                kind: match kinds.get(i).copied() {
                    Some("F") => "F".into(),
                    _ => "P".into(),
                },
            })
            .collect();
        if ranges.is_empty() {
            ranges = vec![RangeRow::empty(); EMPTY_RANGE_ROWS];
        }
        let get = |k: &str| field(fields, k).trim().to_string();
        CumulativeForm {
            name: get("name"),
            active: checked(fields, "active"),
            groups: fields
                .iter()
                .filter(|(k, _)| k == "group")
                .filter_map(|(_, v)| v.parse().ok())
                .collect(),
            ranges,
            type_sum_period: get("type_sum_period"),
            sum_order_start: get("sum_order_start"),
            sum_order_end: get("sum_order_end"),
            sum_period_value: get("sum_period_value"),
            sum_period_type: get("sum_period_type"),
            apply_if_more_profitable: field(fields, "apply_if_more_profitable") == "Y",
            skip_discounted: field(fields, "skip_discounted") == "Y",
        }
    }
}

/// Число формы: запятая — как точка (`str_replace(",", ".")` пресета).
fn parse_amount(raw: &str) -> Option<f64> {
    raw.replace(',', ".")
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
}

/// Число в `DATA`: целое — без дробной части, как значения из Битрикса.
fn json_number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

/// Пороги как `buildRangesFromState`: пустые строки пропускаются, величина > 0 (процент —
/// не больше 100), сортировка по сумме, без повторов границ.
fn cumulative_ranges(rows: &[RangeRow], errors: &mut Vec<String>) -> Vec<(f64, f64, String)> {
    const BAD_VALUE: &str = "Неверное значение величины скидки для диапазона";
    let mut out: Vec<(f64, f64, String)> = Vec::new();
    for r in rows {
        if r.sum.is_empty() && r.value.is_empty() {
            continue;
        }
        let Some(sum) = parse_amount(&r.sum).filter(|s| *s >= 0.0) else {
            errors.push("Неверное значение границы диапазона".into());
            return Vec::new();
        };
        match parse_amount(&r.value) {
            Some(v) if v > 0.0 && !(r.kind == "P" && v > 100.0) => {
                out.push((sum, v, r.kind.clone()))
            }
            _ => {
                errors.push(BAD_VALUE.into());
                return Vec::new();
            }
        }
    }
    if out.is_empty() {
        errors.push("Не указаны величины скидок".into());
        return out;
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    if out.windows(2).any(|w| w[0].0 == w[1].0) {
        errors.push("Дублируются границы диапазонов".into());
        return Vec::new();
    }
    out
}

/// Дерево действий пресета (`Cumulative::generateDiscount`); незаданные поля периода — пустые.
fn cumulative_actions(form: &CumulativeForm, errors: &mut Vec<String>) -> Option<Node> {
    let ranges = cumulative_ranges(&form.ranges, errors);
    let empty = || Value::from("");
    let mut period = json!({
        "discount_sum_order_start": "",
        "discount_sum_order_end": "",
        "discount_sum_period_value": "",
        "discount_sum_period_type": "",
    });
    match form.type_sum_period.as_str() {
        "all_time" => {}
        "interval" => {
            let ts = |d: Option<DateTime<Utc>>| d.map_or_else(empty, |d| d.timestamp().into());
            period["discount_sum_order_start"] =
                ts(parse_date(&form.sum_order_start, "Период: от", errors));
            period["discount_sum_order_end"] =
                ts(parse_date(&form.sum_order_end, "Период: до", errors));
        }
        "relative" => {
            match form.sum_period_value.parse::<i64>() {
                Ok(n) if n > 0 => period["discount_sum_period_value"] = n.into(),
                _ => errors.push("За последние: целое число больше 0".into()),
            }
            match form.sum_period_type.as_str() {
                t @ ("D" | "M" | "Y") => period["discount_sum_period_type"] = t.into(),
                _ => errors.push("За последние: неизвестная единица периода".into()),
            }
        }
        _ => errors.push("Неизвестный период для суммирования заказов".into()),
    }
    if !errors.is_empty() {
        return None;
    }
    let ranges: Vec<Value> = ranges
        .into_iter()
        .map(|(sum, value, kind)| {
            json!({"sum": json_number(sum), "value": json_number(value), "type": kind})
        })
        .collect();
    let filter: Vec<Value> = if form.skip_discounted {
        vec![json!({
            "CLASS_ID": "CondBsktAppliedDiscount",
            "DATA": {"logic": "Equal", "value": "N"},
            "CHILDREN": [],
        })]
    } else {
        Vec::new()
    };
    let tree = json!({
        "CLASS_ID": "CondGroup",
        "DATA": {"All": "AND"},
        "CHILDREN": [{
            "CLASS_ID": "CumulativeAction",
            "DATA": {
                "ranges": ranges,
                "type_sum_period": form.type_sum_period,
                "sum_period_data": period,
                "apply_if_more_profitable": if form.apply_if_more_profitable { "Y" } else { "N" },
                "All": "AND",
                "True": "True",
            },
            "CHILDREN": filter,
        }],
    });
    serde_json::from_value(tree).ok()
}

async fn render_cumulative(
    state: &AppState,
    user: Access,
    form: CumulativeForm,
    errors: Vec<String>,
) -> AppResult<Html<String>> {
    let groups: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM groups ORDER BY sort, name")
            .fetch_all(&state.db)
            .await?;
    render(
        state,
        "discount_cumulative.html",
        context! {
            user,
            form,
            errors,
            groups,
            periods => vec![
                ("all_time", "За все время"),
                ("interval", "За период"),
                ("relative", "За последние"),
            ],
            units => vec![("Y", "год"), ("M", "месяц"), ("D", "день")],
            kinds => vec![("P", "В процентах"), ("F", "Фиксированная сумма")],
        },
    )
}

/// `GET /admin/discounts/cumulative` — форма пресета «Накопительная скидка».
pub async fn cumulative_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require(DISCOUNTS_MANAGE)?;
    render_cumulative(&state, user, CumulativeForm::default(), vec![]).await
}

/// `POST /admin/discounts/cumulative` — правило из пресета: пустые условия и одно действие
/// `CumulativeAction`; дальше его можно править в обычной форме.
pub async fn cumulative_create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(fields): Form<FormFields>,
) -> AppResult<Response> {
    user.require(DISCOUNTS_MANAGE)?;
    let form = CumulativeForm::from_fields(&fields);
    let mut errors = Vec::new();
    if form.name.is_empty() {
        errors.push("Укажите название".to_string());
    }
    let actions = cumulative_actions(&form, &mut errors);
    let conditions: Node = serde_json::from_str(DEFAULT_CONDITIONS).expect("пустые условия");
    if let Some(actions) = &actions {
        // Собранное дерево проверяется как любое другое
        let refs = Refs::load(&state.db, &[&conditions, actions]).await?;
        errors.extend(
            validate_with(actions, &refs, &[])
                .into_iter()
                .map(|(path, m)| format!("Действия, элемент {path}: {m}")),
        );
    }
    let (Some(actions), true) = (actions, errors.is_empty()) else {
        return Ok(render_cumulative(&state, user, form, errors)
            .await?
            .into_response());
    };
    let groups: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM groups WHERE id = ANY($1) ORDER BY id")
            .bind(&form.groups)
            .fetch_all(&state.db)
            .await?;
    let input = RuleInput {
        name: form.name,
        active: form.active,
        active_from: None,
        active_to: None,
        priority: 1,
        sort: 100,
        last_discount: false,
        last_level_discount: false,
        use_coupons: false,
        groups,
        conditions,
        actions,
        unsupported: Vec::new(),
    };
    save_rule(&state.db, None, &input).await?;
    Ok(Redirect::to("/admin/discounts").into_response())
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
            "/admin/discounts/cumulative".to_string(),
        ];
        for path in &gets {
            let (status, _) = get(&app, path, &none).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
            let (status, html) = get(&app, path, &manager).await;
            assert_eq!(status, StatusCode::OK, "{path}: {html}");
        }
        let posts = [
            ("/admin/discounts".to_string(), "name=x".to_string()),
            (
                "/admin/discounts/cumulative".to_string(),
                "name=x".to_string(),
            ),
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
        let item = "href=\"/admin/discounts\"";
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
            // Время — московское, как у импорта из Битрикса и движка
            ("active_from", "2026-10-20T23:59"),
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
        assert_eq!(from.unwrap().to_rfc3339(), "2026-10-20T20:59:00+00:00");
        let (_, list) = get(&app, "/admin/discounts", &cookie).await;
        assert!(list.contains("20.10.2026 23:59"), "{list}");

        // форма правки показывает поля и дерево
        let (status, html) = get(&app, &format!("/admin/discounts/{id}"), &cookie).await;
        assert_eq!(status, StatusCode::OK);
        for s in [
            r#"value="Скидка 10%""#,
            r#"value="2026-10-20T23:59""#,
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
            {"CLASS_ID": "CondFooGroup", "DATA": {"logic": "EqGr", "Value": 1000}, "CHILDREN": []}
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
        assert_eq!(unsupported, "CondFooGroup");
        assert_eq!(stored["CHILDREN"][0]["DATA"]["Value"], 1000);
        let (_, list) = get(&app, "/admin/discounts", &cookie).await;
        assert!(list.contains("не применяется"), "{list}");
        assert!(list.contains("CondFooGroup"), "{list}");
        let (_, html) = get(&app, &format!("/admin/discounts/{id}"), &cookie).await;
        assert!(html.contains("не применяется") && html.contains("CondFooGroup"));
    }

    /// Правило удалили, пока его правили, — 404, а не 500.
    #[sqlx::test]
    async fn save_of_missing_rule_is_not_found(db: PgPool) {
        let tree = |raw: &str| serde_json::from_str::<crate::discount::model::Node>(raw).unwrap();
        let input = super::RuleInput {
            name: "Нет такого".into(),
            active: true,
            active_from: None,
            active_to: None,
            priority: 1,
            sort: 100,
            last_discount: false,
            last_level_discount: false,
            use_coupons: false,
            groups: Vec::new(),
            conditions: tree(super::DEFAULT_CONDITIONS),
            actions: tree(super::DEFAULT_ACTIONS),
            unsupported: Vec::new(),
        };
        let res = super::save_rule(&db, Some(999), &input).await;
        assert!(
            matches!(res, Err(crate::error::AppError::NotFound)),
            "{res:?}"
        );
        let rules: i64 = sqlx::query_scalar("SELECT count(*) FROM discounts")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(rules, 0);
    }

    #[sqlx::test]
    async fn delete_of_missing_rule_is_not_found(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (status, _) = post_form(&app, "/admin/discounts/999/delete", &cookie, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let id = rule(&db, "Правило").await;
        let path = format!("/admin/discounts/{id}/delete");
        let (status, _) = post_form(&app, &path, &cookie, "").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
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

    /// Дерево пресета «Накопительная скидка» Битрикса (`Cumulative::generateDiscount`).
    fn cumulative_tree() -> Value {
        json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": [{
            "CLASS_ID": "CumulativeAction",
            "DATA": {
                "ranges": [
                    {"sum": 1000, "value": 3, "type": "P"},
                    {"sum": 5000, "value": 7, "type": "P"}
                ],
                "type_sum_period": "relative",
                "sum_period_data": {
                    "discount_sum_order_start": "",
                    "discount_sum_order_end": "",
                    "discount_sum_period_value": 12,
                    "discount_sum_period_type": "M"
                },
                "apply_if_more_profitable": "N",
                "All": "AND",
                "True": "True"
            },
            "CHILDREN": [
                {"CLASS_ID": "CondBsktAppliedDiscount", "DATA": {"logic": "Equal", "value": "N"}, "CHILDREN": []}
            ]
        }]})
    }

    #[sqlx::test]
    async fn cumulative_preset_creates_rule(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = permitted_cookie(&db, "manager", &["discounts.manage"]).await;
        let (status, html) = get(&app, "/admin/discounts", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("&#x2f;admin&#x2f;discounts&#x2f;cumulative")
                || html.contains("/admin/discounts/cumulative"),
            "{html}"
        );
        let (status, html) = get(&app, "/admin/discounts/cumulative", &cookie).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(html.contains("Накопительная скидка"), "{html}");

        let group = c.group_id.to_string();
        // Порядок строк — не по сумме: пресет сортирует пороги, пустые строки пропускает
        let form = body(&[
            ("name", "Накопительная"),
            ("active", "on"),
            ("group", &group),
            ("range_sum", "5000"),
            ("range_value", "7"),
            ("range_type", "P"),
            ("range_sum", ""),
            ("range_value", ""),
            ("range_type", "P"),
            ("range_sum", "1000"),
            ("range_value", "3,0"),
            ("range_type", "P"),
            ("type_sum_period", "relative"),
            ("sum_order_start", "2026-01-01T00:00"),
            ("sum_order_end", ""),
            ("sum_period_value", "12"),
            ("sum_period_type", "M"),
            ("skip_discounted", "Y"),
        ]);
        let (status, html) = post_form(&app, "/admin/discounts/cumulative", &cookie, &form).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
        let (id, name, active, conditions, actions, unsupported): (
            i64,
            String,
            bool,
            Value,
            Value,
            String,
        ) = sqlx::query_as(
            "SELECT id, name, active, conditions, actions, unsupported FROM discounts",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(
            (&name[..], active, &unsupported[..]),
            ("Накопительная", true, "")
        );
        assert_eq!(
            conditions,
            json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": []})
        );
        assert_eq!(actions, cumulative_tree());
        let groups: Vec<i64> =
            sqlx::query_scalar("SELECT group_id FROM discount_groups WHERE discount_id = $1")
                .bind(id)
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(groups, vec![c.group_id]);

        // Интервал «выгоднее» без «пропускать»: даты — московские, в unix-секундах
        let form = body(&[
            ("name", "Интервал"),
            ("range_sum", "0"),
            ("range_value", "100"),
            ("range_type", "F"),
            ("type_sum_period", "interval"),
            ("sum_order_start", "2026-01-01T00:00"),
            ("sum_order_end", ""),
            ("sum_period_value", "12"),
            ("sum_period_type", "M"),
            ("apply_if_more_profitable", "Y"),
        ]);
        let (status, html) = post_form(&app, "/admin/discounts/cumulative", &cookie, &form).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
        let (active, actions): (bool, Value) =
            sqlx::query_as("SELECT active, actions FROM discounts WHERE name = 'Интервал'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(!active);
        let action = &actions["CHILDREN"][0];
        assert_eq!(
            action["DATA"]["ranges"],
            json!([{"sum": 0, "value": 100, "type": "F"}])
        );
        assert_eq!(
            action["DATA"]["sum_period_data"],
            json!({
                "discount_sum_order_start": 1_767_214_800,
                "discount_sum_order_end": "",
                "discount_sum_period_value": "",
                "discount_sum_period_type": ""
            })
        );
        assert_eq!(action["DATA"]["apply_if_more_profitable"], "Y");
        assert_eq!(action["CHILDREN"], json!([]));
    }

    #[sqlx::test]
    async fn cumulative_preset_validates_ranges(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let form = body(&[
            ("name", "Пустая"),
            ("range_sum", ""),
            ("range_value", ""),
            ("range_type", "P"),
            ("type_sum_period", "relative"),
            ("sum_period_value", "6"),
            ("sum_period_type", "M"),
            ("skip_discounted", "Y"),
        ]);
        let (status, html) = post_form(&app, "/admin/discounts/cumulative", &cookie, &form).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(r#"class="alert""#), "{html}");
        assert!(html.contains("Не указаны величины скидок"), "{html}");
        assert!(html.contains(r#"value="Пустая""#), "{html}");
        assert!(
            html.contains(r#"name="sum_period_value" value="6""#),
            "{html}"
        );
        assert!(html.contains(r#"value="relative" selected"#), "{html}");
        assert!(
            html.contains(r#"name="skip_discounted" value="Y" checked"#),
            "{html}"
        );

        let form = body(&[
            ("name", "Больше ста"),
            ("range_sum", "1000"),
            ("range_value", "150"),
            ("range_type", "P"),
            ("range_sum", "2000"),
            ("range_value", "500"),
            ("range_type", "F"),
            ("type_sum_period", "all_time"),
        ]);
        let (status, html) = post_form(&app, "/admin/discounts/cumulative", &cookie, &form).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Неверное значение величины скидки для диапазона"),
            "{html}"
        );
        assert!(html.contains(r#"value="Больше ста""#), "{html}");
        // введённые строки на месте, со своими типами
        assert!(html.contains(r#"name="range_value" value="150""#), "{html}");
        assert!(html.contains(r#"name="range_value" value="500""#), "{html}");
        assert!(html.contains(r#"value="F" selected"#), "{html}");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM discounts")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[sqlx::test]
    async fn discount_form_saves_gift_rule(db: PgPool) {
        let c = content_fixture(&db).await;
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let gift = |value: Value| {
            json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": [{
                "CLASS_ID": "GiftCondGroup", "DATA": {"All": "AND"}, "CHILDREN": [
                    {"CLASS_ID": "GifterCondIBElement", "DATA": {"Type": "one", "Value": value}, "CHILDREN": []}
                ]
            }]})
        };
        let form = body(&[
            ("name", "Подарок"),
            ("conditions", &conditions().to_string()),
            ("actions", &gift(json!([c.item_id])).to_string()),
        ]);
        let (status, html) = post_form(&app, "/admin/discounts", &cookie, &form).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
        let (id, actions, unsupported): (i64, Value, String) =
            sqlx::query_as("SELECT id, actions, unsupported FROM discounts")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(actions, gift(json!([c.item_id])));
        assert_eq!(unsupported, "");
        // в форме правки — название подарка в подсказке
        let (_, html) = get(&app, &format!("/admin/discounts/{id}"), &cookie).await;
        assert!(html.contains("Первая новость"), "{html}");

        // раздел-подарок — одним id
        let section = json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": [{
            "CLASS_ID": "GiftCondGroup", "DATA": {"All": "AND"}, "CHILDREN": [
                {"CLASS_ID": "GifterCondIBSection", "DATA": {"Type": "all", "Value": c.section_id}, "CHILDREN": []}
            ]
        }]});
        let form = body(&[
            ("name", "Подарок из раздела"),
            ("conditions", &conditions().to_string()),
            ("actions", &section.to_string()),
        ]);
        let (status, html) = post_form(&app, "/admin/discounts", &cookie, &form).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{html}");

        let form = body(&[
            ("name", "Нет товара"),
            ("conditions", &conditions().to_string()),
            ("actions", &gift(json!([999_999])).to_string()),
        ]);
        let (status, html) = post_form(&app, "/admin/discounts", &cookie, &form).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Действия, элемент 0.0.0: запись 999999 не найдена"),
            "{html}"
        );
        assert!(html.contains(r#"value="Нет товара""#), "{html}");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM discounts")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 2);
    }

    /// Правило с накопительной скидкой (из пресета или Битрикса) сохраняется из формы правки.
    #[sqlx::test]
    async fn cumulative_rule_resaves(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let id = rule(&db, "Накопительная").await;
        sqlx::query("UPDATE discounts SET actions = $2 WHERE id = $1")
            .bind(id)
            .bind(cumulative_tree())
            .execute(&db)
            .await
            .unwrap();
        let conds = json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": [
            {"CLASS_ID": "CondCumulativeGroup", "DATA": {"logic": "EqGr", "Value": 1000}, "CHILDREN": [
                {"CLASS_ID": "PeriodRelative", "DATA": {"Value": 1, "TypeRelativePeriod": "Y"}, "CHILDREN": []}
            ]}
        ]});
        let form = body(&[
            ("name", "Накопительная"),
            ("conditions", &conds.to_string()),
            ("actions", &cumulative_tree().to_string()),
        ]);
        let (status, html) =
            post_form(&app, &format!("/admin/discounts/{id}"), &cookie, &form).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
        let (stored, unsupported): (Value, String) =
            sqlx::query_as("SELECT actions, unsupported FROM discounts WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(stored, cumulative_tree());
        assert_eq!(unsupported, "");
    }

    /// Значение скрытого поля `#discount-tree-data` (экранировано в атрибуте).
    fn tree_data(html: &str) -> Value {
        let start = html
            .find(r#"id="discount-tree-data" value=""#)
            .expect("нет данных")
            + r#"id="discount-tree-data" value=""#.len();
        let raw = &html[start..start + html[start..].find('"').unwrap()];
        let raw = raw
            .replace("&quot;", "\"")
            .replace("&#x27;", "'")
            .replace("&#x2f;", "/")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        serde_json::from_str(&raw).unwrap()
    }

    #[sqlx::test]
    async fn tree_data_lists_new_classes(db: PgPool) {
        let app = app(&db);
        let cookie = admin_cookie(&db).await;
        let (status, html) = get(&app, "/admin/discounts/new", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        let data = tree_data(&html);
        let class = |id: &str| -> Value {
            data["classes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == id)
                .cloned()
                .unwrap_or_else(|| panic!("нет класса {id}"))
        };
        let kinds = |id: &str| -> Vec<(String, String)> {
            class(id)["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| {
                    (
                        f["key"].as_str().unwrap().to_string(),
                        f["kind"].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        };
        let has = |id: &str, key: &str, kind: &str| {
            assert!(
                kinds(id).contains(&(key.to_string(), kind.to_string())),
                "{id}: {:?}",
                kinds(id)
            );
        };
        for (id, label) in [
            ("CondCumulativeGroup", "Накопительная сумма заказов"),
            ("Period", "Период: с … по …"),
            ("PeriodRelative", "Период: последние N …"),
            ("CumulativeAction", "Накопительная скидка"),
            ("GiftCondGroup", "Подарок"),
            ("GifterCondIBElement", "Товары"),
            ("GifterCondIBSection", "Раздел"),
        ] {
            assert_eq!(class(id)["label"], label, "{id}");
        }
        has("CondCumulativeGroup", "logic", "choice");
        has("CondCumulativeGroup", "Value", "number");
        has("Period", "ValueStart", "datetime");
        has("Period", "ValueEnd", "datetime");
        has("PeriodRelative", "Value", "number");
        has("PeriodRelative", "TypeRelativePeriod", "choice");
        has("CumulativeAction", "ranges", "ranges");
        has("CumulativeAction", "type_sum_period", "choice");
        has(
            "CumulativeAction",
            "sum_period_data.discount_sum_order_start",
            "datetime",
        );
        has(
            "CumulativeAction",
            "sum_period_data.discount_sum_period_type",
            "choice",
        );
        has("CumulativeAction", "apply_if_more_profitable", "choice");
        has("GifterCondIBElement", "Type", "choice");
        has("GifterCondIBElement", "Value", "ids");
        has("GifterCondIBSection", "Value", "ids");
        let units = &class("PeriodRelative")["fields"][1]["options"];
        assert_eq!(
            units,
            &json!([["D", "Дней"], ["M", "Месяцев"], ["Y", "Лет"]])
        );
        let gift_types = &class("GifterCondIBElement")["fields"][0]["options"];
        assert_eq!(gift_types, &json!([["one", "один из"], ["all", "все"]]));
        assert_eq!(class("GifterCondIBElement")["fields"][1]["lookup"], "items");
        assert_eq!(
            class("GifterCondIBSection")["fields"][1]["lookup"],
            "sections"
        );
        assert_eq!(class("GifterCondIBSection")["fields"][1]["single"], true);

        let contains =
            |list: &Value, id: &str| list.as_array().is_some_and(|l| l.iter().any(|c| c == id));
        let ch = &data["children"];
        assert!(
            contains(&ch["conditions"]["#root"], "CondCumulativeGroup"),
            "{ch}"
        );
        assert!(!contains(&ch["conditions"]["#root"], "Period"), "{ch}");
        assert!(
            contains(&ch["conditions"]["CondCumulativeGroup"], "Period"),
            "{ch}"
        );
        assert!(
            contains(&ch["conditions"]["CondCumulativeGroup"], "PeriodRelative"),
            "{ch}"
        );
        assert!(
            contains(&ch["actions"]["#root"], "CumulativeAction"),
            "{ch}"
        );
        assert!(contains(&ch["actions"]["#root"], "GiftCondGroup"), "{ch}");
        assert!(
            contains(
                &ch["actions"]["CumulativeAction"],
                "CondBsktAppliedDiscount"
            ),
            "{ch}"
        );
        assert!(
            !contains(&ch["actions"]["CumulativeAction"], "ActSaleSubGrp"),
            "{ch}"
        );
        assert!(
            contains(&ch["actions"]["GiftCondGroup"], "GifterCondIBElement"),
            "{ch}"
        );
        assert!(
            contains(&ch["actions"]["GiftCondGroup"], "GifterCondIBSection"),
            "{ch}"
        );
    }
}
