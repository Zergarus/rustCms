//! Реестр разделов админки: из него строятся меню, палитра команд и полоса открытых вкладок.
//!
//! Права и условия видимости пунктов те же, что раньше были в `shop/_nav.html` и `base.html`.

// Меню, палитра и вкладки подключаются следующими задачами; до этого часть API не используется.
#![allow(dead_code)]

use serde::Serialize;
use sqlx::PgPool;

use crate::access::{Access, Level};

/// Режим оболочки: контент или магазин.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    Cms,
    Shop,
}

/// Вид вкладки в полосе открытых вкладок.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TabKind {
    Content,
    Shop,
    System,
}

/// Вкладки карточки заказа: (суффикс адреса, название).
pub(crate) const ORDER_TABS: [(&str, &str); 6] = [
    ("", "Обзор"),
    ("items", "Состав и скидки"),
    ("props", "Покупатель и свойства"),
    ("shipment", "Отгрузка и резервы"),
    ("files", "Документы и 1С"),
    ("history", "История"),
];

/// Сколько коллекций показывать в меню до «Ещё…».
const MENU_COLLECTIONS: usize = 8;

/// Совпадает ли `path` с `prefix` целиком или по границе сегмента `/`.
fn under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn without_query(path: &str) -> &str {
    path.split_once('?').map_or(path, |(p, _)| p)
}

pub(crate) fn mode_for(path: &str) -> Mode {
    let path = without_query(path);
    if ["/admin/shop", "/admin/discounts", "/admin/exchange"]
        .iter()
        .any(|p| under(path, p))
    {
        Mode::Shop
    } else {
        Mode::Cms
    }
}

/// Ключ вкладки для адреса: `None` — страница во вкладки не попадает.
pub(crate) fn tab_key(path: &str) -> Option<String> {
    let path = without_query(path);
    if path == "/admin/login"
        || path.rsplit('/').next() == Some("new")
        || [
            "/admin/tabs",
            "/admin/palette",
            "/admin/prefs",
            "/admin/static",
        ]
        .iter()
        .any(|p| under(path, p))
    {
        return None;
    }
    // подстраницы заказа живут в одной вкладке
    if let Some(rest) = path.strip_prefix("/admin/shop/orders/")
        && let Some((id, tab)) = rest.split_once('/')
        && !id.is_empty()
        && ORDER_TABS.iter().any(|(t, _)| !t.is_empty() && *t == tab)
    {
        return Some(format!("/admin/shop/orders/{id}"));
    }
    Some(path.to_string())
}

pub(crate) fn tab_kind(path: &str) -> TabKind {
    let p = without_query(path);
    if ["/admin/collections", "/admin/items", "/admin/sections"]
        .iter()
        .any(|prefix| under(p, prefix))
    {
        TabKind::Content
    } else if mode_for(p) == Mode::Shop {
        TabKind::Shop
    } else {
        TabKind::System
    }
}

/// Раздел админки.
#[derive(Debug, Clone)]
pub(crate) struct Section {
    pub code: String,
    pub label: String,
    pub url: String,
    pub group: &'static str,
    pub mode: Mode,
    /// Действие «создать»: (название, адрес).
    pub create: Option<(String, String)>,
}

#[derive(Debug, Clone)]
pub(crate) struct CollectionLink {
    pub id: i64,
    pub name: String,
    pub count: i64,
    pub catalog: bool,
    pub writable: bool,
}

/// Данные из БД, от которых зависит состав разделов.
#[derive(Debug, Clone)]
pub(crate) struct NavCtx {
    pub shop_enabled: bool,
    pub store_control: bool,
    /// Доступные пользователю (уровень не ниже чтения), по порядку `sort, id`.
    pub collections: Vec<CollectionLink>,
}

/// Лёгкие флаги для каждого запроса: (магазин включён, складской учёт включён).
/// Ошибка БД не валит запрос: магазин считается включённым, учёт — выключенным.
pub(crate) async fn flags(db: &PgPool) -> (bool, bool) {
    let shop = shop_enabled(db).await.unwrap_or_else(|err| {
        tracing::warn!("не удалось прочитать module_shop: {err}");
        true
    });
    let store_control = match db.acquire().await {
        Ok(mut conn) => crate::stock::settings(&mut conn)
            .await
            .map(|s| s.store_control)
            .unwrap_or_else(|err| {
                tracing::warn!("не удалось прочитать настройки склада: {err}");
                false
            }),
        Err(err) => {
            tracing::warn!("нет соединения для настроек склада: {err}");
            false
        }
    };
    (shop, store_control)
}

/// Полный контекст меню (с коллекциями и счётчиками) — только для полных страниц.
pub(crate) async fn load_ctx(db: &PgPool, user: &Access) -> sqlx::Result<NavCtx> {
    load_ctx_with(db, user, true).await
}

/// Контекст без счётчиков записей (`count = 0`): для палитры, где счётчики не нужны.
pub(crate) async fn load_ctx_light(db: &PgPool, user: &Access) -> sqlx::Result<NavCtx> {
    load_ctx_with(db, user, false).await
}

async fn load_ctx_with(db: &PgPool, user: &Access, counts: bool) -> sqlx::Result<NavCtx> {
    let (shop_enabled, store_control) = flags(db).await;
    let rows: Vec<(i64, String, bool, i64)> = sqlx::query_as(
        "SELECT c.id, c.name, c.is_catalog,
                CASE WHEN $1 THEN (SELECT count(*) FROM collection_items i WHERE i.collection_id = c.id)
                     ELSE 0 END
         FROM collections c ORDER BY c.sort, c.id",
    )
    .bind(counts)
    .fetch_all(db)
    .await?;
    let collections = rows
        .into_iter()
        .filter_map(|(id, name, catalog, count)| {
            let level = user.collection_level(id);
            (level >= Level::Read).then_some(CollectionLink {
                id,
                name,
                count,
                catalog,
                writable: level >= Level::Write,
            })
        })
        .collect();
    Ok(NavCtx {
        shop_enabled,
        store_control,
        collections,
    })
}

/// Включён ли режим магазина (по умолчанию да).
pub(crate) async fn shop_enabled(db: &PgPool) -> sqlx::Result<bool> {
    let value: Option<String> = sqlx::query_scalar(
        "SELECT value FROM options WHERE module = 'admin' AND name = 'module_shop'",
    )
    .fetch_optional(db)
    .await?;
    Ok(value.as_deref() != Some("N"))
}

pub(crate) async fn set_shop_enabled(db: &PgPool, on: bool) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO options (module, name, value) VALUES ('admin', 'module_shop', $1)
         ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(if on { "Y" } else { "N" })
    .execute(db)
    .await?;
    Ok(())
}

/// Все разделы, доступные пользователю, в порядке меню.
pub(crate) fn sections(user: &Access, ctx: &NavCtx) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut add = |code: &str, label: &str, url: &str, group: &'static str, mode: Mode| {
        out.push(Section {
            code: code.to_string(),
            label: label.to_string(),
            url: url.to_string(),
            group,
            mode,
            create: None,
        });
    };

    add("desk", "Рабочий стол", "/admin", "main", Mode::Cms);
    for c in &ctx.collections {
        add(
            &format!("collection:{}", c.id),
            &c.name,
            &format!("/admin/collections/{}/items", c.id),
            "content",
            Mode::Cms,
        );
    }
    if user.can_see_collections {
        add(
            "collections",
            "Все коллекции",
            "/admin/collections",
            "content",
            Mode::Cms,
        );
    }
    if user.can(crate::access::USERS_MANAGE) {
        add("users", "Пользователи", "/admin/users", "system", Mode::Cms);
    }
    if user.is_super() {
        add(
            "groups",
            "Группы и права",
            "/admin/groups",
            "system",
            Mode::Cms,
        );
        add("modules", "Модули", "/admin/modules", "system", Mode::Cms);
    }

    if ctx.shop_enabled {
        let shop = user.can(crate::access::SHOP_MANAGE);
        let mut s = |code: &str, label: &str, url: &str, group: &'static str| {
            add(code, label, url, group, Mode::Shop);
        };
        if user.can(crate::access::ORDERS_MANAGE) {
            s("orders", "Заказы", "/admin/shop/orders", "shop_main");
        }
        if shop {
            let catalogs: Vec<&CollectionLink> =
                ctx.collections.iter().filter(|c| c.catalog).collect();
            let url = match catalogs.as_slice() {
                [one] => format!("/admin/collections/{}/items", one.id),
                _ => "/admin/collections".to_string(),
            };
            // без доступного каталога раздел вёл бы в пустоту
            if !catalogs.is_empty() {
                s("products", "Товары", &url, "shop_main");
            }
            for (code, label) in [
                ("price-types", "Типы цен"),
                ("rounding", "Правила округления"),
            ] {
                s(code, label, &format!("/admin/shop/{code}"), "shop_catalog");
            }
            s("stores", "Склады", "/admin/shop/stores", "shop_stock");
            if ctx.store_control {
                s("rests", "Остатки", "/admin/shop/rests", "shop_stock");
                s(
                    "store-docs",
                    "Документы",
                    "/admin/shop/store-docs",
                    "shop_stock",
                );
            }
            s(
                "contractors",
                "Поставщики",
                "/admin/shop/contractors",
                "shop_stock",
            );
        }
        if user.can(crate::access::DISCOUNTS_MANAGE) {
            s("discounts", "Скидки", "/admin/discounts", "shop_marketing");
            s(
                "cumulative",
                "Накопительные скидки",
                "/admin/discounts/cumulative",
                "shop_marketing",
            );
        }
        if shop {
            for (code, label) in [
                ("statuses", "Статусы"),
                ("deliveries", "Доставки"),
                ("pay-systems", "Платёжные системы"),
                ("person-types", "Типы плательщика"),
                ("currencies", "Валюты"),
                ("currency-rates", "Курсы валют"),
                ("settings", "Настройки каталога"),
                ("carts", "Корзины"),
            ] {
                s(code, label, &format!("/admin/shop/{code}"), "shop_settings");
            }
        }
        if user.can(crate::access::EXCHANGE_1C) || user.can(crate::access::EXCHANGE_1C_SALE) {
            s("exchange", "Обмен с 1С", "/admin/exchange", "shop_settings");
        }
    }

    // действия «создать»; для заказов их нет: формы создания заказа в админке нет
    for sec in &mut out {
        if sec.code == "store-docs" {
            sec.create = Some((
                "Новый документ прихода".to_string(),
                "/admin/shop/store-docs/new?type=A".to_string(),
            ));
        } else if let Some(id) = sec.code.strip_prefix("collection:")
            && let Some(c) = ctx.collections.iter().find(|c| c.id.to_string() == id)
            && c.writable
        {
            sec.create = Some((
                format!("Добавить запись в «{}»", c.name),
                format!("/admin/collections/{}/items/new", c.id),
            ));
        }
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct NavItem {
    pub code: String,
    pub label: String,
    pub url: String,
    pub count: Option<i64>,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct NavGroup {
    pub code: &'static str,
    pub label: &'static str,
    pub items: Vec<NavItem>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Nav {
    pub mode: Mode,
    /// Группы текущего режима, пустые выброшены.
    pub groups: Vec<NavGroup>,
    /// Кнопка «Магазин» в режиме Cms; адрес — первый видимый пункт магазина.
    pub shop: Option<NavItem>,
    /// Коллекций больше восьми: показаны первые 8 и «Ещё…».
    pub more_collections: bool,
    /// G-переходы: d, o, c, u — только доступные.
    pub goto: Vec<(char, String)>,
}

const CMS_GROUPS: [(&str, &str); 3] = [
    ("main", "Главное"),
    ("content", "Контент"),
    ("system", "Система"),
];
const SHOP_GROUPS: [(&str, &str); 5] = [
    ("shop_main", "Магазин"),
    ("shop_catalog", "Каталог"),
    ("shop_stock", "Склад"),
    ("shop_marketing", "Маркетинг"),
    ("shop_settings", "Настройки"),
];

/// Адрес раздела `url` «покрывает» `path`: совпадает или является префиксом по границе `/`.
/// `/admin` активен только при точном совпадении.
fn covers(url: &str, path: &str) -> bool {
    url == path || (url != "/admin" && under(path, url))
}

/// Запрос коллекции по `/admin/{items,sections,fields}/{id}`.
fn content_query(path: &str) -> Option<&'static str> {
    [
        (
            "/admin/items/",
            "SELECT collection_id FROM collection_items WHERE id = $1",
        ),
        (
            "/admin/sections/",
            "SELECT collection_id FROM collection_sections WHERE id = $1",
        ),
        (
            "/admin/fields/",
            "SELECT collection_id FROM collection_fields WHERE id = $1",
        ),
    ]
    .into_iter()
    .find(|(prefix, _)| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
    })
    .map(|(_, table)| table)
}

pub(crate) async fn build(
    db: &PgPool,
    user: &Access,
    ctx: &NavCtx,
    path: &str,
) -> sqlx::Result<Nav> {
    let path = without_query(path);
    let mode = mode_for(path);
    let all = sections(user, ctx);

    let orders_count = if ctx.shop_enabled && user.can(crate::access::ORDERS_MANAGE) {
        // нулевой счётчик не показываем
        let n = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM orders WHERE status = $1 AND NOT canceled",
        )
        .bind(crate::sale::INITIAL_STATUS)
        .fetch_one(db)
        .await?;
        (n > 0).then_some(n)
    } else {
        None
    };

    let mut collections_shown = 0;
    let mut more_collections = false;
    let mut shown: Vec<(&Section, NavItem)> = Vec::new();
    for s in all.iter().filter(|s| s.mode == mode) {
        let mut count = None;
        if let Some(id) = s.code.strip_prefix("collection:") {
            if collections_shown >= MENU_COLLECTIONS {
                more_collections = true;
                continue;
            }
            collections_shown += 1;
            count = ctx
                .collections
                .iter()
                .find(|c| c.id.to_string() == id)
                .map(|c| c.count);
        } else if s.code == "orders" {
            count = orders_count;
        }
        shown.push((
            s,
            NavItem {
                code: s.code.clone(),
                label: s.label.clone(),
                url: s.url.clone(),
                count,
                active: false,
            },
        ));
    }
    // активен самый длинный подходящий адрес
    let mut best = shown
        .iter()
        .enumerate()
        .filter(|(_, (s, _))| covers(&s.url, path))
        .max_by_key(|(i, (s, _))| (s.url.len(), std::cmp::Reverse(*i)))
        .map(|(i, _)| i);
    if best.is_none()
        && let Some(sql) = content_query(path)
    {
        // запись, раздел и поле принадлежат коллекции: подсвечиваем её, иначе «Все коллекции»
        let id = path
            .rsplit('/')
            .next()
            .and_then(|id| id.parse::<i64>().ok());
        let collection_id: Option<i64> = match id {
            Some(id) => sqlx::query_scalar(sql).bind(id).fetch_optional(db).await?,
            None => None,
        };
        let own = collection_id.map(|c| format!("collection:{c}"));
        best = shown
            .iter()
            .position(|(s, _)| Some(&s.code) == own.as_ref())
            .or_else(|| shown.iter().position(|(s, _)| s.code == "collections"));
    }
    if let Some(i) = best {
        shown[i].1.active = true;
    }

    let defs: &[(&str, &str)] = if mode == Mode::Cms {
        &CMS_GROUPS
    } else {
        &SHOP_GROUPS
    };
    let groups = defs
        .iter()
        .filter_map(|(code, label)| {
            let items: Vec<NavItem> = shown
                .iter()
                .filter(|(s, _)| s.group == *code)
                .map(|(_, item)| item.clone())
                .collect();
            (!items.is_empty()).then_some(NavGroup { code, label, items })
        })
        .collect();

    // видна, если доступен хоть один раздел магазина (при выключенном модуле их нет)
    let shop = if mode == Mode::Cms {
        all.iter()
            .find(|s| s.mode == Mode::Shop)
            .map(|first| NavItem {
                code: "shop".to_string(),
                label: "Магазин".to_string(),
                url: first.url.clone(),
                count: orders_count,
                active: false,
            })
    } else {
        None
    };

    let has = |code: &str| all.iter().any(|s| s.code == code);
    let goto = [
        ('d', "desk", "/admin"),
        ('o', "orders", "/admin/shop/orders"),
        ('c', "collections", "/admin/collections"),
        ('u', "users", "/admin/users"),
    ]
    .into_iter()
    .filter(|(_, code, _)| has(code))
    .map(|(key, _, url)| (key, url.to_string()))
    .collect();

    Ok(Nav {
        mode,
        groups,
        shop,
        more_collections,
        goto,
    })
}

/// Название группы меню по её коду (для подсказок палитры).
pub(crate) fn group_label(code: &str) -> &'static str {
    CMS_GROUPS
        .iter()
        .chain(SHOP_GROUPS.iter())
        .find(|(c, _)| *c == code)
        .map_or("", |(_, label)| label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_and_keys() {
        assert_eq!(mode_for("/admin/shop/orders/5"), Mode::Shop);
        assert_eq!(mode_for("/admin/discounts/3"), Mode::Shop);
        assert_eq!(mode_for("/admin/exchange"), Mode::Shop);
        assert_eq!(mode_for("/admin/collections/1"), Mode::Cms);
        assert_eq!(mode_for("/admin/shopping"), Mode::Cms); // граница сегмента
        assert_eq!(
            tab_key("/admin/shop/orders/1042/shipment?x=1").as_deref(),
            Some("/admin/shop/orders/1042")
        );
        assert_eq!(
            tab_key("/admin/shop/orders/1042").as_deref(),
            Some("/admin/shop/orders/1042")
        );
        assert_eq!(
            tab_key("/admin/shop/orders/1042/xml1c").as_deref(),
            Some("/admin/shop/orders/1042/xml1c")
        );
        assert_eq!(tab_key("/admin/users/new"), None);
        assert_eq!(tab_key("/admin/login"), None);
        assert_eq!(tab_key("/admin/tabs/close"), None);
        assert_eq!(tab_kind("/admin/collections/3/items/9"), TabKind::Content);
        assert_eq!(tab_kind("/admin/shop/stores"), TabKind::Shop);
        assert_eq!(tab_kind("/admin/users"), TabKind::System);
    }

    use crate::{access::Level, test_support};

    /// Пользователь с правами `permissions`; суперадмин при `None`.
    async fn access(db: &PgPool, login: &str, permissions: Option<&[&str]>) -> Access {
        let user = crate::auth::create_user(db, login, "password123".into(), permissions.is_none())
            .await
            .unwrap();
        if let Some(permissions) = permissions {
            let group: i64 =
                sqlx::query_scalar("INSERT INTO groups (code, name) VALUES ($1, $1) RETURNING id")
                    .bind(login)
                    .fetch_one(db)
                    .await
                    .unwrap();
            for perm in permissions {
                sqlx::query("INSERT INTO group_permissions (group_id, permission) VALUES ($1, $2)")
                    .bind(group)
                    .bind(perm)
                    .execute(db)
                    .await
                    .unwrap();
            }
            sqlx::query("INSERT INTO user_groups (user_id, group_id) VALUES ($1, $2)")
                .bind(user.id)
                .bind(group)
                .execute(db)
                .await
                .unwrap();
        }
        Access::load(db, user).await.unwrap()
    }

    fn codes(nav: &Nav) -> Vec<&str> {
        nav.groups
            .iter()
            .flat_map(|g| g.items.iter().map(|i| i.code.as_str()))
            .collect()
    }

    #[sqlx::test]
    async fn menu_by_permissions(db: PgPool) {
        let user = access(&db, "op", Some(&["admin.access", "orders.manage"])).await;
        let ctx = load_ctx(&db, &user).await.unwrap();
        let cms = build(&db, &user, &ctx, "/admin").await.unwrap();
        assert!(cms.shop.is_some());
        assert!(cms.groups.iter().all(|g| g.code != "system"));
        let shop = build(&db, &user, &ctx, "/admin/shop/orders").await.unwrap();
        assert_eq!(shop.mode, Mode::Shop);
        assert_eq!(shop.groups.len(), 1);
        assert_eq!(shop.groups[0].code, "shop_main");
        assert_eq!(codes(&shop), ["orders"]);

        let user = access(&db, "plain", Some(&["admin.access"])).await;
        let ctx = load_ctx(&db, &user).await.unwrap();
        assert!(
            build(&db, &user, &ctx, "/admin")
                .await
                .unwrap()
                .shop
                .is_none()
        );
    }

    #[sqlx::test]
    async fn shop_disabled_hides_shop(db: PgPool) {
        let user = access(&db, "root", None).await;
        let ctx = load_ctx(&db, &user).await.unwrap();
        assert!(ctx.shop_enabled);
        assert!(
            build(&db, &user, &ctx, "/admin")
                .await
                .unwrap()
                .shop
                .is_some()
        );
        assert!(sections(&user, &ctx).iter().any(|s| s.mode == Mode::Shop));

        set_shop_enabled(&db, false).await.unwrap();
        let row: (String, String, String) =
            sqlx::query_as("SELECT module, name, value FROM options WHERE module = 'admin'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(row, ("admin".into(), "module_shop".into(), "N".into()));
        let ctx = load_ctx(&db, &user).await.unwrap();
        assert!(!ctx.shop_enabled);
        assert!(
            build(&db, &user, &ctx, "/admin")
                .await
                .unwrap()
                .shop
                .is_none()
        );
        assert!(sections(&user, &ctx).iter().all(|s| s.mode != Mode::Shop));

        set_shop_enabled(&db, true).await.unwrap();
        let ctx = load_ctx(&db, &user).await.unwrap();
        assert!(
            build(&db, &user, &ctx, "/admin")
                .await
                .unwrap()
                .shop
                .is_some()
        );
    }

    #[sqlx::test]
    async fn stock_items_follow_store_control(db: PgPool) {
        let user = access(&db, "root", None).await;
        let mut ctx = load_ctx(&db, &user).await.unwrap();
        assert!(!ctx.store_control);
        let has = |ctx: &NavCtx, code: &str| sections(&user, ctx).iter().any(|s| s.code == code);
        assert!(!has(&ctx, "rests") && !has(&ctx, "store-docs"));
        assert!(has(&ctx, "stores"));

        let mut tx = db.begin().await.unwrap();
        crate::stock::set_store_control(&mut tx, true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        ctx = load_ctx(&db, &user).await.unwrap();
        assert!(ctx.store_control);
        assert!(has(&ctx, "rests") && has(&ctx, "store-docs"));
        let docs = sections(&user, &ctx)
            .into_iter()
            .find(|s| s.code == "store-docs")
            .unwrap();
        assert_eq!(docs.create.unwrap().1, "/admin/shop/store-docs/new?type=A");
    }

    #[sqlx::test]
    async fn more_collections_after_eight(db: PgPool) {
        for i in 1..=9 {
            sqlx::query("INSERT INTO collections (code, name) VALUES ($1, $1)")
                .bind(format!("c{i}"))
                .execute(&db)
                .await
                .unwrap();
        }
        let user = access(&db, "root", None).await;
        let ctx = load_ctx(&db, &user).await.unwrap();
        assert_eq!(ctx.collections.len(), 9);
        let nav = build(&db, &user, &ctx, "/admin").await.unwrap();
        assert!(nav.more_collections);
        assert_eq!(
            codes(&nav)
                .iter()
                .filter(|c| c.starts_with("collection:"))
                .count(),
            8
        );

        sqlx::query("DELETE FROM collections WHERE code = 'c9'")
            .execute(&db)
            .await
            .unwrap();
        let ctx = load_ctx(&db, &user).await.unwrap();
        assert!(
            !build(&db, &user, &ctx, "/admin")
                .await
                .unwrap()
                .more_collections
        );
    }

    #[sqlx::test]
    async fn collections_follow_levels(db: PgPool) {
        let f = test_support::content_fixture(&db).await;
        let user = access(&db, "reader", Some(&["admin.access"])).await;
        assert!(load_ctx(&db, &user).await.unwrap().collections.is_empty());
        let group: i64 = sqlx::query_scalar("SELECT id FROM groups WHERE code = 'reader'")
            .fetch_one(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO collection_access (collection_id, group_id, level) VALUES ($1, $2, 'read')")
            .bind(f.collection_id)
            .bind(group)
            .execute(&db)
            .await
            .unwrap();
        let user = Access::load(&db, user.user.clone()).await.unwrap();
        assert_eq!(user.collection_level(f.collection_id), Level::Read);
        let ctx = load_ctx(&db, &user).await.unwrap();
        assert_eq!(ctx.collections.len(), 1);
        assert!(!ctx.collections[0].writable);
        assert_eq!(ctx.collections[0].count, 1);
        let secs = sections(&user, &ctx);
        let sec = secs
            .iter()
            .find(|s| s.code.starts_with("collection:"))
            .unwrap();
        assert!(sec.create.is_none());
    }

    #[sqlx::test]
    async fn orders_badge_counts_new_not_canceled(db: PgPool) {
        let f = test_support::order_fixture(&db).await;
        sqlx::query("INSERT INTO order_statuses (code, name) VALUES ('F', 'Выполнен')")
            .execute(&db)
            .await
            .unwrap();
        test_support::place_order(&db, &f, None).await;
        let canceled = test_support::place_order(&db, &f, None).await;
        let finished = test_support::place_order(&db, &f, None).await;
        sqlx::query("UPDATE orders SET canceled = TRUE WHERE id = $1")
            .bind(canceled)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE orders SET status = 'F' WHERE id = $1")
            .bind(finished)
            .execute(&db)
            .await
            .unwrap();
        let user = access(&db, "root", None).await;
        let ctx = load_ctx(&db, &user).await.unwrap();
        let shop = build(&db, &user, &ctx, "/admin/shop/orders").await.unwrap();
        let orders = shop.groups[0]
            .items
            .iter()
            .find(|i| i.code == "orders")
            .unwrap();
        assert_eq!(orders.count, Some(1));
        let cms = build(&db, &user, &ctx, "/admin").await.unwrap();
        assert_eq!(cms.shop.unwrap().count, Some(1));
    }

    #[sqlx::test]
    async fn active_item_longest_prefix(db: PgPool) {
        let user = access(&db, "root", None).await;
        let mut tx = db.begin().await.unwrap();
        crate::stock::set_store_control(&mut tx, true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let ctx = load_ctx(&db, &user).await.unwrap();
        let active = |nav: &Nav| {
            nav.groups
                .iter()
                .flat_map(|g| &g.items)
                .filter(|i| i.active)
                .map(|i| i.code.clone())
                .collect::<Vec<_>>()
        };
        let nav = build(&db, &user, &ctx, "/admin/shop/store-docs/7")
            .await
            .unwrap();
        assert_eq!(active(&nav), ["store-docs"]);
        let nav = build(&db, &user, &ctx, "/admin/discounts/cumulative")
            .await
            .unwrap();
        assert_eq!(active(&nav), ["cumulative"]);
        let nav = build(&db, &user, &ctx, "/admin").await.unwrap();
        assert_eq!(active(&nav), ["desk"]);
        let nav = build(&db, &user, &ctx, "/admin/users").await.unwrap();
        assert_eq!(active(&nav), ["users"]);
    }

    #[sqlx::test]
    async fn orders_badge_absent_when_zero(db: PgPool) {
        let user = access(&db, "root", None).await;
        let ctx = load_ctx(&db, &user).await.unwrap();
        let shop = build(&db, &user, &ctx, "/admin/shop/orders").await.unwrap();
        let orders = shop.groups[0]
            .items
            .iter()
            .find(|i| i.code == "orders")
            .unwrap();
        assert_eq!(orders.count, None);
        let cms = build(&db, &user, &ctx, "/admin").await.unwrap();
        assert_eq!(cms.shop.unwrap().count, None);
    }

    #[sqlx::test]
    async fn shop_button_for_any_shop_section(db: PgPool) {
        let user = access(&db, "marketer", Some(&["admin.access", "discounts.manage"])).await;
        let ctx = load_ctx(&db, &user).await.unwrap();
        let nav = build(&db, &user, &ctx, "/admin").await.unwrap();
        assert_eq!(nav.shop.unwrap().url, "/admin/discounts");
        set_shop_enabled(&db, false).await.unwrap();
        let ctx = load_ctx(&db, &user).await.unwrap();
        assert!(
            build(&db, &user, &ctx, "/admin")
                .await
                .unwrap()
                .shop
                .is_none()
        );
    }

    #[sqlx::test]
    async fn active_collection_for_item_section_field(db: PgPool) {
        let c = test_support::content_fixture(&db).await;
        let user = access(&db, "root", None).await;
        let ctx = load_ctx(&db, &user).await.unwrap();
        let active = |nav: &Nav| {
            nav.groups
                .iter()
                .flat_map(|g| &g.items)
                .filter(|i| i.active)
                .map(|i| i.code.clone())
                .collect::<Vec<_>>()
        };
        let own = vec![format!("collection:{}", c.collection_id)];
        for path in [
            format!("/admin/items/{}", c.item_id),
            format!("/admin/sections/{}", c.section_id),
            format!("/admin/fields/{}", c.field_id),
        ] {
            let nav = build(&db, &user, &ctx, &path).await.unwrap();
            assert_eq!(active(&nav), own, "{path}");
        }
        // запись не найдена: «Все коллекции»
        let nav = build(&db, &user, &ctx, "/admin/items/999999")
            .await
            .unwrap();
        assert_eq!(active(&nav), ["collections"]);
    }

    #[sqlx::test]
    async fn products_only_with_readable_catalog(db: PgPool) {
        let user = access(&db, "root", None).await;
        let ctx = load_ctx(&db, &user).await.unwrap();
        assert!(!sections(&user, &ctx).iter().any(|s| s.code == "products"));
        sqlx::query(
            "INSERT INTO collections (code, name, is_catalog) VALUES ('cat', 'Каталог', TRUE)",
        )
        .execute(&db)
        .await
        .unwrap();
        let ctx = load_ctx(&db, &user).await.unwrap();
        assert!(sections(&user, &ctx).iter().any(|s| s.code == "products"));
    }

    #[sqlx::test]
    async fn light_ctx_skips_counts(db: PgPool) {
        test_support::content_fixture(&db).await;
        let user = access(&db, "root", None).await;
        let full = load_ctx(&db, &user).await.unwrap();
        let light = load_ctx_light(&db, &user).await.unwrap();
        assert_eq!(full.collections[0].count, 1);
        assert_eq!(light.collections[0].count, 0);
        assert_eq!(full.collections[0].id, light.collections[0].id);
    }
}
