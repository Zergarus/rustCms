//! Реестр форм: описания из кода проекта (`project/mod.rs`) и из `config/forms.toml`.
//!
//! Как в модуле `mediagroup.bxapi`: дефолты из кода, поверх них — конфигурация
//! установки по коду формы. Форма описывается данными: правила полей, writer
//! (коллекция + маппинг), notifier (событие + маппинг), файловые поля.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use serde::Deserialize;

use super::project::{FormConfig, Project, Rule};

/// Форма в рантайме: владеющая копия конфигурации.
#[derive(Clone, Debug, Default)]
pub struct FormSpec {
    pub code: String,
    /// Поле формы → правила (порядок правил важен: `required` проверяется первым).
    pub rules: Vec<(String, Vec<Rule>)>,
    pub writer: Option<WriterSpec>,
    pub notifier: Option<NotifierSpec>,
    /// Поле формы → код файлового поля коллекции.
    pub files: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct WriterSpec {
    /// `api_code` коллекции.
    pub collection: String,
    /// Поле формы → `NAME` | `PREVIEW_TEXT` | `DETAIL_TEXT` | код поля.
    pub mapping: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct NotifierSpec {
    pub event: String,
    pub mapping: Vec<(String, String)>,
}

/// Реестр форм установки.
#[derive(Debug, Default)]
pub struct Forms {
    by_code: HashMap<String, FormSpec>,
}

impl Forms {
    /// Формы, описанные кодом проекта.
    pub fn from_project(project: &Project) -> Self {
        let mut forms = Self::default();
        for form in &project.forms {
            forms.insert(spec_from_code(form));
        }
        forms
    }

    /// Формы проекта плюс файл конфигурации (`FORMS_CONFIG` или `config/forms.toml`).
    /// Ошибка в файле останавливает старт: молча потерять форму хуже.
    pub fn load(project: &Project) -> anyhow::Result<Self> {
        let mut forms = Self::from_project(project);
        let path = config_path();
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("не удалось прочитать {}", path.display()))?;
            let specs = from_toml(&text)
                .with_context(|| format!("ошибка в конфигурации форм {}", path.display()))?;
            forms.merge(specs);
        }
        Ok(forms)
    }

    pub fn get(&self, code: &str) -> Option<&FormSpec> {
        self.by_code.get(code)
    }

    pub fn insert(&mut self, spec: FormSpec) {
        self.by_code.insert(spec.code.clone(), spec);
    }

    /// Оверрайд по коду формы целиком — как второй уровень реестра в Битриксе.
    pub fn merge(&mut self, specs: Vec<FormSpec>) {
        for spec in specs {
            self.insert(spec);
        }
    }
}

fn config_path() -> PathBuf {
    std::env::var("FORMS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new("config/forms.toml").to_path_buf())
}

fn spec_from_code(form: &FormConfig) -> FormSpec {
    FormSpec {
        code: form.code.to_string(),
        rules: form
            .rules
            .iter()
            .map(|(field, rules)| (field.to_string(), rules.to_vec()))
            .collect(),
        writer: form.writer.as_ref().map(|writer| WriterSpec {
            collection: writer.api_code.to_string(),
            mapping: writer
                .field_mapping
                .iter()
                .map(|(field, target)| (field.to_string(), target.to_string()))
                .collect(),
        }),
        notifier: form.notifier.as_ref().map(|notifier| NotifierSpec {
            event: notifier.event.to_string(),
            mapping: notifier
                .field_mapping
                .iter()
                .map(|(field, target)| (field.to_string(), target.to_string()))
                .collect(),
        }),
        files: Vec::new(),
    }
}

/// Разбор TOML-описания форм.
pub fn from_toml(text: &str) -> anyhow::Result<Vec<FormSpec>> {
    let file: FileConfig = toml::from_str(text).context("не удалось разобрать TOML")?;
    let mut specs = Vec::with_capacity(file.forms.len());
    for (code, form) in file.forms {
        if code.trim().is_empty() {
            bail!("пустой код формы");
        }
        let mut rules = Vec::with_capacity(form.fields.len());
        for (field, spec) in form.fields {
            let mut parsed = Vec::with_capacity(spec.rules.len());
            for raw in spec.rules {
                parsed.push(parse_rule(&code, &field, &raw)?);
            }
            rules.push((field, parsed));
        }
        rules.sort_by(|a, b| a.0.cmp(&b.0));
        specs.push(FormSpec {
            rules,
            code,
            writer: form.writer.map(|writer| WriterSpec {
                collection: writer.collection,
                mapping: sorted(writer.mapping),
            }),
            notifier: form.notifier.map(|notifier| NotifierSpec {
                event: notifier.event,
                mapping: sorted(notifier.mapping),
            }),
            files: sorted(form.files),
        });
    }
    Ok(specs)
}

fn sorted(map: HashMap<String, String>) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = map.into_iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

fn parse_rule(form: &str, field: &str, raw: &str) -> anyhow::Result<Rule> {
    match raw {
        "required" => Ok(Rule::Required),
        "email" => Ok(Rule::Email),
        other => bail!("форма {form}, поле {field}: неизвестное правило «{other}»"),
    }
}

#[derive(Deserialize, Default)]
struct FileConfig {
    #[serde(default)]
    forms: HashMap<String, FileForm>,
}

#[derive(Deserialize, Default)]
struct FileForm {
    #[serde(default)]
    fields: HashMap<String, FileField>,
    writer: Option<FileWriter>,
    notifier: Option<FileNotifier>,
    #[serde(default)]
    files: HashMap<String, String>,
}

#[derive(Deserialize, Default)]
struct FileField {
    #[serde(default)]
    rules: Vec<String>,
}

#[derive(Deserialize)]
struct FileWriter {
    collection: String,
    #[serde(default)]
    mapping: HashMap<String, String>,
}

#[derive(Deserialize)]
struct FileNotifier {
    event: String,
    #[serde(default)]
    mapping: HashMap<String, String>,
}
