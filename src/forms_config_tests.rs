use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::bxapi::{
    forms_config::{FormSpec, Forms, NotifierSpec, WriterSpec, from_toml},
    project::Rule,
};

const CALLBACK: &str = r#"
[forms.callback.fields.name]
rules = ["required"]

[forms.callback.fields.email]
rules = ["required", "email"]

[forms.callback.writer]
collection = "callback"

[forms.callback.writer.mapping]
name = "NAME"
email = "PREVIEW_TEXT"

[forms.callback.notifier]
event = "CALLBACK_FORM"

[forms.callback.notifier.mapping]
name = "AUTHOR"
email = "EMAIL"
"#;

fn registry(spec: FormSpec) -> Arc<Forms> {
    let mut forms = Forms::default();
    forms.insert(spec);
    Arc::new(forms)
}

/// Коллекция формы: поля `email` (строка) и `attachment` (файл).
async fn collection(db: &PgPool) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO collections (code, name, api_enabled) VALUES ('callback', 'Заявки', TRUE) RETURNING id",
    )
    .fetch_one(db)
    .await
    .unwrap();
    for (code, kind) in [("email", "string"), ("attachment", "file")] {
        sqlx::query(
            "INSERT INTO collection_fields (collection_id, code, name, kind) VALUES ($1, $2, $2, $3)",
        )
        .bind(id)
        .bind(code)
        .bind(kind)
        .execute(db)
        .await
        .unwrap();
    }
    id
}

async fn post_json(app: &Router, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post_multipart(app: &Router, path: &str, body: Vec<u8>) -> (StatusCode, Value) {
    let req = Request::post(path)
        .header(
            header::CONTENT_TYPE,
            "multipart/form-data; boundary=XBOUNDARY",
        )
        .body(Body::from(body))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[test]
fn toml_parses_rules_writer_notifier() {
    let specs = from_toml(CALLBACK).unwrap();
    assert_eq!(specs.len(), 1);
    let form = &specs[0];
    assert_eq!(form.code, "callback");
    assert_eq!(form.rules.len(), 2);
    let email = form.rules.iter().find(|(f, _)| f == "email").unwrap();
    assert_eq!(email.1, vec![Rule::Required, Rule::Email]);
    let writer = form.writer.as_ref().unwrap();
    assert_eq!(writer.collection, "callback");
    assert_eq!(writer.mapping.len(), 2);
    let notifier = form.notifier.as_ref().unwrap();
    assert_eq!(notifier.event, "CALLBACK_FORM");
}

#[test]
fn unknown_rule_fails_loudly() {
    let err = from_toml(
        r#"
[forms.bad.fields.name]
rules = ["telepathy"]
"#,
    )
    .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("telepathy"), "{text}");
    assert!(text.contains("bad"), "{text}");
}

#[test]
fn file_overrides_project_form() {
    let mut forms = Forms::default();
    forms.insert(FormSpec {
        code: "callback".into(),
        rules: vec![("name".into(), vec![Rule::Required])],
        writer: None,
        notifier: None,
        files: Vec::new(),
    });
    forms.merge(from_toml(CALLBACK).unwrap());

    let form = forms.get("callback").unwrap();
    assert_eq!(form.rules.len(), 2, "оверрайд целиком, а не слияние полей");
    assert_eq!(form.notifier.as_ref().unwrap().event, "CALLBACK_FORM");
}

#[sqlx::test]
async fn form_from_registry_writes_item_and_mails(db: PgPool) {
    let collection = collection(&db).await;
    sqlx::query(
        "INSERT INTO mail_templates (event_name, active, email_from, email_to, bcc, subject, body, body_type)
         VALUES ('CALLBACK_FORM', TRUE, 'site@example.ru', 'manager@example.ru', '', 'Заявка', '#AUTHOR# / #EMAIL#', 'text')",
    )
    .execute(&db)
    .await
    .unwrap();

    let spec = FormSpec {
        code: "callback".into(),
        rules: from_toml(CALLBACK).unwrap().remove(0).rules,
        writer: Some(WriterSpec {
            collection: "callback".into(),
            mapping: vec![
                ("name".into(), "NAME".into()),
                ("email".into(), "PREVIEW_TEXT".into()),
            ],
        }),
        notifier: Some(NotifierSpec {
            event: "CALLBACK_FORM".into(),
            mapping: vec![("name".into(), "AUTHOR".into())],
        }),
        files: Vec::new(),
    };
    let mut state = crate::test_support::test_state(db.clone());
    let mail_dir = state.config.mail_dir.clone();
    state.forms = registry(spec);
    let app = crate::app(state);

    let (status, json) = post_json(
        &app,
        "/api/v1/form/callback",
        json!({"name": " Иван ", "email": "ivan@example.ru"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "success");
    let id = json["data"]["id"].as_i64().unwrap();

    let item: (i64, String, String) = sqlx::query_as(
        "SELECT collection_id, name, preview_text FROM collection_items WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(item.0, collection);
    assert_eq!(item.1, "Иван");
    assert_eq!(item.2, "ivan@example.ru");

    // Письмо уходит в фоне — ждём файл, а не читаем каталог сразу.
    let mut letter = String::new();
    for _ in 0..50 {
        letter = std::fs::read_dir(&mail_dir)
            .map(|dir| {
                dir.filter_map(|e| std::fs::read_to_string(e.unwrap().path()).ok())
                    .collect::<String>()
            })
            .unwrap_or_default();
        if letter.contains("manager@example.ru") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // Тема/тело уходят в base64 из-за кириллицы, поэтому проверяем получателя.
    assert!(letter.contains("manager@example.ru"), "{letter}");
}

#[sqlx::test]
async fn unknown_form_is_404_and_validation_is_unchanged(db: PgPool) {
    let mut state = crate::test_support::test_state(db.clone());
    let mut forms = Forms::default();
    forms.merge(from_toml(CALLBACK).unwrap());
    state.forms = Arc::new(forms);
    let app = crate::app(state);

    let (status, json) = post_json(&app, "/api/v1/form/nope", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["errors"][0]["code"], "form_not_found");

    let (status, json) = post_json(&app, "/api/v1/form/callback", json!({"email": "nope"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = json["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("name is required"), "{message}");
    assert!(message.contains("email must be a valid email"), "{message}");
}

#[sqlx::test]
async fn multipart_upload_stores_file(db: PgPool) {
    let _ = collection(&db).await;
    let spec = FormSpec {
        code: "callback".into(),
        rules: vec![("name".into(), vec![Rule::Required])],
        writer: Some(WriterSpec {
            collection: "callback".into(),
            mapping: vec![("name".into(), "NAME".into())],
        }),
        notifier: None,
        files: vec![("attachment".into(), "attachment".into())],
    };
    let mut state = crate::test_support::test_state(db.clone());
    state.forms = registry(spec);
    let app = crate::app(state);

    let body = "--XBOUNDARY\r\n\
Content-Disposition: form-data; name=\"name\"\r\n\r\n\
Иван\r\n\
--XBOUNDARY\r\n\
Content-Disposition: form-data; name=\"attachment\"; filename=\"scan.pdf\"\r\n\
Content-Type: application/pdf\r\n\r\n\
%PDF-1.4 test\r\n\
--XBOUNDARY--\r\n"
        .as_bytes()
        .to_vec();
    let (status, json) = post_multipart(&app, "/api/v1/form/callback", body).await;
    assert_eq!(status, StatusCode::OK, "{json}");

    let id = json["data"]["id"].as_i64().unwrap();
    let values: sqlx::types::Json<Value> =
        sqlx::query_scalar("SELECT field_values FROM collection_items WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
    let file_id = values.0["attachment"].as_i64().expect("id файла в поле");
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM files WHERE id = $1")
        .bind(file_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(stored, 1);
}
