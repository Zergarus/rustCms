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
    in_basket: Option<String>,
    offer_tree: Option<String>,
}

/// Подключение коллекции предложений: `mode` = `none` | `existing` | `create`.
#[derive(Deserialize)]
pub struct OffersForm {
    #[serde(default)]
    mode: String,
    #[serde(default)]
    offers_id: String,
}

/// Ошибка для системного поля связи предложения с товаром.
const SYSTEM_LINK_ERROR: &str = "Системное поле связи с товаром";

impl FieldForm {
    fn validate(&self) -> Result<FieldInput, String> {
        let name = self.name.trim();
        let code = self.code.trim();
        if name.is_empty() {
            return Err("Укажите название поля".into());
        }
        if !is_valid_code(code) {
            return Err("Код поля: только латиница в нижнем регистре, цифры, «_» и «-»".into());
        }
        let kind = fields::kind(&self.kind).ok_or("Неизвестный тип поля")?;
        let multiple = self.multiple.is_some();
        if multiple && !kind.multiple {
            return Err(format!("Тип «{}» не может быть множественным", kind.name));
        }
        let link_collection_id = match self.link_collection_id.trim() {
            "" => None,
            _ if kind.code != "element" => None,
            raw => Some(raw.parse().map_err(|_| "Неверная коллекция привязки")?),
        };
        let offer_tree = self.offer_tree.is_some();
        if offer_tree && !matches!(kind.code, "list" | "element" | "string") {
            return Err("Поле выбора предложения — только список, привязка или строка".into());
        }
        Ok(FieldInput {
            in_basket: self.in_basket.is_some(),
            offer_tree,
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
    render(
        &state,
        "collections.html",
        context! { user, items, can_manage },
    )
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
    render(&state, "collection_form.html", context! { user, form })
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<CollectionForm>,
) -> AppResult<Response> {
    user.require(COLLECTIONS_MANAGE)?;
    let error = match form.validate() {
        Ok(input) => match repo::create_collection(&state.db, &input).await {
            Ok(collection) => {
                return Ok(
                    Redirect::to(&format!("/admin/collections/{}", collection.id)).into_response(),
                );
            }
            Err(e) if is_unique_violation(&e) => "Коллекция с таким кодом уже существует".into(),
            Err(e) => return Err(e.into()),
        },
        Err(msg) => msg,
    };
    Ok(render(
        &state,
        "collection_form.html",
        context! { user, form, error },
    )?
    .into_response())
}

/// Ошибки блоков страницы коллекции, кроме основной формы.
#[derive(Default)]
struct BlockErrors {
    /// Форма добавления поля (и действия над полями).
    field: Option<String>,
    /// Блок «Торговые предложения».
    offers: Option<String>,
}

/// Страница редактирования коллекции вместе со списком полей.
async fn render_edit(
    state: &AppState,
    user: Access,
    id: i64,
    form: Option<CollectionForm>,
    error: Option<String>,
    prop_form: FieldForm,
    errors: BlockErrors,
) -> AppResult<Html<String>> {
    let BlockErrors {
        field: prop_error,
        offers: offers_error,
    } = errors;
    let collection = repo::get_collection(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let properties = repo::list_fields(&state.db, id).await?;
    let offers = repo::offers_collection(&state.db, id).await?;
    let product_collection = match collection.product_collection_id {
        Some(p) => repo::get_collection(&state.db, p).await?,
        None => None,
    };
    let form = form.unwrap_or_else(|| CollectionForm {
        code: collection.code.clone(),
        name: collection.name.clone(),
        description: collection.description.clone(),
        api_enabled: collection.api_enabled.then(|| "on".into()),
        is_catalog: collection.is_catalog.then(|| "on".into()),
        sort: collection.sort.to_string(),
    });
    render(
        state,
        "collection_form.html",
        context! {
            user,
            collection,
            form,
            error,
            properties,
            prop_form,
            prop_error,
            offers,
            product_collection,
            offers_error,
            collections => repo::list_collections(&state.db).await?,
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
    render_edit(
        &state,
        user,
        id,
        None,
        None,
        empty_field_form(),
        BlockErrors::default(),
    )
    .await
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
                return Ok(Redirect::to(&format!("/admin/collections/{id}")).into_response());
            }
            Ok(false) => return Err(AppError::NotFound),
            Err(e) if is_unique_violation(&e) => "Коллекция с таким кодом уже существует".into(),
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
        BlockErrors::default(),
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
    Ok(Redirect::to("/admin/collections"))
}

pub async fn add_field(
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
                return Ok(Redirect::to(&format!("/admin/fields/{prop_id}")).into_response());
            }
            Ok(_) => return Ok(Redirect::to(&format!("/admin/collections/{id}")).into_response()),
            Err(e) if is_unique_violation(&e) => "Поле с таким кодом уже есть".into(),
            Err(e) => return Err(e.into()),
        },
        Err(msg) => msg,
    };
    let page = render_edit(
        &state,
        user,
        id,
        None,
        None,
        form,
        BlockErrors {
            field: Some(error),
            ..Default::default()
        },
    );
    Ok(page.await?.into_response())
}

/// Блок «Торговые предложения»: нет / существующая коллекция / новая.
pub async fn set_offers(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<OffersForm>,
) -> AppResult<Response> {
    user.require(COLLECTIONS_MANAGE)?;
    let collection = repo::get_collection(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let result = apply_offers(&state, &collection, &form).await?;
    match result {
        Ok(()) => Ok(Redirect::to(&format!("/admin/collections/{id}")).into_response()),
        Err(error) => {
            let page = render_edit(
                &state,
                user,
                id,
                None,
                None,
                empty_field_form(),
                BlockErrors {
                    offers: Some(error),
                    ..Default::default()
                },
            );
            Ok(page.await?.into_response())
        }
    }
}

/// Меняет связь коллекции товаров с коллекцией предложений; `Err` — текст для формы.
async fn apply_offers(
    state: &AppState,
    collection: &crate::collection::Collection,
    form: &OffersForm,
) -> AppResult<Result<(), String>> {
    let db = &state.db;
    if collection.product_collection_id.is_some() {
        return Ok(Err(
            "Это коллекция предложений: подключать к ней предложения нельзя".into(),
        ));
    }
    if !collection.is_catalog {
        return Ok(Err(
            "Предложения есть только у коллекции торгового каталога".into(),
        ));
    }
    let current = repo::offers_collection(db, collection.id).await?;
    let target = match form.mode.as_str() {
        "none" => None,
        "existing" => {
            let Some(target) = form
                .offers_id
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|t| *t != collection.id)
            else {
                return Ok(Err("Выберите коллекцию предложений".into()));
            };
            let Some(offers) = repo::get_collection(db, target).await? else {
                return Ok(Err("Коллекция предложений не найдена".into()));
            };
            if !offers.is_catalog {
                return Ok(Err(
                    "Коллекция предложений должна быть торговым каталогом".into()
                ));
            }
            if offers
                .product_collection_id
                .is_some_and(|p| p != collection.id)
            {
                return Ok(Err(format!(
                    "Коллекция «{}» уже подключена как предложения другой коллекции товаров",
                    offers.name
                )));
            }
            if repo::offers_collection(db, offers.id).await?.is_some() {
                return Ok(Err(
                    "У выбранной коллекции есть свои предложения — она не может быть предложениями"
                        .into(),
                ));
            }
            let foreign = repo::foreign_offers_count(db, offers.id, collection.id).await?;
            if foreign > 0 {
                return Ok(Err(format!(
                    "В коллекции «{}» есть предложения другого товара ({foreign}) — её нельзя использовать",
                    offers.name
                )));
            }
            Some(Some(offers.id))
        }
        "create" => Some(None),
        _ => return Ok(Err("Неизвестный вариант".into())),
    };
    // Прежняя связь снимается, если заменяется другой; с записями — отказ
    let keep = matches!((&target, &current), (Some(Some(t)), Some(c)) if *t == c.id);
    if keep {
        return Ok(Ok(()));
    }
    if current.is_some() {
        match repo::unlink_offers(db, collection.id).await {
            Ok(()) => {}
            Err(repo::UnlinkError::HasOffers(n)) => {
                return Ok(Err(format!("Сначала удалите предложения ({n})")));
            }
            Err(repo::UnlinkError::Db(e)) => return Err(e.into()),
        }
    }
    match target {
        None => {}
        Some(Some(offers_id)) => repo::link_offers(db, collection.id, offers_id).await?,
        Some(None) => match repo::create_offer_collection(db, collection).await {
            Ok(_) => {}
            Err(e) if is_unique_violation(&e) => {
                return Ok(Err(format!(
                    "Коллекция с кодом «{}_offers» уже существует",
                    collection.code
                )));
            }
            Err(e) => return Err(e.into()),
        },
    }
    Ok(Ok(()))
}

pub async fn delete_field(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    user.require(COLLECTIONS_MANAGE)?;
    if repo::is_sku_field(&state.db, id).await? {
        let field = repo::get_field(&state.db, id)
            .await?
            .ok_or(AppError::NotFound)?;
        let page = render_edit(
            &state,
            user,
            field.collection_id,
            None,
            None,
            empty_field_form(),
            BlockErrors {
                field: Some(SYSTEM_LINK_ERROR.to_string()),
                ..Default::default()
            },
        );
        return Ok(page.await?.into_response());
    }
    let collection_id = repo::delete_field(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Redirect::to(&format!("/admin/collections/{collection_id}")).into_response())
}

/// Страница поля: основные настройки и (для списка) варианты.
async fn render_property(
    state: &AppState,
    user: Access,
    property: Field,
    form: Option<FieldForm>,
    error: Option<String>,
    enum_error: Option<String>,
) -> AppResult<Html<String>> {
    let collection = repo::get_collection(&state.db, property.collection_id)
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
        in_basket: property.in_basket.then(|| "on".into()),
        offer_tree: property.offer_tree.then(|| "on".into()),
    });
    let enums = repo::list_options(&state.db, property.id).await?;
    let kind = fields::kind(&property.kind).map(|k| k.name);
    render(
        state,
        "field_form.html",
        context! {
            user, collection, property, form, error, enums, enum_error, kind,
            new_rows => NEW_ENUM_ROWS,
            collections => repo::list_collections(&state.db).await?,
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

pub async fn field_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let property = load_property(&state, &user, id).await?;
    render_property(&state, user, property, None, None, None).await
}

/// Код и тип поля не меняются: от них зависят уже сохранённые значения.
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
    // Связь предложения с товаром: привязку не меняем, пока связь есть
    let system = repo::is_sku_field(&state.db, id).await?;
    let error = match form.validate() {
        Ok(input) if system && input.link_collection_id != property.link_collection_id => {
            SYSTEM_LINK_ERROR.to_string()
        }
        Ok(input) => {
            repo::update_field(&state.db, id, &input).await?;
            let url = format!("/admin/collections/{}", property.collection_id);
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
        return Err(AppError::BadRequest("поле не является списком".into()));
    }
    let existing = repo::list_options(&state.db, id).await?;
    let error = match parse_enums(&fields, &existing) {
        Ok((items, delete)) => {
            match repo::save_options(&state.db, &property, &items, &delete).await {
                Ok(()) => {
                    return Ok(Redirect::to(&format!("/admin/fields/{id}")).into_response());
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
        let tree = |kind: &str| FieldForm {
            offer_tree: Some("on".into()),
            ..form(kind, false)
        };
        assert!(tree("text").validate().is_err());
        assert!(tree("list").validate().unwrap().offer_tree);
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
