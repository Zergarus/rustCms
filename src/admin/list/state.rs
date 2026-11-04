//! Состояние списка в URL: разбор и запись.

use std::collections::BTreeSet;

use chrono::NaiveDate;

use super::spec::{FieldType, Filter, ListSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Has,
    Eq,
    Ne,
    Gt,
    Lt,
    Between,
    Empty,
    NotEmpty,
    In,
    Nin,
    Today,
    D7,
    D30,
    Range,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Has => "has",
            Op::Eq => "eq",
            Op::Ne => "ne",
            Op::Gt => "gt",
            Op::Lt => "lt",
            Op::Between => "between",
            Op::Empty => "empty",
            Op::NotEmpty => "notempty",
            Op::In => "in",
            Op::Nin => "nin",
            Op::Today => "today",
            Op::D7 => "d7",
            Op::D30 => "d30",
            Op::Range => "range",
        }
    }

    fn from_str(s: &str) -> Option<Op> {
        Some(match s {
            "has" => Op::Has,
            "eq" => Op::Eq,
            "ne" => Op::Ne,
            "gt" => Op::Gt,
            "lt" => Op::Lt,
            "between" => Op::Between,
            "empty" => Op::Empty,
            "notempty" => Op::NotEmpty,
            "in" => Op::In,
            "nin" => Op::Nin,
            "today" => Op::Today,
            "d7" => Op::D7,
            "d30" => Op::D30,
            "range" => Op::Range,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    None,
    One(String),
    Two(String, String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cond {
    pub code: String,
    pub op: Op,
    pub val: Val,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sort {
    pub code: String,
    pub desc: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionSel {
    Id(i64),
    Without,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListState {
    pub filters: Vec<Cond>,
    pub sort: Sort,
    pub cols: Vec<String>,
    pub page: i64,
    pub per: i64,
    pub section: Option<SectionSel>,
    pub only: bool,
    pub view: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Parsed {
    pub state: ListState,
    /// Коды фильтров `f.*`, отброшенных из-за неизвестного кода.
    pub dropped: Vec<String>,
    /// Нормализованная строка отличается от исходной (порядок не учитывается).
    pub changed: bool,
}

const DEFAULT_PER: i64 = 50;
const PER_VALUES: [i64; 3] = [20, 50, 100];

fn num_ok(s: &str) -> bool {
    s.parse::<f64>().is_ok_and(|v| v.is_finite())
}

fn date_ok(s: &str) -> bool {
    s.len() == 10 && NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

fn parse_cond(f: &Filter, raw: &str) -> Option<Cond> {
    let (op_s, value) = match raw.split_once(':') {
        Some((o, v)) => (o, Some(v)),
        None => (raw, None),
    };
    let op = Op::from_str(op_s)?;
    let one = || value.filter(|v| !v.is_empty());
    let val = match (f.ty, op) {
        (FieldType::Text, Op::Has | Op::Eq) => Val::One(one()?.to_string()),
        (FieldType::Text, Op::Empty | Op::NotEmpty) => Val::None,
        (FieldType::Number | FieldType::Money, Op::Eq | Op::Ne | Op::Gt | Op::Lt) => {
            let v = one()?;
            if !num_ok(v) {
                return None;
            }
            Val::One(v.to_string())
        }
        (FieldType::Number | FieldType::Money, Op::Between) => {
            let (a, b) = value?.split_once(',')?;
            if !num_ok(a) || !num_ok(b) {
                return None;
            }
            Val::Two(a.to_string(), b.to_string())
        }
        (FieldType::Number | FieldType::Money, Op::Empty) => Val::None,
        (FieldType::Date, Op::Today | Op::D7 | Op::D30) => Val::None,
        (FieldType::Date, Op::Range) => {
            let (a, b) = value?.split_once(',')?;
            if (a.is_empty() && b.is_empty())
                || (!a.is_empty() && !date_ok(a))
                || (!b.is_empty() && !date_ok(b))
            {
                return None;
            }
            Val::Two(a.to_string(), b.to_string())
        }
        (FieldType::Enum, Op::In | Op::Nin) => {
            let mut vals: Vec<String> = Vec::new();
            for v in value?.split(',') {
                if f.options.iter().any(|o| o.value == v) && !vals.iter().any(|x| x == v) {
                    vals.push(v.to_string());
                }
            }
            if vals.is_empty() {
                return None;
            }
            Val::Many(vals)
        }
        (FieldType::Bool, Op::Eq) => match value? {
            v @ ("Y" | "N") => Val::One(v.to_string()),
            _ => return None,
        },
        (FieldType::Link, Op::Eq) => Val::One(one()?.parse::<i64>().ok()?.to_string()),
        (FieldType::Link, Op::Empty | Op::NotEmpty) => Val::None,
        _ => return None,
    };
    Some(Cond {
        code: f.code.clone(),
        op,
        val,
    })
}

fn cond_value(c: &Cond) -> String {
    match &c.val {
        Val::None => c.op.as_str().to_string(),
        Val::One(v) => format!("{}:{v}", c.op.as_str()),
        Val::Two(a, b) => format!("{}:{a},{b}", c.op.as_str()),
        Val::Many(vs) => format!("{}:{}", c.op.as_str(), vs.join(",")),
    }
}

fn norm_cols(spec: &ListSpec, raw: Option<&str>) -> Vec<String> {
    let mut cols: Vec<String> = Vec::new();
    for c in raw.unwrap_or("").split(',') {
        if spec.column(c).is_some() && !cols.iter().any(|x| x == c) {
            cols.push(c.to_string());
        }
    }
    if cols.is_empty() {
        return spec.default_cols();
    }
    for (i, req) in spec.columns.iter().filter(|c| c.required).enumerate() {
        if !cols.contains(&req.code) {
            cols.insert(i, req.code.clone());
        }
    }
    cols
}

/// Верхняя граница номера страницы: больше — приводится к ней (адрес нормализуется).
pub const MAX_PAGE: i64 = 1_000_000;

pub fn parse(spec: &ListSpec, query: &str) -> Parsed {
    let pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    // Повтор f.<code> — берётся последний.
    let mut raw_filters: Vec<(String, String)> = Vec::new();
    for (k, v) in &pairs {
        if let Some(code) = k.strip_prefix("f.") {
            match raw_filters.iter_mut().find(|(c, _)| c == code) {
                Some(slot) => slot.1 = v.clone(),
                None => raw_filters.push((code.to_string(), v.clone())),
            }
        }
    }
    let get = |key: &str| {
        pairs
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };

    let mut dropped: Vec<String> = Vec::new();
    let mut filters = Vec::new();
    for (code, raw) in &raw_filters {
        match spec.filter(code) {
            None => dropped.push(code.clone()),
            Some(f) => filters.extend(parse_cond(f, raw)),
        }
    }

    let sort = get("sort")
        .and_then(|s| {
            let (code, desc) = match s.strip_prefix('-') {
                Some(c) => (c, true),
                None => (s, false),
            };
            spec.column(code).filter(|c| c.sortable).map(|_| Sort {
                code: code.to_string(),
                desc,
            })
        })
        .unwrap_or_else(|| spec.default_sort.clone());

    let page = get("page")
        .and_then(|p| p.parse::<i64>().ok())
        .filter(|p| *p >= 1)
        .map(|p| p.min(MAX_PAGE))
        .unwrap_or(1);
    let per = get("per")
        .and_then(|p| p.parse::<i64>().ok())
        .filter(|p| PER_VALUES.contains(p))
        .unwrap_or(DEFAULT_PER);

    let section = if spec.sections {
        match get("section") {
            Some("none") => Some(SectionSel::Without),
            Some(s) => s.parse::<i64>().ok().map(SectionSel::Id),
            None => None,
        }
    } else {
        None
    };
    let only = section.is_some() && get("only") == Some("1");
    let view = get("view").and_then(|v| v.parse::<i64>().ok());

    let state = ListState {
        filters,
        sort,
        cols: norm_cols(spec, get("cols")),
        page,
        per,
        section,
        only,
        view,
    };

    let key_set = |pairs: Vec<(String, String)>| -> BTreeSet<(String, String)> {
        pairs.into_iter().filter(|(k, _)| k != "view").collect()
    };
    let normalized: Vec<(String, String)> =
        form_urlencoded::parse(to_query(spec, &state, true).as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
    let changed = key_set(pairs.clone()) != key_set(normalized);

    Parsed {
        state,
        dropped,
        changed,
    }
}

pub fn to_query(spec: &ListSpec, st: &ListState, with_page: bool) -> String {
    let mut s = form_urlencoded::Serializer::new(String::new());
    for c in &st.filters {
        s.append_pair(&format!("f.{}", c.code), &cond_value(c));
    }
    if st.sort != spec.default_sort {
        let v = if st.sort.desc {
            format!("-{}", st.sort.code)
        } else {
            st.sort.code.clone()
        };
        s.append_pair("sort", &v);
    }
    if st.cols != spec.default_cols() {
        s.append_pair("cols", &st.cols.join(","));
    }
    match st.section {
        Some(SectionSel::Id(id)) => {
            s.append_pair("section", &id.to_string());
        }
        Some(SectionSel::Without) => {
            s.append_pair("section", "none");
        }
        None => {}
    }
    if st.only && st.section.is_some() {
        s.append_pair("only", "1");
    }
    if st.per != DEFAULT_PER {
        s.append_pair("per", &st.per.to_string());
    }
    if with_page && st.page != 1 {
        s.append_pair("page", &st.page.to_string());
    }
    s.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::list::spec::test_spec;

    fn enc(s: &str) -> String {
        form_urlencoded::byte_serialize(s.as_bytes()).collect()
    }

    fn q(pairs: &[(&str, &str)]) -> String {
        let mut s = form_urlencoded::Serializer::new(String::new());
        for (k, v) in pairs {
            s.append_pair(k, v);
        }
        s.finish()
    }

    #[test]
    fn roundtrip_all_ops() {
        let spec = test_spec();
        let cases = [
            ("name", "has:кед"),
            ("name", "eq:кед"),
            ("name", "empty"),
            ("name", "notempty"),
            ("price", "eq:10.5"),
            ("price", "ne:10"),
            ("price", "gt:1"),
            ("price", "lt:99"),
            ("price", "between:1,5"),
            ("price", "empty"),
            ("qty", "eq:3"),
            ("qty", "between:1,5"),
            ("qty", "empty"),
            ("updated", "today"),
            ("updated", "d7"),
            ("updated", "d30"),
            ("updated", "range:2026-01-01,2026-02-01"),
            ("updated", "range:,2026-02-01"),
            ("status", "in:N,P"),
            ("status", "nin:F"),
            ("paid", "eq:Y"),
            ("paid", "eq:N"),
            ("link", "eq:15"),
            ("link", "empty"),
            ("link", "notempty"),
        ];
        for (code, v) in cases {
            let query = q(&[(&format!("f.{code}"), v)]);
            let p = parse(&spec, &query);
            assert_eq!(p.state.filters.len(), 1, "{code}={v}");
            assert_eq!(to_query(&spec, &p.state, true), query, "{code}={v}");
            assert!(!p.changed, "{code}={v}");
        }
    }

    #[test]
    fn cyrillic_and_specials_survive() {
        let spec = test_spec();
        let query = format!("f.name={}", enc("has:кед%_,:&"));
        let p = parse(&spec, &query);
        assert_eq!(p.state.filters[0].val, Val::One("кед%_,:&".into()));
        assert_eq!(to_query(&spec, &p.state, true), query);
    }

    #[test]
    fn drops_garbage() {
        let spec = test_spec();
        let p = parse(
            &spec,
            "f.nope=eq:1&f.price=gt:abc&f.updated=range:2026-13-01,&f.status=in:N,X&f.paid=eq:maybe&sort=zzz&cols=zzz&per=33&page=-4",
        );
        assert_eq!(p.state.filters.len(), 1);
        assert_eq!(p.state.filters[0].code, "status");
        assert_eq!(p.state.filters[0].val, Val::Many(vec!["N".into()]));
        assert_eq!(p.dropped, vec!["nope".to_string()]);
        assert_eq!(p.state.sort, spec.default_sort);
        assert_eq!(p.state.cols, spec.default_cols());
        assert_eq!(p.state.per, 50);
        assert_eq!(p.state.page, 1);
        assert!(p.changed);
    }

    #[test]
    fn huge_page_is_capped() {
        let spec = test_spec();
        let p = parse(&spec, "page=100000000000000000&per=100");
        assert_eq!(p.state.page, MAX_PAGE);
        assert!(p.changed || to_query(&spec, &p.state, true).contains("page=1000000"));
        // не помещается в i64 — обычный мусор, страница 1
        assert_eq!(parse(&spec, "page=99999999999999999999999").state.page, 1);
    }

    #[test]
    fn required_column_kept() {
        let spec = test_spec();
        let p = parse(&spec, "cols=price");
        assert_eq!(p.state.cols, vec!["name", "price"]);
    }

    #[test]
    fn defaults_are_empty_query() {
        let spec = test_spec();
        let p = parse(&spec, "");
        assert_eq!(to_query(&spec, &p.state, true), "");
        assert!(!p.changed);
    }

    #[test]
    fn range_open_ends() {
        let spec = test_spec();
        for v in ["range:,2026-11-01", "range:2026-11-01,"] {
            let p = parse(&spec, &q(&[("f.updated", v)]));
            assert_eq!(p.state.filters.len(), 1, "{v}");
        }
    }

    #[test]
    fn section_and_only() {
        let mut spec = test_spec();
        spec.sections = true;
        let p = parse(&spec, "section=15&only=1");
        assert_eq!(p.state.section, Some(SectionSel::Id(15)));
        assert!(p.state.only);
        let p = parse(&spec, "section=none");
        assert_eq!(p.state.section, Some(SectionSel::Without));
        let p = parse(&spec, "only=1");
        assert_eq!(p.state.section, None);
        assert!(!p.state.only);
        assert!(p.changed);
        spec.sections = false;
        let p = parse(&spec, "section=15&only=1");
        assert_eq!(p.state.section, None);
        assert!(!p.state.only);
    }

    #[test]
    fn view_kept_and_ignored_for_changed() {
        let spec = test_spec();
        let p = parse(&spec, "view=7");
        assert_eq!(p.state.view, Some(7));
        assert!(!p.changed);
        assert_eq!(to_query(&spec, &p.state, true), "");
    }

    #[test]
    fn param_order_ignored_for_changed() {
        let spec = test_spec();
        let p = parse(&spec, "per=20&f.paid=eq%3AY");
        assert!(!p.changed);
        let p = parse(&spec, "f.paid=eq:Y&per=20");
        assert!(!p.changed);
    }
}
