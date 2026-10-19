//! Периоды накопительных скидок (`CumulativeCalculator::createFilterBySumConfiguration`
//! Битрикса): за какой срок считается сумма оплаченных заказов покупателя.

use chrono::{DateTime, Duration, Months, Utc};
use serde_json::{Map, Value};

use super::model::{Node, Rule, as_num};

/// Период накопленной суммы; границы — unix-секунды.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CumulativePeriod {
    AllTime,
    /// `created_at >= start` (если задано) и `< end` (если задано).
    Interval {
        start: Option<i64>,
        end: Option<i64>,
    },
    /// Последние `value` дней / месяцев / лет (`unit` — `D`/`M`/`Y`).
    Relative {
        value: i64,
        unit: char,
    },
}

/// Unix-время атома; пусто и 0 — не задано (`!empty(...)` Битрикса).
fn timestamp(v: Option<&Value>) -> Option<i64> {
    v.and_then(as_num)
        .map(|n| n.trunc() as i64)
        .filter(|&n| n != 0)
}

fn interval(start: Option<&Value>, end: Option<&Value>) -> CumulativePeriod {
    match (timestamp(start), timestamp(end)) {
        (None, None) => CumulativePeriod::AllTime,
        (start, end) => CumulativePeriod::Interval { start, end },
    }
}

/// `(int)$value` и тип `D`/`M`/`Y`; другой тип — за всё время, как Битрикс.
fn relative(value: Option<&Value>, unit: Option<&Value>) -> CumulativePeriod {
    let unit = match unit.and_then(Value::as_str) {
        Some("D") => 'D',
        Some("M") => 'M',
        Some("Y") => 'Y',
        _ => return CumulativePeriod::AllTime,
    };
    let value = value.and_then(as_num).map_or(0, |n| n.trunc() as i64);
    if value < 0 {
        return CumulativePeriod::AllTime;
    }
    CumulativePeriod::Relative { value, unit }
}

/// Период условия `CondCumulativeGroup`: первый вложенный `Period`/`PeriodRelative`
/// (`$subs[0]` Битрикса); без него — за всё время.
pub fn from_condition(node: &Node) -> CumulativePeriod {
    let Some(child) = node.children.first() else {
        return CumulativePeriod::AllTime;
    };
    let d = &child.data;
    match child.class_id.as_str() {
        "Period" => interval(d.get("ValueStart"), d.get("ValueEnd")),
        "PeriodRelative" => relative(d.get("Value"), d.get("TypeRelativePeriod")),
        _ => CumulativePeriod::AllTime,
    }
}

/// Период действия `CumulativeAction`: `type_sum_period` и `sum_period_data`.
pub fn from_action(data: &Map<String, Value>) -> CumulativePeriod {
    let period = data.get("sum_period_data").and_then(Value::as_object);
    let get = |key: &str| period.and_then(|p| p.get(key));
    match data.get("type_sum_period").and_then(Value::as_str) {
        Some("interval") => interval(
            get("discount_sum_order_start"),
            get("discount_sum_order_end"),
        ),
        Some("relative") => relative(
            get("discount_sum_period_value"),
            get("discount_sum_period_type"),
        ),
        _ => CumulativePeriod::AllTime,
    }
}

/// Уникальные периоды условий и действий правил, в порядке обхода.
pub fn periods(rules: &[Rule]) -> Vec<CumulativePeriod> {
    fn walk(node: &Node, out: &mut Vec<CumulativePeriod>) {
        let found = match node.class_id.as_str() {
            "CondCumulativeGroup" => Some(from_condition(node)),
            "CumulativeAction" => Some(from_action(&node.data)),
            _ => None,
        };
        if let Some(p) = found {
            if !out.contains(&p) {
                out.push(p);
            }
            return;
        }
        for child in &node.children {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    for rule in rules {
        walk(&rule.conditions, &mut out);
        walk(&rule.actions, &mut out);
    }
    out
}

impl CumulativePeriod {
    /// Границы `[start, end)` заказов периода на момент `now`; `None` — без границы.
    pub fn start(&self, now: DateTime<Utc>) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
        let ts = |t: Option<i64>| t.and_then(|t| DateTime::from_timestamp(t, 0));
        match *self {
            CumulativePeriod::AllTime => (None, None),
            CumulativePeriod::Interval { start, end } => (ts(start), ts(end)),
            CumulativePeriod::Relative { value, unit } => {
                let months = |n: i64| {
                    u32::try_from(n)
                        .ok()
                        .and_then(|n| now.checked_sub_months(Months::new(n)))
                };
                let from = match unit {
                    'D' => Duration::try_days(value).and_then(|d| now.checked_sub_signed(d)),
                    'M' => months(value),
                    'Y' => value.checked_mul(12).and_then(months),
                    _ => None,
                };
                (from, None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    fn node(v: Value) -> Node {
        serde_json::from_value(v).unwrap()
    }

    fn cond(children: Value) -> Node {
        node(
            json!({"CLASS_ID": "CondCumulativeGroup", "DATA": {"logic": "EqGr", "Value": 1}, "CHILDREN": children}),
        )
    }

    #[test]
    fn period_from_condition() {
        assert_eq!(from_condition(&cond(json!([]))), CumulativePeriod::AllTime);
        assert_eq!(
            from_condition(&cond(
                json!([{"CLASS_ID": "Period", "DATA": {"ValueStart": 100, "ValueEnd": "200"}}])
            )),
            CumulativePeriod::Interval {
                start: Some(100),
                end: Some(200)
            }
        );
        assert_eq!(
            from_condition(&cond(
                json!([{"CLASS_ID": "Period", "DATA": {"ValueEnd": 200}}])
            )),
            CumulativePeriod::Interval {
                start: None,
                end: Some(200)
            }
        );
        assert_eq!(
            from_condition(&cond(json!([{"CLASS_ID": "Period", "DATA": {}}]))),
            CumulativePeriod::AllTime
        );
        assert_eq!(
            from_condition(&cond(
                json!([{"CLASS_ID": "PeriodRelative", "DATA": {"Value": 3, "TypeRelativePeriod": "M"}}])
            )),
            CumulativePeriod::Relative {
                value: 3,
                unit: 'M'
            }
        );
        // Неизвестный тип — за всё время
        assert_eq!(
            from_condition(&cond(
                json!([{"CLASS_ID": "PeriodRelative", "DATA": {"Value": 3, "TypeRelativePeriod": "W"}}])
            )),
            CumulativePeriod::AllTime
        );
    }

    #[test]
    fn period_from_action() {
        let data = |v: Value| -> Map<String, Value> { serde_json::from_value(v).unwrap() };
        assert_eq!(
            from_action(&data(json!({"type_sum_period": "all_time"}))),
            CumulativePeriod::AllTime
        );
        assert_eq!(
            from_action(&data(json!({
                "type_sum_period": "interval",
                "sum_period_data": {"discount_sum_order_start": 100, "discount_sum_order_end": ""}
            }))),
            CumulativePeriod::Interval {
                start: Some(100),
                end: None
            }
        );
        assert_eq!(
            from_action(&data(json!({
                "type_sum_period": "relative",
                "sum_period_data": {"discount_sum_period_value": "2", "discount_sum_period_type": "Y"}
            }))),
            CumulativePeriod::Relative {
                value: 2,
                unit: 'Y'
            }
        );
        assert_eq!(
            from_action(&data(json!({
                "type_sum_period": "relative",
                "sum_period_data": {"discount_sum_period_value": 2, "discount_sum_period_type": "H"}
            }))),
            CumulativePeriod::AllTime
        );
        assert_eq!(from_action(&Map::new()), CumulativePeriod::AllTime);
    }

    #[test]
    fn period_bounds() {
        let now = Utc.with_ymd_and_hms(2026, 3, 31, 12, 0, 0).unwrap();
        assert_eq!(CumulativePeriod::AllTime.start(now), (None, None));
        assert_eq!(
            CumulativePeriod::Interval {
                start: Some(0),
                end: Some(86400)
            }
            .start(now),
            (
                Some(Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap()),
                Some(Utc.with_ymd_and_hms(1970, 1, 2, 0, 0, 0).unwrap())
            )
        );
        let rel = |value, unit| CumulativePeriod::Relative { value, unit }.start(now).0;
        assert_eq!(
            rel(10, 'D'),
            Some(Utc.with_ymd_and_hms(2026, 3, 21, 12, 0, 0).unwrap())
        );
        assert_eq!(
            rel(1, 'M'),
            Some(Utc.with_ymd_and_hms(2026, 2, 28, 12, 0, 0).unwrap())
        );
        assert_eq!(
            rel(2, 'Y'),
            Some(Utc.with_ymd_and_hms(2024, 3, 31, 12, 0, 0).unwrap())
        );
    }

    #[test]
    fn periods_unique_in_order() {
        let rule = |conditions: Value, actions: Value| Rule {
            id: 1,
            name: String::new(),
            active: true,
            active_from: None,
            active_to: None,
            priority: 1,
            sort: 100,
            last_discount: false,
            last_level_discount: false,
            use_coupons: false,
            conditions: node(conditions),
            actions: node(actions),
            group_ids: vec![],
            unsupported: String::new(),
            xml_id: String::new(),
        };
        let grp = |children: Value| json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND", "True": "True"}, "CHILDREN": children});
        let rel = json!({"CLASS_ID": "CondCumulativeGroup", "DATA": {"logic": "EqGr", "Value": 1}, "CHILDREN": [
            {"CLASS_ID": "PeriodRelative", "DATA": {"Value": 1, "TypeRelativePeriod": "M"}}
        ]});
        let all = json!({"CLASS_ID": "CondCumulativeGroup", "DATA": {"logic": "EqGr", "Value": 1}});
        let act = json!({"CLASS_ID": "CumulativeAction", "DATA": {"ranges": [], "type_sum_period": "all_time"}});
        let rules = vec![
            rule(grp(json!([grp(json!([rel.clone()]))])), grp(json!([]))),
            rule(grp(json!([all, rel])), grp(json!([act]))),
            rule(grp(json!([])), grp(json!([]))),
        ];
        assert_eq!(
            periods(&rules),
            vec![
                CumulativePeriod::Relative {
                    value: 1,
                    unit: 'M'
                },
                CumulativePeriod::AllTime
            ]
        );
        assert!(periods(&rules[2..]).is_empty());
    }
}
