//! Коллекции — настраиваемые типы контента (аналог коллекций Битрикса).

pub mod fields;
pub mod repo;
pub mod sku;

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::{FromRow, types::Json};

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Collection {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub description: String,
    pub api_enabled: bool,
    pub sort: i32,
    /// Шаблоны URL как в Битриксе: `#SITE_DIR#/catalog/#SECTION_CODE_PATH#/#ELEMENT_ID#/`.
    pub detail_page_url: String,
    pub section_page_url: String,
    pub list_page_url: String,
    /// Торговый каталог: у записей есть цены и остатки.
    pub is_catalog: bool,
    /// Для коллекции предложений: коллекция товаров, чьи предложения она хранит.
    pub product_collection_id: Option<i64>,
    /// Системное поле связи предложения с товаром (`CML2_LINK`).
    pub sku_field_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct CollectionSummary {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub collection: Collection,
    pub item_count: i64,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Field {
    pub id: i64,
    pub collection_id: i64,
    pub code: String,
    pub name: String,
    pub kind: String,
    pub is_required: bool,
    pub sort: i32,
    pub multiple: bool,
    /// Для привязки к записи: коллекция, из которой выбираются записи.
    pub link_collection_id: Option<i64>,
    /// `directory` — привязка по внешнему коду записи (бывший справочник HL-блока).
    pub user_type: String,
    /// Показывать значение в позиции корзины.
    pub in_basket: bool,
    /// Поле выбора предложения.
    pub offer_tree: bool,
}

/// Вариант значения поля-списка.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct FieldOption {
    pub id: i64,
    pub field_id: i64,
    pub value: String,
    pub xml_id: String,
    pub sort: i32,
    pub is_default: bool,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Section {
    pub id: i64,
    pub collection_id: i64,
    pub parent_id: Option<i64>,
    pub code: String,
    pub xml_id: String,
    pub name: String,
    pub active: bool,
    pub sort: i32,
    pub depth_level: i32,
    pub description: String,
    pub picture_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct SectionInput {
    pub parent_id: Option<i64>,
    pub code: String,
    pub xml_id: String,
    pub name: String,
    pub active: bool,
    pub sort: i32,
    pub description: String,
    pub picture_id: Option<i64>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Item {
    pub id: i64,
    pub collection_id: i64,
    pub section_id: Option<i64>,
    pub code: String,
    pub xml_id: String,
    pub name: String,
    pub active: bool,
    pub sort: i32,
    pub preview_text: String,
    pub detail_text: String,
    pub preview_picture_id: Option<i64>,
    pub detail_picture_id: Option<i64>,
    pub published_at: Option<DateTime<Utc>>,
    pub field_values: Json<Map<String, Value>>,
    /// Для предложения: родительский товар (не путать с `product_id` позиции корзины).
    pub product_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct CollectionInput {
    pub code: String,
    pub name: String,
    pub description: String,
    pub api_enabled: bool,
    pub sort: i32,
    /// Торговый каталог: у записей цены, остатки, покупка.
    pub is_catalog: bool,
}

#[derive(Debug)]
pub struct FieldInput {
    pub code: String,
    pub name: String,
    pub kind: String,
    pub is_required: bool,
    pub sort: i32,
    pub multiple: bool,
    pub link_collection_id: Option<i64>,
}

#[derive(Debug)]
pub struct ItemInput {
    pub section_id: Option<i64>,
    pub code: String,
    pub xml_id: String,
    pub name: String,
    pub active: bool,
    pub sort: i32,
    pub preview_text: String,
    pub detail_text: String,
    pub preview_picture_id: Option<i64>,
    pub detail_picture_id: Option<i64>,
    pub published_at: Option<DateTime<Utc>>,
    pub field_values: Map<String, Value>,
    /// Для предложения: родительский товар (не путать с `product_id` позиции корзины).
    pub product_id: Option<i64>,
}

/// Упорядочивает разделы деревом: родитель, затем его потомки (для списков и
/// выпадающих меню, отступ — по `depth_level`). Порядок братьев сохраняется из входа.
pub fn section_tree(sections: Vec<Section>) -> Vec<Section> {
    let mut children: HashMap<Option<i64>, Vec<Section>> = HashMap::new();
    let known: HashSet<i64> = sections.iter().map(|s| s.id).collect();
    for s in sections {
        // Родитель из другого коллекции или удалённый — показываем как корень
        let parent = s.parent_id.filter(|p| known.contains(p));
        children.entry(parent).or_default().push(s);
    }
    let mut out = Vec::with_capacity(known.len());
    let mut stack: Vec<Section> = children.remove(&None).unwrap_or_default();
    stack.reverse();
    while let Some(s) = stack.pop() {
        if let Some(mut kids) = children.remove(&Some(s.id)) {
            kids.reverse();
            stack.extend(kids);
        }
        out.push(s);
    }
    out
}

/// Id раздела и всех его потомков — куда нельзя переносить раздел.
pub fn section_subtree_ids(sections: &[Section], root: i64) -> HashSet<i64> {
    let mut ids = HashSet::from([root]);
    loop {
        let before = ids.len();
        for s in sections {
            if s.parent_id.is_some_and(|p| ids.contains(&p)) {
                ids.insert(s.id);
            }
        }
        if ids.len() == before {
            return ids;
        }
    }
}

/// Символьный код: латиница в нижнем регистре, цифры, `_` и `-`.
pub fn is_valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 100
        && code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Символьный код записи или раздела: как у коллекции, но допускает
/// заглавные буквы (в данных из Битрикса встречаются коды вида `01M`).
pub fn is_valid_slug(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 255
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
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
        assert!(is_valid_slug("01M"));
        assert!(!is_valid_slug("a b"));
    }

    fn section(id: i64, parent_id: Option<i64>) -> Section {
        Section {
            id,
            collection_id: 1,
            parent_id,
            code: String::new(),
            xml_id: String::new(),
            name: id.to_string(),
            active: true,
            sort: 500,
            depth_level: 1,
            description: String::new(),
            picture_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn tree_order_and_subtree() {
        let sections = vec![
            section(3, Some(1)),
            section(1, None),
            section(2, None),
            section(4, Some(3)),
            section(5, Some(1)),
        ];
        let order: Vec<i64> = section_tree(sections.clone())
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(order, [1, 3, 4, 5, 2]);
        assert_eq!(
            section_subtree_ids(&sections, 1),
            HashSet::from([1, 3, 4, 5])
        );
    }
}
