//! Раздел «Группы»: права на разделы и доступ к коллекциям. Только для суперадминистраторов —
//! иначе пользователь с правом управления мог бы выдать себе любые права.

use std::collections::HashMap;

use axum::{
    Extension, Form,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use minijinja::{Value, context};
use serde::Serialize;

use super::{parse_sort, render};
use crate::{
    access::{Access, Level, PERMISSIONS, is_known_permission},
    collection::{is_valid_code, repo as collection_repo},
    error::{AppError, AppResult, is_unique_violation},
    groups::{self, GroupInput},
    state::AppState,
};

/// Поля формы: code, name, description, sort, `permission` (повторяется),
/// `collection_<id>` = none | read | write.
type FormFields = Vec<(String, String)>;

#[derive(Default, Serialize)]
struct GroupFormView {
    code: String,
    name: String,
    description: String,
    sort: String,
    permissions: Vec<String>,
    /// collection_id (строкой) → "read" | "write"
    levels: HashMap<String, String>,
}

fn parse_form(fields: &FormFields) -> (GroupFormView, Result<GroupInput, String>) {
    let mut view = GroupFormView::default();
    let mut collection_levels = Vec::new();
    let mut errors = Vec::new();

    for (key, value) in fields {
        match key.as_str() {
            "code" => view.code = value.trim().to_string(),
            "name" => view.name = value.trim().to_string(),
            "description" => view.description = value.trim().to_string(),
            "sort" => view.sort = value.trim().to_string(),
            "permission" => {
                if is_known_permission(value) && !view.permissions.contains(value) {
                    view.permissions.push(value.clone());
                }
            }
            key => {
                if let Some(id) = key
                    .strip_prefix("collection_")
                    .and_then(|s| s.parse::<i64>().ok())
                {
                    let level = Level::from_db(value);
                    if let Some(db) = level.as_db() {
                        view.levels.insert(id.to_string(), db.to_string());
                    }
                    collection_levels.push((id, level));
                }
            }
        }
    }

    if view.name.is_empty() {
        errors.push("Укажите название");
    }
    if !is_valid_code(&view.code) {
        errors.push("Код: только латиница в нижнем регистре, цифры, «_» и «-»");
    }
    let result = if errors.is_empty() {
        Ok(GroupInput {
            code: view.code.clone(),
            name: view.name.clone(),
            description: view.description.clone(),
            sort: parse_sort(&view.sort),
            permissions: view.permissions.clone(),
            collection_levels,
        })
    } else {
        Err(errors.join("; "))
    };
    (view, result)
}

async fn render_form(
    state: &AppState,
    user: Access,
    group_id: Option<i64>,
    form: GroupFormView,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let collections = collection_repo::list_collections(&state.db).await?;
    let members = match group_id {
        Some(id) => groups::members(&state.db, id).await?,
        None => Vec::new(),
    };
    render(
        state,
        "group_form.html",
        context! {
            user,
            group_id,
            form,
            error,
            collections,
            members,
            all_permissions => Value::from_serialize(PERMISSIONS),
        },
    )
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require_super()?;
    let items = groups::list(&state.db).await?;
    render(
        &state,
        "groups.html",
        context! { user, items, all_permissions => Value::from_serialize(PERMISSIONS) },
    )
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require_super()?;
    let form = GroupFormView {
        sort: "500".into(),
        permissions: vec![crate::access::ADMIN_ACCESS.into()],
        ..Default::default()
    };
    render_form(&state, user, None, form, None).await
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(fields): Form<FormFields>,
) -> AppResult<Response> {
    user.require_super()?;
    save(state, user, None, fields).await
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    user.require_super()?;
    let group = groups::get(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let form = GroupFormView {
        code: group.code,
        name: group.name,
        description: group.description,
        sort: group.sort.to_string(),
        permissions: groups::permissions(&state.db, id).await?,
        levels: groups::collection_levels(&state.db, id).await?,
    };
    render_form(&state, user, Some(id), form, None).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(fields): Form<FormFields>,
) -> AppResult<Response> {
    user.require_super()?;
    groups::get(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    save(state, user, Some(id), fields).await
}

async fn save(
    state: AppState,
    user: Access,
    id: Option<i64>,
    fields: FormFields,
) -> AppResult<Response> {
    let (view, parsed) = parse_form(&fields);
    let error = match parsed {
        Ok(input) => match groups::save(&state.db, id, &input).await {
            Ok(_) => return Ok(Redirect::to("/admin/groups").into_response()),
            Err(e) if is_unique_violation(&e) => "Группа с таким кодом уже есть".to_string(),
            Err(e) => return Err(e.into()),
        },
        Err(msg) => msg,
    };
    Ok(render_form(&state, user, id, view, Some(error))
        .await?
        .into_response())
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    user.require_super()?;
    groups::delete(&state.db, id).await?;
    Ok(Redirect::to("/admin/groups"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(pairs: &[(&str, &str)]) -> FormFields {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_permissions_and_levels() {
        let (view, input) = parse_form(&f(&[
            ("name", "Редакторы"),
            ("code", "editors"),
            ("permission", "admin.access"),
            ("permission", "users.manage"),
            ("permission", "bogus.perm"),
            ("collection_1", "write"),
            ("collection_2", "none"),
            ("collection_x", "read"),
        ]));
        let input = input.unwrap();
        assert_eq!(input.permissions, vec!["admin.access", "users.manage"]);
        assert_eq!(
            input.collection_levels,
            vec![(1, Level::Write), (2, Level::None)]
        );
        assert_eq!(view.levels.get("1").map(String::as_str), Some("write"));
        assert!(!view.levels.contains_key("2"));
    }

    #[test]
    fn validates() {
        let (_, input) = parse_form(&f(&[("name", ""), ("code", "Bad Code")]));
        let err = input.unwrap_err();
        assert!(err.contains("название") && err.contains("Код"));
    }
}
