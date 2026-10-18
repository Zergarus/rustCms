//! Проверка условий правил (как код, который генерируют `Generate` контролов
//! `sale/general/sale_cond.php` и `catalog/general/catalog_cond.php`).
//!
//! Неизвестный класс, недопустимое `logic` или значение — условие не выполнено (у Битрикса
//! такое условие не генерируется, и правило не срабатывает).

use std::cmp::Ordering;
use std::collections::HashMap;

use serde_json::{Map, Value};

use super::context::{Context, CtxItem};
use super::model::{Node, as_num, prop_class};
use crate::bxapi::project::DiscountConditionHook;

/// Текущее состояние расчёта.
pub struct State<'a> {
    pub ctx: &'a Context,
    /// Текущие цены позиций (после уже применённых правил); нет — базовая.
    pub prices: &'a HashMap<i64, f64>,
    /// Позиция → применённые к ней правила.
    pub applied: &'a HashMap<i64, Vec<i64>>,
}

impl State<'_> {
    fn price(&self, item: &CtxItem) -> f64 {
        self.prices
            .get(&item.key)
            .copied()
            .unwrap_or(item.base_price)
    }
}

type Hook<'a> = Option<&'a dyn DiscountConditionHook>;

/// Условия правила (`conditions`).
pub fn check_order(node: &Node, st: &State, hook: Hook) -> bool {
    eval(node, None, st, hook)
}

/// Подходит ли позиция под условие: фильтр позиций в действиях (`ActSaleBsktGrp`,
/// `ActSaleSubGrp`) и группах по позициям.
pub fn check_item(node: &Node, item: &CtxItem, st: &State, hook: Hook) -> bool {
    eval(node, Some(item), st, hook)
}

fn eval(node: &Node, item: Option<&CtxItem>, st: &State, hook: Hook) -> bool {
    let class = node.class_id.as_str();
    if let Some(h) = hook
        && h.class_ids().contains(&class)
    {
        return h.check(class, &node.data, st.ctx, item).unwrap_or(false);
    }
    match class {
        "CondGroup" | "CondBsktSubGroup" | "ActSaleBsktGrp" | "ActSaleSubGrp" => {
            group(node, item, st, hook)
        }
        "CondBsktCntGroup" | "CondBsktAmtGroup" | "CondBsktAmtBaseGroup" | "CondBsktRowGroup" => {
            basket_total(node, st, hook)
        }
        "CondBsktProductGroup" => {
            let found = st.ctx.items.iter().any(|it| row_filter(node, it, st, hook));
            match node.data.get("Found").and_then(Value::as_str) {
                Some("Found") => found,
                Some("NoFound") => !found,
                _ => false,
            }
        }
        _ => leaf(node, item, st),
    }
}

/// Строковый ключ `DATA`; нет ключа — `default`.
fn atom<'a>(d: &'a Map<String, Value>, key: &str, default: &'static str) -> Option<&'a str> {
    match d.get(key) {
        None => Some(default),
        Some(v) => v.as_str(),
    }
}

/// `CGlobalCondCtrlGroup::Generate`: пустая группа истинна; `AND`+`False` — все ложны
/// (`!a && !b`), `OR`+`False` — хотя бы одно ложно (`!(a && b)`).
fn group(node: &Node, item: Option<&CtxItem>, st: &State, hook: Hook) -> bool {
    let (Some(all), Some(truth)) = (
        atom(&node.data, "All", "AND"),
        atom(&node.data, "True", "True"),
    ) else {
        return false;
    };
    if node.children.is_empty() {
        return matches!((all, truth), ("AND" | "OR", "True" | "False"));
    }
    let mut res = node.children.iter().map(|c| eval(c, item, st, hook));
    match (all, truth) {
        ("AND", "True") => res.all(|r| r),
        ("AND", "False") => res.all(|r| !r),
        ("OR", "True") => res.any(|r| r),
        ("OR", "False") => !res.all(|r| r),
        _ => false,
    }
}

/// Отбор позиций группы по позициям: вложенные условия через `All`, без вложенных — все.
fn row_filter(node: &Node, item: &CtxItem, st: &State, hook: Hook) -> bool {
    let mut res = node.children.iter().map(|c| eval(c, Some(item), st, hook));
    match atom(&node.data, "All", "AND") {
        Some("AND") => res.all(|r| r),
        Some("OR") => node.children.is_empty() || res.any(|r| r),
        _ => false,
    }
}

/// `CSaleBasketFilter::CountFilter` / `AmountFilter` / `AmountBaseFilter` / `RowFilter`.
fn basket_total(node: &Node, st: &State, hook: Hook) -> bool {
    let Some(logic) = logic(&node.data, L_NUM) else {
        return false;
    };
    let Some(value) = node.data.get("Value").and_then(as_num) else {
        return false;
    };
    let rows = st
        .ctx
        .items
        .iter()
        .filter(|it| row_filter(node, it, st, hook));
    let total: f64 = match node.class_id.as_str() {
        "CondBsktCntGroup" => rows.map(|it| it.quantity).sum(),
        "CondBsktAmtGroup" => rows.map(|it| st.price(it) * it.quantity).sum(),
        "CondBsktAmtBaseGroup" => rows.map(|it| it.base_price * it.quantity).sum(),
        _ => rows.count() as f64,
    };
    compare(&[Value::from(total)], logic, &[&Value::from(value)])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Logic {
    Equal,
    Not,
    Great,
    Less,
    EqGr,
    EqLs,
    Contain,
    NotCont,
}

use Logic::*;

const L_EQ: &[Logic] = &[Equal, Not];
const L_NUM: &[Logic] = &[Equal, Not, Great, Less, EqGr, EqLs];
const L_STR: &[Logic] = &[Equal, Not, Contain, NotCont];
/// Свойство: допустимое сравнение зависит от типа поля, его проверяет `validate`.
const L_ANY: &[Logic] = &[Equal, Not, Great, Less, EqGr, EqLs, Contain, NotCont];

/// `logic` узла, если оно из допустимых для класса (`CGlobalCondCtrl::CheckLogic`).
fn logic(d: &Map<String, Value>, allowed: &[Logic]) -> Option<Logic> {
    let l = match d.get("logic")?.as_str()? {
        "Equal" => Equal,
        "Not" => Not,
        "Great" => Great,
        "Less" => Less,
        "EqGr" => EqGr,
        "EqLs" => EqLs,
        "Contain" => Contain,
        "NotCont" => NotCont,
        _ => return None,
    };
    allowed.contains(&l).then_some(l)
}

/// Значения `value`: одно или список («любое из»); пусто — `None`.
fn values(d: &Map<String, Value>) -> Option<Vec<&Value>> {
    let list: Vec<&Value> = match d.get("value")? {
        Value::Array(a) => a.iter().collect(),
        Value::Null => return None,
        v => vec![v],
    };
    (!list.is_empty()).then_some(list)
}

const EPS: f64 = 1e-9;

/// Число как в PHP: число или числовая строка.
fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(_) | Value::String(_) => as_num(v),
        _ => None,
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "Y" } else { "N" }.to_string(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// `==` PHP 8: числа и числовые строки — как числа, иначе строки.
fn loose_eq(a: &Value, b: &Value) -> bool {
    match (num(a), num(b)) {
        (Some(x), Some(y)) => (x - y).abs() < EPS,
        _ => text(a) == text(b),
    }
}

fn order(a: &Value, b: &Value) -> Option<Ordering> {
    match (num(a), num(b)) {
        (Some(x), Some(y)) if (x - y).abs() < EPS => Some(Ordering::Equal),
        (Some(x), Some(y)) => x.partial_cmp(&y),
        _ => Some(text(a).cmp(&text(b))),
    }
}

fn contains(a: &Value, b: &Value) -> bool {
    text(a).to_lowercase().contains(&text(b).to_lowercase())
}

/// Сравнение значений поля `fields` (у одиночного — одно) со значениями условия: «равно» —
/// любое из (`in_array` через `||`), «не равно» — ни одно (через `&&`), сравнения порядка —
/// одно значение условия и любое непустое значение поля (`CGlobalCondCtrl::LogicGreat` …).
fn compare(fields: &[Value], logic: Logic, vals: &[&Value]) -> bool {
    let has = |v: &Value, f: fn(&Value, &Value) -> bool| fields.iter().any(|x| f(x, v));
    match logic {
        Equal => vals.iter().any(|v| has(v, loose_eq)),
        Not => vals.iter().all(|v| !has(v, loose_eq)),
        Contain => vals.iter().any(|v| has(v, contains)),
        NotCont => vals.iter().all(|v| !has(v, contains)),
        Great | Less | EqGr | EqLs => {
            let [v] = vals else {
                return false;
            };
            fields
                .iter()
                .filter(|x| !matches!(x, Value::Null | Value::Bool(false)) && !text(x).is_empty())
                .filter_map(|x| order(x, v))
                .any(|o| match logic {
                    Great => o == Ordering::Greater,
                    Less => o == Ordering::Less,
                    EqGr => o != Ordering::Less,
                    _ => o != Ordering::Greater,
                })
        }
    }
}

/// Поле с проверкой `logic` и `value`; `numeric` — значения условия обязаны быть числами
/// (поля типов `int`/`double`).
fn field(d: &Map<String, Value>, allowed: &[Logic], numeric: bool, fields: &[Value]) -> bool {
    let (Some(logic), Some(vals)) = (logic(d, allowed), values(d)) else {
        return false;
    };
    if numeric && vals.iter().any(|v| num(v).is_none()) {
        return false;
    }
    compare(fields, logic, &vals)
}

/// Поле товара, которое у предложения проверяется и у родителя (`PARENT => true`):
/// «равно»/«содержит» — у предложения или у родителя, «не равно»/«не содержит» — ни там,
/// ни там; сравнения порядка — только у самого предложения.
fn with_parent(
    d: &Map<String, Value>,
    allowed: &[Logic],
    numeric: bool,
    own: Value,
    parent: Option<Value>,
) -> bool {
    let own_res = field(d, allowed, numeric, &[own]);
    let Some(parent) = parent else {
        return own_res;
    };
    let parent_res = || field(d, allowed, numeric, &[parent]);
    match logic(d, allowed) {
        Some(Equal | Contain) => own_res || parent_res(),
        Some(Not | NotCont) => own_res && parent_res(),
        _ => own_res,
    }
}

/// Поле заказа: не задано — ложь при любом сравнении (`isset(...) && ...`).
fn order_id(d: &Map<String, Value>, id: Option<i64>) -> bool {
    id.is_some_and(|id| field(d, L_EQ, true, &[Value::from(id)]))
}

/// `ORDER_PRICE` `Sale\DiscountBase`: сумма базовых цен корзины (`BasketBase::getBasePrice`,
/// каждая строка округлена) плюс базовая цена доставки заказа; от скидок не меняется.
fn order_price(ctx: &Context) -> f64 {
    let basket: f64 = ctx
        .items
        .iter()
        .map(|it| round2(it.base_price * it.quantity))
        .sum();
    basket + ctx.order.delivery_price.unwrap_or(0.0)
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn leaf(node: &Node, item: Option<&CtxItem>, st: &State) -> bool {
    let d = &node.data;
    let class = node.class_id.as_str();
    if let Some((_, field_id)) = prop_class(class) {
        return item.is_some_and(|it| prop(d, it, field_id));
    }
    let ctx = st.ctx;
    match class {
        "CondSaleOrderSumm" => field(d, L_NUM, true, &[Value::from(order_price(ctx))]),
        "CondSaleOrderWeight" => {
            let weight: f64 = ctx.items.iter().map(|it| it.weight * it.quantity).sum();
            field(d, L_NUM, true, &[Value::from(weight)])
        }
        "CondSalePersonType" => order_id(d, ctx.order.person_type_id),
        "CondSalePaySystem" => order_id(d, ctx.order.pay_system_id),
        "CondSaleDelivery" => order_id(d, ctx.order.delivery_id),
        "CondSaleCmnDayOfWeek" => day_of_week(d, ctx.day_of_week()),
        _ => item.is_some_and(|it| item_leaf(class, d, it, st)),
    }
}

/// `CSaleCondCtrlCommon::Generate`: несколько дней соединяются через `||` при любом `logic`.
fn day_of_week(d: &Map<String, Value>, day: u32) -> bool {
    let (Some(logic), Some(vals)) = (logic(d, L_EQ), values(d)) else {
        return false;
    };
    let Some(days) = vals.iter().map(|v| num(v)).collect::<Option<Vec<f64>>>() else {
        return false;
    };
    let day = f64::from(day);
    days.iter()
        .any(|&v| ((v - day).abs() < EPS) == (logic == Equal))
}

fn item_leaf(class: &str, d: &Map<String, Value>, it: &CtxItem, st: &State) -> bool {
    let num_field = |v: f64| field(d, L_NUM, true, &[Value::from(v)]);
    let str_parent = |own: &str, parent: Option<&str>| {
        with_parent(d, L_STR, false, Value::from(own), parent.map(Value::from))
    };
    let parent = it.parent.as_ref();
    match class {
        "CondBsktFldProduct" => field(d, L_EQ, true, &[Value::from(it.product_id)]),
        "CondBsktFldName" => field(d, L_STR, false, &[Value::from(it.name.as_str())]),
        "CondBsktFldPrice" => num_field(st.price(it)),
        "CondBsktFldQuantity" => num_field(it.quantity),
        "CondBsktFldSumm" => num_field(st.price(it) * it.quantity),
        "CondBsktFldWeight" | "CondCatWeight" => num_field(it.weight),
        "CondCatQuantity" => num_field(it.stock_quantity),
        "CondIBElement" => with_parent(
            d,
            L_EQ,
            true,
            Value::from(it.product_id),
            it.parent_id.map(Value::from),
        ),
        "CondIBIBlock" => with_parent(
            d,
            L_EQ,
            true,
            Value::from(it.collection_id),
            parent.map(|p| Value::from(p.collection_id)),
        ),
        "CondIBSection" => {
            let sections: Vec<Value> = it.section_ids.iter().map(|&s| Value::from(s)).collect();
            field(d, L_EQ, true, &sections)
        }
        "CondIBXmlID" => str_parent(&it.xml_id, parent.map(|p| p.xml_id.as_str())),
        "CondIBName" => str_parent(&it.name, parent.map(|p| p.name.as_str())),
        "CondIBCode" => str_parent(&it.code, parent.map(|p| p.code.as_str())),
        "CondBsktAppliedDiscount" => {
            let applied = st.applied.get(&it.key).is_some_and(|r| !r.is_empty());
            // Совместимость, как `CSaleCondCtrlBasketItemConditions::Parse`: «logic: Not»
            // переворачивает значение
            let not = d.get("logic").and_then(Value::as_str) == Some("Not");
            match (d.get("value").and_then(Value::as_str), not) {
                (Some("Y"), false) | (Some("N"), true) => applied,
                (Some("N"), false) | (Some("Y"), true) => !applied,
                _ => false,
            }
        }
        _ => false,
    }
}

/// `CondIBProp:<коллекция>:<поле>` (`CCatalogCondCtrlIBlockProps`, `MULTIPLE => 'Y'`): поля
/// нет у коллекции записи (и у родителя предложения) — ложь при любом сравнении; поле без
/// значения — пустая строка (`fillEmptyProperties`), так что «не равно» истинно.
fn prop(d: &Map<String, Value>, it: &CtxItem, field_id: i64) -> bool {
    let Some(code) = it.field_ids.get(&field_id) else {
        return false;
    };
    let list = match it.fields.get(code) {
        None | Some(Value::Null) => vec![Value::from("")],
        Some(Value::Array(a)) if a.is_empty() => vec![Value::from("")],
        Some(Value::Array(a)) => a.clone(),
        Some(v) => vec![v.clone()],
    };
    field(d, L_ANY, false, &list)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::{Map, Value, json};

    use super::*;
    use crate::discount::context::{CtxParent, OrderCtx};

    fn node(v: Value) -> Node {
        serde_json::from_value(v).unwrap()
    }

    fn leaf(class: &str, logic: &str, value: Value) -> Node {
        node(json!({"CLASS_ID": class, "DATA": {"logic": logic, "value": value}}))
    }

    fn grp(class: &str, data: Value, children: Vec<Node>) -> Node {
        Node {
            class_id: class.to_string(),
            data: serde_json::from_value(data).unwrap(),
            children,
        }
    }

    fn and(children: Vec<Node>) -> Node {
        grp("CondGroup", json!({"All": "AND", "True": "True"}), children)
    }

    fn fields(v: Value) -> Map<String, Value> {
        serde_json::from_value(v).unwrap()
    }

    /// Товар коллекции 4: разделы 3 и его предок 1, цена 1000, со скидкой уже 900.
    fn goods() -> CtxItem {
        CtxItem {
            key: 1,
            product_id: 10,
            parent_id: None,
            collection_id: 4,
            parent: None,
            section_ids: vec![3, 1],
            xml_id: "X-10".to_string(),
            name: "Шина Летняя".to_string(),
            code: "shina-letnyaya".to_string(),
            fields: fields(
                json!({"transopt": 32, "brand": "Nokian", "width": 205, "tags": [7, 8]}),
            ),
            field_ids: HashMap::from([
                (55, "transopt".to_string()),
                (56, "brand".to_string()),
                (57, "width".to_string()),
                (58, "tags".to_string()),
            ]),
            quantity: 2.0,
            base_price: 1000.0,
            custom_price: false,
            weight: 9.5,
            stock_quantity: 12.0,
        }
    }

    /// Предложение коллекции 5 товара 11 коллекции 4; поля и разделы — вместе с родителем.
    fn offer() -> CtxItem {
        CtxItem {
            key: 2,
            product_id: 20,
            parent_id: Some(11),
            collection_id: 5,
            parent: Some(CtxParent {
                collection_id: 4,
                xml_id: "X-11".to_string(),
                name: "Диск".to_string(),
                code: "disk".to_string(),
            }),
            section_ids: vec![2, 1],
            xml_id: "X-20".to_string(),
            name: "Диск R16".to_string(),
            code: "disk-r16".to_string(),
            fields: fields(json!({"transopt": [32], "color": 41})),
            field_ids: HashMap::from([(55, "transopt".to_string()), (60, "color".to_string())]),
            quantity: 1.0,
            base_price: 500.0,
            custom_price: false,
            weight: 7.0,
            stock_quantity: 0.0,
        }
    }

    fn ctx() -> Context {
        Context {
            items: vec![goods(), offer()],
            user_id: Some(5),
            group_ids: vec![2],
            coupons: Vec::new(),
            order: OrderCtx {
                person_type_id: Some(1),
                pay_system_id: Some(2),
                delivery_id: Some(3),
                delivery_price: Some(300.0),
            },
            // Вторник 22:30 UTC — среда 01:30 по МСК
            now: Utc.with_ymd_and_hms(2026, 10, 13, 22, 30, 0).unwrap(),
        }
    }

    struct Fixture {
        ctx: Context,
        prices: HashMap<i64, f64>,
        applied: HashMap<i64, Vec<i64>>,
    }

    impl Fixture {
        fn new(ctx: Context) -> Fixture {
            Fixture {
                ctx,
                prices: HashMap::from([(1, 900.0)]),
                applied: HashMap::from([(1, vec![7])]),
            }
        }

        fn st(&self) -> State<'_> {
            State {
                ctx: &self.ctx,
                prices: &self.prices,
                applied: &self.applied,
            }
        }

        fn order(&self, n: &Node) -> bool {
            check_order(n, &self.st(), None)
        }

        fn item(&self, n: &Node, i: usize) -> bool {
            check_item(n, &self.ctx.items[i], &self.st(), None)
        }
    }

    fn f() -> Fixture {
        Fixture::new(ctx())
    }

    const GOODS: usize = 0;
    const OFFER: usize = 1;

    fn pay(id: i64) -> Node {
        leaf("CondSalePaySystem", "Equal", json!([id]))
    }

    #[test]
    fn cond_group() {
        let f = f();
        assert!(f.order(&and(vec![])));
        assert!(f.order(&grp(
            "CondGroup",
            json!({"All": "OR", "True": "False"}),
            vec![]
        )));
        assert!(f.order(&and(vec![pay(2)])));
        assert!(!f.order(&and(vec![pay(9)])));
    }

    #[test]
    fn and_or_false_groups() {
        let f = f();
        let g = |all: &str, t: &str, children: Vec<Node>| {
            f.order(&grp("CondGroup", json!({"All": all, "True": t}), children))
        };
        // Все условия выполнены
        assert!(g("AND", "True", vec![pay(2), pay(2)]));
        assert!(!g("AND", "True", vec![pay(2), pay(9)]));
        // Все условия не выполнены: !a && !b
        assert!(g("AND", "False", vec![pay(9), pay(8)]));
        assert!(!g("AND", "False", vec![pay(2), pay(9)]));
        // Любое выполнено
        assert!(g("OR", "True", vec![pay(2), pay(9)]));
        assert!(!g("OR", "True", vec![pay(9), pay(8)]));
        // Любое не выполнено: !(a && b)
        assert!(g("OR", "False", vec![pay(2), pay(9)]));
        assert!(!g("OR", "False", vec![pay(2), pay(2)]));
        // Вложенная группа
        assert!(g("AND", "True", vec![pay(2), g_or(vec![pay(9), pay(2)])]));
        assert!(!g("AND", "True", vec![pay(2), g_or(vec![pay(9), pay(8)])]));
    }

    fn g_or(children: Vec<Node>) -> Node {
        grp("CondGroup", json!({"All": "OR", "True": "True"}), children)
    }

    fn cnt(class: &str, logic: &str, value: f64, all: &str, children: Vec<Node>) -> Node {
        grp(
            class,
            json!({"logic": logic, "Value": value, "All": all}),
            children,
        )
    }

    #[test]
    fn cond_bskt_cnt_group() {
        let f = f();
        // Всего 2 + 1 штуки
        assert!(f.order(&cnt("CondBsktCntGroup", "Equal", 3.0, "AND", vec![])));
        assert!(!f.order(&cnt("CondBsktCntGroup", "Great", 3.0, "AND", vec![])));
        // Только позиции с текущей ценой > 600 — товар, 2 штуки
        let price = leaf("CondBsktFldPrice", "Great", json!(600));
        assert!(f.order(&cnt(
            "CondBsktCntGroup",
            "EqLs",
            2.0,
            "AND",
            vec![price.clone()]
        )));
        assert!(!f.order(&cnt("CondBsktCntGroup", "Less", 2.0, "AND", vec![price])));
    }

    #[test]
    fn cond_bskt_amt_group() {
        let f = f();
        // По текущим ценам: 900 × 2 + 500
        assert!(f.order(&cnt("CondBsktAmtGroup", "Equal", 2300.0, "AND", vec![])));
        assert!(!f.order(&cnt("CondBsktAmtGroup", "Not", 2300.0, "AND", vec![])));
    }

    #[test]
    fn cond_bskt_amt_base_group() {
        let f = f();
        // По базовым ценам: 1000 × 2 + 500
        assert!(f.order(&cnt("CondBsktAmtBaseGroup", "EqGr", 2500.0, "AND", vec![])));
        assert!(!f.order(&cnt("CondBsktAmtBaseGroup", "Great", 2500.0, "AND", vec![])));
    }

    #[test]
    fn basket_subgroups_count_amount_base_amount() {
        let f = f();
        // Позиции не из раздела 3 (подгруппа «все условия не выполнены») или дороже 2000
        let filter = || {
            vec![
                grp(
                    "CondBsktSubGroup",
                    json!({"All": "AND", "True": "False"}),
                    vec![leaf("CondIBSection", "Equal", json!([3]))],
                ),
                leaf("CondBsktFldPrice", "Great", json!(2000)),
            ]
        };
        assert!(f.order(&cnt("CondBsktCntGroup", "Equal", 1.0, "OR", filter())));
        assert!(f.order(&cnt("CondBsktAmtGroup", "Equal", 500.0, "OR", filter())));
        assert!(f.order(&cnt("CondBsktAmtBaseGroup", "Equal", 500.0, "OR", filter())));
        assert!(f.order(&cnt("CondBsktRowGroup", "Equal", 1.0, "OR", filter())));
        // И то и другое — ни одной позиции
        assert!(f.order(&cnt("CondBsktCntGroup", "Equal", 0.0, "AND", filter())));
        assert!(f.order(&cnt("CondBsktAmtGroup", "Equal", 0.0, "AND", filter())));
        assert!(!f.order(&cnt("CondBsktAmtBaseGroup", "Great", 0.0, "AND", filter())));
    }

    #[test]
    fn cond_bskt_product_group() {
        let f = f();
        let found = |v: &str, children: Vec<Node>| {
            f.order(&grp(
                "CondBsktProductGroup",
                json!({"Found": v, "All": "AND"}),
                children,
            ))
        };
        assert!(found(
            "Found",
            vec![leaf("CondIBElement", "Equal", json!([10]))]
        ));
        assert!(!found(
            "NoFound",
            vec![leaf("CondIBElement", "Equal", json!([10]))]
        ));
        assert!(!found(
            "Found",
            vec![leaf("CondIBElement", "Equal", json!([99]))]
        ));
        assert!(found(
            "NoFound",
            vec![leaf("CondIBElement", "Equal", json!([99]))]
        ));
        assert!(found("Found", vec![]));
    }

    #[test]
    fn cond_bskt_row_group() {
        let f = f();
        assert!(f.order(&cnt("CondBsktRowGroup", "Equal", 2.0, "AND", vec![])));
        assert!(!f.order(&cnt("CondBsktRowGroup", "Great", 2.0, "AND", vec![])));
    }

    #[test]
    fn cond_bskt_sub_group() {
        let f = f();
        let sub = |all: &str, t: &str| {
            grp(
                "CondBsktSubGroup",
                json!({"All": all, "True": t}),
                vec![
                    leaf("CondBsktFldName", "Contain", json!("r16")),
                    leaf("CondBsktFldName", "Equal", json!("x")),
                ],
            )
        };
        assert!(f.item(&sub("OR", "True"), OFFER));
        assert!(!f.item(&sub("AND", "True"), OFFER));
        assert!(!f.item(&sub("OR", "True"), GOODS));
        assert!(f.item(&sub("AND", "False"), GOODS));
        assert!(f.item(
            &grp(
                "CondBsktSubGroup",
                json!({"All": "AND", "True": "True"}),
                vec![]
            ),
            GOODS
        ));
    }

    #[test]
    fn cond_bskt_fld_product() {
        let f = f();
        assert!(f.item(&leaf("CondBsktFldProduct", "Equal", json!(10)), GOODS));
        // Позиция — само предложение, родителя поле не учитывает
        assert!(!f.item(&leaf("CondBsktFldProduct", "Equal", json!(11)), OFFER));
        assert!(f.item(&leaf("CondBsktFldProduct", "Not", json!(11)), OFFER));
        assert!(!f.item(&leaf("CondBsktFldProduct", "Not", json!([20, 30])), OFFER));
    }

    #[test]
    fn cond_bskt_fld_name() {
        let f = f();
        assert!(f.item(
            &leaf("CondBsktFldName", "Equal", json!("Шина Летняя")),
            GOODS
        ));
        assert!(f.item(&leaf("CondBsktFldName", "Contain", json!("летняя")), GOODS));
        assert!(!f.item(&leaf("CondBsktFldName", "NotCont", json!("ЛЕТНЯЯ")), GOODS));
        assert!(!f.item(&leaf("CondBsktFldName", "Not", json!("Шина Летняя")), GOODS));
    }

    #[test]
    fn cond_bskt_fld_price() {
        let f = f();
        // Текущая цена, не базовая
        assert!(f.item(&leaf("CondBsktFldPrice", "Equal", json!(900)), GOODS));
        assert!(!f.item(&leaf("CondBsktFldPrice", "EqGr", json!(1000)), GOODS));
        // Без текущей цены — базовая
        assert!(f.item(&leaf("CondBsktFldPrice", "Equal", json!("500")), OFFER));
    }

    #[test]
    fn cond_bskt_fld_quantity() {
        let f = f();
        assert!(f.item(&leaf("CondBsktFldQuantity", "EqGr", json!(2)), GOODS));
        assert!(!f.item(&leaf("CondBsktFldQuantity", "Less", json!(2)), GOODS));
    }

    #[test]
    fn cond_bskt_fld_summ() {
        let f = f();
        assert!(f.item(&leaf("CondBsktFldSumm", "Equal", json!(1800)), GOODS));
        assert!(!f.item(&leaf("CondBsktFldSumm", "Great", json!(1800)), GOODS));
    }

    #[test]
    fn cond_bskt_fld_weight() {
        let f = f();
        assert!(f.item(&leaf("CondBsktFldWeight", "EqLs", json!(9.5)), GOODS));
        assert!(!f.item(&leaf("CondBsktFldWeight", "Great", json!(9.5)), GOODS));
    }

    #[test]
    fn cond_ib_element() {
        let f = f();
        assert!(f.item(&leaf("CondIBElement", "Equal", json!([10, 99])), GOODS));
        assert!(!f.item(&leaf("CondIBElement", "Equal", json!([11])), GOODS));
        // У предложения — и родитель
        assert!(f.item(&leaf("CondIBElement", "Equal", json!([11])), OFFER));
        assert!(f.item(&leaf("CondIBElement", "Equal", json!(20)), OFFER));
        assert!(!f.item(&leaf("CondIBElement", "Not", json!([11])), OFFER));
        assert!(f.item(&leaf("CondIBElement", "Not", json!([12])), OFFER));
    }

    #[test]
    fn cond_ib_iblock() {
        let f = f();
        assert!(f.item(&leaf("CondIBIBlock", "Equal", json!([4])), GOODS));
        assert!(!f.item(&leaf("CondIBIBlock", "Equal", json!([5])), GOODS));
        assert!(f.item(&leaf("CondIBIBlock", "Equal", json!([4])), OFFER));
        assert!(f.item(&leaf("CondIBIBlock", "Equal", json!([5])), OFFER));
        assert!(!f.item(&leaf("CondIBIBlock", "Not", json!([4])), OFFER));
    }

    #[test]
    fn cond_ib_section() {
        let f = f();
        // Раздел-предок тоже считается
        assert!(f.item(&leaf("CondIBSection", "Equal", json!([1])), GOODS));
        assert!(!f.item(&leaf("CondIBSection", "Not", json!([3])), GOODS));
        assert!(!f.item(&leaf("CondIBSection", "Equal", json!([3])), OFFER));
        assert!(f.item(&leaf("CondIBSection", "Not", json!([3, 4])), OFFER));
    }

    #[test]
    fn cond_ib_xml_id() {
        let f = f();
        assert!(f.item(&leaf("CondIBXmlID", "Equal", json!("X-11")), OFFER));
        assert!(!f.item(&leaf("CondIBXmlID", "Equal", json!("X-11")), GOODS));
        assert!(f.item(&leaf("CondIBXmlID", "Contain", json!("x-1")), GOODS));
    }

    #[test]
    fn cond_ib_name() {
        let f = f();
        assert!(f.item(&leaf("CondIBName", "Contain", json!("диск")), OFFER));
        assert!(!f.item(&leaf("CondIBName", "NotCont", json!("диск")), OFFER));
        assert!(f.item(&leaf("CondIBName", "Equal", json!("Диск")), OFFER));
        assert!(!f.item(&leaf("CondIBName", "Equal", json!("Диск")), GOODS));
    }

    #[test]
    fn cond_ib_code() {
        let f = f();
        assert!(f.item(&leaf("CondIBCode", "Equal", json!("disk-r16")), OFFER));
        // Не равно — ни у предложения, ни у родителя
        assert!(!f.item(&leaf("CondIBCode", "Not", json!("disk")), OFFER));
        assert!(f.item(&leaf("CondIBCode", "Not", json!("disk")), GOODS));
    }

    #[test]
    fn cond_ib_prop() {
        let f = f();
        let p =
            |field: i64, logic: &str, v: Value| leaf(&format!("CondIBProp:4:{field}"), logic, v);
        assert!(f.item(&p(56, "Equal", json!("Nokian")), GOODS));
        assert!(f.item(&p(56, "Contain", json!("nok")), GOODS));
        assert!(!f.item(&p(56, "NotCont", json!("NOK")), GOODS));
        assert!(f.item(&p(57, "Great", json!(200)), GOODS));
        assert!(!f.item(&p(57, "Less", json!(200)), GOODS));
        // Множественное — любое из значений
        assert!(f.item(&p(58, "Equal", json!(8)), GOODS));
        assert!(f.item(&p(58, "Equal", json!([9, 7])), GOODS));
        assert!(!f.item(&p(58, "Not", json!([7])), GOODS));
        assert!(f.item(&p(58, "Not", json!([9])), GOODS));
        // Поля нет у коллекции записи — ложь при любом сравнении
        assert!(!f.item(&p(60, "Not", json!(1)), GOODS));
        assert!(!f.item(&p(56, "Not", json!("x")), OFFER));
        // Поле есть, значение другое — «не равно» истинно
        assert!(f.item(&p(60, "Not", json!(1)), OFFER));
        let mut empty = goods();
        empty.fields.insert("brand".to_string(), Value::Null);
        let fx = Fixture::new(Context {
            items: vec![empty],
            ..ctx()
        });
        assert!(fx.item(&p(56, "Not", json!("Nokian")), 0));
        assert!(!fx.item(&p(56, "Equal", json!("Nokian")), 0));
        assert!(!fx.item(&p(56, "Contain", json!("N")), 0));
    }

    #[test]
    fn cond_cat_quantity() {
        let f = f();
        assert!(f.item(&leaf("CondCatQuantity", "Great", json!(10)), GOODS));
        assert!(!f.item(&leaf("CondCatQuantity", "Great", json!(0)), OFFER));
    }

    #[test]
    fn cond_cat_weight() {
        let f = f();
        assert!(f.item(&leaf("CondCatWeight", "Equal", json!(7)), OFFER));
        assert!(!f.item(&leaf("CondCatWeight", "Less", json!(7)), OFFER));
    }

    #[test]
    fn cond_sale_order_summ() {
        let f = f();
        // Базовые цены 2500 + доставка 300
        assert!(f.order(&leaf("CondSaleOrderSumm", "Equal", json!(2800))));
        assert!(!f.order(&leaf("CondSaleOrderSumm", "Great", json!(2800))));
        // В корзине доставки ещё нет
        let mut c = ctx();
        c.order = OrderCtx::default();
        let fx = Fixture::new(c);
        assert!(fx.order(&leaf("CondSaleOrderSumm", "Equal", json!(2500))));
    }

    #[test]
    fn cond_sale_order_weight() {
        let f = f();
        assert!(f.order(&leaf("CondSaleOrderWeight", "Equal", json!(26))));
        assert!(!f.order(&leaf("CondSaleOrderWeight", "Less", json!(26))));
    }

    #[test]
    fn cond_sale_person_type() {
        let f = f();
        assert!(f.order(&leaf("CondSalePersonType", "Equal", json!([1]))));
        assert!(!f.order(&leaf("CondSalePersonType", "Not", json!([1, 2]))));
        // Не выбран — ложь при любом сравнении
        let mut c = ctx();
        c.order.person_type_id = None;
        let fx = Fixture::new(c);
        assert!(!fx.order(&leaf("CondSalePersonType", "Not", json!([1]))));
        assert!(!fx.order(&leaf("CondSalePersonType", "Equal", json!([1]))));
    }

    #[test]
    fn cond_sale_pay_system() {
        let f = f();
        assert!(f.order(&leaf("CondSalePaySystem", "Equal", json!([3, 2]))));
        assert!(!f.order(&leaf("CondSalePaySystem", "Equal", json!([3]))));
    }

    #[test]
    fn cond_sale_delivery() {
        let f = f();
        assert!(f.order(&leaf("CondSaleDelivery", "Equal", json!(["3"]))));
        assert!(!f.order(&leaf("CondSaleDelivery", "Not", json!([3]))));
        assert!(f.order(&leaf("CondSaleDelivery", "Not", json!([4]))));
    }

    #[test]
    fn cond_sale_cmn_day_of_week() {
        let f = f();
        // Среда по МСК
        assert!(f.order(&leaf("CondSaleCmnDayOfWeek", "Equal", json!([3]))));
        assert!(!f.order(&leaf("CondSaleCmnDayOfWeek", "Equal", json!([2]))));
        assert!(!f.order(&leaf("CondSaleCmnDayOfWeek", "Not", json!([3]))));
        // Как Битрикс: значения «не равно» соединяются через «или»
        assert!(f.order(&leaf("CondSaleCmnDayOfWeek", "Not", json!([3, 4]))));
    }

    fn applied(v: &str) -> Node {
        node(json!({"CLASS_ID": "CondBsktAppliedDiscount", "DATA": {"value": v}}))
    }

    #[test]
    fn cond_bskt_applied_discount() {
        let f = f();
        assert!(f.item(&applied("Y"), GOODS));
        assert!(!f.item(&applied("N"), GOODS));
        assert!(f.item(&applied("N"), OFFER));
        assert!(!f.item(&applied("Y"), OFFER));
    }

    #[test]
    fn applied_discount_condition() {
        let mut f = f();
        let none_applied = cnt("CondBsktCntGroup", "Equal", 1.0, "AND", vec![applied("N")]);
        assert!(f.order(&none_applied));
        let product = grp(
            "CondBsktProductGroup",
            json!({"Found": "Found", "All": "AND"}),
            vec![applied("N"), leaf("CondIBElement", "Equal", json!([11]))],
        );
        assert!(f.order(&product));
        // Правило уже применено и к предложению
        f.applied.insert(2, vec![8]);
        assert!(!f.order(&none_applied));
        assert!(!f.order(&product));
        // Пустой список правил — скидки нет
        f.applied.insert(2, vec![]);
        assert!(f.order(&product));
        // Совместимость: старое «logic: Not» переворачивает значение
        let legacy = node(
            json!({"CLASS_ID": "CondBsktAppliedDiscount", "DATA": {"logic": "Not", "value": "Y"}}),
        );
        assert!(f.item(&legacy, OFFER));
        assert!(!f.item(&legacy, GOODS));
    }

    #[test]
    fn offer_matches_parent_section_and_props() {
        let f = f();
        // Раздел 1 — раздел родителя предложения; свойство 55 — поле родителя
        let filter = grp(
            "ActSaleBsktGrp",
            json!({"Type": "Discount", "Value": 10, "Unit": "Perc", "Max": 0, "All": "AND", "True": "True"}),
            vec![
                leaf("CondIBSection", "Equal", json!([1])),
                leaf("CondIBProp:4:55", "Equal", json!(32)),
                leaf("CondIBIBlock", "Equal", json!([4])),
                leaf("CondIBXmlID", "Equal", json!("X-11")),
            ],
        );
        assert!(f.item(&filter, OFFER));
        assert!(!f.item(&filter, GOODS));
        let sub = grp(
            "ActSaleSubGrp",
            json!({"All": "OR", "True": "True"}),
            vec![
                leaf("CondIBElement", "Equal", json!([11])),
                leaf("CondIBCode", "Equal", json!("x")),
            ],
        );
        assert!(f.item(&sub, OFFER));
        assert!(!f.item(&sub, GOODS));
    }

    struct Vip;

    impl DiscountConditionHook for Vip {
        fn class_ids(&self) -> &[&'static str] {
            &["CondProjVip", "CondProjHeavy"]
        }

        fn check(
            &self,
            class_id: &str,
            data: &Map<String, Value>,
            ctx: &Context,
            item: Option<&CtxItem>,
        ) -> Option<bool> {
            match (class_id, item) {
                ("CondProjVip", None) => Some(ctx.user_id == data["user"].as_i64()),
                ("CondProjHeavy", Some(item)) => Some(item.weight > 8.0),
                _ => None,
            }
        }
    }

    #[test]
    fn hook_class_is_checked() {
        let f = f();
        let st = f.st();
        let vip = |user: i64| node(json!({"CLASS_ID": "CondProjVip", "DATA": {"user": user}}));
        assert!(check_order(&and(vec![vip(5)]), &st, Some(&Vip)));
        assert!(!check_order(&and(vec![vip(6)]), &st, Some(&Vip)));
        // Без модуля проекта класс неизвестен — ложь
        assert!(!check_order(&and(vec![vip(5)]), &st, None));
        let heavy = node(json!({"CLASS_ID": "CondProjHeavy", "DATA": {}}));
        let products = cnt("CondBsktCntGroup", "Equal", 2.0, "AND", vec![heavy.clone()]);
        assert!(check_order(&and(vec![products.clone()]), &st, Some(&Vip)));
        assert!(!check_order(&and(vec![products]), &st, None));
        assert!(check_item(&heavy, &f.ctx.items[GOODS], &st, Some(&Vip)));
        assert!(!check_item(&heavy, &f.ctx.items[OFFER], &st, Some(&Vip)));
        // Ответ None — условие не выполнено
        assert!(!check_order(&and(vec![heavy]), &st, Some(&Vip)));
    }

    #[test]
    fn rule_with_dangling_reference() {
        let f = f();
        for i in [GOODS, OFFER] {
            assert!(!f.item(&leaf("CondIBElement", "Equal", json!([999])), i));
            assert!(!f.item(&leaf("CondIBSection", "Equal", json!([999])), i));
            assert!(!f.item(&leaf("CondIBIBlock", "Equal", json!([999])), i));
            assert!(!f.item(&leaf("CondIBProp:4:999", "Equal", json!(1)), i));
            assert!(!f.item(&leaf("CondIBProp:4:999", "Not", json!(1)), i));
            assert!(!f.item(&leaf("CondIBProp:4:55", "Equal", json!(999)), i));
        }
        assert!(!f.order(&and(vec![leaf("CondSalePaySystem", "Equal", json!([999]))])));
        assert!(!f.order(&and(vec![leaf("CondSaleDelivery", "Equal", json!([999]))])));
        // Неизвестный класс и испорченные данные — ложь
        assert!(!f.order(&and(vec![node(json!({"CLASS_ID": "CondFoo", "DATA": {}}))])));
        assert!(!f.item(&leaf("CondBsktFldPrice", "Contain", json!(1)), GOODS));
        assert!(!f.item(&leaf("CondBsktFldPrice", "Great", json!("abc")), GOODS));
        assert!(!f.order(&grp(
            "CondGroup",
            json!({"All": "XOR", "True": "True"}),
            vec![pay(2)]
        )));
    }

    /// Фильтр действия правил трансопта №24–27 (`ActSaleBsktGrp` с `CondIBProp:4:55` `Equal` 32).
    #[test]
    fn transopt_property_condition() {
        let filter = node(json!({
            "CLASS_ID": "ActSaleBsktGrp",
            "DATA": {"Type": "Extra", "Value": 10.0, "Unit": "Perc", "Max": 0, "All": "AND", "True": "True"},
            "CHILDREN": [{"CLASS_ID": "CondIBProp:4:55", "DATA": {"logic": "Equal", "value": 32}}]
        }));
        let item = |v: Option<Value>| {
            let mut it = goods();
            it.fields = Map::new();
            if let Some(v) = v {
                it.fields.insert("transopt".to_string(), v);
            }
            it
        };
        let fx = Fixture::new(Context {
            items: vec![
                item(Some(json!(32))),
                item(Some(json!([32]))),
                item(Some(json!("32"))),
                item(None),
                item(Some(Value::Null)),
                item(Some(json!(33))),
            ],
            ..ctx()
        });
        assert!(fx.item(&filter, 0));
        assert!(fx.item(&filter, 1));
        assert!(fx.item(&filter, 2));
        assert!(!fx.item(&filter, 3));
        assert!(!fx.item(&filter, 4));
        assert!(!fx.item(&filter, 5));
        // Условия правил — пустая группа
        let conditions = node(
            json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": []}),
        );
        assert!(fx.order(&conditions));
    }
}
