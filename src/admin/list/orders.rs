//! Спека списка заказов.

use sqlx::PgPool;

use super::{
    bulk::{BulkAction, BulkParam, Confirm},
    spec::{Column, EnumOption, FieldType, Filter, FilterSql, ListSpec},
    state::Sort,
};
use crate::{
    access::{Access, ORDERS_MANAGE},
    admin::shop::orders::BUYER_TEXT_SQL,
};

/// Покупатель для показа: свойство-плательщик, иначе логин; запрос с `orders o` и `users u`.
pub(crate) const BUYER_SQL: &str = "COALESCE(NULLIF((SELECT v.value FROM order_property_values v
                                 JOIN order_properties p ON p.id = v.property_id
                                 WHERE v.order_id = o.id AND p.is_payer LIMIT 1), ''), u.login, '—')";

const DELIVERY_SQL: &str =
    "COALESCE((SELECT sh.delivery_name FROM shipments sh WHERE sh.order_id = o.id LIMIT 1), '')";

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
        editable: None,
        edit_expr: None,
        badge: false,
    }
}

fn filter(code: &str, label: &str, ty: FieldType, expr: &str, options: Vec<EnumOption>) -> Filter {
    Filter {
        code: code.into(),
        label: label.into(),
        group: "Заказ".into(),
        ty,
        sql: FilterSql::Expr(expr.into()),
        options,
        join: None,
    }
}

async fn options(db: &PgPool, sql: &'static str) -> sqlx::Result<Vec<EnumOption>> {
    let rows: Vec<(String, String)> = sqlx::query_as(sql).fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .map(|(value, label)| EnumOption { value, label })
        .collect())
}

pub async fn orders_spec(db: &PgPool, user: &Access) -> sqlx::Result<ListSpec> {
    build_spec(db, user.can(ORDERS_MANAGE)).await
}

pub(crate) async fn build_spec(db: &PgPool, can_share: bool) -> sqlx::Result<ListSpec> {
    let statuses = options(
        db,
        "SELECT code, name FROM order_statuses ORDER BY sort, code",
    )
    .await?;
    let pay_systems = options(
        db,
        "SELECT id::text, name FROM pay_systems ORDER BY sort, id",
    )
    .await?;
    let deliveries = options(
        db,
        "SELECT id::text, name FROM deliveries ORDER BY sort, id",
    )
    .await?;

    let mut number = col(
        "number",
        "Номер",
        "COALESCE(o.account_number, o.id::text)",
        FieldType::Text,
    );
    number.required = true;
    let mut price = col("price", "Сумма", "o.price", FieldType::Money);
    price.currency_expr = Some("o.currency".into());
    let mut canceled = col("canceled", "Отменён", "o.canceled", FieldType::Bool);
    canceled.default = false;
    let mut status = col(
        "status",
        "Статус",
        "COALESCE(s.name, o.status)",
        FieldType::Enum,
    );
    status.badge = true;
    let columns = vec![
        number,
        col("created_at", "Дата", "o.created_at", FieldType::Date),
        col("buyer", "Покупатель", BUYER_SQL, FieldType::Text),
        status,
        price,
        col("paid", "Оплата", "o.paid", FieldType::Bool),
        col("delivery", "Доставка", DELIVERY_SQL, FieldType::Text),
        canceled,
    ];
    // Платёжная система и служба — по первой оплате / отгрузке заказа.
    let pay_expr = "COALESCE((SELECT pm.pay_system_id::text FROM payments pm WHERE pm.order_id = o.id ORDER BY pm.id LIMIT 1), '')";
    let delivery_expr = "COALESCE((SELECT sh.delivery_id::text FROM shipments sh WHERE sh.order_id = o.id ORDER BY sh.id LIMIT 1), '')";
    let filters = vec![
        filter(
            "number",
            "Номер",
            FieldType::Text,
            "o.account_number",
            vec![],
        ),
        filter(
            "status",
            "Статус",
            FieldType::Enum,
            "o.status",
            statuses.clone(),
        ),
        filter(
            "created_at",
            "Дата",
            FieldType::Date,
            "o.created_at",
            vec![],
        ),
        filter(
            "buyer",
            "Покупатель",
            FieldType::Text,
            BUYER_TEXT_SQL,
            vec![],
        ),
        filter("paid", "Оплачен", FieldType::Bool, "o.paid", vec![]),
        filter("canceled", "Отменён", FieldType::Bool, "o.canceled", vec![]),
        filter("price", "Сумма", FieldType::Money, "o.price", vec![]),
        filter(
            "pay_system",
            "Платёжная система",
            FieldType::Enum,
            pay_expr,
            pay_systems,
        ),
        filter(
            "delivery",
            "Доставка",
            FieldType::Enum,
            delivery_expr,
            deliveries,
        ),
    ];
    let bulk_actions = if can_share {
        let action = |code, label: &str, param, notify| BulkAction {
            code,
            label: label.into(),
            param,
            confirm: Confirm::Always,
            notify,
        };
        vec![
            action(
                "status",
                "Сменить статус…",
                BulkParam::Choice(statuses.clone()),
                true,
            ),
            action("paid", "Отметить оплаченным", BulkParam::None, true),
            action("unpaid", "Снять оплату", BulkParam::None, false),
            action("cancel", "Отменить", BulkParam::Reason, true),
            action("uncancel", "Снять отмену", BulkParam::None, false),
        ]
    } else {
        Vec::new()
    };
    Ok(ListSpec {
        key: "orders".into(),
        id_expr: "o.id".into(),
        from: "FROM orders o LEFT JOIN users u ON u.id = o.user_id LEFT JOIN order_statuses s ON s.code = o.status WHERE TRUE".into(),
        joins: vec![],
        columns,
        filters,
        default_sort: Sort {
            code: "created_at".into(),
            desc: true,
        },
        tie_break: "o.id DESC".into(),
        sections: false,
        peek_url: Some("/admin/shop/orders/{id}/peek".into()),
        open_url: "/admin/shop/orders/{id}".into(),
        can_share,
        muted_expr: Some("o.canceled".into()),
        label_expr: "COALESCE(o.account_number, o.id::text)".into(),
        version_expr: None,
        bulk_actions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::list::{
        sql::{Scope, fetch},
        state::parse,
    };
    use crate::test_support::{order_fixture, place_order};

    async fn ids(db: &PgPool, spec: &ListSpec, q: &str) -> Vec<i64> {
        let st = parse(spec, q).state;
        let (rows, _) = fetch(db, spec, &st, &Scope::All).await.unwrap();
        let mut ids: Vec<i64> = rows.into_iter().map(|r| r.id).collect();
        ids.sort_unstable();
        ids
    }

    #[sqlx::test]
    async fn status_column_is_badge(db: PgPool) {
        let spec = build_spec(&db, false).await.unwrap();
        let status = spec
            .columns
            .iter()
            .find(|column| column.code == "status")
            .expect("колонка статуса");
        assert!(status.badge, "статус должен показываться меткой");
    }

    /// Старая форма фильтров: номер, статус, период, покупатель, оплата, отмена.
    #[sqlx::test]
    async fn orders_filters_match_old_form(db: PgPool) {
        let f = order_fixture(&db).await;
        sqlx::query("INSERT INTO order_statuses (code, name) VALUES ('F', 'Выполнен')")
            .execute(&db)
            .await
            .unwrap();
        let ivan: i64 = sqlx::query_scalar(
            "INSERT INTO users (login, email, password_hash) VALUES ('ivan', 'i@e.x', 'x') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        // a: ivan, N, оплачен, 02.10;  b: F, 05.10;  c: ivan, N, отменён, 20.09;  d: N, 08.10
        let a = place_order(&db, &f, Some(ivan)).await;
        let b = place_order(&db, &f, None).await;
        let c = place_order(&db, &f, Some(ivan)).await;
        let d = place_order(&db, &f, None).await;
        for (id, number, status, paid, canceled, at) in [
            (a, "A-1", "N", true, false, "2026-10-02 12:00+00"),
            (b, "B-2", "F", false, false, "2026-10-05 12:00+00"),
            (c, "C-3", "N", false, true, "2026-09-20 12:00+00"),
            (d, "D-4", "N", false, false, "2026-10-08 12:00+00"),
        ] {
            sqlx::query(
                "UPDATE orders SET account_number = $2, status = $3, paid = $4, canceled = $5,
                        created_at = $6::timestamptz WHERE id = $1",
            )
            .bind(id)
            .bind(number)
            .bind(status)
            .bind(paid)
            .bind(canceled)
            .bind(at)
            .execute(&db)
            .await
            .unwrap();
        }
        let spec = build_spec(&db, false).await.unwrap();
        let mut all = vec![a, b, c, d];
        all.sort_unstable();
        assert_eq!(ids(&db, &spec, "").await, all);
        assert_eq!(ids(&db, &spec, "f.number=eq:A-1").await, vec![a]);
        assert_eq!(ids(&db, &spec, "f.status=in:F").await, vec![b]);
        let mut n = vec![a, c, d];
        n.sort_unstable();
        assert_eq!(ids(&db, &spec, "f.status=in:N").await, n);
        let mut r = vec![a, b, d];
        r.sort_unstable();
        assert_eq!(
            ids(&db, &spec, "f.created_at=range:2026-10-01,2026-10-08").await,
            r
        );
        let mut buyer = vec![a, c];
        buyer.sort_unstable();
        assert_eq!(ids(&db, &spec, "f.buyer=has:ivan").await, buyer);
        assert_eq!(ids(&db, &spec, "f.paid=eq:Y").await, vec![a]);
        assert_eq!(ids(&db, &spec, "f.canceled=eq:N").await, r);
        assert_eq!(
            ids(&db, &spec, "f.status=in:N&f.canceled=eq:N&f.buyer=has:ivan").await,
            vec![a]
        );
        // мусор в старых значениях отбрасывается: остаётся полный список
        assert_eq!(
            ids(&db, &spec, "f.paid=eq:?&f.created_at=range:вчера,").await,
            all
        );
        // платёжка и доставка фикстуры имеют id 1; варианты берутся из справочников
        sqlx::query("INSERT INTO pay_systems (id, name) VALUES (1, 'QR')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO deliveries (id, name) VALUES (1, 'Самовывоз')")
            .execute(&db)
            .await
            .unwrap();
        let spec = build_spec(&db, false).await.unwrap();
        assert_eq!(ids(&db, &spec, "f.pay_system=in:1").await, all);
        assert_eq!(ids(&db, &spec, "f.delivery=in:1").await, all);
        assert_eq!(
            ids(&db, &spec, "f.pay_system=nin:1").await,
            Vec::<i64>::new()
        );
    }

    /// `nin` включает заказы без оплаты/отгрузки.
    #[sqlx::test]
    async fn nin_includes_orders_without_payment(db: PgPool) {
        let f = order_fixture(&db).await;
        let a = place_order(&db, &f, None).await; // платёжка 1
        let b = place_order(&db, &f, None).await; // платёжка 2
        let c = place_order(&db, &f, None).await; // без оплаты
        for (id, ps) in [(1, "QR"), (2, "Нал")] {
            sqlx::query("INSERT INTO pay_systems (id, name) VALUES ($1, $2)")
                .bind(id as i64)
                .bind(ps)
                .execute(&db)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE payments SET pay_system_id = 2 WHERE order_id = $1")
            .bind(b)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM payments WHERE order_id = $1")
            .bind(c)
            .execute(&db)
            .await
            .unwrap();
        let spec = build_spec(&db, false).await.unwrap();
        assert_eq!(ids(&db, &spec, "f.pay_system=in:1").await, vec![a]);
        assert_eq!(ids(&db, &spec, "f.pay_system=in:2").await, vec![b]);
        let mut rest = vec![b, c];
        rest.sort_unstable();
        assert_eq!(ids(&db, &spec, "f.pay_system=nin:1").await, rest);
        let mut rest = vec![a, c];
        rest.sort_unstable();
        assert_eq!(ids(&db, &spec, "f.pay_system=nin:2").await, rest);
    }
}
