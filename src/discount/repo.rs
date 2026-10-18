//! Хранилище правил и купонов: действующие правила, купоны покупателя (как
//! `DiscountCouponsManager` Битрикса в режиме покупателя).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{FromRow, PgPool, types::Json};

use super::engine::CouponStatus;
use super::model::{Coupon, CouponKind, Node, Rule};

/// Купон не годен: статус и текст Битрикса (`BX_SALE_DCM_COUPON_CHECK_*`, через запятую).
#[derive(Debug, Clone, PartialEq)]
pub struct CouponCheck {
    pub status: CouponStatus,
    pub text: String,
}

impl CouponCheck {
    fn not_found() -> CouponCheck {
        CouponCheck {
            status: CouponStatus::NotFound,
            text: "не найден".into(),
        }
    }
}

#[derive(FromRow)]
struct RuleRow {
    id: i64,
    name: String,
    active: bool,
    active_from: Option<DateTime<Utc>>,
    active_to: Option<DateTime<Utc>>,
    priority: i32,
    sort: i32,
    last_discount: bool,
    last_level_discount: bool,
    use_coupons: bool,
    conditions: Json<Value>,
    actions: Json<Value>,
    unsupported: String,
    xml_id: String,
}

/// Действующие на `now` правила (активные, в датах) с группами, в порядке применения.
/// Правило с испорченным деревом пропускается.
pub async fn active_rules(db: &PgPool, now: DateTime<Utc>) -> sqlx::Result<Vec<Rule>> {
    let rows: Vec<RuleRow> = sqlx::query_as(
        "SELECT id, name, active, active_from, active_to, priority, sort, last_discount,
                last_level_discount, use_coupons, conditions, actions, unsupported, xml_id
         FROM discounts
         WHERE active AND (active_from IS NULL OR active_from <= $1)
               AND (active_to IS NULL OR active_to >= $1)
         ORDER BY priority DESC, sort, id",
    )
    .bind(now)
    .fetch_all(db)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let groups: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT discount_id, group_id FROM discount_groups WHERE discount_id = ANY($1)
         ORDER BY discount_id, group_id",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let mut by_rule: HashMap<i64, Vec<i64>> = HashMap::new();
    for (rule, group) in groups {
        by_rule.entry(rule).or_default().push(group);
    }
    let tree = |id: i64, v: Value| {
        serde_json::from_value::<Node>(v)
            .inspect_err(
                |e| tracing::warn!(discount = id, error = %e, "скидки: неверное дерево правила"),
            )
            .ok()
    };
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            Some(Rule {
                conditions: tree(r.id, r.conditions.0)?,
                actions: tree(r.id, r.actions.0)?,
                group_ids: by_rule.remove(&r.id).unwrap_or_default(),
                id: r.id,
                name: r.name,
                active: r.active,
                active_from: r.active_from,
                active_to: r.active_to,
                priority: r.priority,
                sort: r.sort,
                last_discount: r.last_discount,
                last_level_discount: r.last_level_discount,
                use_coupons: r.use_coupons,
                unsupported: r.unsupported,
                xml_id: r.xml_id,
            })
        })
        .collect())
}

#[derive(FromRow)]
struct CouponRow {
    id: i64,
    discount_id: i64,
    coupon: String,
    active: bool,
    active_from: Option<DateTime<Utc>>,
    active_to: Option<DateTime<Utc>>,
    kind: String,
    max_use: i32,
    use_count: i32,
    user_id: Option<i64>,
}

impl From<CouponRow> for Coupon {
    fn from(r: CouponRow) -> Coupon {
        Coupon {
            id: r.id,
            discount_id: r.discount_id,
            coupon: r.coupon,
            active: r.active,
            active_from: r.active_from,
            active_to: r.active_to,
            kind: CouponKind::parse(&r.kind).unwrap_or(CouponKind::Order),
            max_use: r.max_use,
            use_count: r.use_count,
            user_id: r.user_id,
        }
    }
}

const COUPON_COLS: &str = "c.id, c.discount_id, c.coupon, c.active, c.active_from, c.active_to, \
                           c.kind, c.max_use, c.use_count, c.user_id";

/// Купоны, введённые покупателем, по порядку добавления.
pub async fn buyer_coupons(db: &PgPool, buyer_id: i64) -> sqlx::Result<Vec<Coupon>> {
    let rows: Vec<CouponRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COUPON_COLS} FROM buyer_coupons b JOIN discount_coupons c ON c.id = b.coupon_id
         WHERE b.buyer_id = $1 ORDER BY b.added_at, c.id"
    )))
    .bind(buyer_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(Coupon::from).collect())
}

/// Активность и даты правила купона.
type RuleDates = (bool, Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// Проверки `DiscountCouponsManager::checkFullData` (режим покупателя): все не пройденные —
/// текстами через запятую (как `fillCouponHints`), статус — по первой.
fn check(
    cp: &Coupon,
    rule: Option<RuleDates>,
    user_id: Option<i64>,
    now: DateTime<Utc>,
) -> Option<CouponCheck> {
    use CouponStatus::*;
    let (rule_active, rule_from, rule_to) = rule.unwrap_or((false, None, None));
    let checks = [
        (!cp.active, Inactive, "не активен"),
        (
            cp.active_from.is_some_and(|f| f > now),
            Inactive,
            "дата начала активности купона позже текущей",
        ),
        (
            cp.active_to.is_some_and(|t| t < now),
            Expired,
            "дата окончания активности купона раньше текущей",
        ),
        (!rule_active, Inactive, "правило не активно"),
        (
            rule_from.is_some_and(|f| f > now),
            Inactive,
            "дата начала активности правила позже текущей",
        ),
        (
            rule_to.is_some_and(|t| t < now),
            Expired,
            "дата окончания активности правила раньше текущей",
        ),
        (
            cp.user_id.is_some_and(|u| u > 0 && Some(u) != user_id),
            Foreign,
            "неверный владелец купона",
        ),
        (
            cp.max_use > 0 && cp.use_count >= cp.max_use,
            Used,
            "купон был применен максимальное число раз",
        ),
    ];
    let failed: Vec<_> = checks.iter().filter(|(bad, _, _)| *bad).collect();
    let first = failed.first()?;
    Some(CouponCheck {
        status: first.1,
        text: failed
            .iter()
            .map(|(_, _, t)| *t)
            .collect::<Vec<_>>()
            .join(", "),
    })
}

async fn rule_dates(db: &PgPool, discount_id: i64) -> sqlx::Result<Option<RuleDates>> {
    sqlx::query_as("SELECT active, active_from, active_to FROM discounts WHERE id = $1")
        .bind(discount_id)
        .fetch_optional(db)
        .await
}

/// Почему купон сейчас не годен покупателю (`None` — годен).
pub async fn check_coupon(
    db: &PgPool,
    cp: &Coupon,
    user_id: Option<i64>,
    now: DateTime<Utc>,
) -> sqlx::Result<Option<CouponCheck>> {
    Ok(check(
        cp,
        rule_dates(db, cp.discount_id).await?,
        user_id,
        now,
    ))
}

/// Добавляет купон покупателю (`DiscountCouponsManager::add`); негодный не сохраняется,
/// повторный — без дубля.
pub async fn add_buyer_coupon(
    db: &PgPool,
    buyer_id: i64,
    code: &str,
    user_id: Option<i64>,
) -> sqlx::Result<Result<Coupon, CouponCheck>> {
    let code = code.trim();
    if code.is_empty() {
        return Ok(Err(CouponCheck::not_found()));
    }
    let row: Option<CouponRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COUPON_COLS} FROM discount_coupons c WHERE c.coupon = $1"
    )))
    .bind(code)
    .fetch_optional(db)
    .await?;
    let Some(cp) = row.map(Coupon::from) else {
        return Ok(Err(CouponCheck::not_found()));
    };
    if let Some(problem) = check_coupon(db, &cp, user_id, Utc::now()).await? {
        return Ok(Err(problem));
    }
    sqlx::query(
        "INSERT INTO buyer_coupons (buyer_id, coupon_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(buyer_id)
    .bind(cp.id)
    .execute(db)
    .await?;
    Ok(Ok(cp))
}

/// Убирает купон у покупателя; `false` — его не было.
pub async fn remove_buyer_coupon(db: &PgPool, buyer_id: i64, code: &str) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "DELETE FROM buyer_coupons b USING discount_coupons c
         WHERE b.coupon_id = c.id AND b.buyer_id = $1 AND c.coupon = $2",
    )
    .bind(buyer_id)
    .bind(code.trim())
    .execute(db)
    .await?;
    Ok(done.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use serde_json::json;

    use super::*;

    async fn rule(db: &PgPool, name: &str, sql_extra: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "INSERT INTO discounts (name, priority, sort, actions) VALUES ($1, 1, 100, $2) {sql_extra} RETURNING id"
        )))
        .bind(name)
        .bind(json!({"CLASS_ID": "CondGroup", "DATA": {"All": "AND"}, "CHILDREN": []}))
        .fetch_one(db)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn active_rules_filters_and_orders(db: PgPool) {
        let now = Utc::now();
        let a = rule(&db, "a", "").await;
        let b: i64 = sqlx::query_scalar(
            "INSERT INTO discounts (name, priority, sort) VALUES ('b', 5, 100) RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        for sql in [
            "INSERT INTO discounts (name, active) VALUES ('off', FALSE)",
            "INSERT INTO discounts (name, active_from) VALUES ('future', now() + interval '1 day')",
            "INSERT INTO discounts (name, active_to) VALUES ('past', now() - interval '1 day')",
        ] {
            sqlx::query(sql).execute(&db).await.unwrap();
        }
        let g: i64 = sqlx::query_scalar(
            "INSERT INTO groups (code, name) VALUES ('vip', 'VIP') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO discount_groups (discount_id, group_id) VALUES ($1, $2)")
            .bind(a)
            .bind(g)
            .execute(&db)
            .await
            .unwrap();

        let rules = active_rules(&db, now).await.unwrap();
        let ids: Vec<i64> = rules.iter().map(|r| r.id).collect();
        assert_eq!(ids, [b, a], "priority DESC, неактивные и вне дат — нет");
        assert_eq!(rules[1].group_ids, [g]);
        assert_eq!(rules[1].actions.class_id, "CondGroup");
        assert!(rules[0].group_ids.is_empty());
        let later = active_rules(&db, now + Duration::days(2)).await.unwrap();
        assert_eq!(later.len(), 3, "правило «future» начало действовать");
    }

    #[sqlx::test]
    async fn buyer_coupon_add_check_remove(db: PgPool) {
        let f = crate::test_support::order_fixture(&db).await;
        let r = rule(&db, "r", "").await;
        let off = rule(&db, "off", "").await;
        sqlx::query("UPDATE discounts SET active = FALSE WHERE id = $1")
            .bind(off)
            .execute(&db)
            .await
            .unwrap();
        let user: i64 = sqlx::query_scalar(
            "INSERT INTO users (login, password_hash) VALUES ('other', '') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        for (discount, code, extra) in [
            (r, "OK", ""),
            (r, "OLD", ", active_to = now() - interval '1 day'"),
            (r, "SOON", ", active_from = now() + interval '1 day'"),
            (r, "USED", ", max_use = 1, use_count = 1, kind = 'multi'"),
            (r, "NOACT", ", active = FALSE"),
            (off, "OFF", ""),
        ] {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO discount_coupons (discount_id, coupon) VALUES ($1, $2) RETURNING id",
            )
            .bind(discount)
            .bind(code)
            .fetch_one(&db)
            .await
            .unwrap();
            if !extra.is_empty() {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE discount_coupons SET coupon = coupon {extra} WHERE id = $1"
                )))
                .bind(id)
                .execute(&db)
                .await
                .unwrap();
            }
        }
        sqlx::query(
            "INSERT INTO discount_coupons (discount_id, coupon, user_id) VALUES ($1, 'MINE', $2)",
        )
        .bind(r)
        .bind(user)
        .execute(&db)
        .await
        .unwrap();

        let err = |res: Result<Coupon, CouponCheck>| res.unwrap_err();
        let add = |code: &'static str, uid: Option<i64>| {
            let db = db.clone();
            async move { add_buyer_coupon(&db, f.buyer_id, code, uid).await.unwrap() }
        };
        let e = err(add("NOPE", None).await);
        assert_eq!(
            (e.status, e.text.as_str()),
            (CouponStatus::NotFound, "не найден")
        );
        let e = err(add("OLD", None).await);
        assert_eq!(
            (e.status, e.text.as_str()),
            (
                CouponStatus::Expired,
                "дата окончания активности купона раньше текущей"
            )
        );
        let e = err(add("SOON", None).await);
        assert_eq!(
            (e.status, e.text.as_str()),
            (
                CouponStatus::Inactive,
                "дата начала активности купона позже текущей"
            )
        );
        let e = err(add("USED", None).await);
        assert_eq!(
            (e.status, e.text.as_str()),
            (
                CouponStatus::Used,
                "купон был применен максимальное число раз"
            )
        );
        let e = err(add("NOACT", None).await);
        assert_eq!(
            (e.status, e.text.as_str()),
            (CouponStatus::Inactive, "не активен")
        );
        let e = err(add("OFF", None).await);
        assert_eq!(
            (e.status, e.text.as_str()),
            (CouponStatus::Inactive, "правило не активно")
        );
        let e = err(add("MINE", None).await);
        assert_eq!(
            (e.status, e.text.as_str()),
            (CouponStatus::Foreign, "неверный владелец купона")
        );
        assert!(add("MINE", Some(user)).await.is_ok(), "свой купон");
        sqlx::query("DELETE FROM buyer_coupons")
            .execute(&db)
            .await
            .unwrap();

        let ok = add(" OK ", None).await.unwrap();
        assert_eq!(ok.coupon, "OK");
        assert_eq!(ok.discount_id, r);
        add("OK", None).await.unwrap();
        let list = buyer_coupons(&db, f.buyer_id).await.unwrap();
        assert_eq!(list.len(), 1, "повторное добавление — без дубля");
        assert_eq!(list[0].coupon, "OK");

        assert!(remove_buyer_coupon(&db, f.buyer_id, "OK").await.unwrap());
        assert!(!remove_buyer_coupon(&db, f.buyer_id, "OK").await.unwrap());
        assert!(buyer_coupons(&db, f.buyer_id).await.unwrap().is_empty());
    }
}
