//! Партии товара: правила списания по FIFO и пересчёта средней закупочной цены и их
//! применение к таблицам партий при проведении документов и отгрузке.
//!
//! Движение партии (`store_batch_moves`) хранит цену и валюту партии на момент движения:
//! у расхода — цену партии, у прихода — цену поступления в валюте партии (по ней отмена
//! прихода пересчитывает среднюю цену).

use sqlx::PgConnection;

use super::StockError;
use super::documents::{DocType, Line};

/// Погрешность сравнения количеств.
const EPS: f64 = 1e-9;

/// Метод расчёта себестоимости (`catalog.cost_price_calculation_method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `fifo` — списание из самых ранних партий
    Fifo,
    /// `average` — средневзвешенная закупочная цена
    Average,
}

impl Method {
    /// Метод по коду опции; пустой и неизвестный код — метода нет.
    pub fn from_code(code: &str) -> Option<Method> {
        match code {
            "fifo" => Some(Method::Fifo),
            "average" => Some(Method::Average),
            _ => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Method::Fifo => "fifo",
            Method::Average => "average",
        }
    }
}

/// Партия прихода: остаток, закупочная цена и её валюта.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub id: i64,
    pub available: f64,
    pub price: f64,
    pub currency: String,
}

/// Сколько списано из партии.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub batch_id: i64,
    pub amount: f64,
}

/// Округление до 2 знаков.
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Списание `amount` по FIFO: от меньшего `id` к большему, только партии с остатком.
/// Возвращает списанные части и непокрытое количество (0, если партий хватило).
pub fn write_off(batches: &[Batch], amount: f64) -> (Vec<Part>, f64) {
    let mut sorted: Vec<&Batch> = batches.iter().filter(|b| b.available > EPS).collect();
    sorted.sort_by_key(|b| b.id);
    let mut left = amount;
    let mut parts = Vec::new();
    for batch in sorted {
        if left <= EPS {
            break;
        }
        let take = batch.available.min(left);
        parts.push(Part {
            batch_id: batch.id,
            amount: take,
        });
        left -= take;
    }
    (parts, if left <= EPS { 0.0 } else { left })
}

/// Средняя цена после прихода `amount` по `price`; при пустом остатке — просто `price`.
pub fn average_add(old_amount: f64, old_price: f64, amount: f64, price: f64) -> f64 {
    if old_amount <= 0.0 {
        return price;
    }
    round2((old_amount * old_price + amount * price) / (old_amount + amount))
}

/// Обратная операция: средняя цена после ухода `amount` по `price`;
/// если остаток обнулился — прежняя цена.
pub fn average_remove(old_amount: f64, old_price: f64, amount: f64, price: f64) -> f64 {
    let rest = old_amount - amount;
    if rest <= EPS {
        return old_price;
    }
    round2((old_amount * old_price - amount * price) / rest)
}

// --- БД ---

const ALREADY_CHOSEN: &str = "Метод расчёта себестоимости уже выбран";

fn reject(message: &str) -> StockError {
    StockError::Rejected(vec![message.to_string()])
}

/// Выбранный метод (`catalog.cost_price_calculation_method`).
pub async fn method(db: &mut PgConnection) -> sqlx::Result<Option<Method>> {
    let value: Option<String> = sqlx::query_scalar(
        "SELECT value FROM options
         WHERE module = 'catalog' AND name = 'cost_price_calculation_method'",
    )
    .fetch_optional(db)
    .await?;
    Ok(value.as_deref().and_then(Method::from_code))
}

/// Выбирает метод — один раз и только при включённом складском учёте; `true` — выбран сейчас.
/// Перестройку партий по истории не запускает.
pub async fn set_method(tx: &mut PgConnection, method: Method) -> Result<bool, StockError> {
    if !super::settings(&mut *tx).await?.store_control {
        return Err(reject("Складской учёт выключен"));
    }
    // Условие в ON CONFLICT отсекает смену уже выбранного метода и при гонке двух выборов
    let chosen: Option<bool> = sqlx::query_scalar(
        "INSERT INTO options (module, name, value)
         VALUES ('catalog', 'cost_price_calculation_method', $1)
         ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value
         WHERE options.value NOT IN ('fifo', 'average')
         RETURNING TRUE",
    )
    .bind(method.code())
    .fetch_optional(&mut *tx)
    .await?;
    match chosen {
        Some(_) => Ok(true),
        None => Err(reject(ALREADY_CHOSEN)),
    }
}

/// К чему привязано движение партии: строка документа или строка склада отгрузки.
#[derive(Debug, Clone, Copy)]
enum Link {
    Doc(i64),
    Ship(i64),
}

impl Link {
    fn from_columns(doc_item_id: Option<i64>, shipment_store_id: Option<i64>) -> Link {
        match (doc_item_id, shipment_store_id) {
            (Some(doc), _) => Link::Doc(doc),
            (None, ship) => Link::Ship(ship.unwrap_or_default()),
        }
    }

    fn doc(self) -> Option<i64> {
        match self {
            Link::Doc(id) => Some(id),
            Link::Ship(_) => None,
        }
    }

    fn ship(self) -> Option<i64> {
        match self {
            Link::Ship(id) => Some(id),
            Link::Doc(_) => None,
        }
    }
}

/// Израсходованная часть с ценой и валютой партии.
struct Spent {
    amount: f64,
    price: f64,
    currency: String,
}

/// Партии товара на складе под блокировкой, в порядке `id`.
async fn store_batches(tx: &mut PgConnection, item: i64, store: i64) -> sqlx::Result<Vec<Batch>> {
    let rows: Vec<(i64, f64, f64, String)> = sqlx::query_as(
        "SELECT id, available::float8, price::float8, currency FROM catalog_store_batches
         WHERE item_id = $1 AND store_id = $2 ORDER BY id FOR UPDATE",
    )
    .bind(item)
    .bind(store)
    .fetch_all(tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, available, price, currency)| Batch {
            id,
            available,
            price,
            currency,
        })
        .collect())
}

/// Партия по `id` под блокировкой.
async fn lock_batch(tx: &mut PgConnection, id: i64) -> sqlx::Result<Option<Batch>> {
    let row: Option<(f64, f64, String)> = sqlx::query_as(
        "SELECT available::float8, price::float8, currency FROM catalog_store_batches
         WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(tx)
    .await?;
    Ok(row.map(|(available, price, currency)| Batch {
        id,
        available,
        price,
        currency,
    }))
}

async fn insert_move(
    tx: &mut PgConnection,
    batch_id: i64,
    link: Link,
    amount: f64,
    price: f64,
    currency: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO store_batch_moves
             (batch_id, doc_item_id, shipment_store_id, amount, price, currency)
         VALUES ($1, $2, $3, $4::float8::numeric, $5::float8::numeric, $6)",
    )
    .bind(batch_id)
    .bind(link.doc())
    .bind(link.ship())
    .bind(amount)
    .bind(price)
    .bind(currency)
    .execute(tx)
    .await?;
    Ok(())
}

async fn add_available(tx: &mut PgConnection, batch_id: i64, delta: f64) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE catalog_store_batches SET available = available + $2::float8::numeric
         WHERE id = $1",
    )
    .bind(batch_id)
    .bind(delta)
    .execute(tx)
    .await?;
    Ok(())
}

/// Списывает `parts` из `batches` (и в памяти) с движениями `link`.
async fn spend_parts(
    tx: &mut PgConnection,
    batches: &mut [Batch],
    parts: &[Part],
    link: Link,
) -> sqlx::Result<Vec<Spent>> {
    let mut spent = Vec::new();
    for part in parts {
        let Some(batch) = batches.iter_mut().find(|b| b.id == part.batch_id) else {
            continue;
        };
        add_available(&mut *tx, batch.id, -part.amount).await?;
        batch.available -= part.amount;
        insert_move(
            &mut *tx,
            batch.id,
            link,
            -part.amount,
            batch.price,
            &batch.currency,
        )
        .await?;
        spent.push(Spent {
            amount: part.amount,
            price: batch.price,
            currency: batch.currency.clone(),
        });
    }
    Ok(spent)
}

/// Расход из партий склада от старых к новым (у средней партия одна). Нехватка партий —
/// расходуется сколько есть, непокрытое ни к какой партии не привязывается.
async fn consume(
    tx: &mut PgConnection,
    link: Link,
    item: i64,
    store: i64,
    amount: f64,
) -> sqlx::Result<Vec<Spent>> {
    let mut batches = store_batches(&mut *tx, item, store).await?;
    let (parts, _uncovered) = write_off(&batches, amount);
    spend_parts(tx, &mut batches, &parts, link).await
}

/// Приход в партии склада: FIFO — новая партия, средняя — вливание в единственную партию
/// с переводом цены в её валюту.
#[allow(clippy::too_many_arguments)]
async fn receive(
    tx: &mut PgConnection,
    method: Method,
    row_id: i64,
    item: i64,
    store: i64,
    amount: f64,
    price: f64,
    currency: &str,
) -> Result<(), StockError> {
    let link = Link::Doc(row_id);
    let existing = match method {
        Method::Fifo => None,
        Method::Average => store_batches(&mut *tx, item, store)
            .await?
            .into_iter()
            .next(),
    };
    let Some(batch) = existing else {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO catalog_store_batches (item_id, store_id, available, price, currency)
             VALUES ($1, $2, $3::float8::numeric, round($4::float8::numeric, 2), $5)
             RETURNING id",
        )
        .bind(item)
        .bind(store)
        .bind(amount)
        .bind(price)
        .bind(currency)
        .fetch_one(&mut *tx)
        .await?;
        insert_move(tx, id, link, amount, price, currency).await?;
        return Ok(());
    };
    let price = if currency == batch.currency {
        price
    } else {
        crate::currency::Converter::new(&mut *tx, &batch.currency, &[currency])
            .await?
            .convert(price, currency)
            .ok_or_else(|| reject("Невозможно получить курс"))?
    };
    let average = average_add(batch.available.max(0.0), batch.price, amount, price);
    sqlx::query(
        "UPDATE catalog_store_batches
         SET available = available + $2::float8::numeric, price = $3::float8::numeric
         WHERE id = $1",
    )
    .bind(batch.id)
    .bind(amount)
    .bind(average)
    .execute(&mut *tx)
    .await?;
    insert_move(tx, batch.id, link, amount, price, &batch.currency).await?;
    Ok(())
}

/// Возвращает расходы (минусовые движения) в их партии и удаляет движения;
/// средняя цена пересчитывается с ценой движения. `moves` — `(движение, партия, количество > 0,
/// цена)`.
async fn return_moves(
    tx: &mut PgConnection,
    method: Method,
    moves: &[(i64, i64, f64, f64)],
) -> sqlx::Result<()> {
    for &(move_id, batch_id, amount, price) in moves {
        if let Some(batch) = lock_batch(&mut *tx, batch_id).await? {
            let new_price = match method {
                Method::Fifo => batch.price,
                Method::Average => {
                    average_add(batch.available.max(0.0), batch.price, amount, price)
                }
            };
            sqlx::query(
                "UPDATE catalog_store_batches
                 SET available = available + $2::float8::numeric, price = $3::float8::numeric
                 WHERE id = $1",
            )
            .bind(batch_id)
            .bind(amount)
            .bind(new_price)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("DELETE FROM store_batch_moves WHERE id = $1")
            .bind(move_id)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

/// Отмена прихода по FIFO: партии строк `rows` удаляются, расходы из них перепривязываются
/// к остальным партиям склада (`rebindWriteOffValues`); непокрытое остаётся без движения.
async fn drop_fifo(tx: &mut PgConnection, rows: &[i64]) -> sqlx::Result<()> {
    let doomed: Vec<(i64, i64, i64)> = sqlx::query_as(
        "SELECT DISTINCT b.id, b.item_id, b.store_id
         FROM store_batch_moves m JOIN catalog_store_batches b ON b.id = m.batch_id
         WHERE m.doc_item_id = ANY($1) AND m.amount > 0
         ORDER BY b.id",
    )
    .bind(rows)
    .fetch_all(&mut *tx)
    .await?;
    let mut places: Vec<(i64, i64)> = doomed.iter().map(|d| (d.1, d.2)).collect();
    places.sort_unstable();
    places.dedup();
    let doomed_ids: Vec<i64> = doomed.iter().map(|d| d.0).collect();
    for (item, store) in places {
        let mut others: Vec<Batch> = store_batches(&mut *tx, item, store)
            .await?
            .into_iter()
            .filter(|b| !doomed_ids.contains(&b.id))
            .collect();
        let spent: Vec<(Option<i64>, Option<i64>, f64)> = sqlx::query_as(
            "SELECT m.doc_item_id, m.shipment_store_id, -m.amount::float8
             FROM store_batch_moves m JOIN catalog_store_batches b ON b.id = m.batch_id
             WHERE m.batch_id = ANY($1) AND b.item_id = $2 AND b.store_id = $3
               AND m.amount < 0
             ORDER BY m.id",
        )
        .bind(&doomed_ids)
        .bind(item)
        .bind(store)
        .fetch_all(&mut *tx)
        .await?;
        for (doc_item_id, shipment_store_id, amount) in spent {
            let link = Link::from_columns(doc_item_id, shipment_store_id);
            let (parts, _uncovered) = write_off(&others, amount);
            spend_parts(&mut *tx, &mut others, &parts, link).await?;
        }
    }
    // Каскад удаляет и движения удаляемых партий
    sqlx::query("DELETE FROM catalog_store_batches WHERE id = ANY($1)")
        .bind(&doomed_ids)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Отмена прихода по средней: из партии вычитается пришедшее, цена пересчитывается обратно,
/// приходные движения строк удаляются. Остаток партии не уходит ниже нуля: израсходованное
/// уже отгрузками и документами остаётся за ними со своими ценами.
async fn drop_average(tx: &mut PgConnection, rows: &[i64]) -> sqlx::Result<()> {
    let moves: Vec<(i64, i64, f64, f64)> = sqlx::query_as(
        "SELECT id, batch_id, amount::float8, price::float8 FROM store_batch_moves
         WHERE doc_item_id = ANY($1) AND amount > 0 ORDER BY id",
    )
    .bind(rows)
    .fetch_all(&mut *tx)
    .await?;
    for (move_id, batch_id, amount, price) in moves {
        if let Some(batch) = lock_batch(&mut *tx, batch_id).await? {
            let new_price = average_remove(batch.available, batch.price, amount, price);
            sqlx::query(
                "UPDATE catalog_store_batches
                 SET available = GREATEST(available - $2::float8::numeric, 0),
                     price = $3::float8::numeric
                 WHERE id = $1",
            )
            .bind(batch_id)
            .bind(amount)
            .bind(new_price)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("DELETE FROM store_batch_moves WHERE id = $1")
            .bind(move_id)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

/// Партии при проведении (`cancel` = `false`) или отмене проведения документа; вызывается
/// после записи остатков. `currency` — валюта документа (цены прихода — в ней).
pub(super) async fn apply_document(
    tx: &mut PgConnection,
    method: Method,
    doc_type: DocType,
    lines: &[Line],
    currency: &str,
    cancel: bool,
) -> Result<(), StockError> {
    if cancel {
        let rows: Vec<i64> = lines.iter().map(|l| l.row_id).collect();
        // Сначала получатель (его партии могли уже израсходовать), затем возврат в источник
        if doc_type.uses_destination() {
            match method {
                Method::Fifo => drop_fifo(&mut *tx, &rows).await?,
                Method::Average => drop_average(&mut *tx, &rows).await?,
            }
        }
        if doc_type.uses_source() {
            let moves: Vec<(i64, i64, f64, f64)> = sqlx::query_as(
                "SELECT id, batch_id, -amount::float8, price::float8 FROM store_batch_moves
                 WHERE doc_item_id = ANY($1) AND amount < 0 ORDER BY id",
            )
            .bind(&rows)
            .fetch_all(&mut *tx)
            .await?;
            return_moves(&mut *tx, method, &moves).await?;
        }
        return Ok(());
    }
    for line in lines {
        let Some(item) = line.item_id else {
            continue;
        };
        let spent = match line.store_from {
            Some(from) if doc_type.uses_source() => {
                consume(&mut *tx, Link::Doc(line.row_id), item, from, line.amount).await?
            }
            _ => Vec::new(),
        };
        let Some(to) = line.store_to.filter(|_| doc_type.uses_destination()) else {
            continue;
        };
        if doc_type == DocType::Moving {
            // Каждая израсходованная часть источника приходит со своей ценой
            for part in &spent {
                receive(
                    &mut *tx,
                    method,
                    line.row_id,
                    item,
                    to,
                    part.amount,
                    part.price,
                    &part.currency,
                )
                .await?;
            }
        } else {
            receive(
                &mut *tx,
                method,
                line.row_id,
                item,
                to,
                line.amount,
                line.purchasing_price.unwrap_or(0.0),
                currency,
            )
            .await?;
        }
    }
    Ok(())
}

/// Расход отгрузки строки склада отгрузки `shipment_store_id`.
pub(super) async fn ship_out(
    tx: &mut PgConnection,
    _method: Method,
    shipment_store_id: i64,
    item_id: i64,
    store_id: i64,
    amount: f64,
) -> sqlx::Result<()> {
    consume(tx, Link::Ship(shipment_store_id), item_id, store_id, amount).await?;
    Ok(())
}

/// Снятие «Отгружено»: количество движений отгрузки возвращается в их партии, движения
/// удаляются; строки `shipment_item_stores` (выбор склада) остаются.
pub(super) async fn ship_back(
    tx: &mut PgConnection,
    method: Method,
    shipment_id: i64,
) -> sqlx::Result<()> {
    let moves: Vec<(i64, i64, f64, f64)> = sqlx::query_as(
        "SELECT m.id, m.batch_id, -m.amount::float8, m.price::float8
         FROM store_batch_moves m JOIN shipment_item_stores s ON s.id = m.shipment_store_id
         WHERE s.shipment_id = $1 AND m.amount < 0 ORDER BY m.id",
    )
    .bind(shipment_id)
    .fetch_all(&mut *tx)
    .await?;
    return_moves(tx, method, &moves).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(id: i64, available: f64, price: f64) -> Batch {
        Batch {
            id,
            available,
            price,
            currency: "RUB".into(),
        }
    }

    fn part(batch_id: i64, amount: f64) -> Part {
        Part { batch_id, amount }
    }

    #[test]
    fn write_off_oldest_first() {
        let batches = [batch(2, 10.0, 150.0), batch(1, 10.0, 100.0)];
        let (parts, short) = write_off(&batches, 12.0);
        assert_eq!(parts, [part(1, 10.0), part(2, 2.0)]);
        assert_eq!(short, 0.0);
    }

    #[test]
    fn write_off_shortage() {
        let (parts, short) = write_off(&[batch(1, 5.0, 100.0)], 8.0);
        assert_eq!(parts, [part(1, 5.0)]);
        assert_eq!(short, 3.0);
    }

    #[test]
    fn write_off_skips_empty() {
        let batches = [batch(1, 0.0, 100.0), batch(2, 4.0, 150.0)];
        let (parts, short) = write_off(&batches, 3.0);
        assert_eq!(parts, [part(2, 3.0)]);
        assert_eq!(short, 0.0);
    }

    #[test]
    fn average_add_example() {
        assert_eq!(average_add(10.0, 100.0, 10.0, 150.0), 125.0);
        assert_eq!(average_add(0.0, 99.0, 5.0, 150.0), 150.0);
    }

    #[test]
    fn average_rounds_to_two() {
        assert_eq!(average_add(3.0, 10.0, 1.0, 11.0), 10.25);
        assert_eq!(average_add(3.0, 10.0, 4.0, 11.0), 10.57);
    }

    #[test]
    fn average_remove_inverse() {
        assert_eq!(average_remove(20.0, 125.0, 10.0, 150.0), 100.0);
    }

    #[test]
    fn average_remove_to_zero_keeps_price() {
        assert_eq!(average_remove(10.0, 125.0, 10.0, 150.0), 125.0);
        assert_eq!(average_remove(5.0, 125.0, 8.0, 150.0), 125.0);
    }

    #[test]
    fn method_codes() {
        assert_eq!(Method::from_code("fifo"), Some(Method::Fifo));
        assert_eq!(Method::from_code("average"), Some(Method::Average));
        assert_eq!(Method::from_code(""), None);
        assert_eq!(Method::from_code("lifo"), None);
        assert_eq!(Method::Fifo.code(), "fifo");
        assert_eq!(Method::Average.code(), "average");
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::stock::documents::{self, DocInput};
    use crate::stock::{set_store_control, settings, ship};
    use crate::test_support::{Fixture, order_fixture, place_order_with, seed_usd};
    use sqlx::PgPool;

    /// Учёт включён, склад 2, поставщик 1, позиция фикстуры убрана; метод — если задан.
    /// Товар фикстуры: на складе 1 — 5 шт. без партий (как остаток, заведённый мимо документов).
    async fn setup(db: &PgPool, method: Option<Method>) -> Fixture {
        let f = order_fixture(db).await;
        for sql in [
            "INSERT INTO catalog_stores (id, name) VALUES (2, 'Склад 2')",
            "INSERT INTO contractors (id, company) VALUES (1, 'Поставщик')",
        ] {
            sqlx::query(sql).execute(db).await.unwrap();
        }
        sqlx::query("DELETE FROM cart_items WHERE id = $1")
            .bind(f.item_id)
            .execute(db)
            .await
            .unwrap();
        let mut tx = db.begin().await.unwrap();
        set_store_control(&mut tx, true).await.unwrap();
        if let Some(m) = method {
            assert!(set_method(&mut tx, m).await.unwrap());
        }
        tx.commit().await.unwrap();
        f
    }

    fn line(item: i64, from: Option<i64>, to: Option<i64>, amount: f64, price: f64) -> Line {
        Line {
            row_id: 0,
            item_id: Some(item),
            store_from: from,
            store_to: to,
            amount,
            purchasing_price: Some(price),
            base_price: None,
            barcodes: Vec::new(),
        }
    }

    /// Создаёт и проводит документ.
    async fn doc(db: &PgPool, doc_type: &str, currency: &str, lines: Vec<Line>) -> i64 {
        let input = DocInput {
            doc_type: doc_type.into(),
            contractor_id: Some(1),
            currency: currency.into(),
            lines,
            ..DocInput::default()
        };
        let mut tx = db.begin().await.unwrap();
        let id = documents::create(&mut tx, &input, None).await.unwrap();
        documents::conduct(&mut tx, id, None).await.unwrap();
        tx.commit().await.unwrap();
        id
    }

    async fn arrival(db: &PgPool, item: i64, store: i64, amount: f64, price: f64) -> i64 {
        doc(
            db,
            "A",
            "",
            vec![line(item, None, Some(store), amount, price)],
        )
        .await
    }

    async fn cancel(db: &PgPool, id: i64) {
        let mut tx = db.begin().await.unwrap();
        documents::cancel(&mut tx, id, None).await.unwrap();
        tx.commit().await.unwrap();
    }

    /// Заказ на `quantity` со склада 1; возвращает основную отгрузку.
    async fn order(db: &PgPool, f: &Fixture, quantity: f64) -> i64 {
        let order = place_order_with(db, f, None, Some(1), quantity).await;
        sqlx::query_scalar("SELECT min(id) FROM shipments WHERE order_id = $1")
            .bind(order)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn set_shipped(db: &PgPool, shipment: i64, on: bool) {
        let mut tx = db.begin().await.unwrap();
        assert!(
            ship::set_deducted(&mut tx, shipment, on, &[], None)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
    }

    /// Партии товара: `(склад, остаток, цена, валюта)` по `id`.
    async fn batches(db: &PgPool, item: i64) -> Vec<(i64, f64, f64, String)> {
        sqlx::query_as(
            "SELECT store_id, available::float8, price::float8, currency
             FROM catalog_store_batches WHERE item_id = $1 ORDER BY id",
        )
        .bind(item)
        .fetch_all(db)
        .await
        .unwrap()
    }

    fn b(store: i64, available: f64, price: f64) -> (i64, f64, f64, String) {
        (store, available, price, "RUB".into())
    }

    /// Движения отгрузки: `(количество, цена)` по `id`.
    async fn ship_moves(db: &PgPool, shipment: i64) -> Vec<(f64, f64)> {
        sqlx::query_as(
            "SELECT m.amount::float8, m.price::float8
             FROM store_batch_moves m JOIN shipment_item_stores s ON s.id = m.shipment_store_id
             WHERE s.shipment_id = $1 ORDER BY m.id",
        )
        .bind(shipment)
        .fetch_all(db)
        .await
        .unwrap()
    }

    /// Движения строк документа: `(количество, цена)` по `id`.
    async fn doc_moves(db: &PgPool, doc: i64) -> Vec<(f64, f64)> {
        sqlx::query_as(
            "SELECT m.amount::float8, m.price::float8
             FROM store_batch_moves m JOIN store_document_items i ON i.id = m.doc_item_id
             WHERE i.doc_id = $1 ORDER BY m.id",
        )
        .bind(doc)
        .fetch_all(db)
        .await
        .unwrap()
    }

    async fn count(db: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn store_amount(db: &PgPool, item: i64, store: i64) -> f64 {
        sqlx::query_scalar(
            "SELECT amount::float8 FROM catalog_store_amounts WHERE item_id = $1 AND store_id = $2",
        )
        .bind(item)
        .bind(store)
        .fetch_one(db)
        .await
        .unwrap()
    }

    fn rejected<T: std::fmt::Debug>(r: Result<T, StockError>) -> Vec<String> {
        match r {
            Err(StockError::Rejected(m)) => m,
            other => panic!("ожидался отказ, получено {other:?}"),
        }
    }

    #[sqlx::test]
    async fn no_method_no_batches(db: PgPool) {
        let f = setup(&db, None).await;
        let a = arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        doc(
            &db,
            "D",
            "",
            vec![line(f.product_id, Some(1), None, 2.0, 0.0)],
        )
        .await;
        doc(
            &db,
            "M",
            "",
            vec![line(f.product_id, Some(1), Some(2), 3.0, 0.0)],
        )
        .await;
        let sh = order(&db, &f, 4.0).await;
        set_shipped(&db, sh, true).await;
        set_shipped(&db, sh, false).await;
        cancel(&db, a).await;
        assert_eq!(count(&db, "catalog_store_batches").await, 0);
        assert_eq!(count(&db, "store_batch_moves").await, 0);
    }

    #[sqlx::test]
    async fn set_method_once(db: PgPool) {
        order_fixture(&db).await;
        let mut c = db.acquire().await.unwrap();
        assert_eq!(method(&mut c).await.unwrap(), None);
        assert_eq!(
            rejected(set_method(&mut c, Method::Fifo).await),
            ["Складской учёт выключен"]
        );
        set_store_control(&mut c, true).await.unwrap();
        assert!(set_method(&mut c, Method::Fifo).await.unwrap());
        assert_eq!(method(&mut c).await.unwrap(), Some(Method::Fifo));
        assert_eq!(settings(&mut c).await.unwrap().method, Some(Method::Fifo));
        for m in [Method::Average, Method::Fifo] {
            assert_eq!(
                rejected(set_method(&mut c, m).await),
                ["Метод расчёта себестоимости уже выбран"]
            );
        }
        assert_eq!(method(&mut c).await.unwrap(), Some(Method::Fifo));
    }

    #[sqlx::test]
    async fn set_method_over_empty_option(db: PgPool) {
        setup(&db, None).await;
        sqlx::query(
            "INSERT INTO options (module, name, value)
             VALUES ('catalog', 'cost_price_calculation_method', '')",
        )
        .execute(&db)
        .await
        .unwrap();
        let mut c = db.acquire().await.unwrap();
        assert!(set_method(&mut c, Method::Average).await.unwrap());
        assert_eq!(method(&mut c).await.unwrap(), Some(Method::Average));
    }

    #[sqlx::test]
    async fn fifo_arrival_creates_batches(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        let a = doc(
            &db,
            "A",
            "",
            vec![
                line(f.product_id, None, Some(1), 10.0, 100.0),
                line(f.product_id, None, Some(1), 4.0, 150.0),
                Line {
                    purchasing_price: None,
                    ..line(f.product_id, None, Some(2), 3.0, 0.0)
                },
            ],
        )
        .await;
        assert_eq!(
            batches(&db, f.product_id).await,
            [b(1, 10.0, 100.0), b(1, 4.0, 150.0), b(2, 3.0, 0.0)]
        );
        assert_eq!(
            doc_moves(&db, a).await,
            [(10.0, 100.0), (4.0, 150.0), (3.0, 0.0)]
        );
        // Оприходование и возврат — как приход
        doc(
            &db,
            "S",
            "",
            vec![line(f.product_id, None, Some(2), 1.0, 70.0)],
        )
        .await;
        doc(
            &db,
            "R",
            "",
            vec![line(f.product_id, None, Some(2), 2.0, 80.0)],
        )
        .await;
        assert_eq!(
            batches(&db, f.product_id).await[3..],
            [b(2, 1.0, 70.0), b(2, 2.0, 80.0)]
        );
        // Отмена — партии строк удаляются
        cancel(&db, a).await;
        assert_eq!(
            batches(&db, f.product_id).await,
            [b(2, 1.0, 70.0), b(2, 2.0, 80.0)]
        );
        assert!(doc_moves(&db, a).await.is_empty());
    }

    #[sqlx::test]
    async fn fifo_ship_takes_oldest(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        arrival(&db, f.product_id, 1, 10.0, 150.0).await;
        let sh = order(&db, &f, 12.0).await;
        set_shipped(&db, sh, true).await;
        assert_eq!(ship_moves(&db, sh).await, [(-10.0, 100.0), (-2.0, 150.0)]);
        assert_eq!(
            batches(&db, f.product_id).await,
            [b(1, 0.0, 100.0), b(1, 8.0, 150.0)]
        );
    }

    #[sqlx::test]
    async fn fifo_unship_returns(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        arrival(&db, f.product_id, 1, 10.0, 150.0).await;
        let sh = order(&db, &f, 12.0).await;
        set_shipped(&db, sh, true).await;
        set_shipped(&db, sh, false).await;
        assert!(ship_moves(&db, sh).await.is_empty());
        assert_eq!(
            batches(&db, f.product_id).await,
            [b(1, 10.0, 100.0), b(1, 10.0, 150.0)]
        );
        // Выбор склада остаётся; повторная отгрузка — те же движения
        assert_eq!(count(&db, "shipment_item_stores").await, 1);
        set_shipped(&db, sh, true).await;
        assert_eq!(ship_moves(&db, sh).await, [(-10.0, 100.0), (-2.0, 150.0)]);
    }

    #[sqlx::test]
    async fn fifo_moving_carries_prices(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        arrival(&db, f.product_id, 1, 10.0, 150.0).await;
        let m = doc(
            &db,
            "M",
            "",
            vec![line(f.product_id, Some(1), Some(2), 12.0, 0.0)],
        )
        .await;
        assert_eq!(
            batches(&db, f.product_id).await,
            [
                b(1, 0.0, 100.0),
                b(1, 8.0, 150.0),
                b(2, 10.0, 100.0),
                b(2, 2.0, 150.0)
            ]
        );
        assert_eq!(
            doc_moves(&db, m).await,
            [(-10.0, 100.0), (-2.0, 150.0), (10.0, 100.0), (2.0, 150.0)]
        );
        // Отмена: партии получателя удаляются, количество возвращается в те же партии
        cancel(&db, m).await;
        assert_eq!(
            batches(&db, f.product_id).await,
            [b(1, 10.0, 100.0), b(1, 10.0, 150.0)]
        );
        assert!(doc_moves(&db, m).await.is_empty());
    }

    #[sqlx::test]
    async fn fifo_cancel_deduct_returns(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        arrival(&db, f.product_id, 1, 10.0, 150.0).await;
        let d = doc(
            &db,
            "D",
            "",
            vec![line(f.product_id, Some(1), None, 12.0, 0.0)],
        )
        .await;
        assert_eq!(doc_moves(&db, d).await, [(-10.0, 100.0), (-2.0, 150.0)]);
        cancel(&db, d).await;
        assert_eq!(
            batches(&db, f.product_id).await,
            [b(1, 10.0, 100.0), b(1, 10.0, 150.0)]
        );
        assert!(doc_moves(&db, d).await.is_empty());
    }

    #[sqlx::test]
    async fn fifo_cancel_arrival_rebinds_shipment(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        let a1 = arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        arrival(&db, f.product_id, 1, 10.0, 150.0).await;
        let sh = order(&db, &f, 5.0).await;
        set_shipped(&db, sh, true).await;
        assert_eq!(ship_moves(&db, sh).await, [(-5.0, 100.0)]);
        cancel(&db, a1).await;
        assert_eq!(ship_moves(&db, sh).await, [(-5.0, 150.0)]);
        assert_eq!(batches(&db, f.product_id).await, [b(1, 5.0, 150.0)]);
        // Снятие отгрузки возвращает в партию прихода 2
        set_shipped(&db, sh, false).await;
        assert_eq!(batches(&db, f.product_id).await, [b(1, 10.0, 150.0)]);
    }

    /// Решение: отмена перемещения, чьи партии получателя уже израсходованы, перепривязывает
    /// расход к остальным партиям склада-получателя; источнику возвращается всё перемещённое.
    #[sqlx::test]
    async fn fifo_cancel_moving_destination_rebinds(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        arrival(&db, f.product_id, 2, 10.0, 200.0).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        let m = doc(
            &db,
            "M",
            "",
            vec![line(f.product_id, Some(1), Some(2), 4.0, 0.0)],
        )
        .await;
        arrival(&db, f.product_id, 2, 10.0, 300.0).await;
        let order = place_order_with(&db, &f, None, Some(2), 12.0).await;
        let sh: i64 = sqlx::query_scalar("SELECT min(id) FROM shipments WHERE order_id = $1")
            .bind(order)
            .fetch_one(&db)
            .await
            .unwrap();
        set_shipped(&db, sh, true).await;
        assert_eq!(ship_moves(&db, sh).await, [(-10.0, 200.0), (-2.0, 100.0)]);

        cancel(&db, m).await;
        assert_eq!(ship_moves(&db, sh).await, [(-10.0, 200.0), (-2.0, 300.0)]);
        assert_eq!(
            batches(&db, f.product_id).await,
            [b(2, 0.0, 200.0), b(1, 10.0, 100.0), b(2, 8.0, 300.0)]
        );
        assert_eq!(store_amount(&db, f.product_id, 2).await, 8.0);
    }

    #[sqlx::test]
    async fn average_merges_and_ships(db: PgPool) {
        let f = setup(&db, Some(Method::Average)).await;
        let a1 = arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        let a2 = arrival(&db, f.product_id, 1, 10.0, 150.0).await;
        assert_eq!(batches(&db, f.product_id).await, [b(1, 20.0, 125.0)]);
        assert_eq!(doc_moves(&db, a1).await, [(10.0, 100.0)]);
        assert_eq!(doc_moves(&db, a2).await, [(10.0, 150.0)]);
        let sh = order(&db, &f, 5.0).await;
        set_shipped(&db, sh, true).await;
        assert_eq!(ship_moves(&db, sh).await, [(-5.0, 125.0)]);
        assert_eq!(batches(&db, f.product_id).await, [b(1, 15.0, 125.0)]);
        set_shipped(&db, sh, false).await;
        assert_eq!(batches(&db, f.product_id).await, [b(1, 20.0, 125.0)]);
        // Отмена прихода 2 — цена обратно
        cancel(&db, a2).await;
        assert_eq!(batches(&db, f.product_id).await, [b(1, 10.0, 100.0)]);
        assert!(doc_moves(&db, a2).await.is_empty());
    }

    #[sqlx::test]
    async fn average_moving_merges(db: PgPool) {
        let f = setup(&db, Some(Method::Average)).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        arrival(&db, f.product_id, 2, 10.0, 200.0).await;
        let m = doc(
            &db,
            "M",
            "",
            vec![line(f.product_id, Some(1), Some(2), 10.0, 0.0)],
        )
        .await;
        assert_eq!(
            batches(&db, f.product_id).await,
            [b(1, 0.0, 100.0), b(2, 20.0, 150.0)]
        );
        cancel(&db, m).await;
        assert_eq!(
            batches(&db, f.product_id).await,
            [b(1, 10.0, 100.0), b(2, 10.0, 200.0)]
        );
    }

    /// Решение: отмена прихода по средней, когда партия уже израсходована, не уводит остаток
    /// партии ниже нуля; движения отгрузки остаются со своей ценой.
    #[sqlx::test]
    async fn average_cancel_keeps_shipment_history(db: PgPool) {
        let f = setup(&db, Some(Method::Average)).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        let a2 = arrival(&db, f.product_id, 1, 10.0, 150.0).await;
        let sh = order(&db, &f, 15.0).await;
        set_shipped(&db, sh, true).await;
        cancel(&db, a2).await;
        assert_eq!(store_amount(&db, f.product_id, 1).await, 0.0);
        assert_eq!(batches(&db, f.product_id).await, [b(1, 0.0, 125.0)]);
        assert_eq!(ship_moves(&db, sh).await, [(-15.0, 125.0)]);
    }

    #[sqlx::test]
    async fn average_merges_foreign_currency(db: PgPool) {
        let f = setup(&db, Some(Method::Average)).await;
        seed_usd(&db, Some(90.0)).await;
        arrival(&db, f.product_id, 1, 10.0, 100.0).await;
        let usd = doc(
            &db,
            "A",
            "USD",
            vec![line(f.product_id, None, Some(1), 10.0, 2.0)],
        )
        .await;
        assert_eq!(batches(&db, f.product_id).await, [b(1, 20.0, 140.0)]);
        // Движение — в валюте партии
        assert_eq!(doc_moves(&db, usd).await, [(10.0, 180.0)]);
        cancel(&db, usd).await;
        assert_eq!(batches(&db, f.product_id).await, [b(1, 10.0, 100.0)]);
    }

    #[sqlx::test]
    async fn shortage_leaves_uncovered(db: PgPool) {
        let f = setup(&db, Some(Method::Fifo)).await;
        // На складе 1 — 5 шт. без партий, приход ещё 5: остаток 10, партии 5
        arrival(&db, f.product_id, 1, 5.0, 100.0).await;
        assert_eq!(store_amount(&db, f.product_id, 1).await, 10.0);
        let sh = order(&db, &f, 8.0).await;
        set_shipped(&db, sh, true).await;
        assert_eq!(ship_moves(&db, sh).await, [(-5.0, 100.0)]);
        assert_eq!(batches(&db, f.product_id).await, [b(1, 0.0, 100.0)]);
        set_shipped(&db, sh, false).await;
        assert_eq!(batches(&db, f.product_id).await, [b(1, 5.0, 100.0)]);
    }
}
