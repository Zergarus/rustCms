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
        let (path, explicit) = config_path();
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("не удалось прочитать {}", path.display()))?;
            let specs = from_toml(&text)
                .with_context(|| format!("ошибка в конфигурации форм {}", path.display()))?;
            forms.merge(specs);
        } else if explicit {
            // FORMS_CONFIG задан явно — отсутствие файла это ошибка деплоя.
            bail!("файл форм {} не найден", path.display());
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

fn config_path() -> (PathBuf, bool) {
    match std::env::var("FORMS_CONFIG") {
        Ok(value) if !value.trim().is_empty() => (PathBuf::from(value), true),
        _ => (Path::new("config/forms.toml").to_path_buf(), false),
    }
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
        let code = code.trim().to_string();
        if code.is_empty() {
            bail!("пустой код формы");
        }
        if form.fields.is_empty() && form.writer.is_none() && form.notifier.is_none() {
            bail!("форма {code}: пустое описание — нет полей, writer и notifier");
        }
        let mut rules = Vec::with_capacity(form.fields.len());
        for (field, spec) in &form.fields {
            if field.trim().is_empty() {
                bail!("форма {code}: пустое имя поля");
            }
            let mut parsed = Vec::with_capacity(spec.rules.len());
            for raw in &spec.rules {
                parsed.push(parse_rule(&code, field, raw)?);
            }
            rules.push((field.clone(), parsed));
        }
        rules.sort_by(|a, b| a.0.cmp(&b.0));
        let writer = match form.writer {
            Some(writer) if writer.collection.trim().is_empty() => {
                bail!("форма {code}: пустая коллекция writer");
            }
            Some(writer) => Some(WriterSpec {
                collection: writer.collection.trim().to_string(),
                mapping: checked_mapping(&code, &form.fields, writer.mapping)?,
            }),
            None => None,
        };
        let notifier = match form.notifier {
            Some(notifier) if notifier.event.trim().is_empty() => {
                bail!("форма {code}: пустое событие notifier");
            }
            Some(notifier) => Some(NotifierSpec {
                event: notifier.event.trim().to_string(),
                mapping: checked_mapping(&code, &form.fields, notifier.mapping)?,
            }),
            None => None,
        };
        let files = checked_mapping(&code, &form.fields, form.files)?;
        specs.push(FormSpec {
            rules,
            code,
            writer,
            notifier,
            files,
        });
    }
    Ok(specs)
}

/// Маппинг «поле формы → цель»: все поля должны быть объявлены в `fields`.
fn checked_mapping(
    form: &str,
    fields: &HashMap<String, FileField>,
    map: HashMap<String, String>,
) -> anyhow::Result<Vec<(String, String)>> {
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(map.len());
    for (field, target) in map {
        if !fields.contains_key(&field) {
            bail!("форма {form}: поле «{field}» есть в маппинге, но не объявлено в fields");
        }
        pairs.push((field, target));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(pairs)
}

fn parse_rule(form: &str, field: &str, raw: &str) -> anyhow::Result<Rule> {
    match raw {
        "required" => Ok(Rule::Required),
        "email" => Ok(Rule::Email),
        other => bail!("форма {form}, поле {field}: неизвестное правило «{other}»"),
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    forms: HashMap<String, FileForm>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileForm {
    #[serde(default)]
    fields: HashMap<String, FileField>,
    writer: Option<FileWriter>,
    notifier: Option<FileNotifier>,
    #[serde(default)]
    files: HashMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileField {
    #[serde(default)]
    rules: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileWriter {
    collection: String,
    #[serde(default)]
    mapping: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileNotifier {
    event: String,
    #[serde(default)]
    mapping: HashMap<String, String>,
}
