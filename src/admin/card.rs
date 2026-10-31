//! Общая механика карточек с вкладками: ошибки по вкладкам и выбор активной.

use std::collections::BTreeMap;

use minijinja::Value;

/// Ошибки формы, привязанные к вкладкам; пустой ключ — «без вкладки».
#[derive(Debug, Default)]
pub(crate) struct TabErrors {
    items: Vec<(&'static str, String)>,
}

#[allow(dead_code)] // используется карточками следующих задач
impl TabErrors {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(&mut self, tab: &'static str, msg: impl Into<String>) {
        self.items.push((tab, msg.into()));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Добавляет общие ошибки (без вкладки).
    pub(crate) fn extend_general(&mut self, msgs: Vec<String>) {
        self.items.extend(msgs.into_iter().map(|m| ("", m)));
    }
}

/// Контекст вкладок для шаблона: `{ active, error_tabs, by_tab, general }`.
/// Активна первая видимая вкладка с ошибкой, иначе запрошенная (если видима),
/// иначе первая видимая. Ошибки невидимых вкладок и без вкладки попадают в `general`.
#[allow(dead_code)] // используется карточками следующих задач
pub(crate) fn tab_context(
    errors: &TabErrors,
    visible: &[&'static str],
    requested: Option<&str>,
) -> Value {
    let mut by_tab: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut general: Vec<&str> = Vec::new();
    for (tab, msg) in &errors.items {
        if visible.contains(tab) {
            by_tab.entry(tab).or_default().push(msg);
        } else {
            general.push(msg);
        }
    }
    let error_tabs: Vec<&str> = visible
        .iter()
        .copied()
        .filter(|t| by_tab.contains_key(t))
        .collect();
    let active = error_tabs
        .first()
        .copied()
        .or_else(|| requested.and_then(|r| visible.iter().copied().find(|v| *v == r)))
        .or_else(|| visible.first().copied())
        .unwrap_or("");
    minijinja::context! { active, error_tabs, by_tab, general }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &Value, key: &str) -> Value {
        v.get_attr(key).unwrap()
    }

    #[test]
    fn active_prefers_first_error_tab() {
        let mut e = TabErrors::new();
        e.push("text", "Плохо");
        let c = tab_context(&e, &["main", "text"], Some("main"));
        assert_eq!(s(&c, "active").to_string(), "text");
        assert_eq!(s(&c, "error_tabs").len(), Some(1));
    }

    #[test]
    fn active_uses_requested_when_valid() {
        let c = tab_context(&TabErrors::new(), &["main", "text"], Some("text"));
        assert_eq!(s(&c, "active").to_string(), "text");
    }

    #[test]
    fn unknown_requested_falls_back_first() {
        let c = tab_context(&TabErrors::new(), &["main", "text"], Some("nope"));
        assert_eq!(s(&c, "active").to_string(), "main");
        let c = tab_context(&TabErrors::new(), &["main", "text"], None);
        assert_eq!(s(&c, "active").to_string(), "main");
    }

    #[test]
    fn error_on_hidden_tab_goes_general() {
        let mut e = TabErrors::new();
        e.push("catalog", "Скрыто");
        let c = tab_context(&e, &["main"], None);
        assert_eq!(s(&c, "error_tabs").len(), Some(0));
        assert_eq!(s(&c, "general").len(), Some(1));
        assert_eq!(s(&c, "active").to_string(), "main");
    }

    #[test]
    fn general_errors_kept() {
        let mut e = TabErrors::new();
        assert!(e.is_empty());
        e.extend_general(vec!["Общая".into()]);
        e.push("", "Ещё");
        assert!(!e.is_empty());
        let c = tab_context(&e, &["main"], None);
        assert_eq!(s(&c, "general").len(), Some(2));
        assert_eq!(s(&c, "error_tabs").len(), Some(0));
    }

    fn render(tpl: &str, ctx: Value) -> String {
        let mut env = minijinja::Environment::new();
        env.set_loader(minijinja::path_loader("templates/admin"));
        env.add_template_owned("t.html", tpl.to_string()).unwrap();
        env.get_template("t.html").unwrap().render(ctx).unwrap()
    }

    #[test]
    fn card_tabs_marks_active_and_error() {
        let mut e = TabErrors::new();
        e.push("text", "Ошибка <b>");
        e.push("", "Общая");
        let tabctx = tab_context(&e, &["main", "text"], Some("main"));
        let html = render(
            r#"{% import "_ui.html" as ui %}{{ ui.card_errors(tabctx) }}{{ ui.card_tabs([("main","Основное"),("text","Описание")], tabctx) }}{% call ui.tab_pane("main", tabctx) %}A{% endcall %}{% call ui.tab_pane("text", tabctx) %}B{% endcall %}"#,
            minijinja::context! { tabctx },
        );
        assert!(html.contains(r#"data-tab-btn="text""#), "{html}");
        assert_eq!(html.matches("tab-dot err").count(), 1, "{html}");
        assert_eq!(html.matches("tab-dot dirty").count(), 2, "{html}");
        // активна вкладка с ошибкой, скрыта другая панель
        assert!(
            html.contains(r#"<section data-tab="main" hidden>"#),
            "{html}"
        );
        assert!(html.contains(r#"<section data-tab="text">"#), "{html}");
        assert!(html.contains("Ошибка &lt;b&gt;"), "{html}");
        assert!(html.contains("Общая"), "{html}");
        assert!(html.contains("data-card-tabs"), "{html}");
    }
}
