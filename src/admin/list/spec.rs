//! Описание списка: колонки, фильтры, источник данных.

use super::{bulk::BulkAction, state::Sort};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Text,
    Number,
    Money,
    Date,
    Bool,
    Enum,
    Link,
    File,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumOption {
    pub value: String,
    pub label: String,
}

/// Редактор значения в ячейке.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    Text,
    Number,
    Bool,
    Date,
    List,
}

/// Стандартная колонка записи, доступная для правки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdField {
    Name,
    Code,
    Sort,
    Active,
}

/// Что именно правит ячейка: стандартную колонку или поле коллекции (`id` поля).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTarget {
    Standard(StdField),
    Field(i64),
}

#[derive(Debug, Clone)]
pub struct CellEdit {
    /// Куда писать значение; читается обработчиком сохранения ячейки.
    #[allow(dead_code)]
    pub target: EditTarget,
    pub kind: EditKind,
    pub required: bool,
    /// Варианты поля-списка; у остальных видов пусто.
    pub options: Vec<EnumOption>,
}

/// `" LEFT JOIN … "` целиком.
#[derive(Debug, Clone)]
pub struct Join {
    pub code: String,
    pub sql: String,
}

#[derive(Debug, Clone)]
pub enum FilterSql {
    /// SQL-выражение значения.
    Expr(String),
    /// `field_values` по коду, enum/link.
    JsonField { key: String, multiple: bool },
}

#[derive(Debug, Clone)]
pub struct Column {
    pub code: String,
    pub title: String,
    pub expr: String,
    pub ty: FieldType,
    pub sortable: bool,
    pub default: bool,
    pub required: bool,
    pub join: Option<String>,
    pub currency_expr: Option<String>,
    /// Дескриптор редактора ячейки; `None` — колонка не редактируется.
    pub editable: Option<CellEdit>,
    /// SQL-выражение сырого значения для редактора (не подпись из `expr`).
    pub edit_expr: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Filter {
    pub code: String,
    pub label: String,
    pub group: String,
    pub ty: FieldType,
    pub sql: FilterSql,
    pub options: Vec<EnumOption>,
    pub join: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ListSpec {
    pub key: String,
    pub id_expr: String,
    /// `"FROM … WHERE <условие>"` без пользовательских строк.
    pub from: String,
    pub joins: Vec<Join>,
    pub columns: Vec<Column>,
    pub filters: Vec<Filter>,
    pub default_sort: Sort,
    pub tie_break: String,
    pub sections: bool,
    /// Шаблон с `{id}`.
    pub peek_url: Option<String>,
    /// Шаблон с `{id}`.
    pub open_url: String,
    pub can_share: bool,
    /// Булево SQL-выражение: строки с истиной выводятся приглушёнными.
    pub muted_expr: Option<String>,
    /// SQL-выражение подписи записи для итога массовых действий (номер заказа, название).
    pub label_expr: String,
    /// SQL-выражение `updated_at` для защиты от параллельной правки; `None` — список не редактируется.
    pub version_expr: Option<String>,
    /// Массовые действия, разрешённые пользователю; пусто — нет чекбоксов и панели.
    pub bulk_actions: Vec<BulkAction>,
}

impl ListSpec {
    pub fn column(&self, code: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.code == code)
    }

    pub fn filter(&self, code: &str) -> Option<&Filter> {
        self.filters.iter().find(|f| f.code == code)
    }

    pub fn default_cols(&self) -> Vec<String> {
        self.columns
            .iter()
            .filter(|c| c.default || c.required)
            .map(|c| c.code.clone())
            .collect()
    }
}

#[cfg(test)]
pub(crate) fn test_spec() -> ListSpec {
    fn filter(code: &str, ty: FieldType, options: &[&str]) -> Filter {
        Filter {
            code: code.into(),
            label: code.into(),
            group: "Основное".into(),
            ty,
            sql: FilterSql::Expr(format!("t.{code}")),
            options: options
                .iter()
                .map(|v| EnumOption {
                    value: (*v).into(),
                    label: (*v).into(),
                })
                .collect(),
            join: None,
        }
    }
    fn col(code: &str, ty: FieldType, required: bool) -> Column {
        Column {
            code: code.into(),
            title: code.into(),
            expr: format!("t.{code}"),
            ty,
            sortable: true,
            default: true,
            required,
            join: None,
            currency_expr: None,
            editable: None,
            edit_expr: None,
        }
    }
    let mut columns = vec![
        col("id", FieldType::Number, false),
        col("name", FieldType::Text, true),
        col("price", FieldType::Money, false),
        col("updated", FieldType::Date, false),
    ];
    columns[2].expr = "p1.value".into();
    columns[2].join = Some("price_1".into());
    let mut filters = vec![
        filter("name", FieldType::Text, &[]),
        filter("price", FieldType::Money, &[]),
        filter("qty", FieldType::Number, &[]),
        filter("updated", FieldType::Date, &[]),
        filter("status", FieldType::Enum, &["N", "P", "F"]),
        filter("paid", FieldType::Bool, &[]),
        filter("link", FieldType::Link, &[]),
    ];
    filters[1].sql = FilterSql::Expr("p1.value".into());
    filters[1].join = Some("price_1".into());
    ListSpec {
        key: "test".into(),
        id_expr: "t.id".into(),
        from: "FROM t WHERE true".into(),
        joins: vec![Join {
            code: "price_1".into(),
            sql: " LEFT JOIN prices p1 ON p1.item_id = t.id AND p1.type_id = 1".into(),
        }],
        columns,
        filters,
        default_sort: Sort {
            code: "id".into(),
            desc: true,
        },
        tie_break: "t.id DESC".into(),
        sections: false,
        peek_url: None,
        open_url: "/admin/test/{id}".into(),
        can_share: false,
        muted_expr: None,
        label_expr: "t.id::text".into(),
        version_expr: None,
        bulk_actions: Vec::new(),
    }
}
