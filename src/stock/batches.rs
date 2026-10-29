//! Партии товара: чистые правила списания по FIFO и пересчёта средней закупочной цены.
//!
//! Работа с БД (таблицы партий) добавляется ниже, в этом же модуле; здесь только арифметика.

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
