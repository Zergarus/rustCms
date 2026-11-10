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
    transport::smtp::{
        authentication::Credentials,
        client::{Tls, TlsParameters},
    },
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
fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

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
        // HTML-письмо: значения полей экранируются, иначе имя или сообщение
        // пользователя вносит в письмо разметку и ссылки.
        let body = if t.body_type == "html" {
            let escaped: HashMap<String, String> = all
                .iter()
                .map(|(k, v)| (k.clone(), escape_html(v)))
                .collect();
            substitute(&t.body, &escaped)
        } else {
            substitute(&t.body, &all)
        };
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
        deliver(db, config, event, message).await?;
        sent += 1;
    }
    Ok(sent)
}

/// Режим шифрования SMTP: `auto` — 465 это неявный TLS, остальное STARTTLS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TlsMode {
    None,
    Starttls,
    Wrapper,
}

pub(crate) fn tls_mode(port: u16, secure: &str) -> TlsMode {
    match secure {
        "none" => TlsMode::None,
        "tls" => TlsMode::Wrapper,
        "starttls" => TlsMode::Starttls,
        _ if port == 465 => TlsMode::Wrapper,
        _ => TlsMode::Starttls,
    }
}

/// SMTP из окружения (деплой) или из настроек в БД; `None` — письма пишутся в `.eml`.
async fn transport(
    db: &PgPool,
    config: &Config,
) -> anyhow::Result<Option<AsyncSmtpTransport<Tokio1Executor>>> {
    if let Some(url) = &config.mail_smtp_url {
        return Ok(Some(
            AsyncSmtpTransport::<Tokio1Executor>::from_url(url)
                .context("MAIL_SMTP_URL")?
                .build(),
        ));
    }
    let Some(smtp) = crate::settings::smtp(db).await? else {
        return Ok(None);
    };
    let mut builder =
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&smtp.host).port(smtp.port);
    if !smtp.user.is_empty() {
        builder = builder.credentials(Credentials::new(smtp.user.clone(), smtp.password.clone()));
    }
    let tls = match tls_mode(smtp.port, &smtp.secure) {
        TlsMode::None => Tls::None,
        TlsMode::Wrapper => Tls::Wrapper(TlsParameters::new(smtp.host.clone())?),
        TlsMode::Starttls => Tls::Required(TlsParameters::new(smtp.host.clone())?),
    };
    Ok(Some(builder.tls(tls).build()))
}

async fn deliver(
    db: &PgPool,
    config: &Config,
    event: &str,
    message: Message,
) -> anyhow::Result<()> {
    if let Some(transport) = transport(db, config).await? {
        transport.send(message).await.context("отправка письма")?;
        return Ok(());
    }
    tokio::fs::create_dir_all(&config.mail_dir).await?;
    let name = format!(
        "{}_{event}_{:08x}.eml",
        Utc::now().format("%Y%m%d-%H%M%S"),
        rand::random::<u32>()
    );
    let path = config.mail_dir.join(name);
    tokio::fs::write(&path, message.formatted()).await?;
    tracing::info!(path = %path.display(), event, "письмо сохранено (SMTP не настроен)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_mode_matrix() {
        assert_eq!(tls_mode(465, "auto"), TlsMode::Wrapper);
        assert_eq!(tls_mode(587, "auto"), TlsMode::Starttls);
        assert_eq!(tls_mode(25, ""), TlsMode::Starttls);
        assert_eq!(tls_mode(465, "none"), TlsMode::None);
        assert_eq!(tls_mode(25, "starttls"), TlsMode::Starttls);
        assert_eq!(tls_mode(25, "tls"), TlsMode::Wrapper);
    }

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
