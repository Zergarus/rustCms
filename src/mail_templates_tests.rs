use axum::http::StatusCode;
use sqlx::PgPool;

fn form(pairs: &[(&str, &str)]) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

async fn post(app: &axum::Router, path: &str, cookie: &str, pairs: &[(&str, &str)]) -> StatusCode {
    let (status, _, _) = crate::test_support::post_form_raw(app, path, cookie, &form(pairs)).await;
    status
}

const TEMPLATE: [(&str, &str); 6] = [
    ("event_name", "CALLBACK_FORM"),
    ("email_from", "#DEFAULT_EMAIL_FROM#"),
    ("email_to", "manager@example.ru"),
    ("subject", "Заявка"),
    ("body", "Имя: #AUTHOR#"),
    ("active", "on"),
];

#[sqlx::test]
async fn mail_template_crud(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;

    let (status, _, page) = crate::test_support::get_raw(&app, "/admin/mail", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&page).contains("Письма"));

    assert_eq!(
        post(&app, "/admin/mail", &cookie, &TEMPLATE).await,
        StatusCode::SEE_OTHER
    );
    let template = crate::mail_templates::list(&db)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(template.event_name, "CALLBACK_FORM");
    assert!(template.active);
    assert_eq!(template.body_type, "text");

    let mut updated = TEMPLATE;
    updated[3] = ("subject", "Новая заявка");
    let status = post(
        &app,
        &format!("/admin/mail/{}", template.id),
        &cookie,
        &updated,
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        crate::mail_templates::get(&db, template.id)
            .await
            .unwrap()
            .subject,
        "Новая заявка"
    );

    let status = post(
        &app,
        &format!("/admin/mail/{}/delete", template.id),
        &cookie,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(crate::mail_templates::list(&db).await.unwrap().is_empty());
}

#[sqlx::test]
async fn invalid_template_is_not_saved(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;

    let bad_event = [
        ("event_name", "bad event"),
        ("subject", "Тема"),
        ("body", "Тело"),
    ];
    assert_eq!(
        post(&app, "/admin/mail", &cookie, &bad_event).await,
        StatusCode::OK
    );
    assert!(crate::mail_templates::list(&db).await.unwrap().is_empty());

    let empty_subject = [
        ("event_name", "OK_EVENT"),
        ("subject", " "),
        ("body", "Тело"),
    ];
    assert_eq!(
        post(&app, "/admin/mail", &cookie, &empty_subject).await,
        StatusCode::OK
    );
    assert!(crate::mail_templates::list(&db).await.unwrap().is_empty());
}

#[sqlx::test]
async fn mail_admin_requires_super(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::permitted_cookie(&db, "editor", &[]).await;
    let (status, _, _) = crate::test_support::get_raw(&app, "/admin/mail", &cookie).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        post(&app, "/admin/mail", &cookie, &TEMPLATE).await,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test]
async fn changed_template_changes_letter_text(db: PgPool) {
    let app = crate::app(crate::test_support::test_state(db.clone()));
    let cookie = crate::test_support::admin_cookie(&db).await;
    let body = [
        ("event_name", "CALLBACK_FORM"),
        ("email_from", "site@example.ru"),
        ("email_to", "manager@example.ru"),
        ("subject", "Заявка"),
        ("body", "NEW BODY by #AUTHOR#"),
        ("active", "on"),
    ];
    assert_eq!(
        post(&app, "/admin/mail", &cookie, &body).await,
        StatusCode::SEE_OTHER
    );

    let mut state = crate::test_support::test_state(db.clone());
    let mail_dir = state.config.mail_dir.clone();
    // Тот же каталог писем, что у тестового состояния выше, не нужен: шлём напрямую.
    state.config = state.config.clone();
    let fields = std::collections::HashMap::from([("AUTHOR".to_string(), "Ivan".to_string())]);
    crate::mail::send_event(&db, &state.config, "CALLBACK_FORM", &fields)
        .await
        .unwrap();

    let letter = std::fs::read_dir(&mail_dir)
        .map(|dir| {
            dir.filter_map(|e| std::fs::read_to_string(e.unwrap().path()).ok())
                .collect::<String>()
        })
        .unwrap_or_default();
    assert!(letter.contains("NEW BODY by Ivan"), "{letter}");
}
