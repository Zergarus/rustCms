//! Заказы личного кабинета (`/profile/orders`, `OrderHistoryReader` модуля bxapi).

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};
use serde_json::{Value, json};

use crate::{files::FileRecord, sale::repo::file_ids};

/// Ключ свойства заказа: код в camelCase (`SDEK_TRACKING_URL` → `sdekTrackingUrl`),
/// без кода — `prop<ID>`.
pub(super) fn camel_key(code: &str, id: i64) -> String {
    let mut out = String::new();
    for word in code.to_lowercase().split(['_', '.']).filter(|w| !w.is_empty()) {
        if out.is_empty() {
            out.push_str(word);
        } else {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
        }
    }
    if out.is_empty() { format!("prop{id}") } else { out }
}

/// «1 товар», «4 товара», «11 товаров».
pub(super) fn items_count_label(n: i64) -> String {
    let n100 = n.abs() % 100;
    let n10 = n.abs() % 10;
    let word = if (11..=14).contains(&n100) {
        "товаров"
    } else if n10 == 1 {
        "товар"
    } else if (2..=4).contains(&n10) {
        "товара"
    } else {
        "товаров"
    };
    format!("{n} {word}")
}

/// Число товаров: `positions` — позиций, иначе округлённая сумма количеств.
pub(super) fn items_count(quantities: &[f64], positions: bool) -> i64 {
    if positions {
        quantities.len() as i64
    } else {
        quantities.iter().sum::<f64>().round() as i64
    }
}

/// Дата по формату PHP `date()`: токены `d m Y H i s`, остальное — как есть.
pub(super) fn php_date(dt: &DateTime<FixedOffset>, fmt: &str) -> String {
    let mut out = String::new();
    for c in fmt.chars() {
        let part = match c {
            'd' => "%d",
            'm' => "%m",
            'Y' => "%Y",
            'H' => "%H",
            'i' => "%M",
            's' => "%S",
            _ => {
                out.push(c);
                continue;
            }
        };
        out.push_str(&dt.format(part).to_string());
    }
    out
}

fn file_json(f: &FileRecord) -> Value {
    json!({
        "ID": f.id,
        "SRC": f.url(),
        "ORIGINAL_NAME": f.original_name,
        "FILE_NAME": f.path.rsplit('/').next().unwrap_or(&f.path),
        "CONTENT_TYPE": f.content_type,
        "FILE_SIZE": f.size,
    })
}

/// Значение файлового свойства как у Битрикса: объект файла (одиночное, пусто — `""`)
/// или массив объектов (множественное). Пропавшие файлы пропускаются.
pub(super) fn file_value(raw: &str, multiple: bool, files: &HashMap<i64, FileRecord>) -> Value {
    let mut found = file_ids(raw).into_iter().filter_map(|id| files.get(&id)).map(file_json);
    if multiple {
        Value::Array(found.collect())
    } else {
        found.next().unwrap_or_else(|| Value::from(""))
    }
}

/// Лимит максимум заказов на странице.
const MAX_LIMIT: i64 = 200;

/// `limit` и `offset` запроса: некорректный лимит — по умолчанию, больше предела — предел.
pub(super) fn page(limit: Option<&str>, offset: Option<&str>, default: i64) -> (i64, i64) {
    let limit = match limit.and_then(|l| l.trim().parse::<i64>().ok()) {
        Some(l) if l > MAX_LIMIT => MAX_LIMIT,
        Some(l) if l >= 1 => l,
        _ => default,
    };
    let offset = offset
        .and_then(|o| o.trim().parse::<i64>().ok())
        .filter(|o| *o >= 0)
        .unwrap_or(0);
    (limit, offset)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::*;

    fn file(id: i64, path: &str) -> FileRecord {
        FileRecord {
            id,
            path: path.into(),
            original_name: "Накладная.pdf".into(),
            content_type: "application/pdf".into(),
            size: 1024,
            width: None,
            height: None,
            created_at: DateTime::<Utc>::default(),
        }
    }

    fn files() -> HashMap<i64, FileRecord> {
        HashMap::from([(5, file(5, "sale/ab/x.pdf"))])
    }

    fn file_json() -> serde_json::Value {
        json!({
            "ID": 5,
            "SRC": "/upload/sale/ab/x.pdf",
            "ORIGINAL_NAME": "Накладная.pdf",
            "FILE_NAME": "x.pdf",
            "CONTENT_TYPE": "application/pdf",
            "FILE_SIZE": 1024,
        })
    }

    #[test]
    fn camel_keys() {
        assert_eq!(camel_key("SDEK_TRACKING_URL", 1), "sdekTrackingUrl");
        assert_eq!(camel_key("Store_Keeper", 1), "storeKeeper");
        assert_eq!(camel_key("phone", 1), "phone");
        assert_eq!(camel_key("WAYBILL", 1), "waybill");
        assert_eq!(camel_key("", 23), "prop23");
    }

    #[test]
    fn count_labels() {
        for (n, s) in [
            (1, "1 товар"),
            (4, "4 товара"),
            (11, "11 товаров"),
            (21, "21 товар"),
            (0, "0 товаров"),
            (112, "112 товаров"),
        ] {
            assert_eq!(items_count_label(n), s);
        }
        assert_eq!(items_count(&[2.0, 1.5], false), 4);
        assert_eq!(items_count(&[2.0, 1.5], true), 2);
    }

    #[test]
    fn date_format() {
        let dt = DateTime::parse_from_rfc3339("2026-05-01T10:00:00+03:00").unwrap();
        assert_eq!(php_date(&dt, "d.m.Y H:i:s"), "01.05.2026 10:00:00");
        assert_eq!(php_date(&dt, "Y-m-d"), "2026-05-01");
    }

    #[test]
    fn file_value_single_and_multiple() {
        assert_eq!(file_value("5", false, &files()), file_json());
        assert_eq!(file_value("", false, &files()), json!(""));
        assert_eq!(file_value("5", true, &files()), json!([file_json()]));
        assert_eq!(file_value("", true, &files()), json!([]));
    }

    #[test]
    fn file_value_skips_missing_and_garbage() {
        assert_eq!(file_value("abc,,7,5", false, &files()), file_json());
        assert_eq!(file_value("abc,,7,5", true, &files()), json!([file_json()]));
        assert_eq!(file_value("7", false, &files()), json!(""));
    }

    #[test]
    fn page_parsing() {
        assert_eq!(page(None, None, 50), (50, 0));
        assert_eq!(page(Some("20"), Some("40"), 50), (20, 40));
        assert_eq!(page(Some("0"), Some("-5"), 50), (50, 0));
        assert_eq!(page(Some("1000"), Some("x"), 50), (200, 0));
        assert_eq!(page(Some("abc"), None, 50), (50, 0));
    }
}
