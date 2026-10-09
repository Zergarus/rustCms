//! Компиляция `filter` bxapi в SQL над `collection_items`.
//!
//! Ключ фильтра — оператор + путь: `!@id`, `%name`, `relatedBrands.value`,
//! `alwaysShow.item.value`, `autoModel.element.id`, `iblockSection.globalActive`.
//! Вложенные группы — объекты под числовыми ключами с `logic: "OR"|"AND"`.
//! Значения свойств лежат в JSONB: одиночное — скаляр, множественное — массив;
//! условие на свойство проверяет, есть ли подходящее значение среди всех.

use std::cell::Cell;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde_json::{Map, Value};
use sqlx::{Postgres, QueryBuilder};

use super::{
    BxError,
    project::Project,
    registry::{Schema, Snapshot},
    to_snake,
};
use crate::iblock::{Property, is_valid_code};

/// Глубина вложенных связей `a.element.b.element.c` (`bxapi.relation_depth.max`).
pub const MAX_DEPTH: usize = 3;
/// Часовой пояс дат Битрикса — Москва.
const MSK_OFFSET_SECS: i32 = 3 * 3600;

pub struct Ctx<'a> {
    pub snap: &'a Snapshot,
    pub schema: &'a Schema,
    pub project: &'a Project,
    /// SQL-алиас строки `collection_items`.
    pub alias: String,
    pub depth: usize,
    /// Счётчик для уникальных алиасов вложенных подзапросов.
    pub counter: &'a Cell<usize>,
}

impl<'a> Ctx<'a> {
    pub fn root(
        snap: &'a Snapshot,
        schema: &'a Schema,
        project: &'a Project,
        counter: &'a Cell<usize>,
    ) -> Self {
        Ctx {
            snap,
            schema,
            project,
            alias: "e".into(),
            depth: 0,
            counter,
        }
    }

    fn next_alias(&self, prefix: &str) -> String {
        let n = self.counter.get() + 1;
        self.counter.set(n);
        format!("{prefix}{n}")
    }

    fn nested(&self, schema: &'a Schema, alias: String) -> Ctx<'a> {
        Ctx {
            snap: self.snap,
            schema,
            project: self.project,
            alias,
            depth: self.depth + 1,
            counter: self.counter,
        }
    }

    fn iblock_code(&self) -> &str {
        &self.schema.iblock.code
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Eq,
    Ne,
    In,
    NotIn,
    Lt,
    Gt,
    Le,
    Ge,
    Like,
    NotLike,
}

impl Op {
    fn negative(self) -> bool {
        matches!(self, Op::Ne | Op::NotIn | Op::NotLike)
    }

    /// Противоположный положительный оператор: `!=` → `=`, `!@` → `@`, `!%` → `%`.
    fn positive(self) -> Op {
        match self {
            Op::Ne => Op::Eq,
            Op::NotIn => Op::In,
            Op::NotLike => Op::Like,
            other => other,
        }
    }

    fn sql(self) -> &'static str {
        match self {
            Op::Lt => "<",
            Op::Gt => ">",
            Op::Le => "<=",
            Op::Ge => ">=",
            _ => "=",
        }
    }
}

/// Оператор из начала ключа и путь поля.
pub fn split_op(key: &str) -> (Op, &str) {
    const PREFIXES: &[(&str, Op)] = &[
        ("!@", Op::NotIn),
        ("!=", Op::Ne),
        ("<>", Op::Ne),
        (">=", Op::Ge),
        ("<=", Op::Le),
        ("!%", Op::NotLike),
        ("=", Op::Eq),
        ("@", Op::In),
        ("!", Op::Ne),
        ("<", Op::Lt),
        (">", Op::Gt),
        ("%", Op::Like),
    ];
    for (prefix, op) in PREFIXES {
        if let Some(rest) = key.strip_prefix(prefix) {
            return (*op, rest);
        }
    }
    (Op::Eq, key)
}

fn flatten(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        other => vec![other.clone()],
    }
}

fn unknown_field(path: &str) -> BxError {
    BxError::new("invalid_filter", format!("Unknown filter field: {path}"))
}

// ---------------------------------------------------------------------------
// Группы
// ---------------------------------------------------------------------------

/// Условие фильтра целиком; пустой фильтр — `TRUE`.
pub fn push_filter(
    qb: &mut QueryBuilder<Postgres>,
    ctx: &Ctx,
    filter: &Map<String, Value>,
) -> Result<(), BxError> {
    push_group(qb, ctx, filter)
}

fn push_group(
    qb: &mut QueryBuilder<Postgres>,
    ctx: &Ctx,
    map: &Map<String, Value>,
) -> Result<(), BxError> {
    let is_or = map
        .get("logic")
        .and_then(Value::as_str)
        .is_some_and(|l| l.eq_ignore_ascii_case("or"));
    let entries: Vec<(&String, &Value)> = map
        .iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("logic"))
        .collect();
    if entries.is_empty() {
        qb.push("TRUE");
        return Ok(());
    }
    qb.push("(");
    for (i, (key, value)) in entries.into_iter().enumerate() {
        if i > 0 {
            qb.push(if is_or { " OR " } else { " AND " });
        }
        match value {
            // Подгруппа: числовой ключ и объект-значение
            Value::Object(sub) if key.bytes().all(|b| b.is_ascii_digit()) => {
                push_group(qb, ctx, sub)?
            }
            _ => push_leaf(qb, ctx, key, value)?,
        }
    }
    qb.push(")");
    Ok(())
}

// ---------------------------------------------------------------------------
// Условие на одно поле
// ---------------------------------------------------------------------------

pub fn push_leaf(
    qb: &mut QueryBuilder<Postgres>,
    ctx: &Ctx,
    key: &str,
    value: &Value,
) -> Result<(), BxError> {
    let (op, path) = split_op(key);
    let mut segments: Vec<&str> = path.split('.').collect();
    let mut value = value.clone();
    if segments.len() == 1
        && let Some(alias) = ctx.project.alias(ctx.iblock_code(), segments[0])
    {
        segments = alias.path.split('.').collect();
        value = map_bools(&value, alias.on, alias.off);
    }
    let value = map_bools(&value, "Y", "N");
    let mut values = flatten(&value);
    let op = match (op, value.is_array()) {
        (Op::Eq, true) => Op::In,
        (Op::Ne, true) => Op::NotIn,
        (op, _) => op,
    };
    let a = ctx.alias.clone();

    match segments.as_slice() {
        ["id"] => push_column(qb, &format!("{a}.id"), Ty::Int, op, &values),
        ["name"] => push_column(qb, &format!("{a}.name"), Ty::Text, op, &values),
        ["code"] => push_column(qb, &format!("{a}.code"), Ty::Text, op, &values),
        ["xmlId" | "externalId"] => push_column(qb, &format!("{a}.xml_id"), Ty::Text, op, &values),
        ["previewText"] => push_column(qb, &format!("{a}.preview_text"), Ty::Text, op, &values),
        ["detailText"] => push_column(qb, &format!("{a}.detail_text"), Ty::Text, op, &values),
        ["sort"] => push_column(qb, &format!("{a}.sort"), Ty::Int, op, &values),
        ["active"] => push_column(qb, &format!("{a}.active"), Ty::Bool, op, &values),
        ["dateCreate"] => push_column(qb, &format!("{a}.created_at"), Ty::Ts, op, &values),
        ["timestampX"] => push_column(qb, &format!("{a}.updated_at"), Ty::Ts, op, &values),
        ["activeFrom"] => push_column(qb, &format!("{a}.published_at"), Ty::Ts, op, &values),
        ["createdBy"] => push_column(qb, &format!("{a}.created_by"), Ty::Int, op, &values),
        ["iblockId"] => push_column(qb, &format!("{a}.collection_id"), Ty::Int, op, &values),
        ["iblockSectionId"] => {
            // 0 и null — «без раздела»
            let no_section = values
                .iter()
                .any(|v| v.is_null() || v.as_i64() == Some(0) || v.as_str() == Some("0"));
            values.retain(|v| !(v.is_null() || v.as_i64() == Some(0) || v.as_str() == Some("0")));
            push_section_ids(qb, &a, op, ids_of(&values)?, no_section)
        }
        ["sectionSubtreeId"] => {
            let roots = ids_of(&values)?;
            let ids: Vec<i64> = ctx.schema.subtree(&roots).into_iter().collect();
            push_section_ids(qb, &a, op, ids, false)
        }
        ["sectionCode"] => {
            let ids = matching_sections(ctx.schema, op.positive(), &values, |s| {
                Value::String(s.code.clone())
            });
            push_section_ids(
                qb,
                &a,
                if op.negative() { Op::NotIn } else { Op::In },
                ids,
                false,
            )
        }
        ["iblockSection", field] => {
            let ids: Vec<i64> = match *field {
                "globalActive" => {
                    let want = values.first().map(truthy).unwrap_or(true);
                    ctx.schema
                        .sections
                        .keys()
                        .filter(|id| ctx.schema.globally_active.contains(id) == want)
                        .copied()
                        .collect()
                }
                "active" => {
                    let want = values.first().map(truthy).unwrap_or(true);
                    ctx.schema
                        .sections
                        .values()
                        .filter(|s| s.active == want)
                        .map(|s| s.id)
                        .collect()
                }
                "id" => ids_of(&values)?,
                "code" => matching_sections(ctx.schema, op.positive(), &values, |s| {
                    Value::String(s.code.clone())
                }),
                "name" => matching_sections(ctx.schema, op.positive(), &values, |s| {
                    Value::String(s.name.clone())
                }),
                "depthLevel" => matching_sections(ctx.schema, op.positive(), &values, |s| {
                    Value::from(s.depth_level)
                }),
                _ => return Err(unknown_field(path)),
            };
            // Как JOIN в Битриксе: элементы без раздела под условие на раздел не попадают
            push_section_ids(
                qb,
                &a,
                if op.negative() { Op::NotIn } else { Op::In },
                ids,
                false,
            )
        }
        [root, rest @ ..] => {
            let prop = ctx
                .schema
                .prop(&to_snake(root))
                .ok_or_else(|| unknown_field(path))?;
            push_prop(qb, ctx, prop, rest, op, &values)
        }
        [] => Err(unknown_field(path)),
    }
}

/// `true`/`false` → маркеры (`Y`/`N`), в том числе внутри массива.
fn map_bools(value: &Value, on: &str, off: &str) -> Value {
    match value {
        Value::Bool(true) => Value::from(on),
        Value::Bool(false) => Value::from(off),
        Value::Array(items) => Value::Array(items.iter().map(|v| map_bools(v, on, off)).collect()),
        other => other.clone(),
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_i64() != Some(0),
        Value::String(s) => matches!(s.to_ascii_uppercase().as_str(), "Y" | "1" | "TRUE"),
        _ => false,
    }
}

fn ids_of(values: &[Value]) -> Result<Vec<i64>, BxError> {
    values
        .iter()
        .map(|v| match v {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        })
        .collect::<Option<Vec<i64>>>()
        .ok_or_else(|| BxError::new("invalid_filter", "Expected numeric id"))
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Сравнение значения с условием в памяти (для разделов и вариантов списков).
pub fn value_matches(op: Op, field: &Value, values: &[Value]) -> bool {
    let field_text = text_of(field);
    match op {
        Op::Eq | Op::In => values.iter().any(|v| text_of(v) == field_text),
        Op::Like => values.iter().any(|v| {
            field_text
                .to_lowercase()
                .contains(&text_of(v).to_lowercase())
        }),
        Op::Lt | Op::Gt | Op::Le | Op::Ge => {
            let (Some(a), Some(b)) = (
                field_text.parse::<f64>().ok(),
                values.first().and_then(|v| text_of(v).parse::<f64>().ok()),
            ) else {
                return false;
            };
            match op {
                Op::Lt => a < b,
                Op::Gt => a > b,
                Op::Le => a <= b,
                _ => a >= b,
            }
        }
        negative => !value_matches(negative.positive(), field, values),
    }
}

fn matching_sections(
    schema: &Schema,
    op: Op,
    values: &[Value],
    field: impl Fn(&crate::iblock::Section) -> Value,
) -> Vec<i64> {
    schema
        .sections
        .values()
        .filter(|s| value_matches(op, &field(s), values))
        .map(|s| s.id)
        .collect()
}

/// Условие на раздел элемента. `or_null` — «без раздела» тоже подходит.
fn push_section_ids(
    qb: &mut QueryBuilder<Postgres>,
    alias: &str,
    op: Op,
    ids: Vec<i64>,
    or_null: bool,
) -> Result<(), BxError> {
    let col = format!("{alias}.section_id");
    if op.negative() {
        // Отрицание «в разделах [или без раздела]»
        let null_part = if or_null {
            format!("{col} IS NOT NULL AND")
        } else {
            format!("{col} IS NULL OR")
        };
        qb.push(format!("({null_part} NOT ({col} = ANY("));
        qb.push_bind(ids).push(")))");
    } else {
        qb.push(format!("({col} = ANY(")).push_bind(ids).push(")");
        if or_null {
            qb.push(format!(" OR {col} IS NULL"));
        }
        qb.push(")");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Колонки элемента
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Ty {
    Int,
    Text,
    Bool,
    Ts,
}

fn parse_ts(v: &Value) -> Option<DateTime<Utc>> {
    let s = text_of(v);
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    let offset = chrono::FixedOffset::east_opt(MSK_OFFSET_SECS)?;
    let naive = NaiveDateTime::parse_from_str(s, "%d.%m.%Y %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(s, "%d.%m.%Y")
                .or_else(|_| NaiveDate::parse_from_str(s, "%Y-%m-%d"))
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        })?;
    Some(
        naive
            .and_local_timezone(offset)
            .single()?
            .with_timezone(&Utc),
    )
}

fn like_pattern(v: &Value) -> String {
    let raw = text_of(v);
    if raw.contains('%') {
        return raw; // шаблон задан явно
    }
    let escaped = raw.replace('\\', "\\\\").replace('_', "\\_");
    format!("%{escaped}%")
}

fn push_column(
    qb: &mut QueryBuilder<Postgres>,
    col: &str,
    ty: Ty,
    op: Op,
    values: &[Value],
) -> Result<(), BxError> {
    let bad = || BxError::new("invalid_filter", format!("Invalid value for {col}"));
    if matches!(op, Op::Like | Op::NotLike) {
        if op == Op::NotLike {
            qb.push("NOT ");
        }
        qb.push(format!("({col}::text ILIKE ANY("));
        qb.push_bind(values.iter().map(like_pattern).collect::<Vec<_>>());
        qb.push("))");
        return Ok(());
    }
    let negative = op.negative();
    let cmp = op.positive();
    if negative {
        qb.push("NOT ");
    }
    qb.push(format!("COALESCE({col} "));
    match cmp {
        Op::Eq | Op::In => qb.push("= ANY("),
        other => qb.push(format!("{} (", other.sql())),
    };
    let single = !matches!(cmp, Op::Eq | Op::In);
    match ty {
        Ty::Int => {
            let ids = ids_of(values)?;
            if single {
                qb.push_bind(*ids.first().ok_or_else(bad)?);
            } else {
                qb.push_bind(ids);
            }
        }
        Ty::Text => {
            let texts: Vec<String> = values.iter().map(text_of).collect();
            if single {
                qb.push_bind(texts.into_iter().next().ok_or_else(bad)?);
            } else {
                qb.push_bind(texts);
            }
        }
        Ty::Bool => {
            let flags: Vec<bool> = values.iter().map(truthy).collect();
            if single {
                qb.push_bind(*flags.first().ok_or_else(bad)?);
            } else {
                qb.push_bind(flags);
            }
        }
        Ty::Ts => {
            let dates = values
                .iter()
                .map(parse_ts)
                .collect::<Option<Vec<_>>>()
                .ok_or_else(bad)?;
            if single {
                qb.push_bind(*dates.first().ok_or_else(bad)?);
            } else {
                qb.push_bind(dates);
            }
        }
    }
    qb.push("), FALSE)");
    Ok(())
}

// ---------------------------------------------------------------------------
// Свойства
// ---------------------------------------------------------------------------

/// Все значения свойства строки как массив JSONB (одиночное → массив из одного).
fn prop_values_sql(alias: &str, code: &str) -> String {
    debug_assert!(is_valid_code(code));
    let v = format!("{alias}.field_values -> '{code}'");
    format!(
        "(CASE WHEN jsonb_typeof({v}) = 'array' THEN {v} \
         WHEN jsonb_typeof({v}) IN ('string', 'number', 'boolean') THEN jsonb_build_array({v}) \
         ELSE '[]'::jsonb END)"
    )
}

/// `EXISTS (значение свойства, удовлетворяющее условию)`; отрицание — `NOT EXISTS`.
/// `cond` пишет условие на `<x>` — JSONB-значение.
fn push_exists(
    qb: &mut QueryBuilder<Postgres>,
    ctx: &Ctx,
    prop: &Property,
    negative: bool,
    cond: impl FnOnce(&mut QueryBuilder<Postgres>, &str) -> Result<(), BxError>,
) -> Result<(), BxError> {
    if !is_valid_code(&prop.code) {
        return Err(unknown_field(&prop.code));
    }
    let v = ctx.next_alias("v");
    qb.push(if negative { "NOT EXISTS (" } else { "EXISTS (" });
    qb.push(format!(
        "SELECT 1 FROM jsonb_array_elements({}) AS {v}(x) WHERE ",
        prop_values_sql(&ctx.alias, &prop.code)
    ));
    cond(qb, &format!("{v}.x"))?;
    qb.push(")");
    Ok(())
}

fn push_prop_ids(
    qb: &mut QueryBuilder<Postgres>,
    ctx: &Ctx,
    prop: &Property,
    negative: bool,
    ids: Vec<i64>,
) -> Result<(), BxError> {
    let ids: Vec<String> = ids.iter().map(i64::to_string).collect();
    push_exists(qb, ctx, prop, negative, |qb, x| {
        qb.push(format!("({x} #>> '{{}}') = ANY("))
            .push_bind(ids)
            .push(")");
        Ok(())
    })
}

/// Привязка, у которой связанный элемент удовлетворяет вложенному условию.
fn push_prop_related(
    qb: &mut QueryBuilder<Postgres>,
    ctx: &Ctx,
    prop: &Property,
    negative: bool,
    key: &str,
    values: &[Value],
) -> Result<(), BxError> {
    if ctx.depth >= MAX_DEPTH {
        return Err(BxError::new(
            "invalid_filter",
            "Relation depth limit exceeded",
        ));
    }
    let linked = prop
        .link_collection_id
        .and_then(|id| ctx.snap.get(id))
        .ok_or_else(|| {
            BxError::new(
                "invalid_filter",
                format!("Property {} has no linked iblock", prop.code),
            )
        })?
        .clone();
    let le = ctx.next_alias("le");
    let nested = ctx.nested(&linked, le.clone());
    let value = Value::Array(values.to_vec());
    push_exists(qb, ctx, prop, negative, |qb, x| {
        qb.push(format!(
            "({x} #>> '{{}}') IN (SELECT {le}.id::text FROM collection_items {le} WHERE {le}.collection_id = "
        ));
        qb.push_bind(linked.iblock.id).push(" AND ");
        push_leaf(qb, &nested, key, &value)?;
        qb.push(")");
        Ok(())
    })
}

fn push_prop(
    qb: &mut QueryBuilder<Postgres>,
    ctx: &Ctx,
    prop: &Property,
    rest: &[&str],
    op: Op,
    values: &[Value],
) -> Result<(), BxError> {
    let negative = op.negative();
    let pos = op.positive();
    let rest: &[&str] = if rest == ["value"] { &[] } else { rest };
    let directory = prop.user_type == "directory";
    let bad_path = || unknown_field(&format!("{}.{}", prop.code, rest.join(".")));

    // Пустое значение: {"prop": null} или {"prop": ""} — «не заполнено»
    let empty = |v: &Value| v.is_null() || v.as_str() == Some("");
    if rest.is_empty()
        && matches!(pos, Op::Eq | Op::In)
        && !values.is_empty()
        && values.iter().all(empty)
    {
        return push_exists(qb, ctx, prop, !negative, |qb, _| {
            qb.push("TRUE");
            Ok(())
        });
    }

    match (prop.kind.as_str(), rest) {
        ("list", []) => push_prop_ids(qb, ctx, prop, negative, ids_of(values)?),
        ("list", ["item", field]) => {
            let ids: Vec<i64> = ctx
                .snap
                .property_enums(prop.id)
                .into_iter()
                .filter(|e| {
                    let v = match *field {
                        "value" => Value::String(e.value.clone()),
                        "xmlId" => Value::String(e.xml_id.clone()),
                        "id" => Value::from(e.id),
                        _ => Value::Null,
                    };
                    value_matches(pos, &v, values)
                })
                .map(|e| e.id)
                .collect();
            if !matches!(*field, "value" | "xmlId" | "id") {
                return Err(bad_path());
            }
            push_prop_ids(qb, ctx, prop, negative, ids)
        }
        ("element", []) if directory => {
            push_prop_related(qb, ctx, prop, negative, &op_key(pos, "xmlId"), values)
        }
        ("element", ["item", field]) if directory => {
            let key = directory_field(ctx, prop, field).ok_or_else(bad_path)?;
            push_prop_related(qb, ctx, prop, negative, &op_key(pos, &key), values)
        }
        ("element" | "file", [] | ["element"] | ["element", "id"]) => {
            push_prop_ids(qb, ctx, prop, negative, ids_of(values)?)
        }
        ("element", ["element", sub @ ..]) => push_prop_related(
            qb,
            ctx,
            prop,
            negative,
            &op_key(pos, &sub.join(".")),
            values,
        ),
        ("string" | "text" | "number" | "date" | "boolean", []) => {
            push_prop_scalar(qb, ctx, prop, pos, negative, values)
        }
        _ => Err(bad_path()),
    }
}

/// Поле связанного элемента-справочника по имени UF-поля HL-блока.
fn directory_field(ctx: &Ctx, prop: &Property, field: &str) -> Option<String> {
    if field == "ufXmlId" {
        return Some("xmlId".into());
    }
    let linked = ctx.snap.get(prop.link_collection_id?)?;
    if linked.prop(&to_snake(field)).is_some() {
        return Some(field.to_string());
    }
    (field == "ufName").then(|| "name".into())
}

fn op_key(op: Op, path: &str) -> String {
    let prefix = match op {
        Op::Eq => "=",
        Op::In => "@",
        Op::Lt => "<",
        Op::Gt => ">",
        Op::Le => "<=",
        Op::Ge => ">=",
        Op::Like => "%",
        Op::Ne => "!=",
        Op::NotIn => "!@",
        Op::NotLike => "!%",
    };
    format!("{prefix}{path}")
}

fn push_prop_scalar(
    qb: &mut QueryBuilder<Postgres>,
    ctx: &Ctx,
    prop: &Property,
    op: Op,
    negative: bool,
    values: &[Value],
) -> Result<(), BxError> {
    let numeric = prop.kind == "number";
    let texts: Vec<String> = values.iter().map(text_of).collect();
    let bad = || BxError::new("invalid_filter", format!("Invalid value for {}", prop.code));
    push_exists(qb, ctx, prop, negative, |qb, x| {
        let text = format!("({x} #>> '{{}}')");
        match op {
            Op::Like => {
                qb.push(format!("{text} ILIKE ANY("));
                qb.push_bind(values.iter().map(like_pattern).collect::<Vec<_>>())
                    .push(")");
            }
            Op::Eq | Op::In if numeric => {
                let nums = texts
                    .iter()
                    .map(|t| t.trim().replace(',', ".").parse::<f64>().ok())
                    .collect::<Option<Vec<f64>>>()
                    .ok_or_else(bad)?;
                qb.push(format!(
                    "jsonb_typeof({x}) = 'number' AND {text}::float8 = ANY("
                ))
                .push_bind(nums)
                .push(")");
            }
            Op::Eq | Op::In if prop.kind == "boolean" => {
                let flags: Vec<bool> = values.iter().map(truthy).collect();
                qb.push(format!(
                    "jsonb_typeof({x}) = 'boolean' AND {text}::boolean = ANY("
                ))
                .push_bind(flags)
                .push(")");
            }
            Op::Eq | Op::In => {
                qb.push(format!("{text} = ANY(")).push_bind(texts).push(")");
            }
            cmp if numeric => {
                let n: f64 = texts
                    .first()
                    .and_then(|t| t.trim().replace(',', ".").parse().ok())
                    .ok_or_else(bad)?;
                qb.push(format!(
                    "jsonb_typeof({x}) = 'number' AND {text}::float8 {} ",
                    cmp.sql()
                ))
                .push_bind(n);
            }
            cmp => {
                qb.push(format!("{text} {} ", cmp.sql()))
                    .push_bind(texts.into_iter().next().ok_or_else(bad)?);
            }
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Сортировка
// ---------------------------------------------------------------------------

pub fn push_order(
    qb: &mut QueryBuilder<Postgres>,
    ctx: &Ctx,
    order: &[super::query::Order],
) -> Result<(), BxError> {
    let a = &ctx.alias;
    qb.push(" ORDER BY ");
    if order.is_empty() {
        // Без сортировки Битрикс (ORM) отдаёт новые элементы первыми
        qb.push(format!("{a}.id DESC"));
        return Ok(());
    }
    for (i, o) in order.iter().enumerate() {
        if i > 0 {
            qb.push(", ");
        }
        let expr = match o.field.as_str() {
            "id" | "ID" => format!("{a}.id"),
            // Строки — как в MySQL Битрикса: без учёта регистра (ICU)
            "name" => format!("{a}.name COLLATE \"ru-x-icu\""),
            "code" => format!("{a}.code COLLATE \"ru-x-icu\""),
            "xmlId" | "externalId" => format!("{a}.xml_id COLLATE \"ru-x-icu\""),
            "sort" => format!("{a}.sort"),
            "active" => format!("{a}.active"),
            "dateCreate" => format!("{a}.created_at"),
            "timestampX" => format!("{a}.updated_at"),
            "activeFrom" => format!("{a}.published_at"),
            "iblockSectionId" => format!("{a}.section_id"),
            "createdBy" => format!("{a}.created_by"),
            "sectionCode" => {
                format!(
                    "(SELECT s.code FROM collection_sections s WHERE s.id = {a}.section_id) COLLATE \"ru-x-icu\""
                )
            }
            other => {
                let root = other.strip_suffix(".value").unwrap_or(other);
                let prop = ctx
                    .schema
                    .prop(&to_snake(root))
                    .filter(|p| is_valid_code(&p.code))
                    .ok_or_else(|| {
                        BxError::new("invalid_order", format!("Unknown order field: {other}"))
                    })?;
                if prop.kind == "number" {
                    format!("({a}.field_values -> '{}')", prop.code)
                } else {
                    format!(
                        "({a}.field_values ->> '{}') COLLATE \"ru-x-icu\"",
                        prop.code
                    )
                }
            }
        };
        qb.push(format!(
            "{expr} {}",
            if o.desc {
                "DESC NULLS LAST"
            } else {
                "ASC NULLS FIRST"
            }
        ));
    }
    qb.push(format!(", {a}.id ASC"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn operators() {
        assert_eq!(split_op("!@id"), (Op::NotIn, "id"));
        assert_eq!(split_op("=active"), (Op::Eq, "active"));
        assert_eq!(split_op(">=price"), (Op::Ge, "price"));
        assert_eq!(split_op("%name"), (Op::Like, "name"));
        assert_eq!(split_op("!id"), (Op::Ne, "id"));
        assert_eq!(
            split_op("@relatedBrands.value"),
            (Op::In, "relatedBrands.value")
        );
        assert_eq!(split_op("code"), (Op::Eq, "code"));
    }

    #[test]
    fn memory_matching() {
        assert!(value_matches(
            Op::In,
            &json!("a"),
            &[json!("b"), json!("a")]
        ));
        assert!(value_matches(Op::NotIn, &json!("c"), &[json!("a")]));
        assert!(value_matches(
            Op::Like,
            &json!("Фильтр АКПП"),
            &[json!("акпп")]
        ));
        assert!(value_matches(Op::Ge, &json!(3), &[json!("2")]));
        assert!(value_matches(Op::Eq, &json!(5), &[json!("5")]));
    }

    #[test]
    fn dates_and_bools() {
        assert_eq!(
            parse_ts(&json!("24.07.2026 11:45:07"))
                .unwrap()
                .to_rfc3339(),
            "2026-07-24T08:45:07+00:00"
        );
        assert!(parse_ts(&json!("2026-07-24")).is_some());
        assert_eq!(
            map_bools(&json!([true, false]), "Y", "N"),
            json!(["Y", "N"])
        );
        assert_eq!(like_pattern(&json!("a_b")), "%a\\_b%");
    }
}
