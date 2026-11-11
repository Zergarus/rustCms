//! Rust-скрипты: хранение кода и редактор.
//!
//! Запуск скриптов — отдельная задача; здесь только список, создание, правка и
//! удаление. Код не выполняется и не проверяется, поэтому страница постоянно
//! предупреждает о произвольном коде.

use axum::{
    Extension, Form,
    extract::{Path, State},
    response::{Html, IntoResponse, Response},
};
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use super::{guard, pg, render};
use crate::{
    access::Access,
    admin::flash::{self, FlashKind},
    audit,
    error::{AppError, AppResult, is_unique_violation},
    state::AppState,
};

/// Ограничение длины имени из формы.
const MAX_NAME_CHARS: usize = 100;
/// Сколько последних запусков показываем в истории.
const HISTORY_LIMIT: i64 = 50;
/// Сколько символов вывода запуска показываем в истории.
const OUTPUT_PREVIEW_CHARS: usize = 200;

/// Поля формы редактора.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct RustForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub code: String,
}

/// Сохранённый скрипт для страницы редактора.
#[derive(Debug, FromRow, Serialize)]
pub struct RustScript {
    pub id: i64,
    pub name: String,
    pub code: String,
    pub updated_at: String,
}

/// Строка списка: имя, время и автор последней правки.
#[derive(Debug, FromRow, Serialize)]
struct ScriptRow {
    id: i64,
    name: String,
    updated_at: String,
    updated_by: String,
}

/// Один запуск из истории.
#[derive(Debug, FromRow)]
struct RunRow {
    started_at: String,
    ok: Option<bool>,
    duration_ms: Option<i32>,
    output: String,
    error: String,
}

/// Запуск в том виде, в каком его показывает шаблон.
#[derive(Debug, Serialize)]
struct RunView {
    started_at: String,
    ok: Option<bool>,
    duration_ms: Option<i32>,
    output_short: String,
}

/// GET `/admin/tools/rust` — список скриптов и форма нового.
pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    guard(&state, &user)?;
    render_list(&state, user, RustForm::default(), None).await
}

/// GET `/admin/tools/rust/new` — редактор нового скрипта.
pub async fn new_form(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
) -> AppResult<Html<String>> {
    guard(&state, &user)?;
    render_editor(&state, user, None, RustForm::default(), None).await
}

/// GET `/admin/tools/rust/{id}` — редактор существующего скрипта и история запусков.
pub async fn edit(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    guard(&state, &user)?;
    let script = load_script(&state.db, id).await?;
    let form = RustForm {
        name: script.name.clone(),
        code: script.code.clone(),
    };
    render_editor(&state, user, Some(script), form, None).await
}

/// POST `/admin/tools/rust` — создать скрипт.
pub async fn save(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Form(form): Form<RustForm>,
) -> AppResult<Response> {
    guard(&state, &user)?;
    let name = match validate_name(&form.name) {
        Ok(name) => name,
        Err(error) => {
            return Ok(render_editor(&state, user, None, form, Some(error))
                .await?
                .into_response());
        }
    };
    let result = sqlx::query_scalar::<_, i64>(
        "INSERT INTO admin_rust_scripts (name, code, created_by, updated_by)
         VALUES ($1, $2, $3, $3) RETURNING id",
    )
    .bind(&name)
    .bind(&form.code)
    .bind(user.user.id)
    .fetch_one(&state.db)
    .await;
    match result {
        Ok(id) => {
            audit::record(
                &state.db,
                user.user.id,
                &user.user.login,
                "rust-save",
                "tools",
                id,
                &name,
            )
            .await;
            Ok(flash::redirect(
                &format!("/admin/tools/rust/{id}"),
                "Сохранено",
                FlashKind::Ok,
            ))
        }
        Err(error) if is_unique_violation(&error) => {
            let message = "Скрипт с таким именем уже есть".to_string();
            Ok(render_editor(&state, user, None, form, Some(message))
                .await?
                .into_response())
        }
        Err(error) => Err(error.into()),
    }
}

/// POST `/admin/tools/rust/{id}` — обновить скрипт.
pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
    Form(form): Form<RustForm>,
) -> AppResult<Response> {
    guard(&state, &user)?;
    let script = load_script(&state.db, id).await?;
    let name = match validate_name(&form.name) {
        Ok(name) => name,
        Err(error) => {
            return Ok(render_editor(&state, user, Some(script), form, Some(error))
                .await?
                .into_response());
        }
    };
    let result = sqlx::query_scalar::<_, i64>(
        "UPDATE admin_rust_scripts
         SET name = $2, code = $3, updated_by = $4, updated_at = now()
         WHERE id = $1 RETURNING id",
    )
    .bind(id)
    .bind(&name)
    .bind(&form.code)
    .bind(user.user.id)
    .fetch_optional(&state.db)
    .await;
    match result {
        Ok(Some(_)) => {
            audit::record(
                &state.db,
                user.user.id,
                &user.user.login,
                "rust-save",
                "tools",
                id,
                &name,
            )
            .await;
            Ok(flash::redirect(
                &format!("/admin/tools/rust/{id}"),
                "Сохранено",
                FlashKind::Ok,
            ))
        }
        Ok(None) => Err(AppError::NotFound),
        Err(error) if is_unique_violation(&error) => {
            let script = load_script(&state.db, id).await?;
            let message = "Скрипт с таким именем уже есть".to_string();
            Ok(
                render_editor(&state, user, Some(script), form, Some(message))
                    .await?
                    .into_response(),
            )
        }
        Err(error) => Err(error.into()),
    }
}

/// POST `/admin/tools/rust/{id}/delete` — удалить скрипт.
///
/// История запусков остаётся: внешний ключ `ON DELETE SET NULL`.
pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<Access>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    guard(&state, &user)?;
    let name: Option<String> =
        sqlx::query_scalar("DELETE FROM admin_rust_scripts WHERE id = $1 RETURNING name")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    let name = name.ok_or(AppError::NotFound)?;
    audit::record(
        &state.db,
        user.user.id,
        &user.user.login,
        "rust-delete",
        "tools",
        id,
        &name,
    )
    .await;
    Ok(flash::redirect(
        "/admin/tools/rust",
        "Удалено",
        FlashKind::Ok,
    ))
}

/// Проверка имени: тримим, требуем 1–100 символов.
fn validate_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    let len = name.chars().count();
    if len == 0 {
        return Err("Укажите имя скрипта".into());
    }
    if len > MAX_NAME_CHARS {
        return Err(format!("Имя скрипта — не больше {MAX_NAME_CHARS} символов"));
    }
    Ok(name.to_string())
}

async fn load_script(db: &sqlx::PgPool, id: i64) -> AppResult<RustScript> {
    sqlx::query_as(
        "SELECT id, name, code,
                to_char(updated_at, 'DD.MM.YYYY HH24:MI') AS updated_at
         FROM admin_rust_scripts WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)
}

/// Последние запуски скрипта; пока их пишет только Task 7.
async fn load_runs(db: &sqlx::PgPool, script_id: i64) -> AppResult<Vec<RunView>> {
    let rows: Vec<RunRow> = sqlx::query_as(
        "SELECT COALESCE(to_char(started_at, 'DD.MM.YYYY HH24:MI:SS'), '—') AS started_at,
                ok, duration_ms,
                COALESCE(output, '') AS output,
                COALESCE(error, '') AS error
         FROM admin_rust_runs
         WHERE script_id = $1
         ORDER BY started_at DESC NULLS LAST, id DESC
         LIMIT $2",
    )
    .bind(script_id)
    .bind(HISTORY_LIMIT)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(RunView::from).collect())
}

impl From<RunRow> for RunView {
    fn from(row: RunRow) -> Self {
        let text = if row.output.trim().is_empty() {
            row.error
        } else {
            row.output
        };
        let output_short = if text.trim().is_empty() {
            "—".to_string()
        } else {
            pg::truncate(&text, OUTPUT_PREVIEW_CHARS)
        };
        RunView {
            started_at: row.started_at,
            ok: row.ok,
            duration_ms: row.duration_ms,
            output_short,
        }
    }
}

async fn render_list(
    state: &AppState,
    user: Access,
    form: RustForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let items: Vec<ScriptRow> = sqlx::query_as(
        "SELECT s.id, s.name,
                to_char(s.updated_at, 'DD.MM.YYYY HH24:MI') AS updated_at,
                COALESCE(u.login, '—') AS updated_by
         FROM admin_rust_scripts s
         LEFT JOIN users u ON u.id = s.updated_by
         ORDER BY lower(s.name), s.id",
    )
    .fetch_all(&state.db)
    .await?;
    render(
        state,
        "tools/rust.html",
        context! { user, items, form, error, editor => false },
    )
}

async fn render_editor(
    state: &AppState,
    user: Access,
    script: Option<RustScript>,
    form: RustForm,
    error: Option<String>,
) -> AppResult<Html<String>> {
    let id = script.as_ref().map(|script| script.id);
    let runs = match id {
        Some(id) => load_runs(&state.db, id).await?,
        None => Vec::new(),
    };
    render(
        state,
        "tools/rust.html",
        context! { user, script, form, error, runs, id, editor => true },
    )
}

#[cfg(test)]
mod tests {
    use axum::http::{StatusCode, header};
    use sqlx::PgPool;

    use crate::{
        smoke_tests::{get, post_form},
        test_support::{admin_cookie, permitted_cookie, post_form_raw, test_state},
    };

    const PATH: &str = "/admin/tools/rust";

    fn form_body(pairs: &[(&str, &str)]) -> String {
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        for (name, value) in pairs {
            serializer.append_pair(name, value);
        }
        serializer.finish()
    }

    async fn insert_script(db: &PgPool, name: &str, code: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO admin_rust_scripts (name, code) VALUES ($1, $2) RETURNING id",
        )
        .bind(name)
        .bind(code)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn script_count(db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM admin_rust_scripts")
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn last_audit(db: &PgPool) -> (String, String, String) {
        sqlx::query_as("SELECT action, entity, summary FROM audit_log ORDER BY id DESC LIMIT 1")
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn admin_id(db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT id FROM users WHERE login = 'smoke'")
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn list_has_new_form_and_empty_state(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, PATH, &cookie).await;

        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("Новый скрипт"),
            "нет формы нового скрипта: {body}"
        );
        assert!(
            body.contains("action=\"/admin/tools/rust\""),
            "нет формы создания: {body}"
        );
        assert!(body.contains("name=\"name\""), "нет поля имени: {body}");
        assert!(body.contains("name=\"code\""), "нет поля кода: {body}");
        assert!(body.contains("card empty"), "нет пустого состояния: {body}");
        assert!(body.contains("Скриптов пока нет"), "{body}");
        assert!(
            body.contains("Произвольный код"),
            "нет предупреждения о коде: {body}"
        );
    }

    #[sqlx::test]
    async fn list_shows_saved_scripts(db: PgPool) {
        let id = insert_script(&db, "Отчёт", "let x = 1;").await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;
        let admin = admin_id(&db).await;
        sqlx::query("UPDATE admin_rust_scripts SET updated_by = $2 WHERE id = $1")
            .bind(id)
            .bind(admin)
            .execute(&db)
            .await
            .unwrap();

        let (status, body) = get(&app, PATH, &cookie).await;

        assert_eq!(status, 200, "{body}");
        assert!(body.contains("Отчёт"), "нет имени скрипта: {body}");
        assert!(
            body.contains(&format!("/admin/tools/rust/{id}")),
            "нет ссылки на скрипт: {body}"
        );
        assert!(body.contains("Открыть"), "нет кнопки открытия: {body}");
        assert!(body.contains("Удалить"), "нет кнопки удаления: {body}");
        assert!(body.contains("smoke"), "нет автора правки: {body}");
        assert!(
            body.contains("confirm("),
            "удаление без подтверждения: {body}"
        );
    }

    #[sqlx::test]
    async fn create_saves_redirects_and_writes_audit(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, headers, body) = post_form_raw(
            &app,
            PATH,
            &cookie,
            &form_body(&[("name", "  Мой скрипт  "), ("code", "let answer = 42;")]),
        )
        .await;

        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        let id: i64 = sqlx::query_scalar("SELECT id FROM admin_rust_scripts")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(headers[header::LOCATION], format!("/admin/tools/rust/{id}"));
        let (name, code, created_by, updated_by): (String, String, Option<i64>, Option<i64>) =
            sqlx::query_as(
                "SELECT name, code, created_by, updated_by FROM admin_rust_scripts WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(name, "Мой скрипт", "имя не тримится");
        assert_eq!(code, "let answer = 42;");
        let admin = admin_id(&db).await;
        assert_eq!(created_by, Some(admin), "не записан создатель");
        assert_eq!(updated_by, Some(admin), "не записан редактор");

        let (action, entity, summary) = last_audit(&db).await;
        assert_eq!(action, "rust-save");
        assert_eq!(entity, "tools");
        assert_eq!(summary, "Мой скрипт");

        // flash показывается на следующей странице
        let flash = headers
            .get_all(header::SET_COOKIE)
            .iter()
            .find(|value| {
                value
                    .to_str()
                    .unwrap_or_default()
                    .starts_with("admin_flash=")
            })
            .expect("нет flash-cookie");
        let flash = flash.to_str().unwrap().split(';').next().unwrap();
        let (status, html) = get(
            &app,
            &format!("/admin/tools/rust/{id}"),
            &format!("{cookie}; {flash}"),
        )
        .await;
        assert_eq!(status, 200, "{html}");
        assert!(html.contains("Сохранено"), "нет flash: {html}");
        assert!(html.contains("Мой скрипт"), "{html}");
        assert!(html.contains("let answer = 42;"), "{html}");
    }

    #[sqlx::test]
    async fn create_allows_empty_code(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(
            &app,
            PATH,
            &cookie,
            &form_body(&[("name", "Пустой"), ("code", "")]),
        )
        .await;

        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        let code: String = sqlx::query_scalar("SELECT code FROM admin_rust_scripts")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(code, "");
    }

    #[sqlx::test]
    async fn update_changes_code_and_editor(db: PgPool) {
        let id = insert_script(&db, "Старое имя", "old").await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, headers, body) = post_form_raw(
            &app,
            &format!("{PATH}/{id}"),
            &cookie,
            &form_body(&[("name", "Новое имя"), ("code", "new code")]),
        )
        .await;

        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        assert_eq!(headers[header::LOCATION], format!("{PATH}/{id}"));
        let (name, code, created_by, updated_by): (String, String, Option<i64>, Option<i64>) =
            sqlx::query_as(
                "SELECT name, code, created_by, updated_by FROM admin_rust_scripts WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(name, "Новое имя");
        assert_eq!(code, "new code");
        assert_eq!(created_by, None, "создатель не должен меняться");
        assert_eq!(updated_by, Some(admin_id(&db).await));

        let (action, entity, summary) = last_audit(&db).await;
        assert_eq!(action, "rust-save");
        assert_eq!(entity, "tools");
        assert_eq!(summary, "Новое имя");
    }

    #[sqlx::test]
    async fn duplicate_name_on_create_is_200_alert(db: PgPool) {
        insert_script(&db, "Занято", "").await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(
            &app,
            PATH,
            &cookie,
            &form_body(&[("name", "Занято"), ("code", "x")]),
        )
        .await;

        assert_eq!(status, 200, "конфликт имени должен быть 200: {body}");
        assert!(body.contains("уже есть"), "нет понятного текста: {body}");
        assert!(body.contains("name=\"name\""), "форма не показана: {body}");
        assert_eq!(script_count(&db).await, 1);
    }

    #[sqlx::test]
    async fn duplicate_name_on_update_is_200_alert(db: PgPool) {
        insert_script(&db, "Занято", "").await;
        let id = insert_script(&db, "Другой", "").await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = post_form(
            &app,
            &format!("{PATH}/{id}"),
            &cookie,
            &form_body(&[("name", "Занято"), ("code", "x")]),
        )
        .await;

        assert_eq!(status, 200, "конфликт имени должен быть 200: {body}");
        assert!(body.contains("уже есть"), "нет понятного текста: {body}");
        let name: String = sqlx::query_scalar("SELECT name FROM admin_rust_scripts WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(name, "Другой", "конфликт изменил имя");
    }

    #[sqlx::test]
    async fn empty_and_too_long_names_are_rejected(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        for name in ["", "   ", &"x".repeat(101)] {
            let (status, body) = post_form(
                &app,
                PATH,
                &cookie,
                &form_body(&[("name", name), ("code", "x")]),
            )
            .await;
            assert_eq!(status, 200, "невалидное имя принято: {body}");
            assert!(body.contains("Ошибка"), "нет ошибки: {body}");
            assert_eq!(script_count(&db).await, 0);
        }
    }

    #[sqlx::test]
    async fn delete_removes_script_and_writes_audit(db: PgPool) {
        let id = insert_script(&db, "Удаляемый", "").await;
        sqlx::query(
            "INSERT INTO admin_rust_runs (script_id, user_login, started_at, duration_ms, ok, output)
             VALUES ($1, 'smoke', now(), 5, TRUE, 'ok')",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, headers, body) =
            post_form_raw(&app, &format!("{PATH}/{id}/delete"), &cookie, "").await;

        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        assert_eq!(headers[header::LOCATION], PATH);
        assert_eq!(script_count(&db).await, 0);
        let script_id: Option<i64> = sqlx::query_scalar("SELECT script_id FROM admin_rust_runs")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(script_id, None, "история должна пережить удаление");

        let (action, entity, summary) = last_audit(&db).await;
        assert_eq!(action, "rust-delete");
        assert_eq!(entity, "tools");
        assert_eq!(summary, "Удаляемый");
    }

    #[sqlx::test]
    async fn editor_shows_code_hint_warning_and_history(db: PgPool) {
        let id = insert_script(&db, "Отчёт", "println!(\"hi\");").await;
        sqlx::query(
            "INSERT INTO admin_rust_runs (script_id, user_login, started_at, duration_ms, ok, output)
             VALUES ($1, 'smoke', now(), 125, TRUE, 'готово: 42')",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, &format!("{PATH}/{id}"), &cookie).await;

        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("async fn main"),
            "нет подсказки о main: {body}"
        );
        assert!(body.contains("pool"), "нет подсказки о pool: {body}");
        assert!(
            body.contains("Произвольный код"),
            "нет предупреждения: {body}"
        );
        assert!(
            body.contains("появится позже") || body.contains("следующем обновлении"),
            "нет предупреждения об отсутствии запуска: {body}"
        );
        assert!(body.contains("Сохранить"), "нет кнопки сохранения: {body}");
        assert!(
            !body.contains("Запустить"),
            "кнопка запуска не должна быть: {body}"
        );
        assert!(body.contains("История запусков"), "нет истории: {body}");
        assert!(body.contains("готово: 42"), "нет вывода запуска: {body}");
        assert!(body.contains("125"), "нет длительности: {body}");
    }

    #[sqlx::test]
    async fn new_editor_and_empty_history(db: PgPool) {
        let id = insert_script(&db, "Без запусков", "").await;
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        let (status, body) = get(&app, &format!("{PATH}/new"), &cookie).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("Новый скрипт"), "нет заголовка: {body}");
        assert!(body.contains("async fn main"), "нет подсказки: {body}");
        assert!(body.contains("Сохранить"), "нет кнопки: {body}");

        let (status, body) = get(&app, &format!("{PATH}/{id}"), &cookie).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("Запусков пока нет"),
            "нет пустого состояния истории: {body}"
        );
    }

    #[sqlx::test]
    async fn unknown_script_is_404(db: PgPool) {
        let app = crate::app(test_state(db.clone()));
        let cookie = admin_cookie(&db).await;

        for path in [format!("{PATH}/999999"), format!("{PATH}/999999")] {
            let (status, _) = get(&app, &path, &cookie).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
        let (status, _) = post_form(&app, &format!("{PATH}/999999/delete"), &cookie, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn non_super_is_forbidden_everywhere(db: PgPool) {
        let id = insert_script(&db, "Чужой", "").await;
        let app = crate::app(test_state(db.clone()));
        let cookie = permitted_cookie(&db, "op", &[]).await;

        for path in [
            PATH.to_string(),
            format!("{PATH}/new"),
            format!("{PATH}/{id}"),
        ] {
            let (status, body) = get(&app, &path, &cookie).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
        }
        for (path, body) in [
            (PATH.to_string(), form_body(&[("name", "Взлом")])),
            (
                format!("{PATH}/{id}"),
                form_body(&[("name", "Взлом"), ("code", "x")]),
            ),
            (format!("{PATH}/{id}/delete"), String::new()),
        ] {
            let (status, response) = post_form(&app, &path, &cookie, &body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {response}");
        }
        assert_eq!(script_count(&db).await, 1);
        let name: String = sqlx::query_scalar("SELECT name FROM admin_rust_scripts WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(name, "Чужой");
    }
}
