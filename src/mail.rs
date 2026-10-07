//! Почтовые события — аналог `CEvent::Send` Битрикса: на событие берутся активные
//! шаблоны из `mail_templates`, в них подставляются `#ПОЛЯ#`. Отправка — по SMTP
//! (`MAIL_SMTP_URL`), без него письма сохраняются в `MAIL_DIR` (как `mail_capture`
//! в dev-окружении bxapi).

use std::collections::HashMap;

use anyhow::Context;
use chrono::Utc;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, header::ContentType},
};
use sqlx::{FromRow, PgPool};

use crate::config::Config;

#[derive(FromRow)]
struct Template {
    id: i64,
    email_from: String,
    email_to: String,
    bcc: String,
    subject: String,
    body: String,
    body_type: String,
}

/// Подставляет `#КЛЮЧ#` из `fields`; неизвестные поля — пустая строка.
/// HTML-сущности вида `&#8381;` не трогает.
pub fn substitute(template: &str, fields: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('#') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let key_len = after
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(after.len());
        if key_len > 0 && after[key_len..].starts_with('#') && !out.ends_with('&') {
            let key = &after[..key_len];
            out.push_str(fields.get(key).map(String::as_str).unwrap_or(""));
            rest = &after[key_len + 1..];
        } else {
            out.push('#');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn addresses(raw: &str) -> Vec<Mailbox> {
    raw.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Отправляет письма по шаблонам события. Возвращает число отправленных писем.
pub async fn send_event(
    db: &PgPool,
    config: &Config,
    event: &str,
    fields: &HashMap<String, String>,
) -> anyhow::Result<usize> {
    let templates: Vec<Template> = sqlx::query_as(
        "SELECT id, email_from, email_to, bcc, subject, body, body_type
         FROM mail_templates WHERE event_name = $1 AND active ORDER BY id",
    )
    .bind(event)
    .fetch_all(db)
    .await?;
    if templates.is_empty() {
        tracing::warn!(event, "нет активных почтовых шаблонов для события");
        return Ok(0);
    }

    // Поля по умолчанию, как у Битрикса
    let options: HashMap<String, String> = sqlx::query_as::<_, (String, String)>(
        "SELECT name, value FROM options WHERE module = 'main'",
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .collect();
    let mut all = fields.clone();
    let defaults = [
        ("DEFAULT_EMAIL_FROM", "email_from"),
        ("SITE_NAME", "site_name"),
        ("SERVER_NAME", "server_name"),
    ];
    for (key, option) in defaults {
        all.entry(key.into())
            .or_insert_with(|| options.get(option).cloned().unwrap_or_default());
    }

    let mut sent = 0;
    for t in templates {
        let from = substitute(&t.email_from, &all);
        let to = substitute(&t.email_to, &all);
        let subject = substitute(&t.subject, &all);
        let body = substitute(&t.body, &all);
        let Some(from_box) = addresses(&from).into_iter().next() else {
            tracing::warn!(
                template = t.id,
                from,
                "у письма нет корректного отправителя"
            );
            continue;
        };
        let recipients = addresses(&to);
        if recipients.is_empty() {
            tracing::warn!(template = t.id, to, "у письма нет корректных получателей");
            continue;
        }
        let mut builder = Message::builder().from(from_box).subject(subject);
        for r in recipients {
            builder = builder.to(r);
        }
        for b in addresses(&substitute(&t.bcc, &all)) {
            builder = builder.bcc(b);
        }
        let content_type = if t.body_type == "html" {
            ContentType::TEXT_HTML
        } else {
            ContentType::TEXT_PLAIN
        };
        let message = builder.header(content_type).body(body)?;
        deliver(config, event, message).await?;
        sent += 1;
    }
    Ok(sent)
}

async fn deliver(config: &Config, event: &str, message: Message) -> anyhow::Result<()> {
    match &config.mail_smtp_url {
        Some(url) => {
            let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(url)
                .context("MAIL_SMTP_URL")?
                .build();
            transport.send(message).await.context("отправка письма")?;
        }
        None => {
            tokio::fs::create_dir_all(&config.mail_dir).await?;
            let name = format!(
                "{}_{event}_{:08x}.eml",
                Utc::now().format("%Y%m%d-%H%M%S"),
                rand::random::<u32>()
            );
            let path = config.mail_dir.join(name);
            tokio::fs::write(&path, message.formatted()).await?;
            tracing::info!(path = %path.display(), event, "письмо сохранено (SMTP не настроен)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitution() {
        let fields = HashMap::from([
            ("AUTHOR".to_string(), "Иван".to_string()),
            ("SITE_NAME".to_string(), "site.ru".to_string()),
        ]);
        assert_eq!(
            substitute(
                "#SITE_NAME#: вопрос от #AUTHOR# (#MISSING#) № 5 &#8381;",
                &fields
            ),
            "site.ru: вопрос от Иван () № 5 &#8381;"
        );
        assert_eq!(substitute("#not a key#", &fields), "#not a key#");
    }
}
