//! Сборка SQL списка из спеки и состояния.

use sqlx::{PgPool, Postgres, QueryBuilder, Row as _};

use super::spec::{FieldType, FilterSql, ListSpec};
use super::state::{Cond, ListState, Op, Val};

pub enum Scope {
    All,
    Sections(Vec<i64>),
    Without,
}

pub const COUNT_CAP: i64 = 10_001;

pub struct Row {
    pub id: i64,
    pub cells: serde_json::Map<String, serde_json::Value>,
}

/// Шаблон для ILIKE: `\`, `%`, `_` экранируются.
pub(crate) fn like_pattern(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('%');
    for ch in v.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

/// Делит `FROM … WHERE …` по первому `WHERE` вне скобок: join'ы идут между ними.
/// Соглашение: в `spec.from` `WHERE` пишется заглавными и после пробела/перевода строки.
fn split_from(from: &str) -> (&str, &str) {
    let mut depth = 0i32;
    for (i, ch) in from.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            'W' if depth == 0 && i > 0 && from[i..].starts_with("WHERE") => {
                if from[..i].ends_with(char::is_whitespace) {
                    return (from[..i].trim_end(), &from[i..]);
                }
            }
            _ => {}
        }
    }
    debug_assert!(false, "в spec.from нет WHERE: {from}");
    (from, "WHERE TRUE")
}

fn lit(s: &str) -> String {
    s.replace('\'', "''")
}

/// Коды join'ов, нужных запросу, в порядке спеки, без повторов.
fn needed_joins<'a>(spec: &'a ListSpec, codes: Vec<&'a str>) -> Vec<&'a str> {
    spec.joins
        .iter()
        .filter(|j| codes.contains(&j.code.as_str()))
        .map(|j| j.sql.as_str())
        .collect()
}

fn filter_joins<'a>(spec: &'a ListSpec, st: &'a ListState) -> Vec<&'a str> {
    st.filters
        .iter()
        .filter_map(|c| spec.filter(&c.code))
        .filter_map(|f| f.join.as_deref())
        .collect()
}

/// Значения в `field_values`: id варианта списка и id связи хранятся числами
/// (скаляр или массив при `multiple`). Нечисловое значение не подходит.
fn json_value(v: &str, multiple: bool) -> Option<serde_json::Value> {
    let one = serde_json::Value::from(v.parse::<i64>().ok()?);
    Some(if multiple {
        serde_json::Value::Array(vec![one])
    } else {
        one
    })
}

fn push_json(qb: &mut QueryBuilder<Postgres>, c: &Cond, key: &str, multiple: bool) {
    let contains = |qb: &mut QueryBuilder<Postgres>, vals: &[String]| {
        let jsons: Vec<serde_json::Value> = vals
            .iter()
            .filter_map(|v| json_value(v, multiple))
            .collect();
        if jsons.is_empty() {
            qb.push("FALSE");
            return;
        }
        qb.push("(");
        for (i, j) in jsons.into_iter().enumerate() {
            if i > 0 {
                qb.push(" OR ");
            }
            qb.push("ci.field_values @> ");
            qb.push_bind(sqlx::types::Json(serde_json::json!({ key: j })));
        }
        qb.push(")");
    };
    let empty = |qb: &mut QueryBuilder<Postgres>| {
        qb.push("(NOT (ci.field_values ? ");
        qb.push_bind(key.to_string());
        qb.push(") OR ci.field_values -> ");
        qb.push_bind(key.to_string());
        qb.push("::text IN ('null'::jsonb, '[]'::jsonb))");
    };
    let vals: Vec<String> = match &c.val {
        Val::One(v) => vec![v.clone()],
        Val::Many(vs) => vs.clone(),
        _ => vec![],
    };
    match c.op {
        Op::In | Op::Eq if !vals.is_empty() => contains(qb, &vals),
        Op::Nin | Op::Ne if vals.iter().any(|v| json_value(v, multiple).is_some()) => {
            qb.push("NOT ");
            contains(qb, &vals);
        }
        Op::Empty => empty(qb),
        Op::NotEmpty => {
            qb.push("NOT ");
            empty(qb);
        }
        // Сюда не попадаем: разбор состояния отбрасывает неподходящие пары
        // оператор/тип и пустые значения. Лучше ноль строк, чем чужие.
        _ => {
            qb.push("FALSE");
        }
    }
}

fn push_expr(qb: &mut QueryBuilder<Postgres>, c: &Cond, ty: FieldType, e: &str) {
    let cast = match ty {
        FieldType::Number | FieldType::Money => "::numeric",
        FieldType::Date => "::date",
        _ => "",
    };
    let one = match &c.val {
        Val::One(v) => v.clone(),
        _ => String::new(),
    };
    match (c.op, &c.val) {
        (Op::Has, _) => {
            qb.push(format!("{e} ILIKE "));
            qb.push_bind(like_pattern(&one));
        }
        (Op::Eq | Op::Ne | Op::Gt | Op::Lt, _) => {
            let sym = match c.op {
                Op::Eq => "=",
                Op::Ne => "<>",
                Op::Gt => ">",
                _ => "<",
            };
            qb.push(format!("{e} {sym} "));
            match ty {
                FieldType::Bool => {
                    qb.push_bind(one == "Y");
                }
                FieldType::Link => {
                    qb.push_bind(one.parse::<i64>().unwrap_or(0));
                }
                _ => {
                    qb.push_bind(one);
                    qb.push(cast);
                }
            }
        }
        (Op::Between, Val::Two(a, b)) => {
            qb.push(format!("{e} BETWEEN "));
            qb.push_bind(a.clone());
            qb.push(format!("{cast} AND "));
            qb.push_bind(b.clone());
            qb.push(cast);
        }
        (Op::Empty | Op::NotEmpty, _) => {
            let neg = c.op == Op::NotEmpty;
            let text = ty == FieldType::Text;
            let s = match (text, neg) {
                (true, false) => format!("({e} IS NULL OR {e} = '')"),
                (true, true) => format!("({e} IS NOT NULL AND {e} <> '')"),
                (false, false) => format!("({e} IS NULL)"),
                (false, true) => format!("({e} IS NOT NULL)"),
            };
            qb.push(s);
        }
        (Op::Today, _) => {
            qb.push(format!("{e} >= date_trunc('day', now())"));
        }
        (Op::D7, _) => {
            qb.push(format!("{e} >= now() - interval '7 days'"));
        }
        (Op::D30, _) => {
            qb.push(format!("{e} >= now() - interval '30 days'"));
        }
        (Op::Range, Val::Two(a, b)) => {
            qb.push("(");
            if !a.is_empty() {
                qb.push(format!("{e} >= "));
                qb.push_bind(a.clone());
                qb.push("::date");
            }
            if !a.is_empty() && !b.is_empty() {
                qb.push(" AND ");
            }
            if !b.is_empty() {
                qb.push(format!("{e} < ("));
                qb.push_bind(b.clone());
                qb.push("::date + 1)");
            }
            if a.is_empty() && b.is_empty() {
                // Разбор такого не пропускает; на всякий случай — ноль строк.
                qb.push("FALSE");
            }
            qb.push(")");
        }
        (Op::In | Op::Nin, Val::Many(vs)) => {
            if c.op == Op::Nin {
                qb.push("NOT (");
            }
            qb.push(format!("{e} = ANY("));
            qb.push_bind(vs.clone());
            qb.push(")");
            if c.op == Op::Nin {
                qb.push(")");
            }
        }
        // Разбор состояния не пропускает другие пары оператор/значение.
        _ => {
            qb.push("FALSE");
        }
    }
}

fn push_where(qb: &mut QueryBuilder<Postgres>, spec: &ListSpec, st: &ListState, scope: &Scope) {
    for c in &st.filters {
        let Some(f) = spec.filter(&c.code) else {
            continue;
        };
        qb.push(" AND (");
        match &f.sql {
            FilterSql::Expr(e) => push_expr(qb, c, f.ty, e),
            FilterSql::JsonField { key, multiple } => push_json(qb, c, key, *multiple),
        }
        qb.push(")");
    }
    match scope {
        Scope::All => {}
        Scope::Sections(ids) => {
            qb.push(" AND ci.section_id = ANY(");
            qb.push_bind(ids.clone());
            qb.push(")");
        }
        Scope::Without => {
            qb.push(" AND ci.section_id IS NULL");
        }
    }
}

fn sort_column<'a>(spec: &'a ListSpec, st: &'a ListState) -> (&'a super::spec::Column, bool) {
    if let Some(c) = spec.column(&st.sort.code).filter(|c| c.sortable) {
        return (c, st.sort.desc);
    }
    let d = &spec.default_sort;
    let c = spec
        .column(&d.code)
        .or_else(|| spec.columns.first())
        .expect("в спеке нет колонок");
    (c, d.desc)
}

pub fn select(spec: &ListSpec, st: &ListState, scope: &Scope) -> QueryBuilder<Postgres> {
    let visible: Vec<&super::spec::Column> =
        st.cols.iter().filter_map(|c| spec.column(c)).collect();
    let (sort, desc) = sort_column(spec, st);

    let mut codes: Vec<&str> = visible.iter().filter_map(|c| c.join.as_deref()).collect();
    codes.extend(sort.join.as_deref());
    codes.extend(filter_joins(spec, st));

    let mut qb = QueryBuilder::new(format!("SELECT ({})::bigint AS id, ", spec.id_expr));
    let mut pairs: Vec<String> = Vec::new();
    for c in &visible {
        pairs.push(format!("'{}', {}", lit(&c.code), c.expr));
        if let Some(cur) = &c.currency_expr {
            pairs.push(format!("'{}__cur', {}", lit(&c.code), cur));
        }
    }
    if let Some(m) = &spec.muted_expr {
        pairs.push(format!("'__muted', {m}"));
    }
    if pairs.is_empty() {
        qb.push("'{}'::jsonb");
    }
    // jsonb_build_object принимает до 100 аргументов: режем по 40 пар.
    for (i, chunk) in pairs.chunks(40).enumerate() {
        if i > 0 {
            qb.push(" || ");
        }
        qb.push(format!("jsonb_build_object({})", chunk.join(", ")));
    }
    let (head, tail) = split_from(&spec.from);
    qb.push(format!(" AS cells {head}"));
    for j in needed_joins(spec, codes) {
        qb.push(j);
    }
    qb.push(format!(" {tail}"));
    push_where(&mut qb, spec, st, scope);
    qb.push(format!(" ORDER BY {}", sort.expr));
    if desc {
        qb.push(" DESC");
    }
    qb.push(format!(" NULLS LAST, {} LIMIT ", spec.tie_break));
    qb.push_bind(st.per);
    qb.push(" OFFSET ");
    qb.push_bind((st.page.max(1) - 1) * st.per);
    qb
}

pub fn count(spec: &ListSpec, st: &ListState, scope: &Scope) -> QueryBuilder<Postgres> {
    let (head, tail) = split_from(&spec.from);
    let mut qb = QueryBuilder::new(format!("SELECT count(*) FROM (SELECT 1 {head}"));
    for j in needed_joins(spec, filter_joins(spec, st)) {
        qb.push(j);
    }
    qb.push(format!(" {tail}"));
    push_where(&mut qb, spec, st, scope);
    qb.push(format!(" LIMIT {COUNT_CAP}) t"));
    qb
}

pub async fn fetch(
    db: &PgPool,
    spec: &ListSpec,
    st: &ListState,
    scope: &Scope,
) -> sqlx::Result<(Vec<Row>, i64)> {
    let mut qb = select(spec, st, scope);
    let rows = qb.build().fetch_all(db).await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let id: i64 = r.try_get("id")?;
        let cells = match r.try_get::<serde_json::Value, _>("cells")? {
            serde_json::Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        out.push(Row { id, cells });
    }
    let mut cq = count(spec, st, scope);
    let total: i64 = cq.build_query_scalar().fetch_one(db).await?;
    Ok((out, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::list::spec::test_spec;
    use crate::admin::list::spec::{FieldType, FilterSql};
    use crate::admin::list::state::parse;

    fn sql_of(q: &str) -> String {
        let spec = test_spec();
        let st = parse(&spec, q).state;
        select(&spec, &st, &Scope::All).sql().as_str().to_string()
    }

    #[test]
    fn select_sql_snapshot() {
        let got = sql_of("f.name=has:a&f.price=between:1,5&sort=-price");
        assert_eq!(
            got,
            "SELECT (t.id)::bigint AS id, jsonb_build_object('id', t.id, 'name', t.name, 'price', p1.value, 'updated', t.updated) AS cells FROM t LEFT JOIN prices p1 ON p1.item_id = t.id AND p1.type_id = 1 WHERE true AND (t.name ILIKE $1) AND (p1.value BETWEEN $2::numeric AND $3::numeric) ORDER BY p1.value DESC NULLS LAST, t.id DESC LIMIT $4 OFFSET $5"
        );
    }

    #[test]
    fn no_url_text_in_sql() {
        let got = sql_of("f.name=has:'; DROP TABLE x;--&f.status=in:N");
        assert!(!got.contains("DROP"));
        assert!(!got.contains("'; "));
        assert!(!got.contains("TABLE"));
    }

    #[test]
    fn joins_only_when_used() {
        let spec = test_spec();
        let mk = |q: &str| {
            let st = parse(&spec, q).state;
            (
                select(&spec, &st, &Scope::All).sql().as_str().to_string(),
                count(&spec, &st, &Scope::All).sql().as_str().to_string(),
            )
        };
        // price видна по умолчанию
        assert!(mk("").0.contains("prices p1 ON"));
        // скрыта, нет фильтра и сортировки
        let (s, c) = mk("cols=id,name");
        assert!(!s.contains("prices p1"), "{s}");
        assert!(!c.contains("prices p1"));
        // фильтр
        let (s, c) = mk("cols=id,name&f.price=gt:1");
        assert_eq!(s.matches("prices p1 ON").count(), 1, "{s}");
        assert!(c.contains("prices p1"));
        // сортировка
        let (s, _) = mk("cols=id,name&sort=price");
        assert!(s.contains("prices p1"));
        // не больше одного раза
        let (s, _) = mk("f.price=gt:1&sort=price");
        assert_eq!(s.matches("prices p1 ON").count(), 1);
    }

    #[test]
    fn like_escaped() {
        assert_eq!(like_pattern("50%_"), "%50\\%\\_%");
        assert_eq!(like_pattern("a\\b"), "%a\\\\b%");
    }

    fn series_spec() -> ListSpec {
        let mut spec = test_spec();
        spec.from = "FROM generate_series(1, 10050) AS g(id) WHERE TRUE".into();
        spec.id_expr = "g.id".into();
        spec.joins.clear();
        spec.tie_break = "g.id".into();
        for c in &mut spec.columns {
            c.expr = format!("g.id::text || '{}'", c.code);
            c.join = None;
        }
        spec.columns[0].expr = "g.id".into();
        for f in &mut spec.filters {
            f.join = None;
        }
        spec.filters.push(crate::admin::list::spec::Filter {
            code: "id".into(),
            label: "id".into(),
            group: "g".into(),
            ty: FieldType::Number,
            sql: FilterSql::Expr("g.id".into()),
            options: vec![],
            join: None,
        });
        spec
    }

    #[sqlx::test]
    async fn fetch_counts_and_caps(db: sqlx::PgPool) {
        let spec = series_spec();
        let st = parse(&spec, "sort=id").state;
        let (rows, total) = fetch(&db, &spec, &st, &Scope::All).await.unwrap();
        assert_eq!(rows.len(), 50);
        assert_eq!(rows[0].id, 1);
        assert_eq!(rows[0].cells["id"], serde_json::json!(1));
        assert_eq!(total, COUNT_CAP);
        let st = parse(&spec, "f.id=lt:11").state;
        let (rows, total) = fetch(&db, &spec, &st, &Scope::All).await.unwrap();
        assert_eq!(total, 10);
        assert_eq!(rows.len(), 10);
    }

    #[sqlx::test]
    async fn json_field_conditions_run(db: sqlx::PgPool) {
        let mut spec = test_spec();
        spec.from = "FROM (VALUES \
            (1, '{\"st\":12,\"ln\":5,\"tg\":[3,4]}'::jsonb, 7), \
            (2, '{\"st\":13,\"tg\":[5]}'::jsonb, NULL), \
            (3, '{}'::jsonb, 7), \
            (4, '{\"st\":null,\"ln\":null,\"tg\":[]}'::jsonb, 8)) \
            AS ci(id, field_values, section_id) WHERE TRUE"
            .into();
        spec.id_expr = "ci.id".into();
        spec.joins.clear();
        spec.tie_break = "ci.id".into();
        for c in &mut spec.columns {
            c.expr = "ci.id".into();
            c.join = None;
        }
        spec.default_sort.code = "id".into();
        spec.default_sort.desc = false;
        for (code, key, ty, multiple) in [
            ("status", "st", FieldType::Enum, false),
            ("link", "ln", FieldType::Link, false),
            ("qty", "tg", FieldType::Enum, true),
        ] {
            let f = spec.filters.iter_mut().find(|f| f.code == code).unwrap();
            f.ty = ty;
            f.sql = FilterSql::JsonField {
                key: key.into(),
                multiple,
            };
            f.join = None;
            f.options = (1..=20)
                .map(|i| crate::admin::list::spec::EnumOption {
                    value: i.to_string(),
                    label: i.to_string(),
                })
                .collect();
        }
        let ids = |q: &str, scope: Scope| {
            let st = parse(&spec, q).state;
            let db = db.clone();
            let spec = spec.clone();
            async move {
                let (rows, _) = fetch(&db, &spec, &st, &scope).await.unwrap();
                rows.iter().map(|r| r.id).collect::<Vec<_>>()
            }
        };
        // enum, скаляр
        assert_eq!(ids("f.status=in:12", Scope::All).await, vec![1]);
        assert_eq!(ids("f.status=in:12,13", Scope::All).await, vec![1, 2]);
        assert_eq!(ids("f.status=in:20", Scope::All).await, Vec::<i64>::new());
        assert_eq!(ids("f.status=nin:12", Scope::All).await, vec![2, 3, 4]);
        assert_eq!(ids("f.status=nin:12,13", Scope::All).await, vec![3, 4]);
        // enum multiple: вхождение в массив
        assert_eq!(ids("f.qty=in:3", Scope::All).await, vec![1]);
        assert_eq!(ids("f.qty=in:4,5", Scope::All).await, vec![1, 2]);
        assert_eq!(ids("f.qty=nin:3", Scope::All).await, vec![2, 3, 4]);
        // link
        assert_eq!(ids("f.link=eq:5", Scope::All).await, vec![1]);
        assert_eq!(ids("f.link=empty", Scope::All).await, vec![2, 3, 4]);
        assert_eq!(ids("f.link=notempty", Scope::All).await, vec![1]);
        // scope
        assert_eq!(ids("", Scope::Sections(vec![7])).await, vec![1, 3]);
        assert_eq!(ids("", Scope::Sections(vec![7, 8])).await, vec![1, 3, 4]);
        assert_eq!(ids("", Scope::Without).await, vec![2]);
    }
}
