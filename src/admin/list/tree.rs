//! Дерево разделов коллекции для левой колонки списка записей.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;

use super::{
    spec::ListSpec,
    state::{ListState, SectionSel, to_query},
};
use crate::collection::Section;

/// Узел дерева: готовые адреса, число записей вместе с подразделами.
#[derive(Debug, Serialize)]
pub struct TreeNode {
    pub id: i64,
    pub name: String,
    pub count: i64,
    pub active: bool,
    pub selected: bool,
    /// Дети отрисованы сервером и показаны; иначе подгружаются по клику.
    pub open: bool,
    pub has_kids: bool,
    pub url: String,
    pub load_url: String,
    pub edit_url: String,
    pub add_sub_url: String,
    pub add_item_url: String,
    pub kids: Vec<TreeNode>,
}

/// Построитель дерева: общие данные коллекции и состояния списка.
pub struct TreeBuilder<'a> {
    pub collection_id: i64,
    pub spec: &'a ListSpec,
    /// Состояние списка (фильтры, сортировка); раздел в нём подменяется на адресах узлов.
    pub state: &'a ListState,
    pub sections: &'a [Section],
    pub counts: HashMap<i64, i64>,
    pub path: String,
    /// Выбранный раздел и его предки: ветка до него раскрыта.
    pub current: Option<i64>,
    pub ancestors: HashSet<i64>,
}

/// Свои записи каждого раздела (`None` — без раздела) одним запросом.
pub async fn own_counts(
    db: &PgPool,
    collection_id: i64,
) -> sqlx::Result<HashMap<Option<i64>, i64>> {
    let rows: Vec<(Option<i64>, i64)> = sqlx::query_as(
        "SELECT section_id, count(*) FROM collection_items WHERE collection_id = $1 GROUP BY section_id",
    )
    .bind(collection_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Записи раздела вместе с подразделами: собственные числа поднимаются к предкам.
pub fn subtree_counts(sections: &[Section], own: &HashMap<Option<i64>, i64>) -> HashMap<i64, i64> {
    let parent: HashMap<i64, Option<i64>> = sections.iter().map(|s| (s.id, s.parent_id)).collect();
    let mut out: HashMap<i64, i64> = HashMap::new();
    for s in sections {
        let Some(&n) = own.get(&Some(s.id)) else {
            continue;
        };
        let mut cur = Some(s.id);
        // Длина цепочки ограничена числом разделов: цикл в данных не зациклит подсчёт.
        for _ in 0..=sections.len() {
            let Some(id) = cur else { break };
            *out.entry(id).or_default() += n;
            cur = parent.get(&id).copied().flatten();
        }
    }
    out
}

/// Выбранный раздел и цепочка его предков (корень первым не гарантируется).
pub fn ancestors_of(sections: &[Section], id: i64) -> HashSet<i64> {
    let parent: HashMap<i64, Option<i64>> = sections.iter().map(|s| (s.id, s.parent_id)).collect();
    let mut out = HashSet::new();
    let mut cur = parent.get(&id).copied().flatten();
    while let Some(p) = cur {
        if !out.insert(p) {
            break;
        }
        cur = parent.get(&p).copied().flatten();
    }
    out
}

impl TreeBuilder<'_> {
    /// Адрес списка с выбранным разделом (без `only`, с первой страницы).
    pub fn list_url(&self, sel: Option<SectionSel>) -> String {
        let mut st = self.state.clone();
        st.section = sel;
        st.only = false;
        st.page = 1;
        super::render::url_of(self.spec, &self.path, &st)
    }

    fn load_url(&self, parent: i64) -> String {
        let mut st = self.state.clone();
        st.section = None;
        st.only = false;
        st.page = 1;
        let mut q = to_query(self.spec, &st, false);
        if !q.is_empty() {
            q.push('&');
        }
        format!(
            "/admin/collections/{}/tree?{q}parent={parent}",
            self.collection_id
        )
    }

    /// Дети `parent` (корни при `None`); раскрываются только узлы на ветке выбранного раздела.
    pub fn level(&self, parent: Option<i64>) -> Vec<TreeNode> {
        let known: HashSet<i64> = self.sections.iter().map(|s| s.id).collect();
        self.sections
            .iter()
            .filter(|s| match parent {
                Some(p) => s.parent_id == Some(p),
                // Родитель удалён или чужой — корень (как в `section_tree`)
                None => s.parent_id.is_none_or(|p| !known.contains(&p)),
            })
            .map(|s| {
                let has_kids = self.sections.iter().any(|k| k.parent_id == Some(s.id));
                let open =
                    has_kids && (self.ancestors.contains(&s.id) || self.current == Some(s.id));
                let cid = self.collection_id;
                TreeNode {
                    id: s.id,
                    name: s.name.clone(),
                    count: self.counts.get(&s.id).copied().unwrap_or(0),
                    active: s.active,
                    selected: self.current == Some(s.id),
                    open,
                    has_kids,
                    url: self.list_url(Some(SectionSel::Id(s.id))),
                    load_url: self.load_url(s.id),
                    edit_url: format!("/admin/sections/{}", s.id),
                    add_sub_url: format!("/admin/collections/{cid}/sections/new?parent={}", s.id),
                    add_item_url: format!("/admin/collections/{cid}/items/new?section={}", s.id),
                    kids: if open {
                        self.level(Some(s.id))
                    } else {
                        Vec::new()
                    },
                }
            })
            .collect()
    }

    /// Данные для `_tree.html`: целая панель.
    pub fn panel(&self, total: i64, without: i64, selected_none: bool, can_write: bool) -> Value {
        serde_json::json!({
            "panel": true,
            "can_write": can_write,
            "all": {
                "url": self.list_url(None),
                "count": total,
                "selected": self.current.is_none() && !selected_none,
            },
            "none": {
                "url": self.list_url(Some(SectionSel::Without)),
                "count": without,
                "selected": selected_none,
            },
            "add_url": format!("/admin/collections/{}/sections/new", self.collection_id),
            "nodes": self.level(None),
        })
    }

    /// Данные для `_tree.html`: только дети узла (подгрузка по клику).
    pub fn children(&self, parent: i64, can_write: bool) -> Value {
        serde_json::json!({
            "panel": false,
            "can_write": can_write,
            "nodes": self.level(Some(parent)),
        })
    }
}
