//! Разделы инфоблока. Права — как на элементы: нужен уровень «изменение».

use std::collections::HashMap;

use axum::{
    Extension,
    extract::{Multipart, Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use minijinja::context;
use serde::Deserialize;

use super::{parse_sort, read_upload_form, render};
use crate::{
    access::{Access, Level},
    error::{AppError, AppResult},
    files,
    iblock::{
        Iblock, Section, SectionInput, is_valid_slug, repo, section_subtree_ids, section_tree,
        slugify,
    },
    state::AppState,
};

type FormValues = HashMap<String, String>;

/// `self_id` — редактируемый раздел: его самого и потомков нельзя выбрать родителем.
fn build_input(
    form: &FormValues,
    sections: &[Section],
    self_id: Option<i64>,
) -> Result<SectionInput, String> {
    let get = |key: &str| form.get(key).map(|s| s.trim()).unwrap_or("");
    let mut errors = Vec::new();

    let name = get("name");
    if name.is_empty() {
        errors.push("Укажите название".to_string());
    }
    let code = match get("code") {
        "" => slugify(name),
        code => code.to_string(),
    };
    if !code.is_empty() && !is_valid_slug(&code) {
        errors.push("Код: только латиница, цифры, «_» и «-»".into());
    }
    let parent_id = match get("parent_id") {
        "" => None,
        raw => {
            let id = raw.parse::<i64>().ok();
            let forbidden = self_id
                .map(|sid| section_subtree_ids(sections, sid))
                .unwrap_or_default();
            match id {
                Some(id) if forbidden.contains(&id) => {
                    errors.push("Раздел нельзя вложить в самого себя или в свой подраздел".into());
                    None
                }
                Some(id) if sections.iter().any(|s| s.id == id) => Some(id),
                _ => {
                    errors.push("Родительский раздел не найден".into());
                    None
                }
            }
        }
    };
    let picture_id = match get("picture_id") {
        "" => None,
        raw => match raw.parse::<i64>() {
            Ok(id) => Some(id),
            Err(_) => {
                errors.push("Картинка: неверный id".into());
                None
            }
        },
    };

    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(SectionInput {
        parent_id,
        code,
        xml_id: get("xml_id").to_string(),
        name: name.to_string(),
        active: form.contains_key("active"),
        sort: parse_sort(get("sort")),
        description: get("description").to_string(),
        picture_id,
    })
}

fn section_to_form(section: &Section) -> FormValues {
    let mut form = FormValues::from([
        ("name".into(), section.name.clone()),
        ("code".into(), section.code.clone()),
        ("xml_id".into(), section.xml_id.clone()),
        ("sort".into(), section.sort.to_string()),
        ("description".into(), section.description.clone()),
    ]);
    if section.active {
        form.insert("active".into(), "on".into());
    }
    if let Some(parent) = section.parent_id {
        form.insert("parent_id".into(), parent.to_string());
    }
    if let Some(picture) = section.picture_id {
        form.insert("picture_id".into(), picture.to_string());
    }
    form
}

async fn load(state: &AppState, iblock_id: i64) -> AppResult<(Iblock, Vec<Section>)> {
    let iblock = repo::get_iblock(&state.db, iblock_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let sections = section_tree(repo::list_sections(&state.db, iblock_id).await?);
    Ok((iblock, sections))
}

async fn render_form(
    state: &AppState,
    user: Access,
    iblock: Iblock,
    sections: Vec<Section>,
    section_id: Option<i64>,
    form: FormValues,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let picture = match form.get("picture_id").and_then(|s| s.parse::<i64>().ok()) {
        Some(id) => files::get_many(&state.db, &[id])
            .await?
            .pop()
            .map(|f| context! { id => f.id, url => f.url(), name => f.original_name }),
        None => None,
    };
    // Родителем нельзя выбрать сам раздел и его потомков
    let forbidden: Vec<i64> = section_id
        .map(|sid| section_subtree_ids(&sections, sid).into_iter().collect())
        .unwrap_or_default();
    render(
        state,
        "section_form.html",
        context! { user, iblock, sections, section_id, form, error, picture, forbidden },
    )
}

#[derive(Deserialize)]
pub struct NewQuery {
    parent: Option<i64>,
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Query(q): Query<NewQuery>,
) -> AppResult<Html<String>> {
    user.require_iblock(id, Level::Write)?;
    let (iblock, sections) = load(&state, id).await?;
    let mut form = FormValues::from([
        ("active".into(), "on".into()),
        ("sort".into(), "500".into()),
    ]);
    if let Some(parent) = q.parent {
        form.insert("parent_id".into(), parent.to_string());
    }
    render_form(&state, user, iblock, sections, None, form, None).await
}

async fn save(
    state: &AppState,
    user: Access,
    iblock: Iblock,
    sections: Vec<Section>,
    section_id: Option<i64>,
    multipart: Multipart,
) -> AppResult<Response> {
    let upload = read_upload_form(state, multipart, "iblock").await?;
    let mut form: FormValues = upload.fields.into_iter().collect();
    if let Some((_, file)) = upload
        .uploads
        .iter()
        .find(|(f, _)| f == "upload_picture_id")
    {
        form.insert("picture_id".into(), file.id.to_string());
    }
    let result = build_input(&form, &sections, section_id).and_then(|input| {
        if upload.rejected.is_empty() {
            Ok(input)
        } else {
            Err(upload.rejected.join("; "))
        }
    });
    let error = match result {
        Ok(input) => {
            match section_id {
                Some(id) => repo::update_section(&state.db, id, &input).await?,
                None => {
                    repo::create_section(&state.db, iblock.id, &input).await?;
                }
            }
            let mut url = format!("/admin/iblocks/{}/elements", iblock.id);
            if let Some(parent) = input.parent_id {
                url.push_str(&format!("?section={parent}"));
            }
            return Ok(Redirect::to(&url).into_response());
        }
        Err(msg) => msg,
    };
    let page = render_form(state, user, iblock, sections, section_id, form, Some(error)).await?;
    Ok(page.into_response())
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    multipart: Multipart,
) -> AppResult<Response> {
    user.require_iblock(id, Level::Write)?;
    let (iblock, sections) = load(&state, id).await?;
    save(&state, user, iblock, sections, None, multipart).await
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let section = repo::get_section(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    user.require_iblock(section.iblock_id, Level::Write)?;
    let (iblock, sections) = load(&state, section.iblock_id).await?;
    let form = section_to_form(&section);
    render_form(&state, user, iblock, sections, Some(id), form, None).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    multipart: Multipart,
) -> AppResult<Response> {
    let section = repo::get_section(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    user.require_iblock(section.iblock_id, Level::Write)?;
    let (iblock, sections) = load(&state, section.iblock_id).await?;
    save(&state, user, iblock, sections, Some(id), multipart).await
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    let section = repo::get_section(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    user.require_iblock(section.iblock_id, Level::Write)?;
    repo::delete_section(&state.db, id).await?;
    let mut url = format!("/admin/iblocks/{}/elements", section.iblock_id);
    if let Some(parent) = section.parent_id {
        url.push_str(&format!("?section={parent}"));
    }
    Ok(Redirect::to(&url))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn section(id: i64, parent_id: Option<i64>) -> Section {
        Section {
            id,
            iblock_id: 1,
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
    fn parent_validation() {
        let sections = [section(1, None), section(2, Some(1)), section(3, None)];
        let form = |parent: &str| {
            FormValues::from([
                ("name".into(), "Раздел".into()),
                ("parent_id".into(), parent.into()),
            ])
        };
        let ok = build_input(&form("3"), &sections, Some(1)).unwrap();
        assert_eq!(ok.parent_id, Some(3));
        assert_eq!(ok.code, "razdel");
        assert!(build_input(&form("2"), &sections, Some(1)).is_err());
        assert!(build_input(&form("1"), &sections, Some(1)).is_err());
        assert!(build_input(&form("99"), &sections, None).is_err());
    }
}
