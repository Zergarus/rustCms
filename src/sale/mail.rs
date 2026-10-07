//! Письма по заказу (почтовые события модуля `sale`).

use std::collections::HashMap;

use super::repo::{self, OrderView};
use crate::state::AppState;

/// Сумма как в письмах Битрикса: «1 394.22 руб.», нулевые копейки скрываются.
fn money(value: f64, currency: &str) -> String {
    let cents = (value * 100.0).round() as i64;
    let (int, frac) = (cents.abs() / 100, cents.abs() % 100);
    let digits = int.to_string();
    let mut grouped = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(' ');
        }
        grouped.push(ch);
    }
    let sign = if cents < 0 { "-" } else { "" };
    let number = if frac == 0 {
        format!("{sign}{grouped}")
    } else {
        format!("{sign}{grouped}.{frac:02}")
    };
    let suffix = if currency == "RUB" {
        "руб."
    } else {
        currency
    };
    format!("{number} {suffix}")
}

fn quantity(q: f64) -> String {
    if q.fract() == 0.0 {
        format!("{}", q as i64)
    } else {
        format!("{q}")
    }
}

/// Поля почтового события заказа (`#ORDER_ID#`, `#PRICE#`, `#ORDER_LIST#`…).
pub fn order_fields(o: &OrderView, sale_email: &str) -> HashMap<String, String> {
    let prop = |pick: fn(&repo::OrderPropertyValue) -> bool| {
        o.properties
            .iter()
            .find(|p| pick(p) && !p.value.is_empty())
            .map(|p| p.value.clone())
    };
    let email = prop(|p| p.is_email).unwrap_or_else(|| o.user_email.clone());
    let user = prop(|p| p.is_payer).unwrap_or_else(|| o.user_name.clone());
    let list: Vec<String> = o
        .items
        .iter()
        .map(|i| {
            format!(
                "{} - {} шт.: {}",
                i.name,
                quantity(i.quantity),
                money(i.price, &o.currency)
            )
        })
        .collect();
    let id = o.id.to_string();
    HashMap::from([
        ("ORDER_ID".to_string(), o.account_number.clone()),
        ("ORDER_REAL_ID".to_string(), id),
        (
            "ORDER_ACCOUNT_NUMBER_ENCODE".to_string(),
            o.account_number.clone(),
        ),
        (
            "ORDER_DATE".to_string(),
            o.created_at.format("%d.%m.%Y").to_string(),
        ),
        ("ORDER_USER".to_string(), user),
        ("PRICE".to_string(), money(o.price, &o.currency)),
        (
            "DELIVERY_PRICE".to_string(),
            money(o.delivery_price, &o.currency),
        ),
        ("EMAIL".to_string(), email),
        ("BCC".to_string(), sale_email.to_string()),
        ("SALE_EMAIL".to_string(), sale_email.to_string()),
        ("ORDER_LIST".to_string(), list.join("\n")),
        ("ORDER_STATUS".to_string(), o.status_name.clone()),
        (
            "ORDER_CANCEL_DESCRIPTION".to_string(),
            o.cancel_reason.clone(),
        ),
        ("ORDER_PUBLIC_URL".to_string(), String::new()),
    ])
}

/// Отправляет письмо события по заказу; ошибки только пишутся в лог.
pub async fn notify(state: &AppState, event: &str, order_id: i64) {
    let result = async {
        let Some(order) = repo::load(&state.db, order_id).await? else {
            return Ok(0);
        };
        let sale_email: Option<String> = sqlx::query_scalar(
            "SELECT value FROM options WHERE module = 'main' AND name = 'email_from'",
        )
        .fetch_optional(&state.db)
        .await?;
        let fields = order_fields(&order, sale_email.as_deref().unwrap_or_default());
        crate::mail::send_event(&state.db, &state.config, event, &fields).await
    }
    .await;
    if let Err(e) = result {
        tracing::warn!(event, order_id, error = %e, "письмо по заказу не отправлено");
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::sale::repo::{OrderItem, OrderPropertyValue, OrderView};

    fn item(id: i64, name: &str, price: f64) -> OrderItem {
        OrderItem {
            id,
            element_id: id,
            store_id: None,
            quantity: 1.0,
            price,
            name: name.into(),
            custom_price: false,
        }
    }

    fn value(code: &str, value: &str, is_email: bool, is_payer: bool) -> OrderPropertyValue {
        OrderPropertyValue {
            property_id: 0,
            code: code.into(),
            name: code.into(),
            value: value.into(),
            is_email,
            is_phone: false,
            is_payer,
        }
    }

    fn view() -> OrderView {
        let at = Utc.with_ymd_and_hms(2026, 10, 8, 10, 0, 0).unwrap();
        OrderView {
            id: 42,
            account_number: "42".into(),
            user_id: Some(7),
            user_login: "ivan".into(),
            user_email: "user@b.ru".into(),
            user_name: "Профиль".into(),
            person_type_id: 1,
            status: "N".into(),
            status_name: "Принят, обрабатывается".into(),
            goods_price: 1094.22,
            delivery_price: 300.0,
            price: 1394.22,
            currency: "RUB".into(),
            user_comment: String::new(),
            manager_comment: String::new(),
            canceled: false,
            canceled_at: None,
            cancel_reason: String::new(),
            paid: false,
            paid_at: None,
            stock_deducted: true,
            created_at: at,
            updated_at: at,
            properties: vec![
                value("EMAIL", "a@b.ru", true, false),
                value("fio", "Иванов Иван", false, true),
            ],
            items: vec![
                item(1, "Фильтр АКПП", 594.22),
                item(2, "Комплект шариков", 500.0),
            ],
            shipment: None,
            payment: None,
            history: Vec::new(),
        }
    }

    #[test]
    fn order_fields_like_bitrix() {
        let f = order_fields(&view(), "shop@transopt.net");
        assert_eq!(f["ORDER_ID"], "42");
        assert_eq!(f["ORDER_REAL_ID"], "42");
        assert_eq!(f["ORDER_ACCOUNT_NUMBER_ENCODE"], "42");
        assert_eq!(f["ORDER_DATE"], "08.10.2026");
        assert_eq!(f["ORDER_USER"], "Иванов Иван");
        assert_eq!(f["EMAIL"], "a@b.ru");
        assert_eq!(f["SALE_EMAIL"], "shop@transopt.net");
        assert_eq!(f["BCC"], "shop@transopt.net");
        assert_eq!(f["DELIVERY_PRICE"], "300 руб.");
        assert_eq!(f["PRICE"], "1 394.22 руб.");
        assert_eq!(f["ORDER_STATUS"], "Принят, обрабатывается");
        assert_eq!(f["ORDER_PUBLIC_URL"], "");
        assert_eq!(
            f["ORDER_LIST"],
            "Фильтр АКПП - 1 шт.: 594.22 руб.\nКомплект шариков - 1 шт.: 500 руб."
        );
    }

    #[test]
    fn order_fields_fallback_to_user() {
        let mut v = view();
        v.properties.clear();
        let f = order_fields(&v, "");
        assert_eq!(f["EMAIL"], "user@b.ru");
        assert_eq!(f["ORDER_USER"], "Профиль");
    }
}
