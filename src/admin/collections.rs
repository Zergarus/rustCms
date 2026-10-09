use std::collections::HashMap;

use axum::{
    Extension, Form,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use minijinja::{Value, context};
use serde::{Deserialize, Serialize};

use super::{parse_sort, render};
use crate::{
    access::{Access, COLLECTIONS_MANAGE, Level},
    collection::{
        CollectionInput, Field, FieldInput, FieldOption, fields, is_valid_code,
        repo::{self, OptionInput},
        slugify,
    },
    error::{AppError, AppResult, is_unique_violation},
    state::AppState,
};

#[derive(Default, Deserialize, Serialize)]
pub struct CollectionForm {
    #[serde(default)]
    code: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    api_enabled: Option<String>,
    is_catalog: Option<String>,
    #[serde(default)]
    sort: String,
}

impl CollectionForm {
    fn validate(&self) -> Result<CollectionInput, String> {
        let name = self.name.trim();
        let code = self.code.trim();
        if name.is_empty() {
            return Err("Укажите название".into());
        }
        if !is_valid_code(code) {
            return Err("Код: только латиница в нижнем регистре, цифры, «_» и «-»".into());
        }
        Ok(CollectionInput {
            code: code.to_string(),
            name: name.to_string(),
            description: self.description.trim().to_string(),
            api_enabled: self.api_enabled.is_some(),
            is_catalog: self.is_catalog.is_some(),
            sort: parse_sort(&self.sort),
        })
    }
}

#[derive(Default, Deserialize, Serialize)]
pub struct FieldForm {
    #[serde(default)]
    code: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: String,
    is_required: Option<String>,
    #[serde(default)]
    sort: String,
    multiple: Option<String>,
    #[serde(default)]
    link_collection_id: String,
}

impl FieldForm {
    fn validate(&self) -> Result<FieldInput, String> {
        let name = self.name.trim();
        let code = self.code.trim();
        if name.is_empty() {
            return Err("Укажите название свойства".into());
        }
        if !is_valid_code(code) {
            return Err("Код свойства: только латиница в нижнем регистре, цифры, «_» и «-»".into());
        }
        let kind = fields::kind(&self.kind).ok_or("Неизвестный тип свойства")?;
        let multiple = self.multiple.is_some();
        if multiple && !kind.multiple {
            return Err(format!("Тип «{}» не может быть множественным", kind.name));
        }
        let link_collection_id = match self.link_collection_id.trim() {
            "" => None,
            _ if kind.code != "element" => None,
            raw => Some(raw.parse().map_err(|_| "Неверный инфоблок привязки")?),
        };
        Ok(FieldInput {
            code: code.to_string(),
            name: name.to_string(),
            kind: self.kind.clone(),
            is_required: self.is_required.is_some(),
            sort: parse_sort(&self.sort),
            multiple,
            link_collection_id,
        })
    }
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    let items: Vec<_> = repo::list_collections(&state.db)
        .await?
        .into_iter()
        .filter(|s| user.collection_level(s.collection.id) >= Level::Read)
        .collect();
    let can_manage = user.can(COLLECTIONS_MANAGE);
    render(&state, "iblocks.html", context! { user, items, can_manage })
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    user.require(COLLECTIONS_MANAGE)?;
    let form = CollectionForm {
        api_enabled: Some("on".into()),
        sort: "500".into(),
        ..Default::default()
    };
    render(&state, "iblock_form.html", context! { user, form })
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<CollectionForm>,
) -> AppResult<Response> {
    user.require(COLLECTIONS_MANAGE)?;
    let error = match form.validate() {
        Ok(input) => match repo::create_collection(&state.db, &input).await {
            Ok(iblock) => {
                return Ok(Redirect::to(&format!("/admin/iblocks/{}", iblock.id)).into_response());
            }
            Err(e) if is_unique_violation(&e) => "Инфоблок с таким кодом уже существует".into(),
            Err(e) => return Err(e.into()),
        },
        Err(msg) => msg,
    };
    Ok(render(&state, "iblock_form.html", context! { user, form, error })?.into_response())
}

/// Страница редактирования инфоблока вместе со списком свойств.
async fn render_edit(
    state: &AppState,
    user: Access,
    id: i64,
    form: Option<CollectionForm>,
    error: Option<String>,
    prop_form: FieldForm,
    prop_error: Option<String>,
) -> AppResult<Html<String>> {
    let iblock = repo::get_collection(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let properties = repo::list_fields(&state.db, id).await?;
    let form = form.unwrap_or_else(|| CollectionForm {
        code: iblock.code.clone(),
        name: iblock.name.clone(),
        description: iblock.description.clone(),
        api_enabled: iblock.api_enabled.then(|| "on".into()),
        is_catalog: iblock.is_catalog.then(|| "on".into()),
        sort: iblock.sort.to_string(),
    });
    render(
        state,
        "iblock_form.html",
        context! {
            user,
            iblock,
            form,
            error,
            properties,
            prop_form,
            prop_error,
            iblocks => repo::list_collections(&state.db).await?,
            kinds => Value::from_serialize(fields::KINDS),
        },
    )
}

fn empty_field_form() -> FieldForm {
    FieldForm {
        kind: "string".into(),
        sort: "500".into(),
        ..Default::default()
    }
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    user.require(COLLECTIONS_MANAGE)?;
    render_edit(&state, user, id, None, None, empty_field_form(), None).await
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<CollectionForm>,
) -> AppResult<Response> {
    user.require(COLLECTIONS_MANAGE)?;
    let error = match form.validate() {
        Ok(input) => match repo::update_collection(&state.db, id, &input).await {
            Ok(true) => {
                return Ok(Redirect::to(&format!("/admin/iblocks/{id}")).into_response());
            }
            Ok(false) => return Err(AppError::NotFound),
            Err(e) if is_unique_violation(&e) => "Инфоблок с таким кодом уже существует".into(),
            Err(e) => return Err(e.into()),
        },
        Err(msg) => msg,
    };
    let page = render_edit(
        &state,
        user,
        id,
        Some(form),
        Some(error),
        empty_field_form(),
        None,
    );
    Ok(page.await?.into_response())
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    user.require(COLLECTIONS_MANAGE)?;
    repo::delete_collection(&state.db, id).await?;
    Ok(Redirect::to("/admin/iblocks"))
}

pub async fn add_property(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<FieldForm>,
) -> AppResult<Response> {
    user.require(COLLECTIONS_MANAGE)?;
    let error = match form.validate() {
        Ok(input) => match repo::create_field(&state.db, id, &input).await {
            // У списка сразу переходим к вариантам значений
            Ok(prop_id) if input.kind == "list" => {
                return Ok(Redirect::to(&format!("/admin/properties/{prop_id}")).into_response());
            }
            Ok(_) => return Ok(Redirect::to(&format!("/admin/iblocks/{id}")).into_response()),
            Err(e) if is_unique_violation(&e) => "Свойство с таким кодом уже есть".into(),
            Err(e) => return Err(e.into()),
        },
        Err(msg) => msg,
    };
    let page = render_edit(&state, user, id, None, None, form, Some(error));
    Ok(page.await?.into_response())
}

pub async fn delete_field(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    user.require(COLLECTIONS_MANAGE)?;
    let collection_id = repo::delete_field(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Redirect::to(&format!("/admin/iblocks/{collection_id}")))
}

/// Страница свойства: основные настройки и (для списка) варианты значений.
async fn render_property(
    state: &AppState,
    user: Access,
    property: Field,
    form: Option<FieldForm>,
    error: Option<String>,
    enum_error: Option<String>,
) -> AppResult<Html<String>> {
    let iblock = repo::get_collection(&state.db, property.collection_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let form = form.unwrap_or_else(|| FieldForm {
        code: property.code.clone(),
        name: property.name.clone(),
        kind: property.kind.clone(),
        is_required: property.is_required.then(|| "on".into()),
        sort: property.sort.to_string(),
        multiple: property.multiple.then(|| "on".into()),
        link_collection_id: property
            .link_collection_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
    });
    let enums = repo::list_options(&state.db, property.id).await?;
    let kind = fields::kind(&property.kind).map(|k| k.name);
    render(
        state,
        "property_form.html",
        context! {
            user, iblock, property, form, error, enums, enum_error, kind,
            new_rows => NEW_ENUM_ROWS,
            iblocks => repo::list_collections(&state.db).await?,
        },
    )
}

/// Сколько пустых строк для новых вариантов показывать в форме списка.
const NEW_ENUM_ROWS: usize = 5;

async fn load_property(state: &AppState, user: &Access, id: i64) -> AppResult<Field> {
    user.require(COLLECTIONS_MANAGE)?;
    repo::get_field(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn property_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let property = load_property(&state, &user, id).await?;
    render_property(&state, user, property, None, None, None).await
}

/// Код и тип свойства не меняются: от них зависят уже сохранённые значения.
pub async fn update_field(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(mut form): Form<FieldForm>,
) -> AppResult<Response> {
    let property = load_property(&state, &user, id).await?;
    form.code = property.code.clone();
    form.kind = property.kind.clone();
    form.multiple = property.multiple.then(|| "on".into());
    let error = match form.validate() {
        Ok(input) => {
            repo::update_field(&state.db, id, &input).await?;
            let url = format!("/admin/iblocks/{}", property.collection_id);
            return Ok(Redirect::to(&url).into_response());
        }
        Err(msg) => msg,
    };
    let page = render_property(&state, user, property, Some(form), Some(error), None);
    Ok(page.await?.into_response())
}

/// Разбирает строки вариантов из формы: поля `enum_<поле>_<номер строки>`.
/// Пустые новые строки пропускаются; пустой XML_ID генерируется из значения.
fn parse_enums(
    fields: &HashMap<String, String>,
    existing: &[FieldOption],
) -> Result<(Vec<OptionInput>, Vec<i64>), String> {
    let mut rows: Vec<usize> = fields
        .keys()
        .filter_map(|k| k.strip_prefix("enum_value_")?.parse().ok())
        .collect();
    rows.sort_unstable();

    let get = |field: &str, row: usize| {
        fields
            .get(&format!("enum_{field}_{row}"))
            .map(|s| s.trim())
            .unwrap_or("")
    };
    let mut items = Vec::new();
    let mut delete = Vec::new();
    let mut errors = Vec::new();
    for row in rows {
        let id = match get("id", row) {
            "" => None,
            raw => match raw.parse::<i64>() {
                Ok(id) if existing.iter().any(|e| e.id == id) => Some(id),
                _ => return Err("Вариант не найден — обновите страницу".into()),
            },
        };
        if let Some(id) = id
            && !get("delete", row).is_empty()
        {
            delete.push(id);
            continue;
        }
        let value = get("value", row);
        if value.is_empty() {
            if id.is_some() {
                errors.push("Значение варианта не может быть пустым".to_string());
            }
            continue;
        }
        let xml_id = match get("xml_id", row) {
            "" => match slugify(value) {
                slug if slug.is_empty() => format!("value-{}", items.len() + 1),
                slug => slug,
            },
            xml_id => xml_id.to_string(),
        };
        if items.iter().any(|i: &OptionInput| i.xml_id == xml_id) {
            errors.push(format!("XML_ID «{xml_id}» повторяется"));
        }
        items.push(OptionInput {
            id,
            value: value.to_string(),
            xml_id,
            sort: parse_sort(get("sort", row)),
            is_default: !get("default", row).is_empty(),
        });
    }
    if errors.is_empty() {
        Ok((items, delete))
    } else {
        Err(errors.join("; "))
    }
}

pub async fn save_options(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(fields): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let property = load_property(&state, &user, id).await?;
    if property.kind != "list" {
        return Err(AppError::BadRequest("свойство не является списком".into()));
    }
    let existing = repo::list_options(&state.db, id).await?;
    let error = match parse_enums(&fields, &existing) {
        Ok((items, delete)) => {
            match repo::save_options(&state.db, &property, &items, &delete).await {
                Ok(()) => {
                    return Ok(Redirect::to(&format!("/admin/properties/{id}")).into_response());
                }
                Err(e) if is_unique_violation(&e) => {
                    "XML_ID вариантов должны быть уникальны".into()
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(msg) => msg,
    };
    let page = render_property(&state, user, property, None, None, Some(error));
    Ok(page.await?.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_parsing() {
        let existing = [FieldOption {
            id: 7,
            field_id: 1,
            value: "Б/у".into(),
            xml_id: "used".into(),
            sort: 100,
            is_default: false,
        }];
        let fields: HashMap<String, String> = [
            ("enum_id_0", "7"),
            ("enum_value_0", "Б/у"),
            ("enum_xml_id_0", "used"),
            ("enum_sort_0", "100"),
            ("enum_id_1", ""),
            ("enum_value_1", "Новый"),
            ("enum_xml_id_1", ""),
            ("enum_default_1", "on"),
            ("enum_value_2", ""),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let (items, delete) = parse_enums(&fields, &existing).unwrap();
        assert!(delete.is_empty());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, Some(7));
        assert_eq!(items[1].xml_id, "novyy");
        assert!(items[1].is_default);

        let mut deleting = fields.clone();
        deleting.insert("enum_delete_0".into(), "on".into());
        let (items, delete) = parse_enums(&deleting, &existing).unwrap();
        assert_eq!(delete, [7]);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn property_validation() {
        let form = |kind: &str, multiple: bool| FieldForm {
            code: "p".into(),
            name: "P".into(),
            kind: kind.into(),
            multiple: multiple.then(|| "on".into()),
            link_collection_id: "3".into(),
            ..Default::default()
        };
        assert!(form("boolean", true).validate().is_err());
        assert_eq!(
            form("element", true).validate().unwrap().link_collection_id,
            Some(3)
        );
        assert_eq!(
            form("string", false).validate().unwrap().link_collection_id,
            None
        );
    }
}
