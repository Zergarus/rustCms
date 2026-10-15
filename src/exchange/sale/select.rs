//! Выгрузка заказов в 1С: отбор (`sale.export.1c`, mode=query) и сборка документа заказа из БД
//! (`CSaleExport::ExportOrders2Xml` и вызываемые им методы, `sale/general/export.php`).

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};

use super::document::{
    Agent, ExportDelivery, ExportItem, ExportOrder, ExportPayment, ExportShipment, ExportStore,
    PayOperation, RemovedShipment, SchemaVersion, num, trait_date,
};
use super::settings::{CONTRAGENT_CODES, Domain, MapEntry, MapSource, SaleSettings, load_map};
use crate::bxapi::project::OrderExportHook;

/// Заказы одного `query` и метка для `success`.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub orders: Vec<ExportOrder>,
    /// Максимальный `updated_at` выгруженных заказов; `None` — выгружать нечего.
    pub mark: Option<DateTime<Utc>>,
}

/// Отбор заказов к выгрузке: не изменённые из 1С, не созданные в 1С, изменённые после
/// подтверждённой выгрузки, по фильтрам настроек; порядок `updated_at, id`. Выгрузка
/// останавливается, когда истёк `interval` (0 — без остановки), но не внутри группы заказов с
/// одинаковым `updated_at`: следующая выгрузка берёт заказы строго новее метки.
/// Сгенерированный Ид контрагента сохраняется в `users.xml_id`.
pub async fn query(
    db: &PgPool,
    s: &SaleSettings,
    version: SchemaVersion,
    interval: Duration,
    hook: Option<&dyn OrderExportHook>,
) -> anyhow::Result<Batch> {
    let started = Instant::now();
    let statuses = statuses_from(db, s.export_from_status.as_deref()).await?;
    let candidates: Vec<(i64, DateTime<Utc>)> = sqlx::query_as(
        "SELECT o.id, o.updated_at FROM orders o
         WHERE NOT o.updated_1c AND NOT o.external
           AND ($1::timestamptz IS NULL OR o.updated_at > $1)
           AND (NOT $2 OR o.paid)
           AND (NOT $3 OR EXISTS (SELECT 1 FROM shipments s WHERE s.order_id = o.id AND s.allow_delivery))
           AND ($4::text[] IS NULL OR o.status = ANY($4))
         ORDER BY o.updated_at, o.id",
    )
    .bind(s.last_export_committed)
    .bind(s.export_paid_only)
    .bind(s.export_allow_delivery_only)
    .bind(statuses)
    .fetch_all(db)
    .await?;

    let map = load_map(db).await?;
    let mut conn = db.acquire().await?;
    let mut batch = Batch {
        orders: Vec::new(),
        mark: None,
    };
    for (id, updated_at) in candidates {
        // Хотя бы один заказ за вызов; группа с одним временем изменения — целиком
        let timed_out = !interval.is_zero() && started.elapsed() >= interval;
        if timed_out && batch.mark.is_some_and(|m| m != updated_at) {
            break;
        }
        batch
            .orders
            .push(build(&mut conn, id, s, &map, version, hook, true).await?);
        batch.mark = Some(updated_at);
    }
    Ok(batch)
}

/// Статусы начиная с заданного по сортировке (`1C_EXPORT_FINAL_ORDERS`); `None` — без фильтра.
async fn statuses_from(db: &PgPool, from: Option<&str>) -> sqlx::Result<Option<Vec<String>>> {
    let Some(from) = from else {
        return Ok(None);
    };
    let all: Vec<String> =
        sqlx::query_scalar("SELECT code FROM order_statuses ORDER BY sort, code")
            .fetch_all(db)
            .await?;
    Ok(Some(
        all.iter()
            .position(|c| c == from)
            .map(|i| all[i..].to_vec())
            .unwrap_or_default(),
    ))
}

#[derive(FromRow)]
struct OrderRow {
    id: i64,
    account_number: String,
    user_id: Option<i64>,
    person_type_id: i64,
    domain: String,
    status: String,
    status_name: String,
    status_at: Option<DateTime<Utc>>,
    price: f64,
    delivery_price: f64,
    currency: String,
    user_comment: String,
    manager_comment: String,
    canceled: bool,
    canceled_at: Option<DateTime<Utc>>,
    cancel_reason: String,
    paid: bool,
    paid_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    id_1c: String,
    version_1c: String,
    login: Option<String>,
    email: Option<String>,
    name: Option<String>,
    last_name: Option<String>,
    second_name: Option<String>,
    phone: Option<String>,
    xml_id: Option<String>,
}

#[derive(FromRow)]
struct PropRow {
    property_id: i64,
    value: String,
    kind: String,
    variant: Option<String>,
    is_payer: bool,
    is_phone: bool,
    is_email: bool,
    is_address: bool,
    is_location: bool,
}

#[derive(FromRow)]
struct ItemRow {
    id: i64,
    name: String,
    price: f64,
    quantity: f64,
    xml_id: String,
    catalog_xml_id: String,
    unit: String,
}

#[derive(FromRow)]
struct PaymentRow {
    id: i64,
    id_1c: String,
    pay_system_id: Option<i64>,
    name: String,
    sum: f64,
    paid: bool,
    paid_at: Option<DateTime<Utc>>,
    pay_voucher_num: String,
    is_return: bool,
    return_comment: String,
    api_type: String,
    handler: String,
}

#[derive(FromRow)]
struct ShipmentRow {
    id: i64,
    id_1c: String,
    delivery_id: Option<i64>,
    delivery_name: String,
    price: f64,
    allow_delivery: bool,
    allow_delivery_at: Option<DateTime<Utc>>,
    deducted: bool,
    deducted_at: Option<DateTime<Utc>>,
    tracking_number: String,
    store_id: Option<i64>,
    store_xml_id: Option<String>,
    store_name: Option<String>,
    store_address: Option<String>,
    store_phone: Option<String>,
    store_email: Option<String>,
}

impl ShipmentRow {
    fn store(&self) -> Option<ExportStore> {
        self.store_id.filter(|id| *id > 0)?;
        Some(ExportStore {
            xml_id: self.store_xml_id.clone()?,
            name: self.store_name.clone().unwrap_or_default(),
            address: self.store_address.clone().unwrap_or_default(),
            phone: self.store_phone.clone().unwrap_or_default(),
            email: self.store_email.clone().unwrap_or_default(),
        })
    }
}

/// Значения свойств заказа (`prepareSaleProperty`): по id свойства и частям местоположения
/// (`<id>_CITY`, `<id>_COUNTRY`, `<id>_REGION`).
struct Props {
    values: HashMap<String, String>,
    rows: Vec<PropRow>,
    /// Код местоположения покупателя (`DELIVERY_LOCATION` отгрузки).
    location_code: String,
    /// Улица из пути местоположения — дописывается в начало поля STREET.
    street: String,
}

impl Props {
    /// Значение первого свойства с флагом (по сортировке свойств).
    fn flagged(&self, flag: fn(&PropRow) -> bool) -> Option<(&PropRow, &str)> {
        let row = self.rows.iter().find(|r| flag(r))?;
        Some((row, self.values.get(&row.property_id.to_string())?.as_str()))
    }
}

/// Собирает документ заказа. `persist_xml_id` — сохранить сгенерированный Ид контрагента в
/// `users.xml_id` (при выгрузке в 1С — да, при «Выгрузить XML» из админки — нет).
pub async fn build(
    conn: &mut PgConnection,
    order_id: i64,
    s: &SaleSettings,
    map: &[MapEntry],
    version: SchemaVersion,
    hook: Option<&dyn OrderExportHook>,
    persist_xml_id: bool,
) -> anyhow::Result<ExportOrder> {
    let o: OrderRow = sqlx::query_as(
        "SELECT o.id, COALESCE(o.account_number, o.id::text) AS account_number, o.user_id,
                o.person_type_id, COALESCE(pt.domain, 'I') AS domain, o.status,
                COALESCE(st.name, '') AS status_name,
                (SELECT max(h.created_at) FROM order_status_history h
                 WHERE h.order_id = o.id AND h.status = o.status) AS status_at,
                o.price::float8 AS price,
                o.delivery_price::float8 AS delivery_price, o.currency, o.user_comment, o.manager_comment, o.canceled,
                o.canceled_at, o.cancel_reason, o.paid, o.paid_at, o.created_at, o.id_1c,
                o.version_1c, u.login, u.email, u.name, u.last_name, u.second_name, u.phone, u.xml_id
         FROM orders o
         LEFT JOIN person_types pt ON pt.id = o.person_type_id
         LEFT JOIN order_statuses st ON st.code = o.status
         LEFT JOIN users u ON u.id = o.user_id
         WHERE o.id = $1",
    )
    .bind(order_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| anyhow::anyhow!("заказ {order_id} не найден"))?;

    let props = load_props(conn, order_id).await?;
    let items = load_items(conn, order_id).await?;
    let payments: Vec<PaymentRow> = sqlx::query_as(
        "SELECT p.id, p.id_1c, p.pay_system_id, p.name, p.sum::float8 AS sum, p.paid, p.paid_at,
                p.pay_voucher_num, p.is_return, p.return_comment,
                COALESCE(ps.api_type, '') AS api_type, COALESCE(ps.handler, '') AS handler
         FROM payments p LEFT JOIN pay_systems ps ON ps.id = p.pay_system_id
         WHERE p.order_id = $1 ORDER BY p.id",
    )
    .bind(order_id)
    .fetch_all(&mut *conn)
    .await?;
    let shipments: Vec<ShipmentRow> = sqlx::query_as(
        "SELECT s.id, s.id_1c, s.delivery_id, s.delivery_name, s.price::float8 AS price,
                s.allow_delivery, s.allow_delivery_at, s.deducted, s.deducted_at, s.tracking_number,
                s.store_id, cs.xml_id AS store_xml_id, cs.name AS store_name,
                cs.address AS store_address, cs.phone AS store_phone, cs.email AS store_email
         FROM shipments s LEFT JOIN catalog_stores cs ON cs.id = s.store_id
         WHERE s.order_id = $1 ORDER BY s.id",
    )
    .bind(order_id)
    .fetch_all(&mut *conn)
    .await?;
    let removed: Vec<(Option<i64>, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT shipment_id, removed_at FROM shipments_removed WHERE order_id = $1
         ORDER BY removed_at DESC, shipment_id DESC",
    )
    .bind(order_id)
    .fetch_all(&mut *conn)
    .await?;
    let site_name: String = sqlx::query_scalar(
        "SELECT COALESCE((SELECT value FROM options WHERE module = 'main' AND name = 'site_name'), '')",
    )
    .fetch_one(&mut *conn)
    .await?;
    let site = format!("[s1] {site_name}");

    let agent_id = agent_id(conn, &o, persist_xml_id).await?;
    let domain = Domain::from_code(&o.domain);
    let (fields, rekv) = agent_fields(&o, &props, map, domain);
    // Адрес доставки — фактический адрес, иначе адрес контрагента (`setDeliveryAddress`)
    let delivery_address = [fields.get("F_ADDRESS_FULL"), fields.get("ADDRESS_FULL")]
        .into_iter()
        .flatten()
        .find(|v| !v.is_empty())
        .cloned()
        .unwrap_or_default();
    let date = |dt: Option<DateTime<Utc>>| dt.map(|d| trait_date(d, version)).unwrap_or_default();
    let flag = |b: bool| if b { "true" } else { "false" }.to_string();
    let currency = if s.replace_currency.is_empty() {
        o.currency.clone()
    } else {
        s.replace_currency.clone()
    };

    let export_items: Vec<ExportItem> = items.iter().map(export_item).collect();

    // Документы оплат
    let export_payments: Vec<ExportPayment> = payments
        .iter()
        .map(|p| {
            let mut traits = vec![
                ("Дата оплаты".to_string(), date(p.paid_at)),
                ("Отменен".into(), flag(false)),
                (
                    "Метод оплаты ИД".into(),
                    p.pay_system_id.map(|id| id.to_string()).unwrap_or_default(),
                ),
                ("Метод оплаты".into(), p.name.clone()),
                ("Оплачено".into(), flag(p.paid)),
                ("Признак возврата".into(), flag(p.is_return)),
                ("Причина к возврату".into(), p.return_comment.clone()),
                ("Сайт".into(), site.clone()),
            ];
            traits.extend(rekv.iter().cloned());
            ExportPayment {
                id: doc_id(&p.id_1c, p.id),
                number: p.id.to_string(),
                // DATE_BILL оплаты нет — дата заказа, как у Битрикса при пустой
                date: o.created_at,
                operation: pay_operation(&p.api_type, &p.handler),
                sum: p.sum,
                version_1c: o.version_1c.clone(),
                comment: String::new(),
                traits,
            }
        })
        .collect();

    // Документы отгрузок: позиции заказа — в первой отгрузке (отдельных позиций отгрузок нет)
    let export_shipments: Vec<ExportShipment> = shipments
        .iter()
        .enumerate()
        .map(|(i, sh)| {
            let items = if i == 0 {
                export_items.clone()
            } else {
                Vec::new()
            };
            let goods: f64 = items.iter().map(|it| it.price * it.quantity).sum();
            let mut traits = vec![
                ("Сумма доставки".to_string(), num(sh.price)),
                (
                    "Дата разрешения доставки".into(),
                    date(sh.allow_delivery_at),
                ),
                (
                    "Местоположения доставки".into(),
                    props.location_code.clone(),
                ),
                (
                    "Статус отгрузки".into(),
                    if sh.deducted { "DF" } else { "DN" }.into(),
                ),
                ("Отгружено".into(), flag(sh.deducted)),
                ("Дата отгрузки".into(), date(sh.deducted_at)),
                ("Причина отмены отгрузки".into(), String::new()),
                ("Резервирование".into(), flag(false)),
                (
                    "Метод доставки ИД".into(),
                    sh.delivery_id.map(|id| id.to_string()).unwrap_or_default(),
                ),
                ("Метод доставки".into(), sh.delivery_name.clone()),
                ("Отменен".into(), flag(false)),
                ("Дата отмены".into(), String::new()),
                ("Причина отмены".into(), String::new()),
                ("Ошибка отгрузки".into(), String::new()),
                (
                    "Идентификатор отправления".into(),
                    sh.tracking_number.clone(),
                ),
                ("Сайт".into(), site.clone()),
            ];
            if !delivery_address.is_empty() {
                traits.push(("Адрес доставки".into(), delivery_address.clone()));
            }
            traits.extend(rekv.iter().cloned());
            ExportShipment {
                id: doc_id(&sh.id_1c, sh.id),
                number: sh.id.to_string(),
                date: o.created_at,
                // Σ цена × количество + intval(стоимость доставки), export.php:2240
                sum: goods + sh.price.trunc(),
                version_1c: o.version_1c.clone(),
                comment: String::new(),
                items,
                delivery: Some(ExportDelivery { price: sh.price }),
                store: sh.store(),
                traits,
            }
        })
        .collect();

    // Склады отгрузок без повторов (array_unique, export.php:2210)
    let mut seen = Vec::new();
    let mut stores: Vec<ExportStore> = Vec::new();
    for sh in &shipments {
        if let Some(st) = sh.store()
            && !seen.contains(&sh.store_id)
        {
            seen.push(sh.store_id);
            stores.push(st);
        }
    }

    // Реквизиты заказа — порядок getXmlSaleProperties (export.php:1853)
    let payment = payments.first();
    let shipment = shipments.first();
    let mut traits: Vec<(String, String)> = Vec::new();
    if let Some(at) = o.paid_at {
        traits.push(("Дата оплаты".into(), trait_date(at, version)));
    }
    if version.documents() {
        let last_allowed = if !shipments.is_empty() && shipments.iter().all(|s| s.allow_delivery) {
            shipments.iter().filter_map(|s| s.allow_delivery_at).max()
        } else {
            None
        };
        traits.push((
            "Дата последнего разрешения по доставке".into(),
            date(last_allowed),
        ));
    } else {
        if let Some(p) = payment.filter(|p| !p.pay_voucher_num.is_empty()) {
            traits.push((
                "Номер платежного документа".into(),
                p.pay_voucher_num.clone(),
            ));
        }
        if let Some(at) = shipment.and_then(|s| s.allow_delivery_at) {
            traits.push(("Дата разрешения доставки".into(), trait_date(at, version)));
        }
    }
    if let Some((sh, id)) = shipment.and_then(|s| Some((s, s.delivery_id?))) {
        traits.push(("Способ доставки".into(), sh.delivery_name.clone()));
        traits.push(("Метод доставки ИД".into(), id.to_string()));
    }
    if let Some((p, id)) = payment.and_then(|p| Some((p, p.pay_system_id.filter(|id| *id > 0)?))) {
        traits.push(("Метод оплаты".into(), p.name.clone()));
        traits.push(("Метод оплаты ИД".into(), id.to_string()));
    }
    traits.push(("Заказ оплачен".into(), flag(o.paid)));
    if !version.documents() {
        traits.push((
            "Доставка разрешена".into(),
            flag(shipment.is_some_and(|s| s.allow_delivery)),
        ));
    }
    traits.push(("Отменен".into(), flag(o.canceled)));
    traits.push(("Финальный статус".into(), flag(o.status == "F")));
    traits.push((
        "Статус заказа".into(),
        format!("[{}] {}", o.status, o.status_name),
    ));
    traits.push(("Статуса заказа ИД".into(), o.status.clone()));
    if let Some(at) = o.canceled_at {
        traits.push(("Дата отмены заказа".into(), trait_date(at, version)));
        traits.push(("Причина отмены".into(), o.cancel_reason.clone()));
    }
    traits.push((
        "Дата изменения статуса".into(),
        trait_date(o.status_at.unwrap_or(o.created_at), version),
    ));
    if !o.user_comment.is_empty() {
        traits.push(("Комментарии покупателя".into(), o.user_comment.clone()));
    }
    traits.push(("Сайт".into(), site.clone()));
    traits.extend(rekv.iter().cloned());
    if !delivery_address.is_empty() {
        traits.push(("Адрес доставки".into(), delivery_address.clone()));
    }
    if let Some(hook) = hook {
        traits.extend(hook.traits(&mut *conn, order_id).await?);
    }

    Ok(ExportOrder {
        id: o.id,
        number: format!("{}{}", s.number_prefix, o.account_number),
        created_at: o.created_at,
        currency,
        sum: o.price,
        comment: to_text(&o.manager_comment),
        id_1c: o.id_1c.clone(),
        version_1c: o.version_1c.clone(),
        agent: Agent {
            id: agent_id,
            domain,
            fields,
        },
        items: export_items,
        // До 2.10 — доставка заказа; от 2.10 строки доставки берутся из отгрузок
        delivery: Some(ExportDelivery {
            price: o.delivery_price,
        }),
        stores,
        traits,
        payments: export_payments,
        shipments: export_shipments,
        removed_shipments: removed
            .into_iter()
            .filter_map(|(id, at)| {
                Some(RemovedShipment {
                    id: id?.to_string(),
                    date: at?,
                })
            })
            .collect(),
    })
}

/// Ид документа оплаты или отгрузки: Ид из 1С, иначе наш id.
fn doc_id(id_1c: &str, id: i64) -> String {
    if id_1c.is_empty() {
        id.to_string()
    } else {
        id_1c.to_string()
    }
}

/// ХозОперация оплаты по типу платёжной системы (`PaySystem\Manager::getPsType`): онлайн-оплата —
/// эквайринг, безналичные — безналичные, остальное — наличные (как у Битрикса по умолчанию).
fn pay_operation(api_type: &str, handler: &str) -> PayOperation {
    if !handler.is_empty() || api_type == "redirect" {
        PayOperation::Acquiring
    } else if matches!(api_type, "cashless" | "document" | "qr") {
        PayOperation::Cashless
    } else {
        PayOperation::Cash
    }
}

/// `normalizeExternalCode`: `X#X` → `X`.
fn normalize_xml_id(xml_id: &str) -> String {
    match xml_id.split_once('#') {
        Some((product, offer)) if product == offer => product.to_string(),
        _ => xml_id.to_string(),
    }
}

fn export_item(it: &ItemRow) -> ExportItem {
    let code = it.unit.trim().to_string();
    // Справочник единиц: из обмена каталогом известен только код; штука — постоянные Битрикса
    let (name, symbol, intl) = match code.as_str() {
        "796" => ("Штука", "шт", "PCE"),
        _ => ("", "", ""),
    };
    ExportItem {
        xml_id: normalize_xml_id(&it.xml_id),
        catalog_xml_id: it.catalog_xml_id.clone(),
        name: it.name.clone(),
        basket_id: it.id,
        unit_code: code,
        unit_name: name.into(),
        unit_symbol: symbol.into(),
        unit_intl: intl.into(),
        price: it.price,
        quantity: it.quantity,
        sum: it.price * it.quantity,
    }
}

/// Позиции заказа; базовая единица — поле CML2_BASE_UNIT коллекции (по его коду, иначе
/// `cml2_base_unit`), у предложения без своей — единица товара.
async fn load_items(conn: &mut PgConnection, order_id: i64) -> sqlx::Result<Vec<ItemRow>> {
    sqlx::query_as(
        "SELECT c.id, c.name, COALESCE(c.price, 0)::float8 AS price, c.quantity::float8 AS quantity,
                COALESCE(i.xml_id, '') AS xml_id, COALESCE(col.xml_id, '') AS catalog_xml_id,
                COALESCE(NULLIF(COALESCE(
                    i.field_values ->> (SELECT f.code FROM collection_fields f
                        WHERE f.collection_id = i.collection_id AND f.xml_id = 'CML2_BASE_UNIT' LIMIT 1),
                    i.field_values ->> 'cml2_base_unit'), ''),
                  NULLIF(COALESCE(
                    p.field_values ->> (SELECT f.code FROM collection_fields f
                        WHERE f.collection_id = p.collection_id AND f.xml_id = 'CML2_BASE_UNIT' LIMIT 1),
                    p.field_values ->> 'cml2_base_unit'), ''), '') AS unit
         FROM cart_items c
         LEFT JOIN collection_items i ON i.id = c.item_id
         LEFT JOIN collection_items p ON p.id = i.product_id
         LEFT JOIN collections col ON col.id = i.collection_id
         WHERE c.order_id = $1 ORDER BY c.name, c.id",
    )
    .bind(order_id)
    .fetch_all(&mut *conn)
    .await
}

async fn load_props(conn: &mut PgConnection, order_id: i64) -> sqlx::Result<Props> {
    let rows: Vec<PropRow> = sqlx::query_as(
        "SELECT v.property_id, v.value, COALESCE(p.kind, 'text') AS kind, pv.name AS variant,
                COALESCE(p.is_payer, FALSE) AS is_payer, COALESCE(p.is_phone, FALSE) AS is_phone,
                COALESCE(p.is_email, FALSE) AS is_email, COALESCE(p.is_address, FALSE) AS is_address,
                COALESCE(p.is_location, FALSE) AS is_location
         FROM order_property_values v
         LEFT JOIN order_properties p ON p.id = v.property_id
         LEFT JOIN LATERAL (SELECT name FROM order_property_variants
                            WHERE property_id = v.property_id AND value = v.value
                            ORDER BY sort, id LIMIT 1) pv ON TRUE
         WHERE v.order_id = $1 ORDER BY COALESCE(p.sort, 100), v.property_id",
    )
    .bind(order_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut props = Props {
        values: HashMap::new(),
        rows: Vec::new(),
        location_code: String::new(),
        street: String::new(),
    };
    for r in &rows {
        let id = r.property_id.to_string();
        let value = match r.kind.as_str() {
            "checkbox" => (if r.value == "Y" { "true" } else { "false" }).to_string(),
            "select" => r.variant.clone().unwrap_or_default(),
            "location" => {
                let path: Vec<(String, String, String)> = sqlx::query_as(
                    "WITH RECURSIVE chain AS (
                         SELECT l.id, l.parent_id, l.type_code, l.name, l.code, 0 AS depth
                         FROM locations l
                         WHERE l.id = (SELECT id FROM locations WHERE code = $1 OR id::text = $1
                                       ORDER BY (code = $1) DESC LIMIT 1)
                         UNION ALL
                         SELECT l.id, l.parent_id, l.type_code, l.name, l.code, c.depth + 1
                         FROM locations l JOIN chain c ON l.id = c.parent_id
                         WHERE c.depth < 16)
                     SELECT type_code, name, code FROM chain ORDER BY depth DESC",
                )
                .bind(&r.value)
                .fetch_all(&mut *conn)
                .await?;
                let part = |t: &str| {
                    path.iter()
                        .find(|(kind, _, _)| kind == t)
                        .map(|(_, name, _)| name.clone())
                        .unwrap_or_default()
                };
                for kind in ["CITY", "COUNTRY", "REGION"] {
                    props.values.insert(format!("{id}_{kind}"), part(kind));
                }
                if r.is_location && props.location_code.is_empty() {
                    props.location_code =
                        path.last().map(|(_, _, c)| c.clone()).unwrap_or_default();
                    props.street = part("STREET");
                }
                path.iter()
                    .map(|(_, name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(" - ")
            }
            _ => r.value.clone(),
        };
        props.values.insert(id, value);
    }
    props.rows = rows;
    Ok(props)
}

/// Ид контрагента (`getUserXmlId`): `users.xml_id`, пустой — первые 40 символов
/// `<id>#<логин>#<фамилия> <имя> <отчество>` (сохраняется при `persist`); гость — `<id заказа>#guest`.
async fn agent_id(conn: &mut PgConnection, o: &OrderRow, persist: bool) -> sqlx::Result<String> {
    let (Some(user_id), Some(login)) = (o.user_id, o.login.as_deref()) else {
        return Ok(format!("{}#guest", o.id));
    };
    if let Some(xml_id) = o.xml_id.as_deref().filter(|x| !x.is_empty()) {
        return Ok(xml_id.to_string());
    }
    let full = format!(
        "{user_id}#{login}#{} {} {}",
        o.last_name.as_deref().unwrap_or(""),
        o.name.as_deref().unwrap_or(""),
        o.second_name.as_deref().unwrap_or("")
    );
    let xml_id: String = full.chars().take(40).collect();
    if persist {
        // Как UserImportBase::updateEmptyXmlId: только пустой и не занятый другим пользователем
        sqlx::query(
            "UPDATE users SET xml_id = $2 WHERE id = $1 AND xml_id = ''
               AND NOT EXISTS (SELECT 1 FROM users WHERE xml_id = $2)",
        )
        .bind(user_id)
        .bind(&xml_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(xml_id)
}

/// Поля контрагента и произвольные реквизиты `1C_REKV_*` (`prepareSalePropertyRekv`) по
/// соответствию типа плательщика (строки типа перекрывают общие, тип 0). Поля без строки
/// соответствия — по умолчанию: почта — пользователя, ФИО / наименование — плательщик, телефон,
/// адрес, город — свойства с флагами.
fn agent_fields(
    o: &OrderRow,
    props: &Props,
    map: &[MapEntry],
    domain: Domain,
) -> (BTreeMap<String, String>, Vec<(String, String)>) {
    let mut entries: BTreeMap<&str, &MapEntry> = BTreeMap::new();
    for e in map.iter().filter(|e| e.person_type_id == 0) {
        entries.insert(e.code.as_str(), e);
    }
    for e in map.iter().filter(|e| e.person_type_id == o.person_type_id) {
        entries.insert(e.code.as_str(), e);
    }
    let resolve = |e: &MapEntry| -> String {
        match e.source {
            MapSource::Value => e.value.clone(),
            MapSource::Property => props.values.get(&e.value).cloned().unwrap_or_default(),
            MapSource::User => match e.value.as_str() {
                "email" => o.email.clone(),
                "login" => o.login.clone(),
                "name" => o.name.clone(),
                "last_name" => o.last_name.clone(),
                "second_name" => o.second_name.clone(),
                "phone" => o.phone.clone(),
                _ => None,
            }
            .unwrap_or_default(),
            MapSource::Order => match e.value.as_str() {
                "id" => o.id.to_string(),
                "account_number" => o.account_number.clone(),
                "user_comment" => o.user_comment.clone(),
                "price" => num(o.price),
                _ => String::new(),
            },
        }
    };

    let mut fields = BTreeMap::new();
    for (code, key, _) in CONTRAGENT_CODES.iter().filter(|(_, _, d)| *d == domain) {
        let value = match entries.get(code) {
            Some(e) => resolve(e),
            None => default_field(key, o, props),
        };
        fields.insert(key.to_string(), value);
    }
    // Улица из местоположения — в начало поля улицы (export.php:810)
    if !props.street.is_empty() {
        let street = fields.entry("STREET".into()).or_default();
        *street = if street.is_empty() {
            props.street.clone()
        } else {
            format!("{}, {street}", props.street)
        };
    }

    let mut rekv: Vec<(u64, String, String)> = entries
        .values()
        .filter_map(|e| {
            let n = e.code.strip_prefix("1C_REKV_")?;
            let value = resolve(e);
            (!e.name.is_empty() && !value.is_empty())
                .then(|| (n.parse().unwrap_or(u64::MAX), e.name.clone(), value))
        })
        .collect();
    rekv.sort_by_key(|(n, _, _)| *n);
    (
        fields,
        rekv.into_iter()
            .map(|(_, name, value)| (name, value))
            .collect(),
    )
}

/// Значение поля контрагента без строки соответствия.
fn default_field(key: &str, o: &OrderRow, props: &Props) -> String {
    let flagged = |flag: fn(&PropRow) -> bool| {
        props
            .flagged(flag)
            .map(|(_, v)| v.to_string())
            .unwrap_or_default()
    };
    match key {
        "EMAIL" => o
            .email
            .clone()
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| flagged(|r| r.is_email)),
        "FULL_NAME" | "AGENT_NAME" => flagged(|r| r.is_payer),
        "PHONE" => flagged(|r| r.is_phone),
        "ADDRESS_FULL" => flagged(|r| r.is_address),
        "CITY" => props
            .flagged(|r| r.is_location)
            .and_then(|(r, _)| props.values.get(&format!("{}_CITY", r.property_id)))
            .cloned()
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// `CSaleExport::toText`: без `style`/`blockquote`/`script`, `<br>` → перевод строки, без тегов,
/// HTML-сущности раскрыты.
pub fn to_text(value: &str) -> String {
    let mut s = value.to_string();
    for tag in ["style", "blockquote", "script"] {
        s = drop_blocks(&s, tag);
    }
    let s = replace_br(&s);
    decode_entities(&strip_tags(&s))
}

/// Удаляет `<tag …>…</tag>` без учёта регистра (нежадно, как `/<tag[^>]*>.*?<\/tag>/is`).
fn drop_blocks(s: &str, tag: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;
    while let Some(start) = lower[pos..].find(&open).map(|i| i + pos) {
        let Some(end) = lower[start..].find(&close).map(|i| i + start + close.len()) else {
            break;
        };
        out.push_str(&s[pos..start]);
        pos = end;
    }
    out.push_str(&s[pos..]);
    out
}

/// Подряд идущие `<br…>` → один перевод строки (`/(<br[^>]*>)+/isu`).
fn replace_br(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;
    let mut last_br_end = usize::MAX;
    while let Some(start) = lower[pos..].find("<br").map(|i| i + pos) {
        let Some(end) = lower[start..].find('>').map(|i| i + start + 1) else {
            break;
        };
        out.push_str(&s[pos..start]);
        if last_br_end != start {
            out.push('\n');
        }
        last_br_end = end;
        pos = end;
    }
    out.push_str(&s[pos..]);
    out
}

/// `strip_tags`: `<` перед буквой, `/`, `!` или `?` открывает тег до `>`.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<'
            && chars
                .peek()
                .is_some_and(|n| n.is_alphabetic() || matches!(n, '/' | '!' | '?'))
        {
            for t in chars.by_ref() {
                if t == '>' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `html_entity_decode`: числовые и распространённые именованные сущности.
fn decode_entities(s: &str) -> String {
    const NAMED: &[(&str, &str)] = &[
        ("amp", "&"),
        ("lt", "<"),
        ("gt", ">"),
        ("quot", "\""),
        ("apos", "'"),
        ("nbsp", "\u{a0}"),
        ("laquo", "«"),
        ("raquo", "»"),
        ("mdash", "—"),
        ("ndash", "–"),
        ("hellip", "…"),
        ("copy", "©"),
        ("reg", "®"),
        ("trade", "™"),
        ("euro", "€"),
        ("lsquo", "‘"),
        ("rsquo", "’"),
        ("ldquo", "“"),
        ("rdquo", "”"),
        ("bdquo", "„"),
        ("deg", "°"),
        ("times", "×"),
    ];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let decoded = rest[1..].find(';').filter(|n| *n <= 10).and_then(|n| {
            let name = &rest[1..1 + n];
            let ch = if let Some(num) = name.strip_prefix('#') {
                let code = match num.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => num.parse().ok(),
                };
                code.and_then(char::from_u32).map(String::from)
            } else {
                NAMED
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            };
            ch.map(|c| (c, n + 2))
        });
        match decoded {
            Some((c, len)) => {
                out.push_str(&c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::PgPool;

    use super::*;
    use crate::bxapi::project::BoxFuture;
    use crate::exchange::sale::mark_exchange;
    use crate::exchange::sale::settings::{commit_export, save_map};
    use crate::test_support::{Fixture, order_fixture, place_order};

    const V208: SchemaVersion = SchemaVersion(2.08);

    async fn ids(db: &PgPool) -> (Vec<i64>, Option<DateTime<Utc>>) {
        let s = SaleSettings::load(db).await.unwrap();
        let b = query(db, &s, V208, Duration::ZERO, None).await.unwrap();
        (b.orders.iter().map(|o| o.id).collect(), b.mark)
    }

    async fn exec(db: &PgPool, sql: &'static str, id: i64) {
        sqlx::query(sql).bind(id).execute(db).await.unwrap();
    }

    async fn one(db: &PgPool, id: i64) -> ExportOrder {
        let s = SaleSettings::load(db).await.unwrap();
        let map = load_map(db).await.unwrap();
        let mut conn = db.acquire().await.unwrap();
        build(&mut conn, id, &s, &map, V208, None, false)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn new_order_is_selected_then_not_after_commit(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let (got, mark) = ids(&db).await;
        assert_eq!(got, vec![id]);
        let updated: DateTime<Utc> =
            sqlx::query_scalar("SELECT updated_at FROM orders WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(mark, Some(updated));

        // query без success отдаёт то же
        assert_eq!(ids(&db).await.0, vec![id]);

        commit_export(&db, mark.unwrap()).await.unwrap();
        let (got, mark) = ids(&db).await;
        assert!(got.is_empty());
        assert_eq!(mark, None);
    }

    #[sqlx::test]
    async fn order_changed_between_query_and_success_is_exported_again(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let other = place_order(&db, &f, None).await;
        let (got, mark) = ids(&db).await;
        assert_eq!(got, vec![id, other]);

        exec(
            &db,
            "UPDATE orders SET manager_comment = 'правка' WHERE id = $1",
            id,
        )
        .await;
        commit_export(&db, mark.unwrap()).await.unwrap();
        assert_eq!(ids(&db).await.0, vec![id]);

        // Правка позиции (дочерней строки) — тоже изменение заказа
        let (_, mark) = ids(&db).await;
        commit_export(&db, mark.unwrap()).await.unwrap();
        exec(
            &db,
            "UPDATE cart_items SET quantity = 2 WHERE order_id = $1",
            other,
        )
        .await;
        assert_eq!(ids(&db).await.0, vec![other]);
    }

    #[sqlx::test]
    async fn change_from_exchange_is_not_selected(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let mut tx = db.begin().await.unwrap();
        mark_exchange(&mut tx).await.unwrap();
        sqlx::query("UPDATE orders SET id_1c = 'ЗК-1' WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(ids(&db).await.0.is_empty());
    }

    #[sqlx::test]
    async fn filters_paid_delivery_status_and_external(db: PgPool) {
        let f = order_fixture(&db).await;
        let plain = place_order(&db, &f, None).await;
        let paid = place_order(&db, &f, None).await;
        let allowed = place_order(&db, &f, None).await;
        let external = place_order(&db, &f, None).await;
        exec(&db, "UPDATE orders SET paid = TRUE WHERE id = $1", paid).await;
        exec(
            &db,
            "UPDATE shipments SET allow_delivery = TRUE WHERE order_id = $1",
            allowed,
        )
        .await;
        exec(
            &db,
            "UPDATE orders SET external = TRUE WHERE id = $1",
            external,
        )
        .await;
        sqlx::query("INSERT INTO order_statuses (code, name, sort) VALUES ('P', 'Оплачен', 200), ('F', 'Выполнен', 300)")
            .execute(&db)
            .await
            .unwrap();
        exec(&db, "UPDATE orders SET status = 'F' WHERE id = $1", allowed).await;

        assert_eq!(ids(&db).await.0, vec![plain, paid, allowed]);

        let mut s = SaleSettings {
            export_paid_only: true,
            ..SaleSettings::default()
        };
        s.save(&db).await.unwrap();
        assert_eq!(ids(&db).await.0, vec![paid]);

        s.export_paid_only = false;
        s.export_allow_delivery_only = true;
        s.save(&db).await.unwrap();
        assert_eq!(ids(&db).await.0, vec![allowed]);

        s.export_allow_delivery_only = false;
        s.export_from_status = Some("P".into());
        s.save(&db).await.unwrap();
        assert_eq!(ids(&db).await.0, vec![allowed]);

        s.export_from_status = Some("N".into());
        s.save(&db).await.unwrap();
        assert_eq!(ids(&db).await.0, vec![plain, paid, allowed]);
    }

    /// Свойства заказа: плательщик, телефон, адрес, местоположение, ИНН.
    async fn props(db: &PgPool, f: &Fixture, order: i64) -> HashMap<&'static str, i64> {
        sqlx::query(
            "INSERT INTO locations (id, code, parent_id, type_code, name, depth_level) VALUES
             (1, '0000000001', NULL, 'COUNTRY', 'Россия', 1),
             (2, '0000000002', 1, 'REGION', 'Московская область', 2),
             (3, '0000073738', 2, 'CITY', 'Москва', 3)",
        )
        .execute(db)
        .await
        .unwrap();
        let mut out = HashMap::new();
        for (code, kind, flag, value) in [
            ("FIO", "text", "is_payer", "Иванов Иван"),
            ("PHONE", "text", "is_phone", "+7 900 000-00-00"),
            ("ADDRESS", "text", "is_address", "ул. Мясницкая, 1"),
            ("CITY", "location", "is_location", "0000073738"),
            ("INN", "text", "util", "7700000000"),
        ] {
            let id: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "INSERT INTO order_properties (person_type_id, code, name, kind, {flag})
                 VALUES ($1, $2, $2, $3, TRUE) RETURNING id"
            )))
            .bind(f.person_type_id)
            .bind(code)
            .bind(kind)
            .fetch_one(db)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO order_property_values (order_id, property_id, code, name, value)
                 VALUES ($1, $2, $3, $3, $4)",
            )
            .bind(order)
            .bind(id)
            .bind(code)
            .bind(value)
            .execute(db)
            .await
            .unwrap();
            out.insert(code, id);
        }
        out
    }

    async fn user(db: &PgPool, xml_id: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO users (login, email, password_hash, name, last_name, second_name, phone, xml_id)
             VALUES ('ivanov', 'ivan@example.ru', 'x', 'Иван', 'Иванов', 'Иванович', '+7 111', $1)
             RETURNING id",
        )
        .bind(xml_id)
        .fetch_one(db)
        .await
        .unwrap()
    }

    fn entry(code: &str, name: &str, source: MapSource, value: &str) -> MapEntry {
        MapEntry {
            person_type_id: 0,
            code: code.into(),
            name: name.into(),
            source,
            value: value.into(),
        }
    }

    #[sqlx::test]
    async fn agent_from_map_and_defaults(db: PgPool) {
        let f = order_fixture(&db).await;
        let uid = user(&db, "").await;
        let id = place_order(&db, &f, Some(uid)).await;
        let p = props(&db, &f, id).await;

        // Без соответствия — умолчания
        let o = one(&db, id).await;
        let a = &o.agent;
        assert_eq!(a.domain, Domain::Individual);
        assert_eq!(a.fields["EMAIL"], "ivan@example.ru");
        assert_eq!(a.fields["FULL_NAME"], "Иванов Иван");
        assert_eq!(a.fields["AGENT_NAME"], "Иванов Иван");
        assert_eq!(a.fields["PHONE"], "+7 900 000-00-00");
        assert_eq!(a.fields["ADDRESS_FULL"], "ул. Мясницкая, 1");
        assert_eq!(a.fields["CITY"], "Москва");
        assert_eq!(a.fields["INN"], "");
        assert!(
            o.traits
                .contains(&("Адрес доставки".into(), "ул. Мясницкая, 1".into()))
        );

        // Соответствие: свойство, часть местоположения, поле пользователя, константа, общий и
        // тип плательщика, произвольные реквизиты
        let mut common = vec![
            entry("BUYER_PERSON_NAME_FIRST", "", MapSource::User, "name"),
            entry("BUYER_PERSON_NAME_AGENT", "", MapSource::Value, "общий"),
            entry("1C_REKV_10", "Десятый", MapSource::Order, "account_number"),
        ];
        save_map(&db, 0, &common).await.unwrap();
        common = vec![
            entry(
                "BUYER_PERSON_INN",
                "",
                MapSource::Property,
                &p["INN"].to_string(),
            ),
            entry(
                "BUYER_PERSON_REGION",
                "",
                MapSource::Property,
                &format!("{}_REGION", p["CITY"]),
            ),
            entry("BUYER_PERSON_NAME_AGENT", "", MapSource::Value, "ИП Иванов"),
            entry("BUYER_PERSON_EMAIL", "", MapSource::Value, ""),
            entry("1C_REKV_2", "Отсрочка платежа", MapSource::Value, "30 дней"),
            entry("1C_REKV_3", "Пустой", MapSource::Value, ""),
        ];
        save_map(&db, f.person_type_id, &common).await.unwrap();
        let o = one(&db, id).await;
        let a = &o.agent;
        assert_eq!(a.fields["INN"], "7700000000");
        assert_eq!(a.fields["REGION"], "Московская область");
        assert_eq!(a.fields["NAME"], "Иван");
        assert_eq!(a.fields["AGENT_NAME"], "ИП Иванов");
        assert_eq!(
            a.fields["EMAIL"], "",
            "строка соответствия перекрывает умолчание"
        );
        assert_eq!(a.fields["FULL_NAME"], "Иванов Иван");
        let tail: Vec<&str> = o
            .traits
            .iter()
            .rev()
            .take(3)
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(tail, vec!["Адрес доставки", "Десятый", "Отсрочка платежа"]);
        assert!(o.traits.contains(&("Десятый".into(), id.to_string())));

        // Юрлицо — по домену типа плательщика, поля BUYER_COMPANY_*
        exec(
            &db,
            "UPDATE person_types SET domain = 'E' WHERE id = $1",
            f.person_type_id,
        )
        .await;
        save_map(
            &db,
            f.person_type_id,
            &[entry(
                "BUYER_COMPANY_INN",
                "",
                MapSource::Property,
                &p["INN"].to_string(),
            )],
        )
        .await
        .unwrap();
        let a = one(&db, id).await.agent;
        assert_eq!(a.domain, Domain::Entity);
        assert_eq!(a.fields["INN"], "7700000000");
        assert_eq!(a.fields["FULL_NAME"], "Иванов Иван");
        assert!(!a.fields.contains_key("SURNAME"));
    }

    #[sqlx::test]
    async fn agent_xml_id_generated_once(db: PgPool) {
        let f = order_fixture(&db).await;
        let uid = user(&db, "").await;
        exec(
            &db,
            "UPDATE users SET last_name = 'Константинопольский-Задунайский' WHERE id = $1",
            uid,
        )
        .await;
        let id = place_order(&db, &f, Some(uid)).await;
        let stored = || async {
            sqlx::query_scalar::<_, String>("SELECT xml_id FROM users WHERE id = $1")
                .bind(uid)
                .fetch_one(&db)
                .await
                .unwrap()
        };
        let expected: String =
            format!("{uid}#ivanov#Константинопольский-Задунайский Иван Иванович")
                .chars()
                .take(40)
                .collect();

        // persist = false (админка) — не пишет
        assert_eq!(one(&db, id).await.agent.id, expected);
        assert_eq!(stored().await, "");

        let s = SaleSettings::default();
        let mut conn = db.acquire().await.unwrap();
        let o = build(&mut conn, id, &s, &[], V208, None, true)
            .await
            .unwrap();
        assert_eq!(o.agent.id, expected);
        assert_eq!(expected.chars().count(), 40);
        drop(conn);
        assert_eq!(stored().await, expected);

        // Повтор берёт сохранённый, даже если ФИО поменялось
        exec(
            &db,
            "UPDATE users SET last_name = 'Петров' WHERE id = $1",
            uid,
        )
        .await;
        assert_eq!(one(&db, id).await.agent.id, expected);

        // Гость
        let guest = place_order(&db, &f, None).await;
        assert_eq!(one(&db, guest).await.agent.id, format!("{guest}#guest"));
    }

    struct Hook;

    impl OrderExportHook for Hook {
        fn traits<'a>(
            &'a self,
            conn: &'a mut PgConnection,
            order_id: i64,
        ) -> BoxFuture<'a, anyhow::Result<Vec<(String, String)>>> {
            Box::pin(async move {
                let n: String =
                    sqlx::query_scalar("SELECT account_number FROM orders WHERE id = $1")
                        .bind(order_id)
                        .fetch_one(&mut *conn)
                        .await?;
                Ok(vec![("Номер на сайте".into(), n)])
            })
        }
    }

    #[sqlx::test]
    async fn hook_traits_are_appended(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let hook: Arc<dyn OrderExportHook> = Arc::new(Hook);
        let s = SaleSettings::default();
        let b = query(&db, &s, V208, Duration::ZERO, Some(hook.as_ref()))
            .await
            .unwrap();
        assert_eq!(
            b.orders[0].traits.last(),
            Some(&("Номер на сайте".to_string(), id.to_string()))
        );
        assert!(
            crate::bxapi::project::Project::default()
                .order_export_hook
                .is_none()
        );
    }

    #[sqlx::test]
    async fn interval_limits_batch(db: PgPool) {
        let f = order_fixture(&db).await;
        let first = place_order(&db, &f, None).await;
        let second = place_order(&db, &f, None).await;
        let s = SaleSettings::default();
        let tiny = Duration::from_nanos(1);
        let b = query(&db, &s, V208, tiny, None).await.unwrap();
        assert_eq!(
            b.orders.iter().map(|o| o.id).collect::<Vec<_>>(),
            vec![first]
        );
        commit_export(&db, b.mark.unwrap()).await.unwrap();
        let s = SaleSettings::load(&db).await.unwrap();
        let b = query(&db, &s, V208, tiny, None).await.unwrap();
        assert_eq!(
            b.orders.iter().map(|o| o.id).collect::<Vec<_>>(),
            vec![second]
        );

        // Заказы с одинаковым временем изменения не разрываются
        sqlx::query("UPDATE orders SET manager_comment = 'x' WHERE id IN ($1, $2)")
            .bind(first)
            .bind(second)
            .execute(&db)
            .await
            .unwrap();
        let s = SaleSettings::load(&db).await.unwrap();
        let b = query(&db, &s, V208, tiny, None).await.unwrap();
        assert_eq!(b.orders.len(), 2);
    }

    #[sqlx::test]
    async fn builds_order_document(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        sqlx::query(
            "UPDATE collections SET xml_id = 'cat-1'
             WHERE id = (SELECT collection_id FROM collection_items WHERE id = $1)",
        )
        .bind(f.product_id)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE collection_items SET xml_id = 'p1#p1',
                    field_values = field_values || '{\"cml2_base_unit\": \"796\"}' WHERE id = $1",
        )
        .bind(f.product_id)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE catalog_stores SET xml_id = 'st-1', address = 'Склад, 1', phone = '+7 1' WHERE id = 1",
        )
        .execute(&db)
        .await
        .unwrap();
        exec(
            &db,
            "UPDATE orders SET manager_comment = 'Позвонить<br><br>до &laquo;10&raquo; <b>утра</b>',
                    user_comment = 'Побыстрее', version_1c = '7', price = 450 WHERE id = $1",
            id,
        )
        .await;
        exec(
            &db,
            "UPDATE shipments SET price = 250.7, id_1c = 'ОТГ-1' WHERE order_id = $1",
            id,
        )
        .await;
        sqlx::query(
            "INSERT INTO shipments (order_id, delivery_id, delivery_name, price, store_id)
             VALUES ($1, 1, 'Самовывоз', 100, 1)",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pay_systems (id, name, api_type) VALUES (1, 'QR', 'qr')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO options (module, name, value) VALUES ('main', 'site_name', 'Трансопт')",
        )
        .execute(&db)
        .await
        .unwrap();
        let s = SaleSettings {
            replace_currency: "руб".into(),
            number_prefix: "TO-".into(),
            ..SaleSettings::default()
        };
        let mut conn = db.acquire().await.unwrap();
        let o = build(&mut conn, id, &s, &[], V208, None, false)
            .await
            .unwrap();

        assert_eq!(o.number, format!("TO-{id}"));
        assert_eq!(o.currency, "руб");
        assert_eq!(o.sum, 450.0);
        assert_eq!(o.comment, "Позвонить\nдо «10» утра");
        assert_eq!(o.items.len(), 1);
        let it = &o.items[0];
        assert_eq!(
            (
                it.xml_id.as_str(),
                it.catalog_xml_id.as_str(),
                it.unit_code.as_str()
            ),
            ("p1", "cat-1", "796")
        );
        assert_eq!(
            (it.unit_symbol.as_str(), it.unit_intl.as_str()),
            ("шт", "PCE")
        );
        assert_eq!((it.price, it.quantity, it.sum), (100.0, 1.0, 100.0));
        assert!(it.basket_id > 0);
        // Склад двух отгрузок — один раз
        assert_eq!(o.stores.len(), 1);
        assert_eq!(o.stores[0].xml_id, "st-1");

        let names: Vec<&str> = o.traits.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Способ доставки",
                "Метод доставки ИД",
                "Метод оплаты",
                "Метод оплаты ИД",
                "Заказ оплачен",
                "Доставка разрешена",
                "Отменен",
                "Финальный статус",
                "Статус заказа",
                "Статуса заказа ИД",
                "Дата изменения статуса",
                "Комментарии покупателя",
                "Сайт",
            ]
        );
        assert!(
            o.traits
                .contains(&("Статус заказа".into(), "[N] Принят".into()))
        );
        assert!(o.traits.contains(&("Сайт".into(), "[s1] Трансопт".into())));

        assert_eq!(o.payments.len(), 1);
        let p = &o.payments[0];
        assert_eq!(p.operation, PayOperation::Cashless);
        assert_eq!(p.version_1c, "7");
        assert_eq!(o.shipments.len(), 2);
        let sh = &o.shipments[0];
        assert_eq!(sh.id, "ОТГ-1");
        assert_eq!(sh.sum, 350.0, "100 × 1 + intval(250.7)");
        assert_eq!(sh.version_1c, "7");
        assert_eq!(sh.delivery, Some(ExportDelivery { price: 250.7 }));
        assert_eq!(o.shipments[1].items.len(), 0);
        assert_eq!(o.shipments[1].sum, 100.0);

        // От 2.10 — «Дата последнего разрешения по доставке» и даты в ISO
        let o = build(&mut conn, id, &s, &[], SchemaVersion(2.1), None, false)
            .await
            .unwrap();
        assert_eq!(o.traits[0].0, "Дата последнего разрешения по доставке");
        assert!(!o.traits.iter().any(|(n, _)| n == "Доставка разрешена"));
        let status_date = &o
            .traits
            .iter()
            .find(|(n, _)| n == "Дата изменения статуса")
            .unwrap()
            .1;
        assert_eq!(status_date.len(), 19);
        assert_eq!(&status_date[10..11], "T");
    }

    #[test]
    fn to_text_like_bitrix() {
        assert_eq!(
            to_text("a<script>x()</script>b<STYLE>p{}</style>c<br/>d &amp; &#8381; &bogus; 1 < 2"),
            "abc\nd & ₽ &bogus; 1 < 2"
        );
    }
}
