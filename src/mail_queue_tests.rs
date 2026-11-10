//! Очередь писем: немедленная попытка, пометка отправки и повтор при сбое.

use std::collections::HashMap;

use sqlx::PgPool;

async fn template(db: &PgPool) {
    sqlx::query(
        "INSERT INTO mail_templates (event_name, email_from, email_to, subject, body, body_type, active)
         VALUES ('QUEUE_TEST', 'shop@example.ru', 'user@example.ru', 'Тема', 'Привет #NAME#', 'text', TRUE)",
    )
    .execute(db)
    .await
    .unwrap();
}

#[sqlx::test]
async fn sent_mail_is_marked(db: PgPool) {
    template(&db).await;
    let state = crate::test_support::test_state(db.clone());
    let fields = HashMap::from([("NAME".to_string(), "Иван".to_string())]);

    let sent = crate::mail::send_event(&state.db, &state.config, "QUEUE_TEST", &fields)
        .await
        .unwrap();
    assert_eq!(sent, 1);

    let (sent_at, attempts): (Option<chrono::DateTime<chrono::Utc>>, i32) =
        sqlx::query_as("SELECT sent_at, attempts FROM mail_queue WHERE event = 'QUEUE_TEST'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(sent_at.is_some());
    assert_eq!(attempts, 0);

    let letter = std::fs::read_dir(&state.config.mail_dir)
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().ends_with(".eml"))
        .expect("письмо сохранено в MAIL_DIR");
    let text = std::fs::read_to_string(letter.path()).unwrap();
    assert!(text.contains("user@example.ru"), "{text}");
}

#[sqlx::test]
async fn failed_mail_stays_queued(db: PgPool) {
    template(&db).await;
    let mut state = crate::test_support::test_state(db.clone());
    let mut config = (*state.config).clone();
    // Недоступный SMTP: попытка провалится, письмо останется в очереди.
    config.mail_smtp_url = Some("smtp://127.0.0.1:1".into());
    state.config = std::sync::Arc::new(config);
    let fields = HashMap::from([("NAME".to_string(), "Иван".to_string())]);

    let sent = crate::mail::send_event(&state.db, &state.config, "QUEUE_TEST", &fields)
        .await
        .unwrap();
    assert_eq!(sent, 0);

    let (sent_at, attempts, error): (Option<chrono::DateTime<chrono::Utc>>, i32, String) =
        sqlx::query_as(
            "SELECT sent_at, attempts, error FROM mail_queue WHERE event = 'QUEUE_TEST'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(sent_at.is_none());
    assert_eq!(attempts, 1);
    assert!(!error.is_empty());
}

#[sqlx::test]
async fn queue_page_shows_letters(db: PgPool) {
    template(&db).await;
    let state = crate::test_support::test_state(db.clone());
    let fields = HashMap::from([("NAME".to_string(), "Иван".to_string())]);
    crate::mail::send_event(&state.db, &state.config, "QUEUE_TEST", &fields)
        .await
        .unwrap();
    let app = crate::app(state);
    let cookie = crate::test_support::admin_cookie(&db).await;

    let (status, _, page) = crate::test_support::get_raw(&app, "/admin/mail/queue", &cookie).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let page = String::from_utf8_lossy(&page);
    assert!(page.contains("QUEUE_TEST"), "{page}");
    assert!(page.contains("отправлено"), "{page}");
}

#[sqlx::test]
async fn retry_sends_letter_now(db: PgPool) {
    template(&db).await;
    // Письмо, «застрявшее» с будущей попыткой и ошибкой.
    sqlx::query(
        "INSERT INTO mail_queue (event, fields, attempts, next_at, error)
         VALUES ('QUEUE_TEST', '{\"NAME\":\"Иван\"}'::jsonb, 1,
                 now() + interval '1 hour', 'SMTP недоступен')",
    )
    .execute(&db)
    .await
    .unwrap();
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;

    let (status, _, _) =
        crate::test_support::post_form_raw(&app, "/admin/mail/queue/1/retry", &cookie, "").await;
    assert_eq!(status, axum::http::StatusCode::SEE_OTHER);

    let sent: bool = sqlx::query_scalar("SELECT sent_at IS NOT NULL FROM mail_queue WHERE id = 1")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(sent, "письмо отправлено по кнопке");
}
