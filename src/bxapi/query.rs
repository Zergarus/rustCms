//! Разбор тела запросов списка: `select`, `filter`, `order`, `limit`, `offset`, `imageResize`.

use serde_json::{Map, Value};

use super::{BxError, project::Project};

/// Поле `select`, сгруппированное по корню: `relatedBrands.item.ufXmlId` и
/// `relatedBrands.item.ufName` → корень `relatedBrands`,
/// хвосты `[item, ufXmlId]`, `[item, ufName]`. Корень — ключ в ответе.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectField {
    pub root: String,
    /// Пустой список хвостов — «сырое» поле без точки.
    pub rests: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct Select {
    pub fields: Vec<SelectField>,
}

impl Select {
    /// `aliases` — алиасы инфоблока: `recomend` → `recomend.item.xmlId`.
    pub fn parse(raw: Option<&Value>, project: &Project, iblock: &str) -> Select {
        let mut select = Select::default();
        let items = raw.and_then(Value::as_array).cloned().unwrap_or_default();
        for item in items {
            if let Some(path) = item.as_str() {
                select.add(path, project, iblock);
            }
        }
        select
    }

    pub fn add(&mut self, path: &str, project: &Project, iblock: &str) {
        let path = path.trim();
        if path.is_empty() || path == "*" {
            return;
        }
        let mut segments: Vec<String> = path.split('.').map(str::to_string).collect();
        let root = segments.remove(0);
        if segments.is_empty()
            && let Some(alias) = project.alias(iblock, &root)
        {
            segments = alias.path.split('.').skip(1).map(str::to_string).collect();
        }
        match self.fields.iter_mut().find(|f| f.root == root) {
            Some(field) => {
                if !segments.is_empty() && !field.rests.contains(&segments) {
                    field.rests.push(segments);
                }
            }
            None => self.fields.push(SelectField {
                root,
                rests: if segments.is_empty() {
                    Vec::new()
                } else {
                    vec![segments]
                },
            }),
        }
    }

    pub fn has(&self, root: &str) -> bool {
        self.fields.iter().any(|f| f.root == root)
    }
}

#[derive(Debug, Clone)]
pub struct Order {
    pub field: String,
    pub desc: bool,
}

#[derive(Debug, Default)]
pub struct ListRequest {
    pub select: Select,
    pub filter: Map<String, Value>,
    pub order: Vec<Order>,
    pub limit: Option<i64>,
    pub offset: i64,
    pub image_resize: Option<Vec<u32>>,
}

/// Верхняя граница выборки без `limit`: Битрикс отдаёт всё, но совсем без предела
/// одна ошибка на фронте положит сервер.
pub const MAX_LIMIT: i64 = 20_000;

impl ListRequest {
    pub fn parse(
        body: &Map<String, Value>,
        project: &Project,
        iblock: &str,
    ) -> Result<Self, BxError> {
        let filter = match body.get("filter") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(Value::Array(a)) if a.is_empty() => Map::new(),
            Some(_) => return Err(BxError::new("invalid_filter", "filter must be an object")),
        };
        let mut order = Vec::new();
        if let Some(Value::Object(map)) = body.get("order") {
            for (field, dir) in map {
                let desc = dir.as_str().is_some_and(|d| d.eq_ignore_ascii_case("desc"));
                order.push(Order {
                    field: field.clone(),
                    desc,
                });
            }
        }
        let limit = number(body.get("limit")).filter(|n| *n > 0);
        Ok(ListRequest {
            select: Select::parse(body.get("select"), project, iblock),
            filter,
            order,
            limit: Some(limit.unwrap_or(MAX_LIMIT).min(MAX_LIMIT)),
            offset: number(body.get("offset")).unwrap_or(0).max(0),
            image_resize: image_resize(body.get("imageResize")),
        })
    }
}

/// Число из JSON: число или строка с числом.
pub fn number(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub fn image_resize(v: Option<&Value>) -> Option<Vec<u32>> {
    let widths: Vec<u32> = v?
        .as_array()?
        .iter()
        .filter_map(|w| number(Some(w)))
        .filter(|w| *w > 0 && *w <= 4000)
        .map(|w| w as u32)
        .collect();
    (!widths.is_empty()).then_some(widths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn select_groups_roots_and_aliases() {
        let project = Project::default();
        let s = Select::parse(
            Some(&json!([
                "id",
                "relatedBrands.item.ufXmlId",
                "relatedBrands.item.ufName",
                "recomend",
                "id"
            ])),
            &project,
            "catalog",
        );
        assert_eq!(s.fields.len(), 3);
        assert_eq!(s.fields[1].rests.len(), 2);
        assert_eq!(s.fields[2].root, "recomend");
        assert_eq!(
            s.fields[2].rests,
            vec![vec!["item".to_string(), "xmlId".to_string()]]
        );
    }

    #[test]
    fn list_request_defaults() {
        let body = json!({"limit": "5", "offset": -3, "imageResize": [120, "320", 0], "order": {"name": "desc"}});
        let r = ListRequest::parse(body.as_object().unwrap(), &Project::default(), "x").unwrap();
        assert_eq!(r.limit, Some(5));
        assert_eq!(r.offset, 0);
        assert_eq!(r.image_resize, Some(vec![120, 320]));
        assert!(r.order[0].desc);
        let empty = ListRequest::parse(&Map::new(), &Project::default(), "x").unwrap();
        assert_eq!(empty.limit, Some(MAX_LIMIT));
    }
}
