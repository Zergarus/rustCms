//! Модель правил работы с корзиной (как `b_sale_discount` Битрикса) и проверка дерева условий.
//!
//! Дерево — формат Битрикса (`CSaleCondTree`/`CSaleActionTree`): `{"CLASS_ID", "DATA", "CHILDREN"}`,
//! ключи и значения `DATA` — дословно Битрикса; ссылки на сущности — наши id.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::PgPool;

/// Узел дерева условий или действий.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    #[serde(rename = "CLASS_ID")]
    pub class_id: String,
    #[serde(rename = "DATA", default)]
    pub data: Map<String, Value>,
    /// У листьев Битрикс ключ не пишет.
    #[serde(rename = "CHILDREN", default)]
    pub children: Vec<Node>,
}

/// Правило работы с корзиной (строка `discounts`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rule {
    pub id: i64,
    pub name: String,
    pub active: bool,
    pub active_from: Option<DateTime<Utc>>,
    pub active_to: Option<DateTime<Utc>>,
    pub priority: i32,
    pub sort: i32,
    /// Прекратить дальнейшее применение правил.
    pub last_discount: bool,
    /// Прекратить применение остальных правил того же приоритета (`LAST_LEVEL_DISCOUNT`
    /// Битрикса: «на текущем уровне приоритетов»); правила с меньшим приоритетом применяются.
    pub last_level_discount: bool,
    /// Только по купону.
    pub use_coupons: bool,
    pub conditions: Node,
    pub actions: Node,
    /// Пусто — для всех.
    pub group_ids: Vec<i64>,
    /// Неподдерживаемые классы через запятую; правило с ними не применяется.
    pub unsupported: String,
    pub xml_id: String,
}

/// Тип купона: `TYPE` Битрикса 1 / 2 / 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CouponKind {
    /// Одноразовый на позицию.
    Basket,
    /// Одноразовый на заказ.
    Order,
    /// Многоразовый.
    Multi,
}

impl CouponKind {
    pub fn from_bitrix(t: i64) -> Option<CouponKind> {
        match t {
            1 => Some(CouponKind::Basket),
            2 => Some(CouponKind::Order),
            4 => Some(CouponKind::Multi),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CouponKind::Basket => "basket",
            CouponKind::Order => "order",
            CouponKind::Multi => "multi",
        }
    }

    pub fn parse(s: &str) -> Option<CouponKind> {
        match s {
            "basket" => Some(CouponKind::Basket),
            "order" => Some(CouponKind::Order),
            "multi" => Some(CouponKind::Multi),
            _ => None,
        }
    }
}

/// Купон правила (строка `discount_coupons`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Coupon {
    pub id: i64,
    pub discount_id: i64,
    pub coupon: String,
    pub active: bool,
    pub active_from: Option<DateTime<Utc>>,
    pub active_to: Option<DateTime<Utc>>,
    pub kind: CouponKind,
    /// 0 — без лимита.
    pub max_use: i32,
    pub use_count: i32,
    /// `None` — любому покупателю.
    pub user_id: Option<i64>,
}

/// Классы условий, которые знает движок. `CondIBProp` — шаблон `CondIBProp:<коллекция>:<поле>`.
pub const CONDITION_CLASSES: &[&str] = &[
    "CondGroup",
    "CondBsktCntGroup",
    "CondBsktAmtGroup",
    "CondBsktAmtBaseGroup",
    "CondBsktProductGroup",
    "CondBsktRowGroup",
    "CondBsktSubGroup",
    "CondBsktFldProduct",
    "CondBsktFldName",
    "CondBsktFldPrice",
    "CondBsktFldQuantity",
    "CondBsktFldSumm",
    "CondBsktFldWeight",
    "CondIBElement",
    "CondIBIBlock",
    "CondIBSection",
    "CondIBXmlID",
    "CondIBName",
    "CondIBCode",
    "CondIBProp",
    "CondCatQuantity",
    "CondCatWeight",
    "CondSaleOrderSumm",
    "CondSaleOrderWeight",
    "CondSalePersonType",
    "CondSalePaySystem",
    "CondSaleDelivery",
    "CondSaleCmnDayOfWeek",
    "CondBsktAppliedDiscount",
];

/// Классы действий (`CSaleActionCtrlBasketGroup`, `CSaleActionCtrlSubGroup`, `CSaleActionCtrlDelivery`).
pub const ACTION_CLASSES: &[&str] = &["ActSaleBsktGrp", "ActSaleSubGrp", "ActSaleDelivery"];

const PROP_PREFIX: &str = "CondIBProp:";

/// `CondIBProp:<коллекция>:<поле>` → (коллекция, поле).
pub fn prop_class(class_id: &str) -> Option<(i64, i64)> {
    let (c, f) = class_id.strip_prefix(PROP_PREFIX)?.split_once(':')?;
    Some((c.parse().ok()?, f.parse().ok()?))
}

fn is_supported(class_id: &str) -> bool {
    if class_id.starts_with(PROP_PREFIX) {
        return prop_class(class_id).is_some();
    }
    class_id != "CondIBProp"
        && (CONDITION_CLASSES.contains(&class_id) || ACTION_CLASSES.contains(&class_id))
}

/// Классы, которых движок не знает: уникальные, в порядке обхода (сначала условия, затем
/// действия). Детей неизвестного узла не обходим — их смысл задаёт сам узел.
pub fn unsupported_classes(conditions: &Node, actions: &Node) -> Vec<String> {
    unsupported_classes_with(conditions, actions, &[])
}

/// То же с классами модуля проекта (`DiscountConditionHook::class_ids`): они известны,
/// их детей не обходим — смысл узла задаёт модуль.
pub fn unsupported_classes_with(conditions: &Node, actions: &Node, extra: &[&str]) -> Vec<String> {
    fn walk(node: &Node, extra: &[&str], out: &mut Vec<String>) {
        if extra.contains(&node.class_id.as_str()) {
            return;
        }
        if !is_supported(&node.class_id) {
            if !out.contains(&node.class_id) {
                out.push(node.class_id.clone());
            }
            return;
        }
        for child in &node.children {
            walk(child, extra, out);
        }
    }
    let mut out = Vec::new();
    walk(conditions, extra, &mut out);
    walk(actions, extra, &mut out);
    out
}

/// Существующие сущности, на которые ссылается дерево.
#[derive(Debug, Clone, Default)]
pub struct Refs {
    pub collections: HashSet<i64>,
    /// id поля → (коллекция, `kind`).
    pub fields: HashMap<i64, (i64, String)>,
    pub options: HashSet<i64>,
    pub items: HashSet<i64>,
    pub sections: HashSet<i64>,
    pub pay_systems: HashSet<i64>,
    pub deliveries: HashSet<i64>,
    pub person_types: HashSet<i64>,
}

impl Refs {
    /// Загружает из базы только сущности, упомянутые в деревьях.
    pub async fn load(db: &PgPool, nodes: &[&Node]) -> sqlx::Result<Refs> {
        let mut want: HashMap<Ref, Vec<i64>> = HashMap::new();
        for node in nodes {
            mentioned(node, &mut want);
        }
        let ids = |r: Ref| want.get(&r).cloned().unwrap_or_default();
        async fn existing(db: &PgPool, table: &str, ids: Vec<i64>) -> sqlx::Result<HashSet<i64>> {
            if ids.is_empty() {
                return Ok(HashSet::new());
            }
            let rows: Vec<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT id FROM {table} WHERE id = ANY($1)"
            )))
            .bind(ids)
            .fetch_all(db)
            .await?;
            Ok(rows.into_iter().collect())
        }
        let fields: Vec<(i64, i64, String)> = sqlx::query_as(
            "SELECT id, collection_id, kind FROM collection_fields WHERE id = ANY($1)",
        )
        .bind(ids(Ref::Field))
        .fetch_all(db)
        .await?;
        Ok(Refs {
            collections: existing(db, "collections", ids(Ref::Collection)).await?,
            fields: fields.into_iter().map(|(id, c, k)| (id, (c, k))).collect(),
            options: existing(db, "collection_field_options", ids(Ref::Option)).await?,
            items: existing(db, "collection_items", ids(Ref::Item)).await?,
            sections: existing(db, "collection_sections", ids(Ref::Section)).await?,
            pay_systems: existing(db, "pay_systems", ids(Ref::PaySystem)).await?,
            deliveries: existing(db, "deliveries", ids(Ref::Delivery)).await?,
            person_types: existing(db, "person_types", ids(Ref::PersonType)).await?,
        })
    }

    fn has(&self, r: Ref, id: i64) -> bool {
        match r {
            Ref::Collection => self.collections.contains(&id),
            Ref::Field => self.fields.contains_key(&id),
            Ref::Option => self.options.contains(&id),
            Ref::Item => self.items.contains(&id),
            Ref::Section => self.sections.contains(&id),
            Ref::PaySystem => self.pay_systems.contains(&id),
            Ref::Delivery => self.deliveries.contains(&id),
            Ref::PersonType => self.person_types.contains(&id),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Ref {
    Collection,
    Field,
    Option,
    Item,
    Section,
    PaySystem,
    Delivery,
    PersonType,
}

impl Ref {
    fn missing(self, id: i64) -> String {
        match self {
            Ref::Collection => format!("коллекция {id} не найдена"),
            Ref::Field => format!("поле {id} не найдено"),
            Ref::Option => format!("вариант {id} не найден"),
            Ref::Item => format!("запись {id} не найдена"),
            Ref::Section => format!("раздел {id} не найден"),
            Ref::PaySystem => format!("платёжная система {id} не найдена"),
            Ref::Delivery => format!("служба доставки {id} не найдена"),
            Ref::PersonType => format!("тип плательщика {id} не найден"),
        }
    }
}

/// Ссылки узла и его потомков: для `CondIBProp` значение может быть и вариантом, и записью —
/// тип поля станет известен только после загрузки.
fn mentioned(node: &Node, want: &mut HashMap<Ref, Vec<i64>>) {
    let value_ids = || node.data.get("value").and_then(ids).unwrap_or_default();
    if let Some((c, f)) = prop_class(&node.class_id) {
        want.entry(Ref::Collection).or_default().push(c);
        want.entry(Ref::Field).or_default().push(f);
        want.entry(Ref::Option).or_default().extend(value_ids());
        want.entry(Ref::Item).or_default().extend(value_ids());
    } else if let Some(Leaf {
        value: Val::Ids(r), ..
    }) = leaf(&node.class_id)
    {
        want.entry(r).or_default().extend(value_ids());
    }
    for child in &node.children {
        mentioned(child, want);
    }
}

const LOGIC_EQ: &[&str] = &["Equal", "Not"];
const LOGIC_NUM: &[&str] = &["Equal", "Not", "Great", "Less", "EqGr", "EqLs"];
const LOGIC_STR: &[&str] = &["Equal", "Not", "Contain", "NotCont"];
const ALL: &[&str] = &["AND", "OR"];
const TRUE: &[&str] = &["True", "False"];

/// Значение `value` листа.
#[derive(Clone, Copy)]
enum Val {
    Num,
    Str,
    /// Один id или список.
    Ids(Ref),
    DayOfWeek,
    Bool,
}

struct Leaf {
    logic: &'static [&'static str],
    value: Val,
}

fn leaf(class_id: &str) -> Option<Leaf> {
    let (logic, value) = match class_id {
        "CondBsktFldProduct" | "CondIBElement" => (LOGIC_EQ, Val::Ids(Ref::Item)),
        "CondIBIBlock" => (LOGIC_EQ, Val::Ids(Ref::Collection)),
        "CondIBSection" => (LOGIC_EQ, Val::Ids(Ref::Section)),
        "CondBsktFldName" | "CondIBXmlID" | "CondIBName" | "CondIBCode" => (LOGIC_STR, Val::Str),
        "CondBsktFldPrice"
        | "CondBsktFldQuantity"
        | "CondBsktFldSumm"
        | "CondBsktFldWeight"
        | "CondCatQuantity"
        | "CondCatWeight"
        | "CondSaleOrderSumm"
        | "CondSaleOrderWeight" => (LOGIC_NUM, Val::Num),
        "CondSalePersonType" => (LOGIC_EQ, Val::Ids(Ref::PersonType)),
        "CondSalePaySystem" => (LOGIC_EQ, Val::Ids(Ref::PaySystem)),
        "CondSaleDelivery" => (LOGIC_EQ, Val::Ids(Ref::Delivery)),
        "CondSaleCmnDayOfWeek" => (LOGIC_EQ, Val::DayOfWeek),
        _ => return None,
    };
    Some(Leaf { logic, value })
}

/// Условия строки корзины: внутри групп по позициям и групп действий.
fn is_row_condition(class_id: &str) -> bool {
    class_id.starts_with("CondBsktFld")
        || class_id.starts_with("CondIB")
        || class_id.starts_with("CondCat")
        || matches!(class_id, "CondBsktSubGroup" | "CondBsktAppliedDiscount")
}

/// Можно ли вложить `child` в `parent` (как `GetShowIn` контролов Битрикса).
pub fn allowed_in(parent: &str, child: &str, parent_is_root: bool) -> bool {
    match parent {
        "CondGroup" => match child {
            "ActSaleBsktGrp" | "ActSaleDelivery" => parent_is_root,
            "CondGroup"
            | "CondBsktCntGroup"
            | "CondBsktAmtGroup"
            | "CondBsktAmtBaseGroup"
            | "CondBsktProductGroup"
            | "CondBsktRowGroup" => true,
            c => c.starts_with("CondSale"),
        },
        "CondBsktCntGroup"
        | "CondBsktAmtGroup"
        | "CondBsktAmtBaseGroup"
        | "CondBsktProductGroup"
        | "CondBsktRowGroup"
        | "CondBsktSubGroup" => is_row_condition(child),
        "ActSaleBsktGrp" | "ActSaleSubGrp" => child == "ActSaleSubGrp" || is_row_condition(child),
        _ => false,
    }
}

pub fn is_group(class_id: &str) -> bool {
    matches!(
        class_id,
        "CondGroup"
            | "CondBsktCntGroup"
            | "CondBsktAmtGroup"
            | "CondBsktAmtBaseGroup"
            | "CondBsktProductGroup"
            | "CondBsktRowGroup"
            | "CondBsktSubGroup"
            | "ActSaleBsktGrp"
            | "ActSaleSubGrp"
    )
}

/// Проверяет дерево: известность классов, вложенность, ключи и значения `DATA`, ссылки на
/// существующие сущности. Путь узла — индексы от корня: корень `0`, его второй потомок `0.1`.
/// Дерево с ошибками остаётся загружаемым — правило с ними просто показывается в админке.
pub fn validate(node: &Node, refs: &Refs) -> Vec<(String, String)> {
    validate_with(node, refs, &[])
}

/// То же с классами модуля проекта: такой узел допустим в любой группе, его `DATA` и детей
/// проверяет сам модуль.
pub fn validate_with(node: &Node, refs: &Refs, extra: &[&str]) -> Vec<(String, String)> {
    let mut errs = Vec::new();
    if node.class_id != "CondGroup" {
        errs.push((
            "0".to_string(),
            "в корне допустим только «CondGroup»".to_string(),
        ));
    }
    check(node, "0", refs, extra, &mut errs);
    errs
}

fn check(node: &Node, path: &str, refs: &Refs, extra: &[&str], errs: &mut Vec<(String, String)>) {
    let class = node.class_id.as_str();
    if extra.contains(&class) {
        return;
    }
    if !is_supported(class) {
        errs.push((path.to_string(), format!("неизвестный элемент «{class}»")));
        return;
    }
    let mut e = Vec::new();
    check_data(node, refs, &mut e);
    if !is_group(class) && !node.children.is_empty() {
        e.push(format!("у «{class}» не может быть вложенных элементов"));
    }
    errs.extend(e.into_iter().map(|m| (path.to_string(), m)));
    for (i, child) in node.children.iter().enumerate() {
        let child_path = format!("{path}.{i}");
        let hooked = extra.contains(&child.class_id.as_str());
        if (hooked && !is_group(class))
            || (!hooked
                && is_supported(&child.class_id)
                && !allowed_in(class, &child.class_id, path == "0"))
        {
            errs.push((
                child_path.clone(),
                format!("«{}» нельзя вложить в «{class}»", child.class_id),
            ));
        }
        check(child, &child_path, refs, extra, errs);
    }
}

fn check_data(node: &Node, refs: &Refs, e: &mut Vec<String>) {
    let d = &node.data;
    let class = node.class_id.as_str();
    match class {
        "CondGroup" | "CondBsktSubGroup" | "ActSaleSubGrp" => {
            choice(d, "All", ALL, true, e);
            choice(d, "True", TRUE, false, e);
        }
        "CondBsktCntGroup" | "CondBsktAmtGroup" | "CondBsktAmtBaseGroup" | "CondBsktRowGroup" => {
            choice(d, "logic", LOGIC_NUM, true, e);
            number(d, "Value", true, e);
            choice(d, "All", ALL, true, e);
        }
        "CondBsktProductGroup" => {
            choice(d, "Found", &["Found", "NoFound"], true, e);
            choice(d, "All", ALL, true, e);
        }
        "CondBsktAppliedDiscount" => choice(d, "value", &["Y", "N"], true, e),
        "ActSaleBsktGrp" => {
            choice(d, "Type", &["Discount", "Extra", "Closeout"], true, e);
            number(d, "Value", true, e);
            choice(d, "Unit", &["Perc", "CurEach", "CurAll"], true, e);
            number(d, "Max", false, e);
            choice(d, "All", ALL, true, e);
            choice(d, "True", TRUE, false, e);
            // Как CSaleActionCtrlBasketGroup::CheckAtoms
            let max = d.get("Max").and_then(as_num).unwrap_or(0.0);
            if d.get("Unit").and_then(Value::as_str) == Some("CurAll") && max != 0.0 {
                e.push("Max нельзя задать для Unit «CurAll»".to_string());
            }
        }
        "ActSaleDelivery" => {
            choice(d, "Type", &["Discount", "DiscountZero", "Extra"], true, e);
            number(d, "Value", true, e);
            choice(d, "Unit", &["Perc", "Cur"], true, e);
        }
        _ => {
            if let Some((collection, field)) = prop_class(class) {
                check_prop(d, collection, field, refs, e);
            } else if let Some(l) = leaf(class) {
                choice(d, "logic", l.logic, true, e);
                value(d, l.value, refs, e);
            }
        }
    }
}

fn check_prop(
    d: &Map<String, Value>,
    collection: i64,
    field: i64,
    refs: &Refs,
    e: &mut Vec<String>,
) {
    if !refs.collections.contains(&collection) {
        e.push(Ref::Collection.missing(collection));
        return;
    }
    let Some((owner, kind)) = refs.fields.get(&field) else {
        e.push(Ref::Field.missing(field));
        return;
    };
    if *owner != collection {
        e.push(format!("поле {field} не из коллекции {collection}"));
        return;
    }
    // Как CCatalogCondCtrlIBlockProps::GetControls по PROPERTY_TYPE
    let (logic, val) = match kind.as_str() {
        "number" => (LOGIC_NUM, Val::Num),
        "date" => (LOGIC_NUM, Val::Str),
        "string" | "text" => (LOGIC_STR, Val::Str),
        "list" => (LOGIC_EQ, Val::Ids(Ref::Option)),
        "element" => (LOGIC_EQ, Val::Ids(Ref::Item)),
        "boolean" => (LOGIC_EQ, Val::Bool),
        other => {
            e.push(format!(
                "поле {field} типа «{other}» нельзя использовать в условии"
            ));
            return;
        }
    };
    choice(d, "logic", logic, true, e);
    value(d, val, refs, e);
}

fn shown(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

fn bad(key: &str, v: &Value) -> String {
    format!("недопустимое значение {key} «{}»", shown(v))
}

fn choice(
    d: &Map<String, Value>,
    key: &str,
    allowed: &[&str],
    required: bool,
    e: &mut Vec<String>,
) {
    match d.get(key) {
        None if required => e.push(format!("не задано {key}")),
        None => {}
        Some(v) if !v.as_str().is_some_and(|s| allowed.contains(&s)) => e.push(bad(key, v)),
        Some(_) => {}
    }
}

/// Число как в PHP-serialize: `i:`/`d:` или числовая строка.
/// Деньги до копеек, как `round($v, 2)` PHP (`Sale\PriceMaths::roundPrecision`):
/// значение × 100 предокругляется до 15 значащих цифр (как `_php_math_round`), затем
/// половина округляется от нуля.
pub(super) fn round2(x: f64) -> f64 {
    let scaled = x * 100.0;
    if !scaled.is_finite() {
        return x;
    }
    // 15 значащих цифр: мантисса с 14 знаками после точки
    let pre: f64 = format!("{scaled:.14e}").parse().unwrap_or(scaled);
    pre.round() / 100.0
}

pub(super) fn as_num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|f: &f64| f.is_finite())
}

fn as_id(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Один id или список id; `None`, если хоть один элемент — не id.
fn ids(v: &Value) -> Option<Vec<i64>> {
    match v {
        Value::Array(a) => a.iter().map(as_id).collect(),
        v => as_id(v).map(|id| vec![id]),
    }
}

fn number(d: &Map<String, Value>, key: &str, required: bool, e: &mut Vec<String>) {
    match d.get(key) {
        None if required => e.push(format!("не задано {key}")),
        None => {}
        Some(v) if !as_num(v).is_some_and(|n| n >= 0.0) => e.push(bad(key, v)),
        Some(_) => {}
    }
}

fn value(d: &Map<String, Value>, kind: Val, refs: &Refs, e: &mut Vec<String>) {
    let Some(v) = d.get("value") else {
        e.push("не задано value".to_string());
        return;
    };
    match kind {
        Val::Num => {
            if as_num(v).is_none() {
                e.push(bad("value", v));
            }
        }
        Val::Str => {
            if !matches!(v, Value::String(_) | Value::Number(_)) {
                e.push(bad("value", v));
            }
        }
        Val::Bool => {
            if !matches!(v, Value::Bool(_)) && !matches!(v.as_str(), Some("Y" | "N")) {
                e.push(bad("value", v));
            }
        }
        Val::DayOfWeek | Val::Ids(_) => {
            let list: Vec<Value> = match v {
                Value::Array(a) => a.clone(),
                v => vec![v.clone()],
            };
            if list.is_empty() {
                e.push("не задано value".to_string());
            }
            for item in &list {
                let Some(id) = as_id(item) else {
                    e.push(bad("value", item));
                    continue;
                };
                match kind {
                    Val::DayOfWeek if !(1..=7).contains(&id) => e.push(bad("value", item)),
                    Val::Ids(r) if !refs.has(r, id) => e.push(r.missing(id)),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// `round($v, 2)` PHP (`Sale\PriceMaths::roundPrecision`): предокругление до 15 значащих
    /// цифр, половина — от нуля.
    #[test]
    fn round2_matches_php() {
        assert_eq!(round2(1.45 * 10.0 / 100.0), 0.15);
        assert_eq!(round2(1.9 * 15.0 / 100.0), 0.29);
        assert_eq!(round2(-(5.65 * 10.0 / 100.0)), -0.57);
        assert_eq!(round2(1.005), 1.01);
        assert_eq!(round2(-1.005), -1.01);
        assert_eq!(round2(2.675), 2.68);
        assert_eq!(round2(0.0), 0.0);
        assert_eq!(round2(1234567.891), 1234567.89);
        assert_eq!(round2(0.004999), 0.0);
    }

    fn node(v: Value) -> Node {
        serde_json::from_value(v).unwrap()
    }

    /// Правило №24 трансопта «+10%» (`b_sale_discount`), PHP-serialize переведён в JSON.
    fn rule24_conditions() -> Value {
        json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": []})
    }

    fn rule24_actions() -> Value {
        json!({
            "CLASS_ID": "CondGroup",
            "DATA": {"All": "AND"},
            "CHILDREN": [{
                "CLASS_ID": "ActSaleBsktGrp",
                "DATA": {"Type": "Extra", "Value": 10.0, "Unit": "Perc", "Max": 0, "All": "AND", "True": "True"},
                "CHILDREN": [{"CLASS_ID": "CondIBProp:4:55", "DATA": {"logic": "Equal", "value": 32}}]
            }]
        })
    }

    fn transopt_refs() -> Refs {
        Refs {
            collections: HashSet::from([4, 5]),
            fields: HashMap::from([(55, (4, "list".to_string()))]),
            options: HashSet::from([32]),
            ..Refs::default()
        }
    }

    #[test]
    fn node_roundtrip_bitrix_json() {
        let actions = node(rule24_actions());
        assert_eq!(actions.class_id, "CondGroup");
        assert_eq!(actions.data["All"], "AND");
        let grp = &actions.children[0];
        assert_eq!(grp.class_id, "ActSaleBsktGrp");
        assert_eq!(grp.data["Type"], "Extra");
        assert_eq!(grp.data["Value"].as_f64(), Some(10.0));
        assert_eq!(grp.data["Unit"], "Perc");
        let prop = &grp.children[0];
        assert_eq!(prop.class_id, "CondIBProp:4:55");
        assert_eq!(prop.data["value"], 32);
        assert!(prop.children.is_empty());

        // Обратно — тот же JSON; у листа появляется пустой CHILDREN
        let mut expected = rule24_actions();
        expected["CHILDREN"][0]["CHILDREN"][0]["CHILDREN"] = json!([]);
        let back = serde_json::to_value(&actions).unwrap();
        assert_eq!(back, expected);
        assert_eq!(node(back), actions);

        let conditions = node(rule24_conditions());
        assert_eq!(
            serde_json::to_value(&conditions).unwrap(),
            rule24_conditions()
        );
        assert!(validate(&conditions, &transopt_refs()).is_empty());
        assert!(validate(&actions, &transopt_refs()).is_empty());
    }

    #[test]
    fn unsupported_lists_unknown_classes() {
        let conditions = node(json!({
            "CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"},
            "CHILDREN": [
                {"CLASS_ID": "CondCumulativeGroup", "DATA": {"logic": "EqGr", "Value": 1000}, "CHILDREN": []},
                {"CLASS_ID": "CondIBProp:4:55", "DATA": {"logic": "Equal", "value": 32}},
                {"CLASS_ID": "CondIBProp:4", "DATA": {"logic": "Equal", "value": 32}},
                {"CLASS_ID": "CondCumulativeGroup", "DATA": {"logic": "EqGr", "Value": 5}, "CHILDREN": []}
            ]
        }));
        let actions = node(json!({
            "CLASS_ID": "CondGroup", "DATA": {"All": "AND"},
            "CHILDREN": [
                {"CLASS_ID": "ActSaleGift", "DATA": {"Type": "one"}, "CHILDREN": []},
                {"CLASS_ID": "ActSaleBsktGrp",
                 "DATA": {"Type": "Discount", "Value": 5, "Unit": "Perc", "Max": 0, "All": "AND", "True": "True"},
                 "CHILDREN": [{"CLASS_ID": "CondBsktFldPrice", "DATA": {"logic": "Great", "value": 100}}]},
                {"CLASS_ID": "ActSaleDelivery", "DATA": {"Type": "DiscountZero", "Value": 0, "Unit": "Perc"}}
            ]
        }));
        assert_eq!(
            unsupported_classes(&conditions, &actions),
            vec!["CondCumulativeGroup", "CondIBProp:4", "ActSaleGift"]
        );
        assert!(
            unsupported_classes(&node(rule24_conditions()), &node(rule24_actions())).is_empty()
        );
        for class in [
            "CondIBProp:4:55",
            "CondGroup",
            "ActSaleBsktGrp",
            "ActSaleSubGrp",
            "ActSaleDelivery",
        ] {
            let n = node(json!({"CLASS_ID": class, "DATA": {}}));
            assert!(unsupported_classes(&n, &n).is_empty(), "{class}");
        }
    }

    #[test]
    fn validate_reports_path_and_reason() {
        let conditions = node(json!({
            "CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"},
            "CHILDREN": [
                {"CLASS_ID": "CondFoo", "DATA": {}},
                {"CLASS_ID": "CondBsktProductGroup", "DATA": {"Found": "Found", "All": "AND"}, "CHILDREN": [
                    {"CLASS_ID": "CondIBProp:5:55", "DATA": {"logic": "Equal", "value": 32}},
                    {"CLASS_ID": "CondBsktFldPrice", "DATA": {"logic": "Contain", "value": 10}}
                ]},
                {"CLASS_ID": "CondGroup", "DATA": {"All": "XOR", "True": "True"}, "CHILDREN": [
                    {"CLASS_ID": "CondSaleCmnDayOfWeek", "DATA": {"logic": "Equal", "value": [1, 8]}}
                ]},
                {"CLASS_ID": "CondBsktFldName", "DATA": {"logic": "Equal", "value": "x"}}
            ]
        }));
        assert_eq!(
            validate(&conditions, &transopt_refs()),
            vec![
                (
                    "0.0".to_string(),
                    "неизвестный элемент «CondFoo»".to_string()
                ),
                ("0.1.0".to_string(), "поле 55 не из коллекции 5".to_string()),
                (
                    "0.1.1".to_string(),
                    "недопустимое значение logic «Contain»".to_string()
                ),
                (
                    "0.2".to_string(),
                    "недопустимое значение All «XOR»".to_string()
                ),
                (
                    "0.2.0".to_string(),
                    "недопустимое значение value «8»".to_string()
                ),
                (
                    "0.3".to_string(),
                    "«CondBsktFldName» нельзя вложить в «CondGroup»".to_string()
                ),
            ]
        );

        let actions = node(json!({
            "CLASS_ID": "CondGroup", "DATA": {"All": "AND"},
            "CHILDREN": [
                {"CLASS_ID": "ActSaleBsktGrp",
                 "DATA": {"Type": "Discount", "Value": 5, "Unit": "Pct", "Max": 0, "All": "AND", "True": "True"},
                 "CHILDREN": []},
                {"CLASS_ID": "ActSaleBsktGrp",
                 "DATA": {"Type": "Discount", "Value": 500, "Unit": "CurAll", "Max": 100, "All": "AND", "True": "True"},
                 "CHILDREN": []},
                {"CLASS_ID": "ActSaleDelivery", "DATA": {"Type": "Discount", "Value": 50, "Unit": "CurEach"}}
            ]
        }));
        assert_eq!(
            validate(&actions, &transopt_refs()),
            vec![
                (
                    "0.0".to_string(),
                    "недопустимое значение Unit «Pct»".to_string()
                ),
                (
                    "0.1".to_string(),
                    "Max нельзя задать для Unit «CurAll»".to_string()
                ),
                (
                    "0.2".to_string(),
                    "недопустимое значение Unit «CurEach»".to_string()
                ),
            ]
        );
    }

    #[test]
    fn hook_classes_are_supported() {
        let conditions = node(json!({
            "CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"},
            "CHILDREN": [
                {"CLASS_ID": "CondProjVip", "DATA": {"anything": [1, 2]}, "CHILDREN": [
                    {"CLASS_ID": "CondFoo", "DATA": {}}
                ]},
                {"CLASS_ID": "CondBsktCntGroup", "DATA": {"logic": "EqGr", "Value": 1, "All": "AND"}, "CHILDREN": [
                    {"CLASS_ID": "CondProjVip", "DATA": {}}
                ]},
                {"CLASS_ID": "CondBsktFldPrice", "DATA": {"logic": "Great", "value": 1}, "CHILDREN": []}
            ]
        }));
        let actions = node(rule24_actions());
        let extra = ["CondProjVip"];
        assert!(unsupported_classes_with(&conditions, &actions, &extra).is_empty());
        // Без модуля проекта — неизвестный класс
        assert_eq!(
            unsupported_classes(&conditions, &actions),
            vec!["CondProjVip"]
        );
        // DATA и дети класса проекта не проверяются; вложенность в лист — ошибка
        let leaf_parent = node(json!({
            "CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"},
            "CHILDREN": [{"CLASS_ID": "CondSalePaySystem", "DATA": {"logic": "Equal", "value": []}, "CHILDREN": [
                {"CLASS_ID": "CondProjVip", "DATA": {}}
            ]}]
        }));
        assert_eq!(
            validate_with(&conditions, &transopt_refs(), &extra),
            vec![(
                "0.2".to_string(),
                "«CondBsktFldPrice» нельзя вложить в «CondGroup»".to_string()
            )]
        );
        assert_eq!(
            validate(&conditions, &transopt_refs())[0],
            (
                "0.0".to_string(),
                "неизвестный элемент «CondProjVip»".to_string()
            )
        );
        let errs = validate_with(&leaf_parent, &transopt_refs(), &extra);
        assert!(
            errs.contains(&(
                "0.0.0".to_string(),
                "«CondProjVip» нельзя вложить в «CondSalePaySystem»".to_string()
            )),
            "{errs:?}"
        );
    }

    #[test]
    fn coupon_kind_maps_bitrix_type() {
        assert_eq!(CouponKind::from_bitrix(1), Some(CouponKind::Basket));
        assert_eq!(CouponKind::from_bitrix(2), Some(CouponKind::Order));
        assert_eq!(CouponKind::from_bitrix(4), Some(CouponKind::Multi));
        assert_eq!(CouponKind::from_bitrix(3), None);
        assert_eq!(
            serde_json::to_value([CouponKind::Basket, CouponKind::Order, CouponKind::Multi])
                .unwrap(),
            json!(["basket", "order", "multi"])
        );
    }

    #[sqlx::test]
    async fn rule_with_dangling_reference(db: PgPool) {
        let collection: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name) VALUES ('goods', 'Товары') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let field: i64 = sqlx::query_scalar(
            "INSERT INTO collection_fields (collection_id, code, name, kind) VALUES ($1, 'flag', 'Флаг', 'list') RETURNING id",
        )
        .bind(collection)
        .fetch_one(&db)
        .await
        .unwrap();
        let option: i64 = sqlx::query_scalar(
            "INSERT INTO collection_field_options (field_id, value, xml_id) VALUES ($1, 'Y', 'Y') RETURNING id",
        )
        .bind(field)
        .fetch_one(&db)
        .await
        .unwrap();
        let item: i64 = sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name) VALUES ($1, 'x', 'X') RETURNING id",
        )
        .bind(collection)
        .fetch_one(&db)
        .await
        .unwrap();
        let actions = json!({
            "CLASS_ID": "CondGroup", "DATA": {"All": "AND"},
            "CHILDREN": [{
                "CLASS_ID": "ActSaleBsktGrp",
                "DATA": {"Type": "Discount", "Value": 10, "Unit": "Perc", "Max": 0, "All": "OR", "True": "True"},
                "CHILDREN": [
                    {"CLASS_ID": format!("CondIBProp:{collection}:{field}"), "DATA": {"logic": "Equal", "value": option}},
                    {"CLASS_ID": "CondIBElement", "DATA": {"logic": "Equal", "value": [item]}}
                ]
            }]
        });
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO discounts (name, conditions, actions) VALUES ('-10%', $1, $2) RETURNING id",
        )
        .bind(rule24_conditions())
        .bind(&actions)
        .fetch_one(&db)
        .await
        .unwrap();

        let load = |db: PgPool| async move {
            let v: Value = sqlx::query_scalar("SELECT actions FROM discounts WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
            node(v)
        };
        let tree = load(db.clone()).await;
        assert!(validate(&tree, &Refs::load(&db, &[&tree]).await.unwrap()).is_empty());

        sqlx::query("DELETE FROM collection_items WHERE id = $1")
            .bind(item)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM collection_field_options WHERE id = $1")
            .bind(option)
            .execute(&db)
            .await
            .unwrap();
        let tree = load(db.clone()).await;
        assert_eq!(
            serde_json::to_value(&tree).unwrap()["CHILDREN"][0]["CHILDREN"][1]["DATA"]["value"],
            json!([item])
        );
        assert_eq!(
            validate(&tree, &Refs::load(&db, &[&tree]).await.unwrap()),
            vec![
                ("0.0.0".to_string(), format!("вариант {option} не найден")),
                ("0.0.1".to_string(), format!("запись {item} не найдена")),
            ]
        );
    }
}
