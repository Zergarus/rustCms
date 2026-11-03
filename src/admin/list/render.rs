//! Общий обработчик списка: разбор состояния, выборка, подготовка контекста, рендер.

use axum::{
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, NaiveDate, Utc};
use minijinja::context;
use serde::Serialize;
use serde_json::Value;

use super::{
    spec::{FieldType, Filter, ListSpec},
    sql::{COUNT_CAP, Scope, fetch},
    state::{Cond, ListState, Op, Parsed, SectionSel, Val, to_query},
};
use crate::{
    access::Access,
    admin::{is_full_page, render},
    error::AppResult,
    state::AppState,
};

/// Дополнительная разметка страницы списка.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ListPage {
    pub title: String,
    /// `(название, адрес)`; пустой адрес — без ссылки.
    pub crumbs: Vec<(String, String)>,
    pub actions: Vec<Action>,
    /// Данные дерева разделов (рисует шаблон коллекции).
    pub tree: Option<Value>,
    /// Название выбранного раздела для чипа (у «Без раздела» — пусто).
    pub section_name: Option<String>,
    /// У выбранного раздела есть подразделы: показывается галочка «только этот раздел».
    pub section_has_kids: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub label: String,
    pub url: String,
    pub primary: bool,
    pub hotkey: Option<char>,
}

fn group_thousands(n: i64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

fn money(v: f64) -> String {
    let cents = (v * 100.0).round() as i64;
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.abs();
    format!("{sign}{},{:02}", group_thousands(abs / 100), abs % 100)
}

/// Значение ячейки для показа.
pub fn format_cell(ty: &FieldType, v: &Value, currency: Option<&str>) -> String {
    if v.is_null() {
        return "—".into();
    }
    match (ty, v) {
        (FieldType::Money, _) => match v.as_f64() {
            Some(n) => match currency.filter(|c| !c.is_empty()) {
                Some(c) => format!("{} {c}", money(n)),
                None => money(n),
            },
            None => "—".into(),
        },
        (FieldType::Date, Value::String(s)) => {
            if let Ok(d) = DateTime::parse_from_rfc3339(s) {
                d.with_timezone(&Utc).format("%d.%m.%Y %H:%M").to_string()
            } else if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                d.format("%d.%m.%Y").to_string()
            } else {
                s.clone()
            }
        }
        (FieldType::Bool, Value::Bool(b)) => if *b { "да" } else { "нет" }.into(),
        (_, Value::String(s)) => s.clone(),
        (_, Value::Number(n)) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            _ => n.to_string().replace('.', ","),
        },
        (_, Value::Bool(b)) => if *b { "да" } else { "нет" }.into(),
        _ => v.to_string(),
    }
}

#[derive(Serialize)]
struct CellView {
    text: String,
    num: bool,
    first: bool,
    /// `ok` для истинного флага, `none` для ложного, пусто для остальных.
    flag: &'static str,
}

#[derive(Serialize)]
struct RowView {
    id: i64,
    open: String,
    peek: Option<String>,
    muted: bool,
    cells: Vec<CellView>,
}

#[derive(Serialize)]
struct HeadView {
    title: String,
    sortable: bool,
    url: String,
    arrow: &'static str,
    num: bool,
}

#[derive(Serialize)]
struct ChipView {
    /// Чип раздела, а не фильтра.
    section: bool,
    label: String,
    op: String,
    value: String,
    url: String,
}

#[derive(Serialize)]
struct PageLink {
    label: String,
    url: Option<String>,
    current: bool,
}

#[derive(Serialize)]
struct PerLink {
    value: i64,
    url: String,
    current: bool,
}

pub(crate) fn url_of(spec: &ListSpec, path: &str, st: &ListState) -> String {
    let q = to_query(spec, st, true);
    if q.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{q}")
    }
}

fn op_label(f: &Filter, op: Op) -> &'static str {
    match op {
        Op::Has => "содержит",
        Op::Eq if f.ty == FieldType::Text => "равно",
        Op::Eq => "=",
        Op::Ne => "≠",
        Op::Gt => ">",
        Op::Lt => "<",
        Op::Between => "между",
        Op::Empty => "пусто",
        Op::NotEmpty => "не пусто",
        Op::In => "любой из",
        Op::Nin => "ни один из",
        Op::Today => "сегодня",
        Op::D7 => "за 7 дней",
        Op::D30 => "за 30 дней",
        Op::Range => "период",
    }
}

fn chip(spec: &ListSpec, path: &str, st: &ListState, c: &Cond) -> Option<ChipView> {
    let f = spec.filter(&c.code)?;
    let (op, value) = match (&c.val, f.ty) {
        (Val::One(v), FieldType::Bool) => (
            String::new(),
            if v == "Y" { "да" } else { "нет" }.to_string(),
        ),
        (Val::One(v), _) => (op_label(f, c.op).to_string(), v.clone()),
        (Val::Two(a, b), FieldType::Date) => (
            String::new(),
            format!(
                "{} — {}",
                if a.is_empty() { "…" } else { a },
                if b.is_empty() { "…" } else { b }
            ),
        ),
        (Val::Two(a, b), _) => (String::new(), format!("{a} – {b}")),
        (Val::Many(vs), _) => {
            let names: Vec<String> = vs
                .iter()
                .map(|v| {
                    f.options
                        .iter()
                        .find(|o| &o.value == v)
                        .map_or_else(|| v.clone(), |o| o.label.clone())
                })
                .collect();
            let text = if names.len() > 2 {
                format!("{} +{}", names[..2].join(", "), names.len() - 2)
            } else {
                names.join(", ")
            };
            (op_label(f, c.op).to_string(), text)
        }
        (Val::None, _) => (String::new(), op_label(f, c.op).to_string()),
    };
    let mut next = st.clone();
    next.filters.retain(|x| x.code != c.code);
    next.page = 1;
    Some(ChipView {
        section: false,
        label: f.label.clone(),
        op,
        value,
        url: url_of(spec, path, &next),
    })
}

fn pager(spec: &ListSpec, path: &str, st: &ListState, pages: i64) -> Vec<PageLink> {
    let link = |p: i64, label: String| {
        let mut next = st.clone();
        next.page = p;
        PageLink {
            label,
            url: Some(url_of(spec, path, &next)),
            current: p == st.page,
        }
    };
    let gap = || PageLink {
        label: "…".into(),
        url: None,
        current: false,
    };
    let mut out = Vec::new();
    if st.page > 1 {
        out.push(link(st.page - 1, "‹".into()));
    }
    let mut last = 0;
    for p in 1..=pages {
        if p == 1 || p == pages || (p - st.page).abs() <= 2 {
            if p - last > 1 {
                out.push(gap());
            }
            out.push(link(p, p.to_string()));
            last = p;
        }
    }
    if st.page < pages {
        out.push(link(st.page + 1, "›".into()));
    }
    out
}

/// Описание спеки для JS (поповеры, клавиши); `<` экранируется, чтобы не закрыть `<script>`.
fn spec_json(spec: &ListSpec, st: &ListState) -> String {
    let ty = |t: FieldType| format!("{t:?}").to_lowercase();
    let filters: Vec<Value> = spec
        .filters
        .iter()
        .map(|f| {
            serde_json::json!({
                "code": f.code, "label": f.label, "group": f.group, "ty": ty(f.ty),
                "options": f.options.iter().map(|o| serde_json::json!({"value": o.value, "label": o.label})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let columns: Vec<Value> = spec
        .columns
        .iter()
        .map(|c| {
            serde_json::json!({"code": c.code, "title": c.title, "required": c.required, "default": c.default})
        })
        .collect();
    serde_json::json!({"filters": filters, "columns": columns, "per": st.per, "cols": st.cols})
        .to_string()
        .replace('<', "\\u003c")
}

#[allow(clippy::too_many_arguments)]
pub async fn run_list(
    state: &AppState,
    user: &Access,
    headers: &HeaderMap,
    path: &str,
    spec: &ListSpec,
    parsed: Parsed,
    scope: Scope,
    page: ListPage,
    notices: Vec<String>,
) -> AppResult<Response> {
    let full = is_full_page(headers);
    let mut st = parsed.state;
    let normalized = url_of(spec, path, &st);
    if parsed.changed && full {
        return Ok((StatusCode::FOUND, [(header::LOCATION, normalized)]).into_response());
    }

    let (mut rows, total) = fetch(&state.db, spec, &st, &scope).await?;
    let pages = ((total + st.per - 1) / st.per).max(1);
    let mut replace_url = parsed.changed.then(|| normalized.clone());
    if st.page > pages {
        st.page = pages;
        let clamped = url_of(spec, path, &st);
        if full {
            return Ok((StatusCode::FOUND, [(header::LOCATION, clamped)]).into_response());
        }
        rows = fetch(&state.db, spec, &st, &scope).await?.0;
        replace_url = Some(clamped);
    }

    let visible: Vec<_> = st.cols.iter().filter_map(|c| spec.column(c)).collect();
    let heads: Vec<HeadView> = visible
        .iter()
        .map(|c| {
            let mut next = st.clone();
            next.page = 1;
            let cur = st.sort.code == c.code;
            next.sort = super::state::Sort {
                code: c.code.clone(),
                desc: cur && !st.sort.desc,
            };
            HeadView {
                title: c.title.clone(),
                sortable: c.sortable,
                url: url_of(spec, path, &next),
                arrow: match (cur, st.sort.desc) {
                    (true, true) => "▼",
                    (true, false) => "▲",
                    _ => "",
                },
                num: matches!(c.ty, FieldType::Number | FieldType::Money),
            }
        })
        .collect();

    let row_views: Vec<RowView> = rows
        .iter()
        .map(|r| {
            let cells = visible
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let v = r.cells.get(&c.code).cloned().unwrap_or(Value::Null);
                    let cur = r
                        .cells
                        .get(&format!("{}__cur", c.code))
                        .and_then(Value::as_str);
                    let flag = match (c.ty, &v) {
                        (FieldType::Bool, Value::Bool(true)) => "ok",
                        (FieldType::Bool, Value::Bool(false)) => "none",
                        _ => "",
                    };
                    CellView {
                        text: format_cell(&c.ty, &v, cur),
                        num: matches!(c.ty, FieldType::Number | FieldType::Money),
                        first: i == 0,
                        flag,
                    }
                })
                .collect();
            RowView {
                id: r.id,
                muted: r.cells.get("__muted") == Some(&Value::Bool(true)),
                open: spec.open_url.replace("{id}", &r.id.to_string()),
                peek: spec
                    .peek_url
                    .as_ref()
                    .map(|u| u.replace("{id}", &r.id.to_string())),
                cells,
            }
        })
        .collect();

    let mut chips: Vec<ChipView> = st
        .filters
        .iter()
        .filter_map(|c| chip(spec, path, &st, c))
        .collect();
    if let Some(sel) = st.section {
        let value = match sel {
            SectionSel::Without => Some("без раздела".to_string()),
            SectionSel::Id(_) => page.section_name.clone(),
        };
        if let Some(value) = value {
            let mut next = st.clone();
            next.section = None;
            next.only = false;
            next.page = 1;
            chips.insert(
                0,
                ChipView {
                    section: true,
                    label: "Раздел:".into(),
                    op: String::new(),
                    value,
                    url: url_of(spec, path, &next),
                },
            );
        }
    }
    let only_toggle = (matches!(st.section, Some(SectionSel::Id(_))) && page.section_has_kids)
        .then(|| {
            let mut next = st.clone();
            next.only = !st.only;
            next.page = 1;
            serde_json::json!({"checked": st.only, "url": url_of(spec, path, &next)})
        });
    let reset_url = {
        let mut next = st.clone();
        next.filters.clear();
        next.section = None;
        next.only = false;
        next.page = 1;
        url_of(spec, path, &next)
    };
    let per_links: Vec<PerLink> = [20, 50, 100]
        .into_iter()
        .map(|n| {
            let mut next = st.clone();
            next.per = n;
            next.page = 1;
            PerLink {
                value: n,
                url: url_of(spec, path, &next),
                current: n == st.per,
            }
        })
        .collect();
    let capped = total >= COUNT_CAP;
    let ctx = context! {
        user,
        page,
        notices,
        heads,
        rows => row_views,
        chips,
        reset_url,
        per_links,
        total,
        capped,
        pages,
        cur_page => st.page,
        pager => pager(spec, path, &st, pages),
        current_url => url_of(spec, path, &st),
        only_toggle,
        full,
        views => Vec::<Value>::new(),
        spec_json => spec_json(spec, &st),
    };
    let name = if full {
        "_list.html"
    } else {
        "_list_body.html"
    };
    let mut resp = render(state, name, ctx)?.into_response();
    if let Some(Ok(v)) = replace_url.as_deref().map(HeaderValue::from_str) {
        resp.headers_mut().insert("HX-Replace-Url", v);
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn money_cells() {
        let m = FieldType::Money;
        assert_eq!(format_cell(&m, &json!(1234.5), Some("RUB")), "1 234,50 RUB");
        assert_eq!(format_cell(&m, &json!(0), Some("RUB")), "0,00 RUB");
        assert_eq!(format_cell(&m, &json!(1234567.891), None), "1 234 567,89");
        assert_eq!(format_cell(&m, &json!(-5), Some("USD")), "-5,00 USD");
    }

    #[test]
    fn other_cells() {
        assert_eq!(
            format_cell(
                &FieldType::Date,
                &json!("2026-10-02T09:05:00.123456+00:00"),
                None
            ),
            "02.10.2026 09:05"
        );
        assert_eq!(
            format_cell(&FieldType::Date, &json!("2026-10-02"), None),
            "02.10.2026"
        );
        assert_eq!(format_cell(&FieldType::Bool, &json!(true), None), "да");
        assert_eq!(format_cell(&FieldType::Bool, &json!(false), None), "нет");
        assert_eq!(format_cell(&FieldType::Text, &Value::Null, None), "—");
        assert_eq!(format_cell(&FieldType::Number, &json!(5.0), None), "5");
        assert_eq!(format_cell(&FieldType::Number, &json!(5.25), None), "5,25");
        assert_eq!(format_cell(&FieldType::Text, &json!("a<b"), None), "a<b");
    }
}
