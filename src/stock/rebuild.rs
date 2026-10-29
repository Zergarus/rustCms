//! Перестройка партий по истории при первом выборе метода (`ProductBatchConverter` и
//! `Balancer` Битрикса): фоновая задача по товарам, возобновляется после перезапуска.
//!
//! Прогресс — в опциях `catalog.batch_rebuild_last` (последний обработанный товар; пусто —
//! задачи нет), `catalog.batch_rebuild_total` и `catalog.batch_rebuild_done`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};

use super::StockError;
use super::batches::{self, Method};
use super::documents::{self, DocType, Line};
use crate::state::AppState;

const LAST: &str = "batch_rebuild_last";
const TOTAL: &str = "batch_rebuild_total";
const DONE: &str = "batch_rebuild_done";

/// Сколько товаров перестраивается за шаг фоновой задачи.
const LIMIT: i64 = 100;

/// Товары с историей: строки проведённых документов и позиции, отгруженные основной
/// отгрузкой заказа (у которых есть склад отгрузки). Только существующие товары.
macro_rules! items_with_history {
    () => {
        "SELECT h.item_id FROM (
             SELECT i.item_id FROM store_document_items i
             JOIN store_documents d ON d.id = i.doc_id
             WHERE d.status = 'Y' AND i.item_id IS NOT NULL
             UNION
             SELECT c.item_id FROM shipment_item_stores s
             JOIN shipments sh ON sh.id = s.shipment_id
             JOIN cart_items c ON c.id = s.cart_item_id
             WHERE sh.deducted AND c.item_id IS NOT NULL
               AND sh.id = (SELECT min(id) FROM shipments WHERE order_id = sh.order_id)
         ) h
         WHERE EXISTS (SELECT 1 FROM collection_items ci WHERE ci.id = h.item_id)"
    };
}

async fn set_option(tx: &mut PgConnection, name: &str, value: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO options (module, name, value) VALUES ('catalog', $1, $2)
         ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(name)
    .bind(value)
    .execute(tx)
    .await?;
    Ok(())
}

async fn number_option(db: &mut PgConnection, name: &str) -> sqlx::Result<i64> {
    let value: Option<String> =
        sqlx::query_scalar("SELECT value FROM options WHERE module = 'catalog' AND name = $1")
            .bind(name)
            .fetch_optional(db)
            .await?;
    Ok(value.and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}

/// Последний обработанный товар; `None` — задачи нет. `lock` — под блокировкой строки опции.
async fn last(db: &mut PgConnection, lock: bool) -> sqlx::Result<Option<i64>> {
    let sql = if lock {
        "SELECT value FROM options WHERE module = 'catalog' AND name = 'batch_rebuild_last'
         FOR UPDATE"
    } else {
        "SELECT value FROM options WHERE module = 'catalog' AND name = 'batch_rebuild_last'"
    };
    let value: Option<String> = sqlx::query_scalar(sql).fetch_optional(db).await?;
    Ok(value.and_then(|v| v.trim().parse().ok()))
}

/// Партий нет и метод выбран — ставит задачу (`last = 0`, `total`, `done = 0`). `false` —
/// перестраивать нечего.
pub async fn start(tx: &mut PgConnection) -> sqlx::Result<bool> {
    if batches::method(&mut *tx).await?.is_none() {
        return Ok(false);
    }
    let has_batches: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM catalog_store_batches)")
            .fetch_one(&mut *tx)
            .await?;
    if has_batches {
        return Ok(false);
    }
    let total: i64 = sqlx::query_scalar(concat!(
        "SELECT count(*) FROM (",
        items_with_history!(),
        ") t"
    ))
    .fetch_one(&mut *tx)
    .await?;
    set_option(&mut *tx, LAST, "0").await?;
    set_option(&mut *tx, TOTAL, &total.to_string()).await?;
    set_option(&mut *tx, DONE, "0").await?;
    Ok(true)
}

/// Следующие `limit` товаров после `last` по возрастанию, каждый в своей транзакции.
/// `false` — товары кончились (или задачи нет), опция `last` очищена.
pub async fn step(db: &PgPool, limit: i64) -> anyhow::Result<bool> {
    let mut c = db.acquire().await?;
    let Some(after) = last(&mut c, false).await? else {
        return Ok(false);
    };
    let method = batches::method(&mut c).await?;
    let items: Vec<i64> = match method {
        Some(_) => {
            sqlx::query_scalar(concat!(
                "SELECT item_id FROM (",
                items_with_history!(),
                ") t WHERE item_id > $1 ORDER BY item_id LIMIT $2"
            ))
            .bind(after)
            .bind(limit)
            .fetch_all(&mut *c)
            .await?
        }
        None => Vec::new(),
    };
    let Some(method) = method.filter(|_| !items.is_empty()) else {
        set_option(&mut c, LAST, "").await?;
        return Ok(false);
    };
    drop(c);
    for item in items {
        rebuild_item(db, method, item)
            .await
            .map_err(|e| e.context(format!("товар #{item}")))?;
    }
    Ok(true)
}

/// Перестройка партий одного товара: блокировка товара, удаление его партий, проигрыш истории,
/// сдвиг прогресса. Отказ правил (нет курса у средней) — в лог, товар остаётся без партий.
async fn rebuild_item(db: &PgPool, method: Method, item: i64) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    // Под блокировкой опции: параллельный шаг не перестроит товар второй раз
    match last(&mut tx, true).await? {
        Some(after) if after < item => {}
        _ => return Ok(()),
    }
    crate::catalog::load_locked(&mut tx, &[item]).await?;
    // Движения удаляются каскадом
    sqlx::query("DELETE FROM catalog_store_batches WHERE item_id = $1")
        .bind(item)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SAVEPOINT batch_rebuild")
        .execute(&mut *tx)
        .await?;
    match replay(&mut tx, method, item).await {
        Ok(()) => {
            sqlx::query("RELEASE SAVEPOINT batch_rebuild")
                .execute(&mut *tx)
                .await?;
        }
        Err(StockError::Rejected(messages)) => {
            tracing::warn!(
                "перестройка партий: товар #{item} пропущен: {}",
                messages.join("; ")
            );
            sqlx::query("ROLLBACK TO SAVEPOINT batch_rebuild")
                .execute(&mut *tx)
                .await?;
        }
        Err(StockError::Db(e)) => return Err(e.into()),
    }
    set_option(&mut tx, LAST, &item.to_string()).await?;
    let done = number_option(&mut tx, DONE).await? + 1;
    set_option(&mut tx, DONE, &done.to_string()).await?;
    tx.commit().await?;
    Ok(())
}

/// Событие истории товара.
enum Event {
    /// Проведённый документ: тип, валюта (пусто — валюта сайта), строки товара.
    Doc(DocType, String, Vec<Line>),
    /// Строка склада отгрузки: `(id, склад, количество)`.
    Ship(i64, i64, f64),
}

#[derive(sqlx::FromRow)]
struct DocLineRow {
    at: DateTime<Utc>,
    doc_id: i64,
    doc_type: String,
    currency: String,
    id: i64,
    store_from: Option<i64>,
    store_to: Option<i64>,
    amount: f64,
    purchasing_price: Option<f64>,
    base_price: Option<f64>,
}

/// История товара по времени: документы по дате проведения (затем `id`), отгрузки по дате
/// отгрузки; при равном времени документ раньше.
async fn history(tx: &mut PgConnection, item: i64) -> sqlx::Result<Vec<Event>> {
    let rows: Vec<DocLineRow> = sqlx::query_as(
        "SELECT COALESCE(d.date_status, d.updated_at) AS at, d.id AS doc_id,
                d.doc_type::text AS doc_type, d.currency, i.id, i.store_from, i.store_to,
                i.amount::float8 AS amount, i.purchasing_price::float8 AS purchasing_price,
                i.base_price::float8 AS base_price
         FROM store_document_items i JOIN store_documents d ON d.id = i.doc_id
         WHERE d.status = 'Y' AND i.item_id = $1
         ORDER BY at, d.id, i.id",
    )
    .bind(item)
    .fetch_all(&mut *tx)
    .await?;
    let mut docs: Vec<(DateTime<Utc>, Event)> = Vec::new();
    let mut current: Option<i64> = None;
    for r in rows {
        let line = Line {
            row_id: r.id,
            item_id: Some(item),
            store_from: r.store_from,
            store_to: r.store_to,
            amount: r.amount,
            purchasing_price: r.purchasing_price,
            base_price: r.base_price,
            barcodes: Vec::new(),
        };
        if current == Some(r.doc_id)
            && let Some((_, Event::Doc(_, _, lines))) = docs.last_mut()
        {
            lines.push(line);
            continue;
        }
        current = Some(r.doc_id);
        let Some(doc_type) = DocType::from_code(&r.doc_type) else {
            continue;
        };
        docs.push((r.at, Event::Doc(doc_type, r.currency, vec![line])));
    }
    let ships: Vec<(DateTime<Utc>, i64, i64, f64)> = sqlx::query_as(
        "SELECT COALESCE(sh.deducted_at, o.created_at) AS at, s.id, s.store_id,
                s.quantity::float8
         FROM shipment_item_stores s
         JOIN shipments sh ON sh.id = s.shipment_id
         JOIN orders o ON o.id = sh.order_id
         JOIN cart_items c ON c.id = s.cart_item_id
         WHERE c.item_id = $1 AND sh.deducted
           AND sh.id = (SELECT min(id) FROM shipments WHERE order_id = sh.order_id)
         ORDER BY at, s.id",
    )
    .bind(item)
    .fetch_all(&mut *tx)
    .await?;
    let mut events = Vec::with_capacity(docs.len() + ships.len());
    let mut docs = docs.into_iter().peekable();
    let mut ships = ships.into_iter().peekable();
    loop {
        let doc_first = match (docs.peek(), ships.peek()) {
            (Some(d), Some(s)) => d.0 <= s.0,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if doc_first {
            events.extend(docs.next().map(|d| d.1));
        } else {
            events.extend(
                ships
                    .next()
                    .map(|(_, id, store, amount)| Event::Ship(id, store, amount)),
            );
        }
    }
    Ok(events)
}

/// Проигрывает историю товара правилами живого метода.
async fn replay(tx: &mut PgConnection, method: Method, item: i64) -> Result<(), StockError> {
    for event in history(&mut *tx, item).await? {
        match event {
            Event::Doc(doc_type, currency, lines) => {
                let currency = documents::currency_or_site(&mut *tx, &currency).await?;
                batches::apply_document(&mut *tx, method, doc_type, &lines, &currency, false)
                    .await?;
            }
            Event::Ship(id, store, amount) => {
                batches::ship_out(&mut *tx, method, id, item, store, amount).await?;
            }
        }
    }
    Ok(())
}

/// `(готово, всего)`, пока задача есть.
pub async fn progress(db: &mut PgConnection) -> sqlx::Result<Option<(i64, i64)>> {
    if last(&mut *db, false).await?.is_none() {
        return Ok(None);
    }
    let done = number_option(&mut *db, DONE).await?;
    let total = number_option(&mut *db, TOTAL).await?;
    Ok(Some((done, total)))
}

/// Фоновая задача: раз в 5 с, пока есть товары — шаги подряд; ошибки — в лог.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(5));
        loop {
            timer.tick().await;
            loop {
                match step(&state.db, LIMIT).await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => {
                        tracing::error!("перестройка партий: {e:#}");
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stock::batches::db_tests::{
        arrival, b, batches, count, doc, doc_moves, line, order, set_shipped, setup, ship_moves,
    };
    use crate::stock::batches::{Method, set_method};
    use crate::stock::documents::{self, DocInput};
    use crate::test_support::seed_usd;

    async fn choose(db: &PgPool, method: Method) -> bool {
        let mut tx = db.begin().await.unwrap();
        assert!(set_method(&mut tx, method).await.unwrap());
        let started = start(&mut tx).await.unwrap();
        tx.commit().await.unwrap();
        started
    }

    async fn run_to_end(db: &PgPool) {
        while step(db, 100).await.unwrap() {}
    }

    async fn progress_of(db: &PgPool) -> Option<(i64, i64)> {
        let mut c = db.acquire().await.unwrap();
        progress(&mut c).await.unwrap()
    }

    /// Товар того же каталога, что у фикстуры.
    async fn product(db: &PgPool, like: i64, code: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO collection_items (collection_id, code, name)
             SELECT collection_id, $2, $2 FROM collection_items WHERE id = $1 RETURNING id",
        )
        .bind(like)
        .bind(code)
        .fetch_one(db)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn rebuild_replays_history(db: PgPool) {
        let f = setup(&db, None).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        arrival(&db, f.product_id, 1, 10.0, 150.0).await;
        let sh = order(&db, &f, 12.0).await;
        set_shipped(&db, sh, true).await;
        let d = doc(
            &db,
            "D",
            "",
            vec![line(f.product_id, Some(1), None, 3.0, 0.0)],
        )
        .await;
        assert_eq!(count(&db, "catalog_store_batches").await, 0);

        assert!(choose(&db, Method::Fifo).await);
        assert_eq!(progress_of(&db).await, Some((0, 1)));
        run_to_end(&db).await;
        assert_eq!(progress_of(&db).await, None);
        // Как у живого метода в `fifo_ship_takes_oldest`, затем списание из второй партии
        assert_eq!(ship_moves(&db, sh).await, [(-10.0, 100.0), (-2.0, 150.0)]);
        assert_eq!(doc_moves(&db, d).await, [(-3.0, 150.0)]);
        assert_eq!(
            batches(&db, f.product_id).await,
            [b(1, 0.0, 100.0), b(1, 5.0, 150.0)]
        );
    }

    #[sqlx::test]
    async fn rebuild_resumes_after_restart(db: PgPool) {
        let f = setup(&db, None).await;
        let p2 = product(&db, f.product_id, "p2").await;
        let p3 = product(&db, f.product_id, "p3").await;
        for (p, price) in [(f.product_id, 100.0), (p2, 200.0), (p3, 300.0)] {
            arrival(&db, p, 1, 4.0, price).await;
        }
        assert!(choose(&db, Method::Fifo).await);
        assert!(step(&db, 1).await.unwrap());
        assert_eq!(progress_of(&db).await, Some((1, 3)));
        // «Перезапуск»: новый вызов продолжает с сохранённого товара
        assert!(step(&db, 1).await.unwrap());
        assert_eq!(progress_of(&db).await, Some((2, 3)));
        assert!(step(&db, 1).await.unwrap());
        assert_eq!(progress_of(&db).await, Some((3, 3)));
        let done: String = sqlx::query_scalar(
            "SELECT value FROM options WHERE module = 'catalog' AND name = 'batch_rebuild_done'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(done, "3");
        run_to_end(&db).await;
        assert_eq!(progress_of(&db).await, None);
        for (p, price) in [(f.product_id, 100.0), (p2, 200.0), (p3, 300.0)] {
            assert_eq!(batches(&db, p).await, [b(1, 4.0, price)]);
        }
        assert_eq!(count(&db, "store_batch_moves").await, 3);
    }

    #[sqlx::test]
    async fn start_skips_when_batches_exist(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        arrival(&db, f.product_id, 1, 4.0, 100.0).await;
        let mut tx = db.begin().await.unwrap();
        assert!(!start(&mut tx).await.unwrap());
        tx.commit().await.unwrap();
        assert_eq!(progress_of(&db).await, None);
    }

    #[sqlx::test]
    async fn start_skips_without_method(db: PgPool) {
        let f = setup(&db, None).await;
        arrival(&db, f.product_id, 1, 4.0, 100.0).await;
        let mut tx = db.begin().await.unwrap();
        assert!(!start(&mut tx).await.unwrap());
        tx.commit().await.unwrap();
        assert_eq!(progress_of(&db).await, None);
        assert!(!step(&db, 100).await.unwrap());
    }

    #[sqlx::test]
    async fn progress_reports(db: PgPool) {
        let f = setup(&db, None).await;
        let p2 = product(&db, f.product_id, "p2").await;
        let p3 = product(&db, f.product_id, "p3").await;
        // Первый товар — только отгрузка (из остатка без документов), второй — приход,
        // у третьего документ не проведён: не считается
        let sh = order(&db, &f, 3.0).await;
        set_shipped(&db, sh, true).await;
        arrival(&db, p2, 1, 4.0, 200.0).await;
        let input = DocInput {
            doc_type: "A".into(),
            contractor_id: Some(1),
            lines: vec![line(p3, None, Some(1), 4.0, 300.0)],
            ..DocInput::default()
        };
        let mut tx = db.begin().await.unwrap();
        documents::create(&mut tx, &input, None).await.unwrap();
        tx.commit().await.unwrap();

        assert_eq!(progress_of(&db).await, None);
        assert!(choose(&db, Method::Average).await);
        assert_eq!(progress_of(&db).await, Some((0, 2)));
        assert!(step(&db, 1).await.unwrap());
        assert_eq!(progress_of(&db).await, Some((1, 2)));
        run_to_end(&db).await;
        assert_eq!(progress_of(&db).await, None);
        // Отгрузка без партий — непокрытая, партий у товара нет
        assert!(batches(&db, f.product_id).await.is_empty());
        assert_eq!(batches(&db, p2).await, [b(1, 4.0, 200.0)]);
        assert!(batches(&db, p3).await.is_empty());
    }

    #[sqlx::test]
    async fn rebuild_skips_product_without_rate(db: PgPool) {
        let f = setup(&db, None).await;
        seed_usd(&db, None).await;
        let p2 = product(&db, f.product_id, "p2").await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        doc(
            &db,
            "A",
            // Валюты нет среди валют сайта: курса к рублю нет
            "EUR",
            vec![line(f.product_id, None, Some(1), 10.0, 2.0)],
        )
        .await;
        arrival(&db, p2, 1, 4.0, 200.0).await;
        assert!(choose(&db, Method::Average).await);
        run_to_end(&db).await;
        // Средняя без курса EUR → RUB: товар пропущен без партий, перестройка дошла до конца
        assert!(batches(&db, f.product_id).await.is_empty());
        assert_eq!(batches(&db, p2).await, [b(1, 4.0, 200.0)]);
        assert_eq!(progress_of(&db).await, None);
        let done: String = sqlx::query_scalar(
            "SELECT value FROM options WHERE module = 'catalog' AND name = 'batch_rebuild_done'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(done, "2");
    }
}
