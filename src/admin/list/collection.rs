//! Спека списка записей коллекции: стандартные колонки, поля, цены и остатки каталога.

use sqlx::PgPool;

use super::{
    spec::{Column, EnumOption, FieldType, Filter, FilterSql, Join, ListSpec},
    sql::Scope,
    state::{SectionSel, Sort},
};
use crate::{
    access::{Access, COLLECTIONS_MANAGE, Level},
    collection::{Collection, Field, Section, is_valid_code, repo, section_subtree_ids},
};

const STD_GROUP: &str = "Запись";
const FIELDS_GROUP: &str = "Поля коллекции";
const CATALOG_GROUP: &str = "Каталог";

/// Коды стандартных колонок: поле с таким кодом пропускается.
const STD_CODES: [&str; 8] = [
    "id",
    "name",
    "code",
    "active",
    "sort",
    "section",
    "published_at",
    "updated_at",
];

/// Коды каталога: `price_<n>`, `qty`, `store_<n>`, `instock`.
fn is_catalog_code(code: &str) -> bool {
    let numbered = |p: &str| {
        code.strip_prefix(p)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    code == "qty" || code == "instock" || numbered("price_") || numbered("store_")
}

fn col(code: &str, title: &str, expr: &str, ty: FieldType) -> Column {
    Column {
        code: code.into(),
        title: title.into(),
        expr: expr.into(),
        ty,
        sortable: true,
        default: true,
        required: false,
        join: None,
        currency_expr: None,
    }
}

fn filter(code: &str, label: &str, group: &str, ty: FieldType, expr: &str) -> Filter {
    Filter {
        code: code.into(),
        label: label.into(),
        group: group.into(),
        ty,
        sql: FilterSql::Expr(expr.into()),
        options: vec![],
        join: None,
    }
}

/// Значение поля как JSON-массив чисел-id: скаляр оборачивается, остальное — пусто.
fn id_array(v: &str) -> String {
    format!(
        "CASE jsonb_typeof({v}) WHEN 'array' THEN {v} WHEN 'number' THEN jsonb_build_array({v}) ELSE '[]'::jsonb END"
    )
}

/// Целые id из значения: нечисловые и нецелые элементы пропускаются без ошибки приведения.
fn ids_subquery(v: &str) -> String {
    format!(
        "SELECT CASE WHEN x ~ '^\\d{{1,18}}$' THEN x::bigint END AS id FROM jsonb_array_elements_text({}) x",
        id_array(v)
    )
}

/// Календарно допустимая дата `YYYY-MM-DD` без приведения при мусоре.
/// Регулярка отсекает невозможные месяц и день (в т.ч. 31 апреля, 30 февраля, год 0000);
/// 29 февраля проверяется отдельно по високосности года.
/// Остаточного риска нет: ветка с приведением достижима только для существующей даты.
fn date_expr(v: &str) -> String {
    let main = r"^(?!0000)\d{4}-((0[13578]|1[02])-(0[1-9]|[12]\d|3[01])|(0[469]|11)-(0[1-9]|[12]\d|30)|02-(0[1-9]|1\d|2[0-8]))$";
    let leap = format!(
        "{v} ~ '^(?!0000)\\d{{4}}-02-29$' AND ((substr({v}, 1, 4)::int % 4 = 0 AND substr({v}, 1, 4)::int % 100 <> 0) OR substr({v}, 1, 4)::int % 400 = 0)"
    );
    format!("CASE WHEN {v} ~ '{main}' THEN {v}::date WHEN {leap} THEN {v}::date END")
}

fn field_entries(
    f: &Field,
    options: &[EnumOption],
    out_cols: &mut Vec<Column>,
    out_filters: &mut Vec<Filter>,
) {
    let code = f.code.as_str();
    let raw = format!("(ci.field_values->'{code}')");
    let text = format!("(ci.field_values->>'{code}')");
    let single_sortable = !f.multiple;
    let column = |ty: FieldType, expr: String, sortable: bool| {
        let mut c = col(code, &f.name, &expr, ty);
        c.default = false;
        c.sortable = sortable;
        c
    };
    // Элементы массива через запятую; устаревший скаляр (строка или число, записанный до того,
    // как поле стало множественным) показывается как есть; остальное — пусто.
    let joined = |elem: &str| {
        format!(
            "(CASE jsonb_typeof({raw}) \
             WHEN 'array' THEN (SELECT string_agg(e, ', ') FROM jsonb_array_elements_text({raw}) e{elem}) \
             WHEN 'string' THEN {text} WHEN 'number' THEN {text} END)"
        )
    };
    let fl = |ty: FieldType, expr: &str| filter(code, &f.name, FIELDS_GROUP, ty, expr);
    match f.kind.as_str() {
        "string" | "text" => {
            let expr = if f.multiple { joined("") } else { text.clone() };
            out_cols.push(column(FieldType::Text, expr.clone(), single_sortable));
            out_filters.push(fl(FieldType::Text, &expr));
        }
        // Множественные числа и даты: только колонка со значениями через запятую.
        "number" => {
            if f.multiple {
                out_cols.push(column(FieldType::Text, joined(""), false));
            } else {
                let expr =
                    format!("CASE WHEN jsonb_typeof({raw}) = 'number' THEN {text}::numeric END");
                out_cols.push(column(FieldType::Number, expr.clone(), true));
                out_filters.push(fl(FieldType::Number, &expr));
            }
        }
        "date" => {
            if f.multiple {
                out_cols.push(column(FieldType::Text, joined(""), false));
            } else {
                let expr = date_expr(&text);
                out_cols.push(column(FieldType::Date, expr.clone(), true));
                out_filters.push(fl(FieldType::Date, &expr));
            }
        }
        "boolean" => {
            let expr = format!("COALESCE({raw} = 'true'::jsonb, FALSE)");
            out_cols.push(column(FieldType::Bool, expr.clone(), true));
            out_filters.push(fl(FieldType::Bool, &expr));
        }
        "list" => {
            let labels = format!(
                "(SELECT string_agg(o.value, ', ' ORDER BY o.sort, o.id) FROM collection_field_options o \
                 WHERE o.field_id = {} AND o.id IN ({}))",
                f.id,
                ids_subquery(&raw)
            );
            out_cols.push(column(FieldType::Enum, labels, single_sortable));
            let mut flt = fl(FieldType::Enum, "");
            flt.sql = FilterSql::JsonField {
                key: code.into(),
                multiple: f.multiple,
            };
            flt.options = options.to_vec();
            out_filters.push(flt);
        }
        "element" => {
            let names = format!(
                "(SELECT string_agg(l.name, ', ' ORDER BY l.name) FROM collection_items l \
                 WHERE l.id IN ({}))",
                ids_subquery(&raw)
            );
            out_cols.push(column(FieldType::Link, names, single_sortable));
            let mut flt = fl(FieldType::Link, "");
            flt.sql = FilterSql::JsonField {
                key: code.into(),
                multiple: f.multiple,
            };
            out_filters.push(flt);
        }
        "file" => {
            let n = format!(
                "(SELECT count(*) FROM ({}) q WHERE q.id IS NOT NULL)",
                ids_subquery(&raw)
            );
            out_cols.push(column(FieldType::File, n, false));
        }
        _ => {}
    }
}

pub async fn collection_spec(db: &PgPool, user: &Access, c: &Collection) -> sqlx::Result<ListSpec> {
    let id = c.id;
    let mut columns = vec![
        col("id", "ID", "ci.id", FieldType::Number),
        {
            let mut n = col("name", "Название", "ci.name", FieldType::Text);
            n.required = true;
            n
        },
        col("code", "Код", "ci.code", FieldType::Text),
        col("active", "Активна", "ci.active", FieldType::Bool),
        col("sort", "Сорт.", "ci.sort", FieldType::Number),
        col(
            "section",
            "Раздел",
            "(SELECT s.name FROM collection_sections s WHERE s.id = ci.section_id)",
            FieldType::Text,
        ),
        col(
            "published_at",
            "Публикация",
            "ci.published_at",
            FieldType::Date,
        ),
        col("updated_at", "Изменена", "ci.updated_at", FieldType::Date),
    ];
    columns[3].default = true;
    columns[4].default = false;
    columns[5].sortable = false;
    columns[6].default = false;
    let mut filters = vec![
        filter("name", "Название", STD_GROUP, FieldType::Text, "ci.name"),
        filter("code", "Код", STD_GROUP, FieldType::Text, "ci.code"),
        filter("active", "Активна", STD_GROUP, FieldType::Bool, "ci.active"),
        filter("id", "ID", STD_GROUP, FieldType::Number, "ci.id"),
        filter(
            "published_at",
            "Публикация",
            STD_GROUP,
            FieldType::Date,
            "ci.published_at",
        ),
        filter(
            "updated_at",
            "Изменена",
            STD_GROUP,
            FieldType::Date,
            "ci.updated_at",
        ),
    ];

    let fields = repo::list_fields(db, id).await?;
    let all_options = repo::list_collection_options(db, id).await?;
    for f in fields.iter().filter(|f| {
        is_valid_code(&f.code) && !STD_CODES.contains(&f.code.as_str()) && !is_catalog_code(&f.code)
    }) {
        let opts: Vec<EnumOption> = all_options
            .iter()
            .filter(|o| o.field_id == f.id)
            .map(|o| EnumOption {
                value: o.id.to_string(),
                label: o.value.clone(),
            })
            .collect();
        let (mut cs, mut fs) = (vec![], vec![]);
        field_entries(f, &opts, &mut cs, &mut fs);
        columns.extend(cs);
        for mut f in fs {
            f.group = FIELDS_GROUP.into();
            filters.push(f);
        }
    }

    let mut joins: Vec<Join> = vec![];
    if c.is_catalog {
        let types: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, name FROM catalog_price_types ORDER BY is_base DESC, sort, id",
        )
        .fetch_all(db)
        .await?;
        for (i, (tid, name)) in types.iter().enumerate() {
            let code = format!("price_{tid}");
            joins.push(Join {
                code: code.clone(),
                sql: format!(
                    " LEFT JOIN catalog_prices p{tid} ON p{tid}.item_id = ci.id AND p{tid}.price_type_id = {tid} AND p{tid}.quantity_from IS NULL"
                ),
            });
            let expr = format!("p{tid}.price");
            let mut cl = col(&code, name, &expr, FieldType::Money);
            cl.default = i == 0;
            cl.join = Some(code.clone());
            cl.currency_expr = Some(format!("p{tid}.currency"));
            columns.push(cl);
            let mut fl = filter(&code, name, CATALOG_GROUP, FieldType::Money, &expr);
            fl.join = Some(code);
            filters.push(fl);
        }
        joins.push(Join {
            code: "qty".into(),
            sql: " LEFT JOIN catalog_products cp ON cp.item_id = ci.id".into(),
        });
        let qty = "COALESCE(cp.quantity, 0)";
        let mut cl = col("qty", "Остаток", qty, FieldType::Number);
        cl.join = Some("qty".into());
        columns.push(cl);
        let mut fl = filter("qty", "Остаток", CATALOG_GROUP, FieldType::Number, qty);
        fl.join = Some("qty".into());
        filters.push(fl);
        let mut fl = filter(
            "instock",
            "В наличии",
            CATALOG_GROUP,
            FieldType::Bool,
            "(COALESCE(cp.quantity, 0) > 0)",
        );
        fl.join = Some("qty".into());
        filters.push(fl);

        let stores: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, name FROM catalog_stores WHERE active ORDER BY sort, id")
                .fetch_all(db)
                .await?;
        for (sid, name) in stores {
            let code = format!("store_{sid}");
            joins.push(Join {
                code: code.clone(),
                sql: format!(
                    " LEFT JOIN catalog_store_amounts sa{sid} ON sa{sid}.item_id = ci.id AND sa{sid}.store_id = {sid}"
                ),
            });
            let expr = format!("COALESCE(sa{sid}.amount, 0)");
            let mut cl = col(&code, &name, &expr, FieldType::Number);
            cl.default = false;
            cl.join = Some(code.clone());
            columns.push(cl);
            let mut fl = filter(&code, &name, CATALOG_GROUP, FieldType::Number, &expr);
            fl.join = Some(code);
            filters.push(fl);
        }
    }

    Ok(ListSpec {
        key: format!("collection:{id}"),
        id_expr: "ci.id".into(),
        from: format!("FROM collection_items ci WHERE ci.collection_id = {id}"),
        joins,
        columns,
        filters,
        default_sort: Sort {
            code: "updated_at".into(),
            desc: true,
        },
        tie_break: "ci.id DESC".into(),
        sections: true,
        peek_url: c.is_catalog.then(|| "/admin/items/{id}/peek".to_string()),
        open_url: "/admin/items/{id}".into(),
        can_share: user.collection_level(id) >= Level::Write || user.can(COLLECTIONS_MANAGE),
        muted_expr: Some("NOT ci.active".into()),
        label_expr: "ci.name".into(),
        bulk_actions: Vec::new(),
    })
}

/// Область по выбранному разделу. Раздел с потомками, либо только он (`only`);
/// удалённый или чужой раздел — весь список.
pub async fn section_scope(
    db: &PgPool,
    collection_id: i64,
    sel: &Option<SectionSel>,
    only: bool,
) -> sqlx::Result<(Scope, Option<Section>)> {
    match sel {
        None => Ok((Scope::All, None)),
        Some(SectionSel::Without) => Ok((Scope::Without, None)),
        Some(SectionSel::Id(id)) => {
            let sections = repo::list_sections(db, collection_id).await?;
            let Some(section) = sections.iter().find(|s| s.id == *id).cloned() else {
                return Ok((Scope::All, None));
            };
            let mut ids: Vec<i64> = if only {
                vec![*id]
            } else {
                section_subtree_ids(&sections, *id).into_iter().collect()
            };
            ids.sort_unstable();
            Ok((Scope::Sections(ids), Some(section)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        admin::list::{sql::fetch, state::parse},
        auth::User,
        collection::{FieldInput, repo},
        test_support::{cart_fixture, content_fixture},
    };
    use serde_json::json;

    async fn admin(db: &PgPool) -> Access {
        Access::load(
            db,
            User {
                id: 1,
                login: "a".into(),
                email: None,
                is_admin: true,
                active: true,
            },
        )
        .await
        .unwrap()
    }

    async fn ids(db: &PgPool, spec: &ListSpec, q: &str) -> Vec<i64> {
        let st = parse(spec, q).state;
        let (rows, _) = fetch(db, spec, &st, &Scope::All).await.unwrap();
        let mut ids: Vec<i64> = rows.into_iter().map(|r| r.id).collect();
        ids.sort_unstable();
        ids
    }

    async fn field(db: &PgPool, c: i64, code: &str, kind: &str, multiple: bool) -> i64 {
        repo::create_field(
            db,
            c,
            &FieldInput {
                code: code.into(),
                name: code.into(),
                kind: kind.into(),
                is_required: false,
                sort: 500,
                multiple,
                link_collection_id: (kind == "element").then_some(c),
                in_basket: false,
                offer_tree: false,
            },
        )
        .await
        .unwrap()
    }

    async fn item(db: &PgPool, c: i64, code: &str, values: serde_json::Value) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name, field_values)
             VALUES ($1, $2, $2, $3) RETURNING id",
        )
        .bind(c)
        .bind(code)
        .bind(sqlx::types::Json(values))
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn option(db: &PgPool, field_id: i64, value: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO collection_field_options (field_id, value, xml_id) VALUES ($1, $2, $2) RETURNING id",
        )
        .bind(field_id)
        .bind(value)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn spec_of(db: &PgPool, id: i64) -> ListSpec {
        let c = repo::get_collection(db, id).await.unwrap().unwrap();
        collection_spec(db, &admin(db).await, &c).await.unwrap()
    }

    #[sqlx::test]
    async fn multiple_string_with_legacy_scalar(db: PgPool) {
        let f = content_fixture(&db).await;
        let c = f.collection_id;
        sqlx::query("DELETE FROM collection_items WHERE id = $1")
            .bind(f.item_id)
            .execute(&db)
            .await
            .unwrap();
        field(&db, c, "tags", "string", true).await;
        let a = item(&db, c, "a", json!({"tags": "red"})).await;
        let b = item(&db, c, "b", json!({"tags": ["blue", "green"]})).await;
        let d = item(&db, c, "d", json!({"tags": 7})).await;
        let spec = spec_of(&db, c).await;
        assert_eq!(ids(&db, &spec, "f.tags=has:re").await, vec![a, b]);
        assert_eq!(ids(&db, &spec, "f.tags=has:7").await, vec![d]);
        let st = parse(&spec, "cols=name,tags&sort=id").state;
        let (rows, _) = fetch(&db, &spec, &st, &Scope::All).await.unwrap();
        let cell = |id: i64| rows.iter().find(|r| r.id == id).unwrap().cells["tags"].clone();
        assert_eq!(cell(a), json!("red"));
        assert_eq!(cell(b), json!("blue, green"));
        assert_eq!(cell(d), json!("7"));
    }

    #[sqlx::test]
    async fn filters_by_collection_fields(db: PgPool) {
        let f = content_fixture(&db).await;
        let c = f.collection_id;
        sqlx::query("DELETE FROM collection_items WHERE id = $1")
            .bind(f.item_id)
            .execute(&db)
            .await
            .unwrap();
        let blue = option(&db, f.field_id, "Синий").await;
        let tags = field(&db, c, "tags", "list", true).await;
        let t1 = option(&db, tags, "Раз").await;
        let t2 = option(&db, tags, "Два").await;
        field(&db, c, "weight", "number", false).await;
        field(&db, c, "made", "date", false).await;
        field(&db, c, "flag", "boolean", false).await;
        let a = item(
            &db,
            c,
            "a",
            json!({"color": f.option_id, "tags": [t1], "weight": 5, "made": "2026-03-01", "flag": true}),
        )
        .await;
        let b = item(
            &db,
            c,
            "b",
            json!({"color": blue, "tags": [t1, t2], "weight": 0.5, "made": "2026-05-01", "flag": false, "link": a}),
        )
        .await;
        let d = item(&db, c, "d", json!({"tags": []})).await;
        let spec = spec_of(&db, c).await;
        assert_eq!(ids(&db, &spec, "").await, vec![a, b, d]);
        assert_eq!(
            ids(&db, &spec, &format!("f.color=in:{blue}")).await,
            vec![b]
        );
        assert_eq!(
            ids(&db, &spec, &format!("f.color=nin:{blue}")).await,
            vec![a, d]
        );
        assert_eq!(ids(&db, &spec, &format!("f.tags=in:{t2}")).await, vec![b]);
        assert_eq!(
            ids(&db, &spec, &format!("f.tags=in:{t1}")).await,
            vec![a, b]
        );
        assert_eq!(
            ids(&db, &spec, &format!("f.tags=nin:{t2}")).await,
            vec![a, d]
        );
        assert_eq!(ids(&db, &spec, "f.weight=gt:1").await, vec![a]);
        assert_eq!(
            ids(&db, &spec, "f.made=range:2026-04-01,2026-12-31").await,
            vec![b]
        );
        assert_eq!(ids(&db, &spec, &format!("f.link=eq:{a}")).await, vec![b]);
        assert_eq!(ids(&db, &spec, "f.flag=eq:Y").await, vec![a]);
        assert_eq!(ids(&db, &spec, "f.flag=eq:N").await, vec![b, d]);
        // колонка списка показывает подписи вариантов
        let st = parse(&spec, "cols=name,color,tags&sort=id").state;
        let (rows, _) = fetch(&db, &spec, &st, &Scope::All).await.unwrap();
        assert_eq!(rows[1].cells["color"], json!("Синий"));
        assert_eq!(rows[1].cells["tags"], json!("Раз, Два"));
    }

    #[sqlx::test]
    async fn bad_json_value_does_not_break(db: PgPool) {
        let f = content_fixture(&db).await;
        let c = f.collection_id;
        field(&db, c, "weight", "number", false).await;
        field(&db, c, "made", "date", false).await;
        let bad = item(
            &db,
            c,
            "bad",
            json!({"weight": "abc", "made": "вчера", "color": "x", "link": [1, "y"]}),
        )
        .await;
        let bad2 = item(&db, c, "bad2", json!({"made": "2026-13-45"})).await;
        let bad3 = item(&db, c, "bad3", json!({"made": "2026-02-30"})).await;
        let bad4 = item(&db, c, "bad4", json!({"made": "2026-02-29"})).await;
        let ok = item(&db, c, "ok", json!({"made": "2024-02-29", "weight": 3})).await;
        let spec = spec_of(&db, c).await;
        for id in [bad, bad2, bad3, bad4] {
            let q = "f.weight=gt:1&f.made=range:2026-01-01,";
            assert!(!ids(&db, &spec, q).await.contains(&id));
            assert!(
                !ids(&db, &spec, "f.made=range:2000-01-01,")
                    .await
                    .contains(&id)
            );
        }
        assert_eq!(
            ids(&db, &spec, "f.made=range:2024-01-01,2024-12-31").await,
            vec![ok]
        );
        assert_eq!(ids(&db, &spec, "f.weight=gt:1").await, vec![ok]);
        // колонки с мусором тоже не падают
        let st = parse(&spec, "cols=name,weight,made,color,link&sort=made").state;
        fetch(&db, &spec, &st, &Scope::All).await.unwrap();
    }

    #[sqlx::test]
    async fn catalog_price_stock_store(db: PgPool) {
        let cf = cart_fixture(&db).await;
        let c: i64 = sqlx::query_scalar("SELECT collection_id FROM collection_items WHERE id = $1")
            .bind(cf.f.product_id)
            .fetch_one(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE collections SET is_catalog = TRUE WHERE id = $1")
            .bind(c)
            .execute(&db)
            .await
            .unwrap();
        let nop = item(&db, c, "nop", json!({})).await;
        sqlx::query("INSERT INTO catalog_products (item_id, quantity) VALUES ($1, 0) ON CONFLICT (item_id) DO UPDATE SET quantity = 0")
            .bind(nop)
            .execute(&db)
            .await
            .unwrap();
        let prod = cf.f.product_id;
        sqlx::query("UPDATE catalog_products SET quantity = 7 WHERE item_id = $1")
            .bind(prod)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO catalog_stores (id, name, active) VALUES (501, 'Склад', TRUE), (502, 'Скрытый', FALSE)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO catalog_store_amounts (item_id, store_id, amount) VALUES ($1, 501, 4)",
        )
        .bind(prod)
        .execute(&db)
        .await
        .unwrap();
        let spec = spec_of(&db, c).await;
        assert!(spec.filter("store_501").is_some());
        assert!(spec.filter("store_502").is_none());
        assert!(spec.column("price_1").unwrap().default);
        assert!(spec.column("qty").unwrap().default);
        assert!(spec.peek_url.is_some());
        let all = ids(&db, &spec, "").await;
        assert!(all.contains(&prod) && all.contains(&nop));
        let empty = ids(&db, &spec, "f.price_1=empty").await;
        assert!(empty.contains(&nop) && !empty.contains(&prod));
        assert_eq!(ids(&db, &spec, "f.price_1=gt:500").await, vec![prod]);
        assert!(ids(&db, &spec, "f.qty=eq:0").await.contains(&nop));
        assert!(!ids(&db, &spec, "f.qty=eq:0").await.contains(&prod));
        assert_eq!(ids(&db, &spec, "f.store_501=gt:0").await, vec![prod]);
        assert_eq!(ids(&db, &spec, "f.instock=eq:Y").await, vec![prod]);
        assert!(ids(&db, &spec, "f.instock=eq:N").await.contains(&nop));
        let st = parse(&spec, "cols=name,price_1,qty,store_501&sort=-price_1").state;
        let (rows, _) = fetch(&db, &spec, &st, &Scope::All).await.unwrap();
        assert_eq!(rows[0].id, prod);
        assert_eq!(rows[0].cells["price_1__cur"], json!("RUB"));
    }

    #[sqlx::test]
    async fn section_scope_includes_children(db: PgPool) {
        let f = content_fixture(&db).await;
        let c = f.collection_id;
        let child: i64 = sqlx::query_scalar(
            "INSERT INTO collection_sections (collection_id, parent_id, code, name) VALUES ($1, $2, 'child', 'Под') RETURNING id",
        )
        .bind(c)
        .bind(f.section_id)
        .fetch_one(&db)
        .await
        .unwrap();
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name) VALUES ('o', 'O') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let foreign: i64 = sqlx::query_scalar(
            "INSERT INTO collection_sections (collection_id, code, name) VALUES ($1, 'x', 'X') RETURNING id",
        )
        .bind(other)
        .fetch_one(&db)
        .await
        .unwrap();
        let (scope, sec) = section_scope(&db, c, &Some(SectionSel::Id(f.section_id)), false)
            .await
            .unwrap();
        let Scope::Sections(mut v) = scope else {
            panic!("ожидались разделы")
        };
        v.sort_unstable();
        assert_eq!(v, vec![f.section_id, child]);
        assert_eq!(sec.unwrap().id, f.section_id);
        let (scope, _) = section_scope(&db, c, &Some(SectionSel::Id(f.section_id)), true)
            .await
            .unwrap();
        assert!(matches!(scope, Scope::Sections(v) if v == vec![f.section_id]));
        let (scope, sec) = section_scope(&db, c, &Some(SectionSel::Without), false)
            .await
            .unwrap();
        assert!(matches!(scope, Scope::Without) && sec.is_none());
        for sel in [
            Some(SectionSel::Id(foreign)),
            Some(SectionSel::Id(999_999)),
            None,
        ] {
            let (scope, sec) = section_scope(&db, c, &sel, false).await.unwrap();
            assert!(matches!(scope, Scope::All) && sec.is_none());
        }
    }

    #[sqlx::test]
    async fn deleted_field_dropped(db: PgPool) {
        let f = content_fixture(&db).await;
        let spec = spec_of(&db, f.collection_id).await;
        assert!(parse(&spec, "f.color=in:1").dropped.is_empty());
        repo::delete_field(&db, f.field_id).await.unwrap();
        let spec = spec_of(&db, f.collection_id).await;
        assert_eq!(
            parse(&spec, "f.color=in:1").dropped,
            vec!["color".to_string()]
        );
    }
}
