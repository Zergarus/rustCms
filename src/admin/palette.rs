//! Командная палитра: поиск по разделам, действиям, заказам, записям и пользователям.
//! Возвращает фрагмент для `#palette-results`; результаты фильтруются по правам.

use axum::{
    Extension,
    extract::{Query, State},
    response::Html,
};
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::{
    nav::{self, NavCtx},
    render, tabs,
};
use crate::{access::Access, error::AppResult, state::AppState};

/// Сколько результатов в каждой группе.
const PER_GROUP: usize = 5;
/// Длина запроса в символах.
const Q_MAX: usize = 100;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Hit {
    pub label: String,
    pub hint: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Group {
    pub title: &'static str,
    pub items: Vec<Hit>,
}

/// Шаблон для `ILIKE … ESCAPE '\'`: экранирует `\`, `%`, `_` и оборачивает в `%…%`.
pub(crate) fn like_pattern(q: &str) -> String {
    format!("%{}%", escape_like(q))
}

fn escape_like(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for c in q.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Обрезка по краям и до `Q_MAX` символов; NUL Postgres в тексте не принимает.
fn clean_query(q: &str) -> String {
    let q: String = q.chars().filter(|c| *c != '\0').collect();
    q.trim()
        .chars()
        .take(Q_MAX)
        .collect::<String>()
        .trim()
        .to_string()
}

fn push_group(out: &mut Vec<Group>, title: &'static str, items: Vec<Hit>) {
    if !items.is_empty() {
        out.push(Group { title, items });
    }
}

fn actions(user: &Access, ctx: &NavCtx, needle: Option<&str>) -> Vec<Hit> {
    nav::sections(user, ctx)
        .into_iter()
        .filter_map(|s| s.create)
        .filter(|(label, _)| needle.is_none_or(|n| label.to_lowercase().contains(n)))
        .take(PER_GROUP)
        .map(|(label, url)| Hit {
            label,
            hint: "действие".to_string(),
            url,
        })
        .collect()
}

pub(crate) async fn search(
    db: &PgPool,
    user: &Access,
    ctx: &NavCtx,
    q: &str,
) -> sqlx::Result<Vec<Group>> {
    let q = clean_query(q);
    let mut out = Vec::new();
    if q.is_empty() {
        let recent = tabs::recent(db, user.user.id, PER_GROUP as i64).await?;
        push_group(
            &mut out,
            "Недавние",
            recent
                .into_iter()
                .map(|t| Hit {
                    label: t.title,
                    hint: String::new(),
                    url: t.url,
                })
                .collect(),
        );
        push_group(&mut out, "Действия", actions(user, ctx, None));
        return Ok(out);
    }

    let needle = q.to_lowercase();
    push_group(
        &mut out,
        "Разделы",
        nav::sections(user, ctx)
            .into_iter()
            .filter(|s| s.label.to_lowercase().contains(&needle))
            .take(PER_GROUP)
            .map(|s| Hit {
                hint: nav::group_label(s.group).to_string(),
                label: s.label,
                url: s.url,
            })
            .collect(),
    );
    push_group(&mut out, "Действия", actions(user, ctx, Some(&needle)));

    let pattern = like_pattern(&q);
    if ctx.shop_enabled && user.can(crate::access::ORDERS_MANAGE) {
        let prefix = format!("{}%", escape_like(&q));
        let rows: Vec<(i64, String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT o.id, COALESCE(o.account_number, o.id::text),
                    COALESCE(NULLIF((SELECT v.value FROM order_property_values v
                                     JOIN order_properties p ON p.id = v.property_id
                                     WHERE v.order_id = o.id AND p.is_payer LIMIT 1), ''), u.login, '—')
             FROM orders o LEFT JOIN users u ON u.id = o.user_id
             WHERE COALESCE(o.account_number, o.id::text) = $1
                OR COALESCE(o.account_number, o.id::text) ILIKE $2 ESCAPE '\\'
                OR {} ILIKE $3 ESCAPE '\\'
             ORDER BY o.id DESC LIMIT {PER_GROUP}",
            super::shop::orders::BUYER_TEXT_SQL
        )))
        .bind(&q)
        .bind(&prefix)
        .bind(&pattern)
        .fetch_all(db)
        .await?;
        push_group(
            &mut out,
            "Заказы",
            rows.into_iter()
                .map(|(id, number, buyer)| Hit {
                    label: format!("Заказ №{number} · {buyer}"),
                    hint: String::new(),
                    url: format!("/admin/shop/orders/{id}"),
                })
                .collect(),
        );
    }

    let ids: Vec<i64> = ctx.collections.iter().map(|c| c.id).collect();
    if !ids.is_empty() {
        let rows: Vec<(i64, String, i64)> = sqlx::query_as(
            "SELECT id, name, collection_id FROM collection_items
             WHERE collection_id = ANY($1) AND name ILIKE $2 ESCAPE '\\'
             ORDER BY id DESC LIMIT 5",
        )
        .bind(&ids)
        .bind(&pattern)
        .fetch_all(db)
        .await?;
        push_group(
            &mut out,
            "Записи",
            rows.into_iter()
                .map(|(id, name, cid)| Hit {
                    hint: ctx
                        .collections
                        .iter()
                        .find(|c| c.id == cid)
                        .map(|c| c.name.clone())
                        .unwrap_or_default(),
                    label: name,
                    url: format!("/admin/items/{id}"),
                })
                .collect(),
        );
    }

    if user.can(crate::access::USERS_MANAGE) {
        let rows: Vec<(i64, String, Option<String>)> = sqlx::query_as(
            "SELECT id, login, email FROM users
             WHERE login ILIKE $1 ESCAPE '\\' OR COALESCE(email, '') ILIKE $1 ESCAPE '\\'
             ORDER BY id LIMIT 5",
        )
        .bind(&pattern)
        .fetch_all(db)
        .await?;
        push_group(
            &mut out,
            "Пользователи",
            rows.into_iter()
                .map(|(id, login, email)| Hit {
                    label: login,
                    hint: email.unwrap_or_default(),
                    url: format!("/admin/users/{id}"),
                })
                .collect(),
        );
    }
    Ok(out)
}

#[derive(Deserialize)]
pub(super) struct PaletteQuery {
    #[serde(default)]
    q: String,
}

/// `GET /admin/palette?q=`: фрагмент результатов (и для HX, и для обычного запроса).
pub(super) async fn search_fragment(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Query(query): Query<PaletteQuery>,
) -> AppResult<Html<String>> {
    let ctx = nav::load_ctx(&state.db, &user).await?;
    let groups = search(&state.db, &user, &ctx, &query.q).await?;
    render(&state, "_palette.html", context! { groups })
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::like_pattern;
    use crate::{
        smoke_tests::get,
        test_support::{
            admin_cookie, content_fixture, order_fixture, permitted_cookie, place_order, test_state,
        },
    };

    fn enc(s: &str) -> String {
        s.bytes().fold(String::new(), |mut acc, b| {
            if b.is_ascii_alphanumeric() {
                acc.push(b as char);
            } else {
                acc.push_str(&format!("%{b:02X}"));
            }
            acc
        })
    }

    async fn pal(app: &axum::Router, cookie: &str, q: &str) -> (axum::http::StatusCode, String) {
        get(app, &format!("/admin/palette?q={}", enc(q)), cookie).await
    }

    #[test]
    fn like_pattern_escapes() {
        assert_eq!(like_pattern("50%_a\\b"), "%50\\%\\_a\\\\b%");
    }

    #[sqlx::test]
    async fn palette_respects_permissions(db: PgPool) {
        let c = content_fixture(&db).await;
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO collections (code, name) VALUES ('hidden', 'Скрытая') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO collection_items (collection_id, code, name) VALUES ($1, 'b', 'Закрытая новость')",
        )
        .bind(other)
        .execute(&db)
        .await
        .unwrap();
        // покупатель «новость» и заказ
        let f = order_fixture(&db).await;
        let (uid, _) = crate::test_support::user_cookie(&db, "новость", false).await;
        place_order(&db, &f, Some(uid)).await;

        let cookie = permitted_cookie(&db, "reader", &[]).await;
        let reader: i64 = sqlx::query_scalar("SELECT id FROM users WHERE login = 'reader'")
            .fetch_one(&db)
            .await
            .unwrap();
        let g: i64 = sqlx::query_scalar("SELECT id FROM groups WHERE code = 'g_reader'")
            .fetch_one(&db)
            .await
            .unwrap();
        let _ = reader;
        sqlx::query("INSERT INTO collection_access (collection_id, group_id, level) VALUES ($1, $2, 'read')")
            .bind(c.collection_id)
            .bind(g)
            .execute(&db)
            .await
            .unwrap();

        let app = crate::app(test_state(db.clone()));
        let (status, body) = pal(&app, &cookie, "новость").await;
        assert_eq!(status, 200);
        assert!(
            body.contains(&format!("&#x2f;admin&#x2f;items&#x2f;{}\"", c.item_id)),
            "{body}"
        );
        assert!(!body.contains("Закрытая новость"), "{body}");
        assert!(!body.contains("Заказы"), "{body}");
        assert!(!body.contains("Пользователи"), "{body}");
    }

    #[sqlx::test]
    async fn palette_orders_by_number(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let number: String = sqlx::query_scalar(
            "SELECT COALESCE(account_number, id::text) FROM orders WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (status, body) = pal(&app, &cookie, &number).await;
        assert_eq!(status, 200);
        assert!(body.contains("Заказы"), "{body}");
        assert!(
            body.contains(&format!("&#x2f;admin&#x2f;shop&#x2f;orders&#x2f;{id}\"")),
            "{body}"
        );
    }

    #[sqlx::test]
    async fn palette_shop_disabled(db: PgPool) {
        crate::admin::nav::set_shop_enabled(&db, false)
            .await
            .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (status, body) = pal(&app, &cookie, "заказ").await;
        assert_eq!(status, 200);
        assert!(!body.contains("Заказы"), "{body}");
        assert!(!body.contains("Заказ"), "{body}");
    }

    #[sqlx::test]
    async fn palette_weird_queries(db: PgPool) {
        content_fixture(&db).await;
        sqlx::query(
            "INSERT INTO collection_items (collection_id, code, name) SELECT id, 'abc', 'abc' FROM collections LIMIT 1",
        )
        .execute(&db)
        .await
        .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let (s0, empty) = pal(&app, &cookie, "").await;
        let (s1, spaces) = pal(&app, &cookie, "   ").await;
        assert_eq!((s0.as_u16(), s1.as_u16()), (200, 200));
        assert_eq!(empty, spaces);
        let (status, _) = pal(&app, &cookie, &"я".repeat(500)).await;
        assert_eq!(status, 200);
        let (status, body) = pal(&app, &cookie, "%").await;
        assert_eq!(status, 200);
        assert!(!body.contains("abc"), "{body}");
        for q in ["_", "\\", "\0", "'; DROP TABLE users;--"] {
            let (status, _) = pal(&app, &cookie, q).await;
            assert_eq!(status, 200, "{q:?}");
        }
    }

    #[sqlx::test]
    async fn palette_empty_shows_recent(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        for (url, title) in [("/admin/items/1", "Первая"), ("/admin/items/2", "Вторая")]
        {
            let body = format!("url={}&title={}", enc(url), enc(title));
            crate::test_support::post_form_raw(&app, "/admin/tabs", &cookie, &body).await;
        }
        let (status, body) = pal(&app, &cookie, "").await;
        assert_eq!(status, 200);
        assert!(body.contains("Недавние"), "{body}");
        let (a, b) = (body.find("Вторая"), body.find("Первая"));
        assert!(a.is_some() && b.is_some() && a < b, "{body}");
    }
}
