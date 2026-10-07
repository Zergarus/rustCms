//! Проектные настройки bxapi — аналог `bxapi.*` в `/bitrix/.settings_extra.php`
//! и регистрации декораторов в `init.php`.
//!
//! Код конкретных проектов в репозиторий CMS не входит: модули проектов лежат в
//! каталоге `projects/` (`projects/<имя>.rs` с функцией `pub fn project() -> Project`),
//! подключаются при сборке (см. `build.rs`) и выбираются переменной `BXAPI_PROJECT`.

use std::{future::Future, pin::Pin, sync::Arc};

use serde_json::{Map, Value};

use super::BxError;
use crate::state::AppState;

/// Модули проектов из `projects/`, найденные при сборке.
mod installed {
    include!(concat!(env!("OUT_DIR"), "/projects.rs"));
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Короткое имя поля вместо длинного пути (`recomend` → `recomend.item.xmlId`);
/// `true`/`false` в фильтре превращаются в `on`/`off`.
pub struct Alias {
    pub name: &'static str,
    pub path: &'static str,
    pub on: &'static str,
    pub off: &'static str,
}

/// Картинка `image` из свойства (`bxapi.element_image_source`).
pub struct ImageSource {
    /// Код свойства-файла в snake_case.
    pub property: &'static str,
    /// `fallback`: сначала штатные анонс/детальная, потом свойство.
    pub fallback: bool,
}

/// Дописывает поля в элементы ответа (`bxapi.list_decorators` / `detail_decorators`,
/// `DetailItemDecoratorInterface` в Битриксе).
pub trait Decorator: Send + Sync {
    /// Поля, которые декоратор читает из элемента — добавляются в `select`.
    fn required_select(&self) -> &'static [&'static str] {
        &[]
    }

    fn decorate<'a>(
        &'a self,
        state: &'a AppState,
        items: &'a mut [Map<String, Value>],
    ) -> BoxFuture<'a, Result<(), BxError>>;
}

/// Группа в ответе поиска рядом с `items` (`bxapi.search.group_providers`,
/// `SearchGroupProviderInterface` в Битриксе).
pub trait SearchGroup: Send + Sync {
    /// Ключ группы в ответе.
    fn key(&self) -> &'static str;

    fn build<'a>(
        &'a self,
        state: &'a AppState,
        query: &'a str,
    ) -> BoxFuture<'a, Result<Vec<Value>, BxError>>;
}

/// Хлебные крошки для URL, не совпадающего с `/{apiCode}/...`
/// (`bxapi.breadcrumbs.iblock_path_rules`).
pub struct BreadcrumbRule {
    pub prefix: &'static str,
    pub api_code: &'static str,
    pub root_label: &'static str,
}

/// Старый URL с числовым хвостом → новый (`bxapi.legacy_redirect.rules`).
pub struct LegacyRule {
    pub api_code: &'static str,
    /// `id` | `code` | `xmlId` — по чему искать элемент.
    pub match_field: &'static str,
    /// `code` | `id` | `xmlId` — что подставить в шаблон.
    pub value_field: &'static str,
    pub template: &'static str,
    pub prefix: Option<&'static str>,
}

/// Правило проверки поля формы.
#[derive(Clone, Copy, Debug)]
pub enum Rule {
    Required,
    Email,
}

/// Запись формы в инфоблок: поле формы → `NAME` | `PREVIEW_TEXT` | `DETAIL_TEXT` | код свойства.
pub struct IblockWriter {
    pub api_code: &'static str,
    pub field_mapping: Vec<(&'static str, &'static str)>,
}

/// Письмо по почтовому событию: поле формы → `#ПОЛЕ#` шаблона.
pub struct MailNotifier {
    pub event: &'static str,
    pub field_mapping: Vec<(&'static str, &'static str)>,
}

/// Форма (`bxapi.forms`): правила, запись, уведомление.
pub struct FormConfig {
    pub code: &'static str,
    pub rules: Vec<(&'static str, &'static [Rule])>,
    pub writer: Option<IblockWriter>,
    pub notifier: Option<MailNotifier>,
}

/// Проверка запроса формы до её обработки (капча и т.п. — `onBeforeAction` в Битриксе).
pub trait FormGuard: Send + Sync {
    fn check<'a>(
        &'a self,
        state: &'a AppState,
        form: &'a str,
        body: &'a Map<String, Value>,
        ip: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), BxError>>;
}

/// Выбор города (`bxapi.location`).
pub struct LocationConfig {
    /// Типы местоположений в поиске (`CITY`, `VILLAGE`...).
    pub search_types: Vec<&'static str>,
    /// Добавлять страну в `displayName`.
    pub show_country: bool,
    /// Имя поля пользователя в ответе `location/set` (выбор хранится в профиле).
    pub user_field: &'static str,
}

pub struct Project {
    /// Разрешённые ширины ресайза (`bxapi.images.widths`); пусто — любые.
    pub image_widths: Vec<u32>,
    /// apiCode (snake_case) → свойства для `characteristics` в детальном ответе.
    pub characteristics: Vec<(&'static str, Vec<&'static str>)>,
    pub image_sources: Vec<(&'static str, ImageSource)>,
    /// Инфоблоки, где в детальном ответе `image` — сначала детальная картинка
    /// (`bxapi.element_detail_image`).
    pub detail_image: Vec<&'static str>,
    pub aliases: Vec<(&'static str, Vec<Alias>)>,
    pub detail_decorators: Vec<(&'static str, Vec<Arc<dyn Decorator>>)>,
    pub list_decorators: Vec<(&'static str, Vec<Arc<dyn Decorator>>)>,
    /// Свойства, участвующие в поиске (SEARCHABLE в Битриксе).
    pub search_props: Vec<(&'static str, Vec<&'static str>)>,
    pub search_groups: Vec<Arc<dyn SearchGroup>>,
    pub home_label: &'static str,
    pub breadcrumb_rules: Vec<BreadcrumbRule>,
    pub legacy_rules: Vec<LegacyRule>,
    pub location: LocationConfig,
    pub forms: Vec<FormConfig>,
    pub form_guards: Vec<Arc<dyn FormGuard>>,
}

impl Project {
    pub fn from_env() -> Arc<Project> {
        let name = std::env::var("BXAPI_PROJECT").unwrap_or_default();
        let project = match name.trim() {
            "" => Project::default(),
            name => installed::by_name(name).unwrap_or_else(|| {
                tracing::warn!(
                    project = name,
                    available = ?installed::NAMES,
                    "BXAPI_PROJECT: модуль проекта не найден в projects/, настройки по умолчанию"
                );
                Project::default()
            }),
        };
        Arc::new(project)
    }

    fn find<'a, T>(list: &'a [(&'static str, T)], iblock: &str) -> Option<&'a T> {
        list.iter()
            .find(|(code, _)| *code == iblock)
            .map(|(_, v)| v)
    }

    pub fn characteristics(&self, iblock: &str) -> &[&'static str] {
        Self::find(&self.characteristics, iblock).map_or(&[], Vec::as_slice)
    }

    pub fn image_source(&self, iblock: &str) -> Option<&ImageSource> {
        Self::find(&self.image_sources, iblock)
    }

    pub fn detail_image_first(&self, iblock: &str) -> bool {
        self.detail_image.contains(&iblock)
    }

    pub fn alias(&self, iblock: &str, name: &str) -> Option<&Alias> {
        Self::find(&self.aliases, iblock)?
            .iter()
            .find(|a| a.name == name)
    }

    pub fn detail_decorators(&self, iblock: &str) -> &[Arc<dyn Decorator>] {
        Self::find(&self.detail_decorators, iblock).map_or(&[], Vec::as_slice)
    }

    pub fn list_decorators(&self, iblock: &str) -> &[Arc<dyn Decorator>] {
        Self::find(&self.list_decorators, iblock).map_or(&[], Vec::as_slice)
    }

    pub fn form(&self, code: &str) -> Option<&FormConfig> {
        // Последняя с таким кодом: проект может переопределить форму модуля
        self.forms.iter().rev().find(|f| f.code == code)
    }

    pub fn search_props(&self, iblock: &str) -> &[&'static str] {
        Self::find(&self.search_props, iblock).map_or(&[], Vec::as_slice)
    }
}

impl Default for Project {
    /// Настройки модуля по умолчанию (без проекта).
    fn default() -> Self {
        Project {
            image_widths: Vec::new(),
            characteristics: Vec::new(),
            image_sources: Vec::new(),
            detail_image: Vec::new(),
            aliases: vec![(
                "catalog",
                vec![Alias {
                    name: "recomend",
                    path: "recomend.item.xmlId",
                    on: "Y",
                    off: "N",
                }],
            )],
            detail_decorators: Vec::new(),
            list_decorators: Vec::new(),
            search_props: Vec::new(),
            search_groups: Vec::new(),
            home_label: "Главная",
            breadcrumb_rules: Vec::new(),
            legacy_rules: vec![LegacyRule {
                api_code: "catalog",
                match_field: "id",
                value_field: "code",
                template: "/product/{value}",
                prefix: None,
            }],
            location: LocationConfig {
                search_types: vec!["CITY"],
                show_country: true,
                user_field: "UF_CITY",
            },
            forms: default_forms(),
            form_guards: Vec::new(),
        }
    }
}

/// Формы модуля bxapi по умолчанию: обратный звонок и бриф.
fn default_forms() -> Vec<FormConfig> {
    vec![
        FormConfig {
            code: "callback",
            rules: vec![
                ("name", &[Rule::Required]),
                ("contacts", &[Rule::Required]),
                ("personalData", &[]),
            ],
            writer: Some(IblockWriter {
                api_code: "callback",
                field_mapping: vec![("name", "NAME"), ("contacts", "CONTACTS")],
            }),
            notifier: Some(MailNotifier {
                event: "CALLBACK_FORM",
                field_mapping: vec![("name", "AUTHOR"), ("contacts", "TEXT")],
            }),
        },
        FormConfig {
            code: "brief",
            rules: vec![
                ("name", &[Rule::Required]),
                ("contacts", &[Rule::Required]),
                ("comments", &[]),
                ("personalData", &[]),
            ],
            writer: Some(IblockWriter {
                api_code: "brief",
                field_mapping: vec![
                    ("name", "NAME"),
                    ("contacts", "CONTACTS"),
                    ("comments", "PREVIEW_TEXT"),
                ],
            }),
            notifier: Some(MailNotifier {
                event: "BRIEF_FORM",
                field_mapping: vec![
                    ("name", "AUTHOR"),
                    ("contacts", "TEXT"),
                    ("comments", "TEXT"),
                ],
            }),
        },
    ]
}
