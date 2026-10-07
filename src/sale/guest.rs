//! Покупатель-гость: контакты из свойств заказа, поиск или создание пользователя
//! (как provisioner модуля bxapi).

use sqlx::PgPool;

use super::OrderProperty;
use crate::auth::hash_password;

/// Контакты гостя из свойств заказа.
#[derive(Debug, Default, PartialEq)]
pub struct GuestContacts {
    /// В нижнем регистре, без пробелов.
    pub email: String,
    /// Только цифры и ведущий `+`.
    pub phone: String,
    pub full_name: String,
}

fn phone_digits(raw: &str) -> String {
    raw.chars().filter(char::is_ascii_digit).collect()
}

/// Email, телефон и ФИО по флагам свойств (`is_email`, `is_phone`, `is_payer`).
pub fn guest_contacts(props: &[&OrderProperty], values: &[(i64, String)]) -> GuestContacts {
    let mut c = GuestContacts::default();
    for (id, value) in values {
        let Some(p) = props.iter().find(|p| p.id == *id) else {
            continue;
        };
        if p.is_email && c.email.is_empty() {
            c.email = value.trim().to_lowercase();
        } else if p.is_phone && c.phone.is_empty() {
            let digits = phone_digits(value);
            if !digits.is_empty() {
                let plus = if value.trim_start().starts_with('+') {
                    "+"
                } else {
                    ""
                };
                c.phone = format!("{plus}{digits}");
            }
        } else if (p.is_payer || p.is_profile_name) && c.full_name.is_empty() {
            c.full_name = value.trim().to_string();
        }
    }
    c
}

/// Пользователь по email, затем по телефону (сравниваются только цифры).
pub async fn find_user(db: &PgPool, c: &GuestContacts) -> sqlx::Result<Option<i64>> {
    if !c.email.is_empty()
        && let Some(id) = sqlx::query_scalar(
            "SELECT id FROM users WHERE lower(trim(email)) = $1 ORDER BY active DESC, id LIMIT 1",
        )
        .bind(&c.email)
        .fetch_optional(db)
        .await?
    {
        return Ok(Some(id));
    }
    let digits = phone_digits(&c.phone);
    if digits.len() < 5 {
        return Ok(None);
    }
    sqlx::query_scalar(
        "SELECT id FROM users WHERE phone <> '' AND regexp_replace(phone, '[^0-9]', '', 'g') = $1
         ORDER BY active DESC, id LIMIT 1",
    )
    .bind(digits)
    .fetch_optional(db)
    .await
}

/// Новый пользователь для гостя: логин — email (занят — `email_2`…), случайный пароль.
pub async fn create_user(db: &PgPool, c: &GuestContacts, group_ids: &[i64]) -> anyhow::Result<i64> {
    let base = if c.email.is_empty() {
        phone_digits(&c.phone)
    } else {
        c.email.clone()
    };
    anyhow::ensure!(!base.is_empty(), "нет email и телефона покупателя");
    let password = hex::encode(rand::random::<[u8; 12]>());
    let hash = hash_password(password).await?;
    let mut tx = db.begin().await?;
    let mut login = base.clone();
    let mut n = 1;
    while sqlx::query_scalar::<_, i64>("SELECT id FROM users WHERE login = $1")
        .bind(&login)
        .fetch_optional(&mut *tx)
        .await?
        .is_some()
    {
        n += 1;
        login = format!("{base}_{n}");
    }
    let email = Some(c.email.clone()).filter(|e| !e.is_empty());
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO users (login, email, password_hash, name, phone) VALUES ($1, $2, $3, $4, $5)
         RETURNING id",
    )
    .bind(&login)
    .bind(email)
    .bind(hash)
    .bind(&c.full_name)
    .bind(&c.phone)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO user_groups (user_id, group_id) SELECT $1, id FROM groups WHERE id = ANY($2)",
    )
    .bind(id)
    .bind(group_ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sale::OrderProperty;

    fn prop(id: i64, code: &str) -> OrderProperty {
        OrderProperty {
            id,
            person_type_id: 1,
            group_id: None,
            code: code.into(),
            name: code.into(),
            kind: "text".into(),
            required: false,
            util: false,
            is_email: code == "EMAIL",
            is_phone: code == "phone",
            is_payer: code == "fio",
            is_profile_name: false,
            is_location: false,
            is_address: false,
            is_zip: false,
            default_value: String::new(),
            description: String::new(),
            sort: 0,
            active: true,
        }
    }

    #[test]
    fn guest_contacts_normalizes() {
        let props = [prop(22, "EMAIL"), prop(1, "phone"), prop(5, "fio")];
        let refs: Vec<&OrderProperty> = props.iter().collect();
        let c = guest_contacts(
            &refs,
            &[
                (22, "  Ivan@Mail.RU ".into()),
                (1, "+7 (999) 111-22-33".into()),
                (5, "Иванов Иван".into()),
            ],
        );
        assert_eq!(
            c,
            GuestContacts {
                email: "ivan@mail.ru".into(),
                phone: "+79991112233".into(),
                full_name: "Иванов Иван".into(),
            }
        );
        assert_eq!(guest_contacts(&refs, &[]), GuestContacts::default());
    }
}
