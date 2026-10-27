//! Курсы из банков — порт `tools/get_rate.php`: ЦБ РФ, НБУ, НБРБ.

use std::{collections::HashMap, time::Duration};

use chrono::NaiveDate;
use quick_xml::{Reader, events::Event};

/// Текст ошибки Битрикса, когда курс получить нельзя.
const NO_RATE: &str = "Невозможно получить курс";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Cbr,
    Nbu,
    Nbrb,
}

/// Банк по базовой валюте.
pub fn source_for(base: &str) -> Option<Source> {
    match base {
        "RUB" | "RUR" => Some(Source::Cbr),
        "UAH" => Some(Source::Nbu),
        "BYN" | "BYR" => Some(Source::Nbrb),
        _ => None,
    }
}

/// Адрес запроса; `origin` (`http://127.0.0.1:1234`) заменяет схему и хост банка.
pub fn url(source: Source, date: NaiveDate, origin: Option<&str>) -> String {
    let (host, path) = match source {
        Source::Cbr => (
            "https://www.cbr.ru",
            format!(
                "/scripts/XML_daily.asp?date_req={}",
                date.format("%d.%m.%Y")
            ),
        ),
        Source::Nbu => (
            "https://bank.gov.ua",
            format!(
                "/NBUStatService/v1/statdirectory/exchange/?date={}",
                date.format("%Y%m%d")
            ),
        ),
        Source::Nbrb => (
            "https://www.nbrb.by",
            format!(
                "/Services/XmlExRates.aspx?ondate={}",
                date.format("%Y-%m-%d")
            ),
        ),
    };
    let host = origin.map_or(host, |o| o.trim_end_matches('/'));
    format!("{host}{path}")
}

/// Кодировка из XML-пролога, по умолчанию windows-1251.
fn decode(body: &[u8]) -> String {
    let head = String::from_utf8_lossy(&body[..body.len().min(200)]).to_lowercase();
    let label = head
        .find("encoding=")
        .and_then(|i| {
            let rest = &head[i + 9..];
            let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            rest[1..].split(quote).next().map(str::to_string)
        })
        .unwrap_or_default();
    let encoding =
        encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::WINDOWS_1251);
    encoding.decode(body).0.into_owned()
}

/// Разбор ответа банка: код → (`rate_cnt`, `rate`).
pub fn parse(source: Source, body: &[u8]) -> Result<HashMap<String, (i32, f64)>, String> {
    // Элемент валюты и имена полей кода, номинала и курса
    let (item, code_tag, cnt_tag, rate_tag) = match source {
        Source::Cbr => ("Valute", "CharCode", Some("Nominal"), "Value"),
        Source::Nbu => ("currency", "cc", None, "rate"),
        Source::Nbrb => ("Currency", "CharCode", Some("Scale"), "Rate"),
    };
    let text = decode(body);
    let mut reader = Reader::from_str(&text);
    let mut out = HashMap::new();
    let mut in_item = false;
    let mut field = String::new();
    let mut fields: HashMap<String, String> = HashMap::new();
    loop {
        match reader.read_event().map_err(|_| NO_RATE)? {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == item {
                    in_item = true;
                    fields.clear();
                } else if in_item {
                    field = name;
                }
            }
            Event::Text(t) if in_item && !field.is_empty() => {
                let value = t.unescape().map_err(|_| NO_RATE)?;
                fields.insert(field.clone(), value.trim().to_string());
            }
            Event::End(e) => {
                if e.local_name().as_ref() == item.as_bytes() {
                    in_item = false;
                    let rate = fields
                        .get(rate_tag)
                        .and_then(|v| v.replace(',', ".").parse::<f64>().ok());
                    let cnt = match cnt_tag {
                        Some(tag) => fields.get(tag).and_then(|v| v.parse::<i32>().ok()),
                        None => Some(1),
                    };
                    if let (Some(code), Some(rate), Some(cnt)) = (fields.get(code_tag), rate, cnt)
                        && !code.is_empty()
                    {
                        out.insert(code.clone(), (cnt, rate));
                    }
                }
                field.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if out.is_empty() {
        return Err(NO_RATE.to_string());
    }
    Ok(out)
}

/// Загружает курсы банка базовой валюты `base` на дату.
pub async fn fetch(
    client: &reqwest::Client,
    origin: Option<&str>,
    base: &str,
    date: NaiveDate,
) -> Result<HashMap<String, (i32, f64)>, String> {
    let source = source_for(base).ok_or(NO_RATE)?;
    let response = client
        .get(url(source, date, origin))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| NO_RATE)?;
    if !response.status().is_success() {
        return Err(NO_RATE.to_string());
    }
    let body = response.bytes().await.map_err(|_| NO_RATE)?;
    parse(source, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 27).unwrap()
    }

    #[test]
    fn parse_cbr() {
        let rates = parse(
            Source::Cbr,
            include_bytes!("../../tests/fixtures/currency/cbr.xml"),
        )
        .unwrap();
        assert_eq!(rates["USD"], (1, 81.5));
        assert_eq!(rates["UAH"], (10, 19.6543));
        assert_eq!(rates.len(), 2);
    }

    #[test]
    fn parse_nbu() {
        let rates = parse(
            Source::Nbu,
            include_bytes!("../../tests/fixtures/currency/nbu.xml"),
        )
        .unwrap();
        assert_eq!(rates["USD"], (1, 41.5));
        assert_eq!(rates["EUR"], (1, 48.25));
    }

    #[test]
    fn parse_nbrb() {
        let rates = parse(
            Source::Nbrb,
            include_bytes!("../../tests/fixtures/currency/nbrb.xml"),
        )
        .unwrap();
        assert_eq!(rates["RUB"], (100, 3.9871));
        assert_eq!(rates["USD"], (1, 3.2512));
    }

    #[test]
    fn parse_garbage() {
        for body in [&b"<html>502</html>"[..], b"not xml", b""] {
            assert_eq!(
                parse(Source::Cbr, body),
                Err("Невозможно получить курс".to_string())
            );
        }
    }

    #[test]
    fn url_by_source() {
        assert_eq!(
            url(Source::Cbr, d(), None),
            "https://www.cbr.ru/scripts/XML_daily.asp?date_req=27.10.2026"
        );
        assert_eq!(
            url(Source::Nbu, d(), None),
            "https://bank.gov.ua/NBUStatService/v1/statdirectory/exchange/?date=20261027"
        );
        assert_eq!(
            url(Source::Nbrb, d(), None),
            "https://www.nbrb.by/Services/XmlExRates.aspx?ondate=2026-10-27"
        );
        assert_eq!(
            url(Source::Cbr, d(), Some("http://127.0.0.1:9")),
            "http://127.0.0.1:9/scripts/XML_daily.asp?date_req=27.10.2026"
        );
    }

    #[test]
    fn source_for_codes() {
        assert_eq!(source_for("RUR"), Some(Source::Cbr));
        assert_eq!(source_for("UAH"), Some(Source::Nbu));
        assert_eq!(source_for("BYN"), Some(Source::Nbrb));
        assert_eq!(source_for("KZT"), None);
    }
}
