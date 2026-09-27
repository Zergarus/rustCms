//! Инфоблоки — настраиваемые типы контента по мотивам Битрикса.

pub mod props;
pub mod repo;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::{FromRow, types::Json};

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Iblock {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub description: String,
    pub api_enabled: bool,
    pub sort: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct IblockSummary {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub iblock: Iblock,
    pub element_count: i64,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Property {
    pub id: i64,
    pub iblock_id: i64,
    pub code: String,
    pub name: String,
    pub kind: String,
    pub is_required: bool,
    pub sort: i32,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Element {
    pub id: i64,
    pub iblock_id: i64,
    pub code: String,
    pub name: String,
    pub active: bool,
    pub sort: i32,
    pub preview_text: String,
    pub detail_text: String,
    pub published_at: Option<DateTime<Utc>>,
    pub properties: Json<Map<String, Value>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct IblockInput {
    pub code: String,
    pub name: String,
    pub description: String,
    pub api_enabled: bool,
    pub sort: i32,
}

#[derive(Debug)]
pub struct PropertyInput {
    pub code: String,
    pub name: String,
    pub kind: String,
    pub is_required: bool,
    pub sort: i32,
}

#[derive(Debug)]
pub struct ElementInput {
    pub code: String,
    pub name: String,
    pub active: bool,
    pub sort: i32,
    pub preview_text: String,
    pub detail_text: String,
    pub published_at: Option<DateTime<Utc>>,
    pub properties: Map<String, Value>,
}

/// Символьный код: латиница в нижнем регистре, цифры, `_` и `-`.
pub fn is_valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 100
        && code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Генерация символьного кода из названия, как в Битриксе: «Новая статья» → `novaya-statya`.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    for ch in name.to_lowercase().chars() {
        let part = match ch {
            'а' => "a",
            'б' => "b",
            'в' => "v",
            'г' => "g",
            'д' => "d",
            'е' => "e",
            'ё' => "e",
            'ж' => "zh",
            'з' => "z",
            'и' => "i",
            'й' => "y",
            'к' => "k",
            'л' => "l",
            'м' => "m",
            'н' => "n",
            'о' => "o",
            'п' => "p",
            'р' => "r",
            'с' => "s",
            'т' => "t",
            'у' => "u",
            'ф' => "f",
            'х' => "h",
            'ц' => "ts",
            'ч' => "ch",
            'ш' => "sh",
            'щ' => "sch",
            'ъ' | 'ь' => "",
            'ы' => "y",
            'э' => "e",
            'ю' => "yu",
            'я' => "ya",
            c if c.is_ascii_alphanumeric() => {
                out.push(c);
                continue;
            }
            _ => "-",
        };
        out.push_str(part);
    }
    let mut slug = String::with_capacity(out.len());
    for part in out.split('-').filter(|p| !p.is_empty()) {
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(part);
    }
    slug.truncate(100);
    slug.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_cyrillic() {
        assert_eq!(slugify("Новая статья!"), "novaya-statya");
        assert_eq!(slugify("  Hello,  World 2026 "), "hello-world-2026");
        assert_eq!(slugify("Щука и ёж"), "schuka-i-ezh");
    }

    #[test]
    fn code_validation() {
        assert!(is_valid_code("news_2026-a"));
        assert!(!is_valid_code("News"));
        assert!(!is_valid_code(""));
        assert!(!is_valid_code("новости"));
    }
}
