//! Картинки в ответах: без `imageResize` — строка-путь, с ним — `{origin, resized}`.
//! Уменьшенные копии лежат по `/upload/resize_cache/<ширина>/<путь оригинала>` и
//! создаются при первом обращении (см. [`crate::uploads`]).

use serde_json::{Value, json};

use crate::files::FileRecord;

pub fn url(file: &FileRecord) -> String {
    format!("/upload/{}", file.path)
}

/// Ширина из белого списка проекта: ближайшая не меньше запрошенной, иначе максимальная.
pub fn fit_width(requested: u32, allowed: &[u32]) -> u32 {
    if allowed.is_empty() {
        return requested;
    }
    allowed
        .iter()
        .copied()
        .filter(|w| *w >= requested)
        .min()
        .unwrap_or_else(|| allowed.iter().copied().max().unwrap_or(requested))
}

pub fn resized_url(file: &FileRecord, width: u32) -> String {
    format!("/upload/resize_cache/{width}/{}", file.path)
}

/// Значение картинки; `resize` — запрошенные ширины (в ответе остаются запрошенными,
/// путь ведёт к подогнанной под белый список).
pub fn image_value(file: Option<&FileRecord>, resize: Option<&[u32]>, allowed: &[u32]) -> Value {
    let Some(file) = file else {
        return Value::Null;
    };
    match resize {
        None => Value::String(url(file)),
        Some(widths) => {
            let resized: Vec<Value> = widths
                .iter()
                .map(|w| json!({ "width": w, "path": resized_url(file, fit_width(*w, allowed)) }))
                .collect();
            json!({ "origin": url(file), "resized": resized })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths() {
        let allowed = [120, 320, 800];
        assert_eq!(fit_width(120, &allowed), 120);
        assert_eq!(fit_width(300, &allowed), 320);
        assert_eq!(fit_width(1200, &allowed), 800);
        assert_eq!(fit_width(500, &[]), 500);
    }
}
