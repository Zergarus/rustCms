//! Свойства заказа типа плательщика: группы, свойства, варианты, привязки к оплате и доставке.

use axum::{
    Extension,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::Form;
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use super::require_shop;
use crate::{
    access::Access,
    admin::{parse_sort, render},
    error::{AppError, AppResult},
    state::AppState,
};

/// Виды полей свойства заказа.
pub const KINDS: &[(&str, &str)] = &[
    ("text", "Строка"),
    ("textarea", "Многострочный текст"),
    ("number", "Число"),
    ("select", "Список"),
    ("checkbox", "Да/Нет"),
    ("date", "Дата"),
    ("file", "Файл"),
    ("location", "Местоположение"),
    ("address", "Адрес"),
];

/// Коды блоков формы для фронта.
const BLOCK_CODES: &[&str] = &[
    "buyer",
    "recipient",
    "delivery",
    "payment",
    "comment",
    "address",
];

const FLAGS: &[(&str, &str)] = &[
    ("is_email", "Email покупателя"),
    ("is_phone", "Телефон"),
    ("is_payer", "Плательщик (ФИО)"),
    ("is_profile_name", "Имя профиля"),
    ("is_location", "Местоположение покупателя"),
    ("is_address", "Адрес"),
    ("is_zip", "Индекс"),
];

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct PropertyForm {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub group_id: String,
    pub required: Option<String>,
    pub util: Option<String>,
    pub active: Option<String>,
    /// Отмеченные флаги (`is_email`...).
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub default_value: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub sort: String,
    /// Варианты списка: строка «ЗНАЧЕНИЕ=Название».
    #[serde(default)]
    pub variants: String,
    #[serde(default)]
    pub payment_ids: Vec<i64>,
    #[serde(default)]
    pub delivery_ids: Vec<i64>,
}

#[derive(Debug)]
pub struct FieldInput {
    pub code: String,
    pub name: String,
    pub kind: String,
    pub group_id: Option<i64>,
    pub required: bool,
    pub util: bool,
    pub active: bool,
    pub flags: Vec<String>,
    pub default_value: String,
    pub description: String,
    pub sort: i32,
    pub variants: Vec<(String, String)>,
    pub payment_ids: Vec<i64>,
    pub delivery_ids: Vec<i64>,
}

impl PropertyForm {
    pub fn validate(&self) -> Result<FieldInput, String> {
        let code = self.code.trim();
        if code.is_empty() || !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err("Код — латинские буквы, цифры и _".into());
        }
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Укажите название".into());
        }
        let kind = self.kind.trim();
        if !KINDS.iter().any(|(k, _)| *k == kind) {
            return Err("Неизвестный тип поля".into());
        }
        let variants = if kind == "select" {
            self.variants
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| match l.split_once('=') {
                    Some((v, n)) => (v.trim().to_string(), n.trim().to_string()),
                    None => (l.to_string(), l.to_string()),
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(FieldInput {
            code: code.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            group_id: self.group_id.trim().parse().ok(),
            required: self.required.is_some(),
            util: self.util.is_some(),
            active: self.active.is_some(),
            flags: self
                .flags
                .iter()
                .filter(|f| FLAGS.iter().any(|(k, _)| k == f))
                .cloned()
                .collect(),
            default_value: self.default_value.trim().to_string(),
            description: self.description.trim().to_string(),
            sort: parse_sort(&self.sort),
            variants,
            payment_ids: self.payment_ids.clone(),
            delivery_ids: self.delivery_ids.clone(),
        })
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct GroupForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub block_code: String,
}

#[derive(FromRow, Serialize)]
struct GroupRow {
    id: i64,
    name: String,
    sort: i32,
    block_code: String,
}

#[derive(FromRow, Serialize)]
struct PropertyRow {
    id: i64,
    group_id: Option<i64>,
    code: String,
    name: String,
    kind: String,
    required: bool,
    util: bool,
    active: bool,
    sort: i32,
    relations: String,
}

async fn person_type_name(state: &AppState, id: i64) -> AppResult<String> {
    sqlx::query_scalar("SELECT name FROM person_types WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

/// Группы и свойства типа плательщика.
pub async fn page(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(person_type): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let person_type_name = person_type_name(&state, person_type).await?;
    let groups: Vec<GroupRow> = sqlx::query_as(
        "SELECT id, name, sort, block_code FROM order_property_groups
         WHERE person_type_id = $1 ORDER BY sort, id",
    )
    .bind(person_type)
    .fetch_all(&state.db)
    .await?;
    let props: Vec<PropertyRow> = sqlx::query_as(
        "SELECT p.id, p.group_id, p.code, p.name, p.kind, p.required, p.util, p.active, p.sort,
                COALESCE((SELECT string_agg(CASE r.entity_type WHEN 'P' THEN 'оплата ' ELSE 'доставка ' END
                                            || r.entity_id, ', ' ORDER BY r.entity_type, r.entity_id)
                          FROM order_property_relations r WHERE r.property_id = p.id), '') AS relations
         FROM order_properties p WHERE p.person_type_id = $1 ORDER BY p.sort, p.id",
    )
    .bind(person_type)
    .fetch_all(&state.db)
    .await?;
    render(
        &state,
        "shop/order_props.html",
        context! { user, person_type, person_type_name, groups, props, kinds => KINDS },
    )
}

// --- группы

async fn group_page(
    state: &AppState,
    user: Access,
    person_type: i64,
    id: Option<i64>,
    form: GroupForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let person_type_name = person_type_name(state, person_type).await?;
    render(
        state,
        "shop/order_prop_group_form.html",
        context! { user, person_type, person_type_name, id, form, error, block_codes => BLOCK_CODES },
    )
}

pub async fn group_new(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(person_type): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let form = GroupForm {
        sort: "100".into(),
        ..Default::default()
    };
    group_page(&state, user, person_type, None, form, None).await
}

pub async fn group_edit(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let (person_type, name, sort, block_code): (i64, String, i32, String) = sqlx::query_as(
        "SELECT person_type_id, name, sort, block_code FROM order_property_groups WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let form = GroupForm {
        name,
        sort: sort.to_string(),
        block_code,
    };
    group_page(&state, user, person_type, Some(id), form, None).await
}

async fn group_save(
    state: &AppState,
    user: Access,
    person_type: i64,
    id: Option<i64>,
    form: GroupForm,
) -> AppResult<Response> {
    let name = form.name.trim().to_string();
    let block = form.block_code.trim().to_string();
    if name.is_empty() {
        return Ok(group_page(
            state,
            user,
            person_type,
            id,
            form,
            Some("Укажите название".into()),
        )
        .await?
        .into_response());
    }
    if !block
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Ok(group_page(
            state,
            user,
            person_type,
            id,
            form,
            Some("Код блока — строчные латинские буквы, цифры и _".into()),
        )
        .await?
        .into_response());
    }
    match id {
        Some(id) => sqlx::query(
            "UPDATE order_property_groups SET name = $2, sort = $3, block_code = $4 WHERE id = $1",
        )
        .bind(id),
        None => sqlx::query(
            "INSERT INTO order_property_groups (person_type_id, name, sort, block_code) VALUES ($1, $2, $3, $4)",
        )
        .bind(person_type),
    }
    .bind(&name)
    .bind(parse_sort(&form.sort))
    .bind(&block)
    .execute(&state.db)
    .await?;
    Ok(Redirect::to(&format!("/admin/shop/person-types/{person_type}/props")).into_response())
}

pub async fn group_create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(person_type): Path<i64>,
    Form(form): Form<GroupForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    person_type_name(&state, person_type).await?;
    group_save(&state, user, person_type, None, form).await
}

pub async fn group_update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<GroupForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let person_type: i64 =
        sqlx::query_scalar("SELECT person_type_id FROM order_property_groups WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?;
    group_save(&state, user, person_type, Some(id), form).await
}

/// Удаляет группу; её свойства остаются без группы.
pub async fn group_delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let person_type: i64 = sqlx::query_scalar(
        "DELETE FROM order_property_groups WHERE id = $1 RETURNING person_type_id",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Redirect::to(&format!("/admin/shop/person-types/{person_type}/props")).into_response())
}

// --- свойства

async fn prop_page(
    state: &AppState,
    user: Access,
    person_type: i64,
    id: Option<i64>,
    form: PropertyForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let person_type_name = person_type_name(state, person_type).await?;
    let groups: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, name FROM order_property_groups WHERE person_type_id = $1 ORDER BY sort, id",
    )
    .bind(person_type)
    .fetch_all(&state.db)
    .await?;
    let payments: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM pay_systems ORDER BY sort, id")
            .fetch_all(&state.db)
            .await?;
    let deliveries: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM deliveries ORDER BY sort, id")
            .fetch_all(&state.db)
            .await?;
    render(
        state,
        "shop/order_prop_form.html",
        context! {
            user, person_type, person_type_name, id, form, error, groups, payments, deliveries,
            kinds => KINDS, flags => FLAGS,
        },
    )
}

pub async fn prop_new(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(person_type): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let form = PropertyForm {
        kind: "text".into(),
        sort: "100".into(),
        active: Some("on".into()),
        ..Default::default()
    };
    prop_page(&state, user, person_type, None, form, None).await
}

#[derive(FromRow)]
struct PropertyEdit {
    person_type_id: i64,
    group_id: Option<i64>,
    code: String,
    name: String,
    kind: String,
    required: bool,
    util: bool,
    active: bool,
    is_email: bool,
    is_phone: bool,
    is_payer: bool,
    is_profile_name: bool,
    is_location: bool,
    is_address: bool,
    is_zip: bool,
    default_value: String,
    description: String,
    sort: i32,
}

pub async fn prop_edit(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    require_shop(&user)?;
    let p: PropertyEdit = sqlx::query_as(
        "SELECT person_type_id, group_id, code, name, kind, required, util, active, is_email, is_phone,
                is_payer, is_profile_name, is_location, is_address, is_zip, default_value, description, sort
         FROM order_properties WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let variants: Vec<(String, String)> = sqlx::query_as(
        "SELECT value, name FROM order_property_variants WHERE property_id = $1 ORDER BY sort, id",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let relations: Vec<(String, i64)> = sqlx::query_as(
        "SELECT entity_type::text, entity_id FROM order_property_relations WHERE property_id = $1",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let flags = [
        ("is_email", p.is_email),
        ("is_phone", p.is_phone),
        ("is_payer", p.is_payer),
        ("is_profile_name", p.is_profile_name),
        ("is_location", p.is_location),
        ("is_address", p.is_address),
        ("is_zip", p.is_zip),
    ]
    .into_iter()
    .filter(|(_, on)| *on)
    .map(|(k, _)| k.to_string())
    .collect();
    let on = |b: bool| b.then(|| "on".to_string());
    let form = PropertyForm {
        code: p.code,
        name: p.name,
        kind: p.kind,
        group_id: p.group_id.map(|g| g.to_string()).unwrap_or_default(),
        required: on(p.required),
        util: on(p.util),
        active: on(p.active),
        flags,
        default_value: p.default_value,
        description: p.description,
        sort: p.sort.to_string(),
        variants: variants
            .iter()
            .map(|(v, n)| format!("{v}={n}"))
            .collect::<Vec<_>>()
            .join("\n"),
        payment_ids: relations
            .iter()
            .filter(|(t, _)| t == "P")
            .map(|(_, id)| *id)
            .collect(),
        delivery_ids: relations
            .iter()
            .filter(|(t, _)| t == "D")
            .map(|(_, id)| *id)
            .collect(),
    };
    prop_page(&state, user, p.person_type_id, Some(id), form, None).await
}

async fn prop_save(
    state: &AppState,
    user: Access,
    person_type: i64,
    id: Option<i64>,
    form: PropertyForm,
) -> AppResult<Response> {
    let input = match form.validate() {
        Ok(i) => i,
        Err(e) => {
            return Ok(prop_page(state, user, person_type, id, form, Some(e))
                .await?
                .into_response());
        }
    };
    let flag = |name: &str| input.flags.iter().any(|f| f == name);
    let mut tx = state.db.begin().await?;
    let saved: i64 = match id {
        Some(id) => sqlx::query_scalar(
            "UPDATE order_properties SET group_id = $2, code = $3, name = $4, kind = $5, required = $6,
                 util = $7, active = $8, is_email = $9, is_phone = $10, is_payer = $11,
                 is_profile_name = $12, is_location = $13, is_address = $14, is_zip = $15,
                 default_value = $16, description = $17, sort = $18
             WHERE id = $1 RETURNING id",
        )
        .bind(id),
        None => sqlx::query_scalar(
            "INSERT INTO order_properties (person_type_id, group_id, code, name, kind, required, util,
                 active, is_email, is_phone, is_payer, is_profile_name, is_location, is_address, is_zip,
                 default_value, description, sort)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
             RETURNING id",
        )
        .bind(person_type),
    }
    .bind(input.group_id)
    .bind(&input.code)
    .bind(&input.name)
    .bind(&input.kind)
    .bind(input.required)
    .bind(input.util)
    .bind(input.active)
    .bind(flag("is_email"))
    .bind(flag("is_phone"))
    .bind(flag("is_payer"))
    .bind(flag("is_profile_name"))
    .bind(flag("is_location"))
    .bind(flag("is_address"))
    .bind(flag("is_zip"))
    .bind(&input.default_value)
    .bind(&input.description)
    .bind(input.sort)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    sqlx::query("DELETE FROM order_property_variants WHERE property_id = $1")
        .bind(saved)
        .execute(&mut *tx)
        .await?;
    for (i, (value, name)) in input.variants.iter().enumerate() {
        sqlx::query(
            "INSERT INTO order_property_variants (property_id, value, name, sort) VALUES ($1, $2, $3, $4)",
        )
        .bind(saved)
        .bind(value)
        .bind(name)
        .bind((i as i32 + 1) * 100)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM order_property_relations WHERE property_id = $1")
        .bind(saved)
        .execute(&mut *tx)
        .await?;
    for (kind, ids) in [("P", &input.payment_ids), ("D", &input.delivery_ids)] {
        sqlx::query(
            "INSERT INTO order_property_relations (property_id, entity_type, entity_id)
             SELECT $1, $2, unnest($3::bigint[]) ON CONFLICT DO NOTHING",
        )
        .bind(saved)
        .bind(kind)
        .bind(ids)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Redirect::to(&format!("/admin/shop/person-types/{person_type}/props")).into_response())
}

pub async fn prop_create(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(person_type): Path<i64>,
    Form(form): Form<PropertyForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    person_type_name(&state, person_type).await?;
    prop_save(&state, user, person_type, None, form).await
}

pub async fn prop_update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<PropertyForm>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let person_type: i64 =
        sqlx::query_scalar("SELECT person_type_id FROM order_properties WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?;
    prop_save(&state, user, person_type, Some(id), form).await
}

/// Удаляет свойство; значения в оформленных заказах остаются (код и название скопированы).
pub async fn prop_delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    require_shop(&user)?;
    let person_type: i64 =
        sqlx::query_scalar("DELETE FROM order_properties WHERE id = $1 RETURNING person_type_id")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?;
    Ok(Redirect::to(&format!("/admin/shop/person-types/{person_type}/props")).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(code: &str, kind: &str) -> PropertyForm {
        PropertyForm {
            code: code.into(),
            name: "Поле".into(),
            kind: kind.into(),
            variants: "OOO=ООО\nIP=ИП\n\n".into(),
            ..Default::default()
        }
    }

    #[test]
    fn property_input_variants_and_kind() {
        let input = form("UR_LICO", "select").validate().unwrap();
        assert_eq!(
            input.variants,
            vec![
                ("OOO".to_string(), "ООО".to_string()),
                ("IP".to_string(), "ИП".to_string())
            ]
        );
        assert!(form("fio", "text").validate().unwrap().variants.is_empty());
        assert_eq!(
            form("bad code", "text").validate().unwrap_err(),
            "Код — латинские буквы, цифры и _"
        );
        assert_eq!(
            form("x", "radio").validate().unwrap_err(),
            "Неизвестный тип поля"
        );
        let mut plain = form("x", "select");
        plain.variants = "Да".into();
        assert_eq!(
            plain.validate().unwrap().variants,
            vec![("Да".to_string(), "Да".to_string())]
        );
    }
}
