//! Блоки контента — значение поля `blocks`.
//!
//! Формат: массив объектов `{ "type": "text", "text": "…" }`; ключ содержимого
//! называется по типу блока. Значение проверяется, нормализуется и очищается от
//! опасной разметки при сохранении.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

/// Сколько блоков допускается в одном поле.
pub const MAX_BLOCKS: usize = 200;

/// Разбирает значение поля из формы: пустая строка — пустое поле, иначе JSON-массив.
pub fn from_form(raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Value::Null);
    }
    let value: Value =
        serde_json::from_str(raw).map_err(|_| "Блоки: ожидается JSON-массив".to_string())?;
    if !value.is_array() {
        return Err("Блоки: ожидается JSON-массив".to_string());
    }
    Ok(Value::Array(normalize(&value)?))
}

/// Проверяет, нормализует и очищает блоки; пустые отбрасывает.
pub fn normalize(value: &Value) -> Result<Vec<Value>, String> {
    let items = value
        .as_array()
        .ok_or_else(|| "Блоки: ожидается JSON-массив".to_string())?;
    if items.len() > MAX_BLOCKS {
        return Err(format!("Блоки: не больше {MAX_BLOCKS} блоков"));
    }
    let mut out = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if let Some(block) = normalize_block(item, index + 1)? {
            out.push(block);
        }
    }
    Ok(out)
}

fn normalize_block(item: &Value, number: usize) -> Result<Option<Value>, String> {
    let obj = item
        .as_object()
        .ok_or_else(|| format!("Блок {number}: ожидается объект"))?;
    let kind = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Блок {number}: не указан type"))?;
    let err = |message: &str| format!("Блок {number}: {message}");
    match kind {
        "heading" => {
            allowed_keys(obj, &["type", "heading", "level"], number)?;
            let heading = string_field(obj, "heading", number)?.trim().to_string();
            if heading.is_empty() {
                return Ok(None);
            }
            let level = match obj.get("level") {
                None => 2,
                Some(Value::Number(n)) => n
                    .as_u64()
                    .filter(|level| (2..=4).contains(level))
                    .ok_or_else(|| err("level — 2, 3 или 4"))?,
                Some(_) => return Err(err("level — 2, 3 или 4")),
            };
            Ok(Some(
                json!({ "type": "heading", "heading": heading, "level": level }),
            ))
        }
        "text" => {
            allowed_keys(obj, &["type", "text"], number)?;
            let text = string_field(obj, "text", number)?.trim().to_string();
            if text.is_empty() {
                return Ok(None);
            }
            Ok(Some(json!({ "type": "text", "text": text })))
        }
        "image" => {
            allowed_keys(obj, &["type", "image", "alt", "caption", "link"], number)?;
            let image = string_field(obj, "image", number)?.trim().to_string();
            if image.is_empty() {
                return Ok(None);
            }
            if !image_url(&image) {
                return Err(err("image: нужен /upload/… или http(s)://"));
            }
            let mut block = json!({ "type": "image", "image": image });
            for key in ["alt", "caption"] {
                copy_optional(obj, key, &mut block, number)?;
            }
            if let Some(link) = optional_string(obj, "link", number)? {
                if !link.is_empty() {
                    if !link_url(&link) {
                        return Err(err("link: только http(s)://, /… или #…"));
                    }
                    block["link"] = json!(link);
                }
            }
            Ok(Some(block))
        }
        "gallery" => {
            allowed_keys(obj, &["type", "images"], number)?;
            let images = obj
                .get("images")
                .and_then(Value::as_array)
                .ok_or_else(|| err("images: ожидается массив"))?;
            let mut out = Vec::new();
            for (index, image) in images.iter().enumerate() {
                let number = number;
                let image = image.as_object().ok_or_else(|| {
                    format!("Блок {number}: картинка {} — ожидается объект", index + 1)
                })?;
                allowed_keys(image, &["image", "alt", "caption"], number)?;
                let src = image
                    .get("image")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if src.is_empty() {
                    continue;
                }
                if !image_url(src) {
                    return Err(err("images: нужен /upload/… или http(s)://"));
                }
                let mut item = json!({ "image": src });
                for key in ["alt", "caption"] {
                    copy_optional(image, key, &mut item, number)?;
                }
                out.push(item);
            }
            if out.is_empty() {
                return Ok(None);
            }
            Ok(Some(json!({ "type": "gallery", "images": out })))
        }
        "quote" => {
            allowed_keys(obj, &["type", "quote", "author"], number)?;
            let quote = string_field(obj, "quote", number)?.trim().to_string();
            if quote.is_empty() {
                return Ok(None);
            }
            let mut block = json!({ "type": "quote", "quote": quote });
            copy_optional(obj, "author", &mut block, number)?;
            Ok(Some(block))
        }
        "table" => {
            allowed_keys(obj, &["type", "rows"], number)?;
            let rows = obj
                .get("rows")
                .and_then(Value::as_array)
                .ok_or_else(|| err("rows: ожидается массив строк"))?;
            let mut out: Vec<Vec<String>> = Vec::new();
            for row in rows {
                let cells = row
                    .as_array()
                    .ok_or_else(|| err("rows: строка должна быть массивом"))?;
                let mut out_row = Vec::new();
                for cell in cells {
                    let text = cell
                        .as_str()
                        .ok_or_else(|| err("rows: ячейка должна быть строкой"))?;
                    out_row.push(text.trim().to_string());
                }
                if out_row.iter().any(|cell| !cell.is_empty()) {
                    out.push(out_row);
                }
            }
            if out.is_empty() {
                return Ok(None);
            }
            Ok(Some(json!({ "type": "table", "rows": out })))
        }
        "divider" => {
            allowed_keys(obj, &["type"], number)?;
            Ok(Some(json!({ "type": "divider" })))
        }
        "html" => {
            allowed_keys(obj, &["type", "html"], number)?;
            let html = clean_html(&string_field(obj, "html", number)?);
            if html.trim().is_empty() {
                return Ok(None);
            }
            Ok(Some(json!({ "type": "html", "html": html })))
        }
        other => Err(err(&format!("неизвестный тип «{other}»"))),
    }
}

fn allowed_keys(obj: &Map<String, Value>, allowed: &[&str], number: usize) -> Result<(), String> {
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("Блок {number}: неизвестное поле «{key}»"));
        }
    }
    Ok(())
}

fn string_field(obj: &Map<String, Value>, key: &str, number: usize) -> Result<String, String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("Блок {number}: поле «{key}» — ожидается строка"))
}

fn optional_string(
    obj: &Map<String, Value>,
    key: &str,
    number: usize,
) -> Result<Option<String>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.trim().to_string())),
        Some(_) => Err(format!("Блок {number}: поле «{key}» — ожидается строка")),
    }
}

fn copy_optional(
    from: &Map<String, Value>,
    key: &str,
    to: &mut Value,
    number: usize,
) -> Result<(), String> {
    if let Some(value) = optional_string(from, key, number)? {
        if !value.is_empty() {
            to[key] = json!(value);
        }
    }
    Ok(())
}

fn link_url(value: &str) -> bool {
    (value.starts_with('/') && !value.starts_with("//"))
        || value.starts_with('#')
        || value.starts_with("http://")
        || value.starts_with("https://")
}

fn image_url(value: &str) -> bool {
    value.starts_with("/upload/") || value.starts_with("http://") || value.starts_with("https://")
}

/// Разрешённая разметка HTML-блока: без скриптов, iframe и обработчиков.
fn clean_html(input: &str) -> String {
    let tags: HashSet<&str> = [
        "p", "br", "strong", "b", "em", "i", "u", "s", "a", "ul", "ol", "li", "blockquote", "table",
        "thead", "tbody", "tr", "td", "th", "h2", "h3", "h4", "img", "div", "span", "figure",
        "figcaption",
    ]
    .into_iter()
    .collect();
    let mut attributes: HashMap<&str, HashSet<&str>> = HashMap::new();
    attributes.insert("a", ["href", "title"].into_iter().collect());
    attributes.insert("img", ["src", "alt", "title"].into_iter().collect());
    for tag in ["div", "span", "figure", "figcaption"] {
        attributes.insert(tag, ["class"].into_iter().collect());
    }
    let mut builder = ammonia::Builder::default();
    builder
        .tags(tags)
        .tag_attributes(attributes)
        .url_schemes(["http", "https"].into_iter().collect())
        .link_rel(Some("noopener noreferrer"));
    builder.clean(input).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn norm(value: Value) -> Result<Vec<Value>, String> {
        normalize(&value)
    }

    #[test]
    fn heading_gets_default_level() {
        let out = norm(json!([{ "type": "heading", "heading": "Привет" }])).unwrap();
        assert_eq!(out, vec![json!({ "type": "heading", "heading": "Привет", "level": 2 })]);
    }

    #[test]
    fn text_keeps_paragraphs() {
        let out = norm(json!([{ "type": "text", "text": "Первая\n\nВторая" }])).unwrap();
        assert_eq!(out[0]["text"], "Первая\n\nВторая");
    }

    #[test]
    fn unknown_type_is_error() {
        let err = norm(json!([{ "type": "video", "video": "x" }])).unwrap_err();
        assert!(err.contains("видео") || err.contains("video"), "{err}");
    }

    #[test]
    fn extra_key_is_error() {
        let err = norm(json!([{ "type": "text", "text": "a", "b": 1 }])).unwrap_err();
        assert!(err.contains("b"), "{err}");
    }

    #[test]
    fn javascript_image_is_error() {
        let err = norm(json!([{ "type": "image", "image": "javascript:alert(1)" }])).unwrap_err();
        assert!(err.contains("ссылк") || err.contains("image"), "{err}");
    }

    #[test]
    fn html_is_cleaned() {
        let out = norm(json!([{
            "type": "html",
            "html": "<script>alert(1)</script><p onclick=\"x\">ok</p><div class=\"note\">n</div>"
        }]))
        .unwrap();
        assert_eq!(out[0]["html"], "<p>ok</p><div class=\"note\">n</div>");
    }

    #[test]
    fn empty_blocks_are_dropped() {
        let out = norm(json!([
            { "type": "text", "text": "   " },
            { "type": "heading", "heading": "" },
            { "type": "gallery", "images": [] },
            { "type": "table", "rows": [] },
            { "type": "divider" }
        ]))
        .unwrap();
        assert_eq!(out, vec![json!({ "type": "divider" })]);
    }

    #[test]
    fn from_form_empty_is_null() {
        assert_eq!(from_form("  ").unwrap(), Value::Null);
    }

    #[test]
    fn from_form_broken_json_is_error() {
        assert!(from_form("{oops").is_err());
    }

    #[test]
    fn image_blocks_are_validated() {
        let out = norm(json!([{
            "type": "image",
            "image": "/upload/blocks/a.jpg",
            "alt": "",
            "link": "https://example.ru"
        }]))
        .unwrap();
        assert_eq!(out[0]["image"], "/upload/blocks/a.jpg");
        assert!(out[0].get("alt").is_none());
    }
}
