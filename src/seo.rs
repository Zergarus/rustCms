//! Метаданные контента: настройки → наследование → безопасные подстановки.
use std::collections::HashSet;

use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};

use crate::{
    collection::{Collection, Section, repo},
    error::{AppError, AppResult},
    state::AppState,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Collection,
    Section,
    Item,
}
impl Kind {
    pub fn parse(raw: &str) -> AppResult<Self> {
        match raw {
            "collections" => Ok(Self::Collection),
            "sections" => Ok(Self::Section),
            "items" => Ok(Self::Item),
            _ => Err(AppError::NotFound),
        }
    }
    pub fn path(self) -> &'static str {
        match self {
            Self::Collection => "collections",
            Self::Section => "sections",
            Self::Item => "items",
        }
    }
    fn column(self) -> &'static str {
        match self {
            Self::Collection => "collection_id",
            Self::Section => "section_id",
            Self::Item => "item_id",
        }
    }
}

pub struct Target {
    pub kind: Kind,
    pub id: i64,
    pub name: String,
    code: String,
    pub collection: Collection,
    /// От корня к непосредственному разделу (для раздела включает его самого).
    sections: Vec<Section>,
    public: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, FromRow)]
#[serde(default)]
pub struct Settings {
    pub title: String,
    pub description: String,
    pub h1: String,
}

#[derive(Serialize)]
pub struct Metadata {
    pub title: String,
    pub description: String,
    pub h1: String,
}

pub async fn load(db: &PgPool, kind: Kind, id: i64) -> AppResult<Target> {
    let (collection_id, name, code, mut section_id, active) = match kind {
        Kind::Collection => (id, String::new(), String::new(), None, true),
        Kind::Section => {
            let s = repo::get_section(db, id).await?.ok_or(AppError::NotFound)?;
            (s.collection_id, s.name, s.code, Some(id), s.active)
        }
        Kind::Item => {
            let i = repo::get_item(db, id).await?.ok_or(AppError::NotFound)?;
            let active = i.active && i.published_at.is_none_or(|d| d <= chrono::Utc::now());
            (i.collection_id, i.name, i.code, i.section_id, active)
        }
    };
    let collection = repo::get_collection(db, collection_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let (name, code) = if kind == Kind::Collection {
        (collection.name.clone(), collection.code.clone())
    } else {
        (name, code)
    };
    let mut sections = Vec::new();
    let mut seen = HashSet::new();
    let mut public = active && collection.api_enabled;
    while let Some(sid) = section_id {
        if !seen.insert(sid) || seen.len() > 128 {
            return Err(AppError::BadRequest("Некорректная цепочка разделов".into()));
        }
        let s = repo::get_section(db, sid)
            .await?
            .filter(|s| s.collection_id == collection_id)
            .ok_or(AppError::NotFound)?;
        public &= s.active;
        section_id = s.parent_id;
        sections.push(s);
    }
    sections.reverse();
    Ok(Target {
        kind,
        id,
        name,
        code,
        collection,
        sections,
        public,
    })
}

pub async fn settings(db: &PgPool, target: &Target) -> AppResult<Settings> {
    let sql = format!(
        "SELECT title,description,h1 FROM seo_settings WHERE {}=$1",
        target.kind.column()
    );
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(target.id)
        .fetch_optional(db)
        .await?
        .unwrap_or_default())
}

/// Разбирается только текст шаблона. Подставленные значения не интерпретируются.
fn expand(template: &str, target: &Target) -> Result<String, String> {
    let mut rest = template;
    let mut out = String::new();
    while let Some(pos) = rest.find(['{', '}']) {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        if rest.starts_with('}') {
            return Err("Лишняя закрывающая скобка в SEO-шаблоне".into());
        }
        let end = rest
            .find('}')
            .ok_or("Незакрытая переменная в SEO-шаблоне")?;
        let value = match &rest[1..end] {
            "name" => &target.name,
            "code" => &target.code,
            "collection" => &target.collection.name,
            "section" => target
                .sections
                .last()
                .map(|s| s.name.as_str())
                .unwrap_or(""),
            _ => {
                return Err(
                    "Допустимые переменные: {name}, {code}, {collection}, {section}".into(),
                );
            }
        };
        out.push_str(value);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

impl Settings {
    pub fn validate(&mut self, target: &Target) -> Result<(), String> {
        for s in [&mut self.title, &mut self.description, &mut self.h1] {
            *s = s.trim().to_string();
            if s.chars().count() > 2000 {
                return Err("SEO-шаблон: максимум 2000 символов".into());
            }
            expand(s, target)?;
        }
        Ok(())
    }
    fn overlay(&mut self, other: &Self) {
        if !other.title.is_empty() {
            self.title.clone_from(&other.title);
        }
        if !other.description.is_empty() {
            self.description.clone_from(&other.description);
        }
        if !other.h1.is_empty() {
            self.h1.clone_from(&other.h1);
        }
    }
}

pub async fn save(db: &PgPool, target: &Target, mut input: Settings) -> AppResult<()> {
    input.validate(target).map_err(AppError::BadRequest)?;
    let col = target.kind.column();
    let sql = format!(
        "INSERT INTO seo_settings ({col},title,description,h1) VALUES ($1,$2,$3,$4) ON CONFLICT ({col}) DO UPDATE SET title=EXCLUDED.title, description=EXCLUDED.description, h1=EXCLUDED.h1"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(target.id)
        .bind(input.title)
        .bind(input.description)
        .bind(input.h1)
        .execute(db)
        .await?;
    Ok(())
}

#[derive(FromRow)]
struct Layer {
    collection_id: Option<i64>,
    section_id: Option<i64>,
    item_id: Option<i64>,
    #[sqlx(flatten)]
    values: Settings,
}

pub async fn resolve(db: &PgPool, target: &Target) -> AppResult<Metadata> {
    let section_ids: Vec<i64> = target.sections.iter().map(|s| s.id).collect();
    let item_id = (target.kind == Kind::Item).then_some(target.id);
    let layers: Vec<Layer>=sqlx::query_as("SELECT collection_id,section_id,item_id,title,description,h1 FROM seo_settings WHERE collection_id=$1 OR section_id=ANY($2) OR item_id=$3")
        .bind(target.collection.id).bind(&section_ids).bind(item_id).fetch_all(db).await?;
    let mut merged = Settings::default();
    if let Some(l) = layers
        .iter()
        .find(|l| l.collection_id == Some(target.collection.id))
    {
        merged.overlay(&l.values);
    }
    for sid in section_ids {
        if let Some(l) = layers.iter().find(|l| l.section_id == Some(sid)) {
            merged.overlay(&l.values);
        }
    }
    if let Some(i) = item_id
        && let Some(l) = layers.iter().find(|l| l.item_id == Some(i))
    {
        merged.overlay(&l.values);
    }
    let render = |s: &str, default: &str| -> AppResult<String> {
        if s.is_empty() {
            Ok(default.to_string())
        } else {
            expand(s, target).map_err(AppError::BadRequest)
        }
    };
    Ok(Metadata {
        title: render(&merged.title, &target.name)?,
        description: render(&merged.description, "")?,
        h1: render(&merged.h1, &target.name)?,
    })
}

pub async fn public(
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, i64)>,
) -> AppResult<Json<Metadata>> {
    let target = load(&state.db, Kind::parse(&kind)?, id).await?;
    if !target.public {
        return Err(AppError::NotFound);
    }
    Ok(Json(resolve(&state.db, &target).await?))
}
