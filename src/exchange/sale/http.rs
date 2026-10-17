//! Протокол обмена заказами с 1С (`type=sale`, компонент `sale.export.1c`):
//! `init`, `query`, `success`, `file`, `import`, `info`.
//!
//! Вход, сессия, право и журнал — в `exchange::http`. Состояние обмена в `state` сессии:
//! `version` и `cml_version` из `init`, `mark` — метка последнего `query` для `success`,
//! `sale_pos` — следующая группа документов файла при пошаговом `import`, `sale_stats` — итог
//! прошлых шагов файла (входит в итог `success`), `imported` — загруженные файлы.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::apply::{self, SaleStats};
use super::document::{self, PayOperation, SchemaVersion};
use super::settings::{self as sale_settings, SaleSettings};
use super::{parse, select};
use crate::exchange::http::{Outcome, Reply, db_err, file};
use crate::exchange::session::{self, Limits, Session};
use crate::exchange::settings::Settings;
use crate::state::AppState;

const CML_210: &str = "Схема обмена cmlVersion ≥ 2.10 не поддерживается";
/// `ExportOneCBase::SHEM_VERSION_2_10`.
const CML_210_VERSION: f64 = 2.1;

pub(crate) async fn dispatch(
    state: &AppState,
    session: &Session,
    mode: &str,
    query: &HashMap<String, String>,
    headers: &HeaderMap,
    body: Body,
    token: &str,
) -> Reply {
    let filename = || {
        query
            .get("filename")
            .map(String::as_str)
            .filter(|f| !f.is_empty())
            .ok_or_else(|| "Не указан параметр filename".to_string())
    };
    match mode {
        "init" => init(state, session, query, token).await,
        "query" => export(state, session).await,
        "success" => success(state, session).await,
        "file" if old_protocol(session) => {
            old_file(state, session, filename()?, headers, body).await
        }
        "file" => file(state, session, filename()?, headers, body).await,
        "import" => import(state, session, filename()?, Step::Settings).await,
        "info" => info(state).await,
        // `CC_BSC1_ERROR_UNKNOWN_COMMAND`
        _ => Err("Неизвестная команда.".into()),
    }
}

/// Начало обмена: каталог сессии и состояние — с чистого листа; `version` и `cmlVersion`
/// запоминаются. С `version` в ответе ещё `sessid` и `version=2.09`.
async fn init(
    state: &AppState,
    session: &Session,
    query: &HashMap<String, String>,
    token: &str,
) -> Reply {
    let settings = Settings::load(&state.db).await.map_err(db_err)?;
    let dir = session::dir(&state.config.upload_dir, session);
    if let Err(e) = tokio::fs::remove_dir_all(&dir).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(format!("Ошибка работы с файлом: {e}"));
    }
    let version = query.get("version").filter(|v| !v.is_empty());
    // `doubleval($request->get('cmlVersion'))`
    let cml_version = match query.get("cmlVersion").map(|v| v.trim()) {
        Some(v) if !v.is_empty() => SchemaVersion::parse(Some(v)).0,
        _ => 0.0,
    };
    let mut fresh = json!({ "type": session::KIND_SALE, "cml_version": cml_version });
    if let Some(v) = version {
        fresh["version"] = json!(v);
    }
    sqlx::query("UPDATE exchange_sessions SET state = $2 WHERE id = $1")
        .bind(session.id)
        .bind(&fresh)
        .execute(&state.db)
        .await
        .map_err(db_err)?;
    let mut text = format!(
        "zip={}\nfile_limit={}",
        if settings.use_zip { "yes" } else { "no" },
        settings.file_limit
    );
    if version.is_some() {
        text.push_str(&format!("\nsessid={token}\nversion=2.09"));
    }
    Ok(Outcome::text(text))
}

/// Схемы документов (cmlVersion от 2.10) не поддерживаются.
fn check_cml_version(session: &Session) -> Result<(), String> {
    if session.state["cml_version"].as_f64().unwrap_or(0.0) >= CML_210_VERSION {
        Err(CML_210.into())
    } else {
        Ok(())
    }
}

/// `query`: заказы к выгрузке в XML; метка выгрузки — в сессии до `success`. Без `version` в
/// сессии выгрузка без ограничения по времени (Битрикс ставит `INTERVAL = 0`).
async fn export(state: &AppState, session: &Session) -> Reply {
    check_cml_version(session)?;
    let raw = session.state["version"].as_str();
    let version = SchemaVersion::parse(raw);
    let interval = match raw {
        Some(_) => Duration::from_secs(
            Settings::load(&state.db)
                .await
                .map_err(db_err)?
                .interval_secs
                .into(),
        ),
        None => Duration::ZERO,
    };
    let s = SaleSettings::load(&state.db).await.map_err(db_err)?;
    let hook = state.project.order_export_hook.as_deref();
    let batch = select::query(&state.db, &s, version, interval, hook)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let xml = document::render(&batch.orders, version, Utc::now());
    // `mark` есть — был query; `null` — выгружать было нечего
    sqlx::query(
        "UPDATE exchange_sessions SET state = jsonb_set(state, '{mark}', $2) WHERE id = $1",
    )
    .bind(session.id)
    .bind(json!(batch.mark))
    .execute(&state.db)
    .await
    .map_err(db_err)?;
    Ok(Outcome::xml(xml).with_stats(json!({ "orders": batch.orders.len() })))
}

/// `success`: подтверждение последнего `query` сессии; без него — `error`.
async fn success(state: &AppState, session: &Session) -> Reply {
    let Some(mark) = session.state.get("mark") else {
        return Ok(Outcome::text("error"));
    };
    if let Some(at) = mark.as_str() {
        let at: DateTime<Utc> = at
            .parse()
            .map_err(|e| format!("Неверная метка выгрузки «{at}»: {e}"))?;
        sale_settings::commit_export(&state.db, at)
            .await
            .map_err(db_err)?;
    }
    sqlx::query("UPDATE exchange_sessions SET state = state - 'mark' WHERE id = $1")
        .bind(session.id)
        .execute(&state.db)
        .await
        .map_err(db_err)?;
    Ok(Outcome::text("success"))
}

/// Итог приёма для ответа 1С: «Создано: заказы 1. Обновлено: оплаты 2».
pub(crate) fn summary(stats: &SaleStats) -> String {
    let mut parts = Vec::new();
    for (label, counts) in [
        (
            "Создано",
            [
                ("заказы", stats.orders_created),
                ("оплаты", stats.payments_created),
                ("отгрузки", stats.shipments_created),
            ],
        ),
        (
            "Обновлено",
            [
                ("заказы", stats.orders_updated),
                ("оплаты", stats.payments_updated),
                ("отгрузки", stats.shipments_updated),
            ],
        ),
    ] {
        let items: Vec<String> = counts
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(name, n)| format!("{name} {n}"))
            .collect();
        if !items.is_empty() {
            parts.push(format!("{label}: {}", items.join(", ")));
        }
    }
    if stats.skipped > 0 {
        parts.push(format!("Пропущено: {}", stats.skipped));
    }
    if parts.is_empty() {
        "Изменений нет".into()
    } else {
        parts.join(". ")
    }
}

/// `import`: распаковка zip, затем документы файла по шагам — группы с позиции из сессии,
/// пока не истёк шаг.
/// Ответ `progress` — файл не дочитан; `success` с итогом или ошибками документов построчно.
/// Старый протокол (`init` без `version`): `file` сразу загружает файл.
pub(crate) fn old_protocol(session: &Session) -> bool {
    session.state.get("version").is_none()
}

/// `file` старого протокола (`component.php`, ветка «old version»): тело дописывается к файлу,
/// и файл (у архива — первый файл архива) сразу загружается целиком.
async fn old_file(
    state: &AppState,
    session: &Session,
    filename: &str,
    headers: &HeaderMap,
    body: Body,
) -> Reply {
    file(state, session, filename, headers, body).await?;
    let is_zip = !filename.contains('/')
        && std::path::Path::new(filename)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if !is_zip {
        return import(state, session, filename, Step::Whole).await;
    }
    let path = session::dir(&state.config.upload_dir, session).join(filename);
    let first = tokio::task::spawn_blocking(move || first_zip_file(&path))
        .await
        .map_err(|e| e.to_string())?
        // `CC_BSC1_ZIP_ERROR`
        .ok_or("Ошибка распаковки архива.")?;
    import(state, session, &first, Step::Whole).await
}

/// Имя первого файла архива (`current($rArchiver)['filename']`).
fn first_zip_file(path: &std::path::Path) -> Option<String> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path).ok()?).ok()?;
    (0..archive.len()).find_map(|i| {
        let entry = archive.by_index_raw(i).ok()?;
        (!entry.is_dir()).then(|| entry.name().to_string())
    })
}

/// Длина шага `import`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Step {
    /// Интервал настроек; 0 — весь файл за вызов.
    Settings,
    /// Заданная длина (тесты).
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(Duration),
    /// Весь файл за вызов.
    Whole,
}

pub(crate) async fn import(
    state: &AppState,
    session: &Session,
    filename: &str,
    step: Step,
) -> Reply {
    check_cml_version(session)?;
    let rel = session::safe_rel_path(filename)
        .ok_or_else(|| session::SessionError::BadPath(filename.into()).to_string())?;
    let dir = session::dir(&state.config.upload_dir, session);
    let unpack_dir = dir.clone();
    tokio::task::spawn_blocking(move || session::unpack_zips(&unpack_dir, Limits::default()))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let data = tokio::fs::read(dir.join(&rel)).await.unwrap_or_default();
    if data.is_empty() {
        // `CC_BSC1_EMPTY_CML`: нет файла или он пуст
        return Err("Файл для импорта пуст.".into());
    }
    let parsed = tokio::task::spawn_blocking(move || parse::parse(&data))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let s = SaleSettings::load(&state.db).await.map_err(db_err)?;
    let map = sale_settings::load_map(&state.db).await.map_err(db_err)?;
    let step = match step {
        Step::Fixed(step) => Some(step),
        Step::Whole => None,
        Step::Settings => {
            let secs = Settings::load(&state.db)
                .await
                .map_err(db_err)?
                .interval_secs;
            (secs > 0).then(|| Duration::from_secs(secs.into()))
        }
    };

    let mut tx = state.db.begin().await.map_err(db_err)?;
    // Блокировка строки сессии: два одновременных import одного файла не применят его дважды
    let current: Value =
        sqlx::query_scalar("SELECT state FROM exchange_sessions WHERE id = $1 FOR UPDATE")
            .bind(session.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
    let fresh = Session {
        state: current,
        ..session.clone()
    };
    if fresh.is_imported(filename) {
        return Ok(Outcome::text("success\nФайл уже загружен"));
    }
    let start = fresh.state["sale_pos"][filename].as_u64().unwrap_or(0) as usize;
    let deadline = step.map(|step| Instant::now() + step);
    let hook = state.project.order_import_hook.as_deref();
    let (step_stats, next) =
        apply::apply_file(&state.db, &s, &map, &parsed, hook, start, deadline).await;
    // Итог прошлых шагов файла (`progress`) — в итог `success`
    let mut stats: SaleStats =
        serde_json::from_value(fresh.state["sale_stats"][filename].clone()).unwrap_or_default();
    stats.merge(step_stats.clone());
    let saved = serde_json::to_value(&stats).unwrap_or_default();
    match next {
        Some(next) => sqlx::query(
            "UPDATE exchange_sessions
             SET state = jsonb_set(
                     jsonb_set(state, '{sale_pos}',
                               COALESCE(state->'sale_pos', '{}'::jsonb)
                                   || jsonb_build_object($2::text, $3::bigint)),
                     '{sale_stats}',
                     COALESCE(state->'sale_stats', '{}'::jsonb) || jsonb_build_object($2::text, $4::jsonb))
             WHERE id = $1",
        )
        .bind(session.id)
        .bind(filename)
        .bind(next as i64)
        .bind(&saved),
        None => sqlx::query(
            "UPDATE exchange_sessions
             SET state = jsonb_set(
                     jsonb_set(
                         jsonb_set(state, '{imported}',
                                   COALESCE(state->'imported', '[]'::jsonb) || to_jsonb($2::text)),
                         '{sale_pos}', COALESCE(state->'sale_pos', '{}'::jsonb) - $2::text),
                     '{sale_stats}', COALESCE(state->'sale_stats', '{}'::jsonb) - $2::text)
             WHERE id = $1",
        )
        .bind(session.id)
        .bind(filename),
    }
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    // `progress` — итог шага, `success` — итог всего файла
    let stats = if next.is_some() { step_stats } else { stats };
    tx.commit().await.map_err(db_err)?;

    // Письма о смене статуса — после коммита, не задерживая ответ
    for (event, order_id) in &stats.notify {
        let (state, event, order_id) = (state.clone(), event.clone(), *order_id);
        tokio::spawn(async move { crate::sale::mail::notify(&state, &event, order_id).await });
    }
    let first = if next.is_some() {
        "progress"
    } else {
        "success"
    };
    let tail = if !stats.errors.is_empty() {
        stats.errors.join("\n")
    } else if next.is_some() {
        String::new()
    } else {
        summary(&stats)
    };
    let text = if tail.is_empty() {
        first.to_string()
    } else {
        format!("{first}\n{tail}")
    };
    Ok(Outcome::text(text).with_stats(serde_json::to_value(&stats).unwrap_or_default()))
}

/// `info`: справочники — статусы, активные платёжные системы с типом оплаты, активные службы
/// доставки, по сортировке.
async fn info(state: &AppState) -> Reply {
    let statuses: Vec<(String, String)> =
        sqlx::query_as("SELECT code, name FROM order_statuses ORDER BY sort, code")
            .fetch_all(&state.db)
            .await
            .map_err(db_err)?;
    let pay_systems: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT id, name, api_type, handler FROM pay_systems WHERE active ORDER BY sort, id",
    )
    .fetch_all(&state.db)
    .await
    .map_err(db_err)?;
    let pay_systems: Vec<(i64, String, PayOperation)> = pay_systems
        .into_iter()
        .map(|(id, name, api_type, handler)| (id, name, select::pay_operation(&api_type, &handler)))
        .collect();
    let deliveries: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM deliveries WHERE active ORDER BY sort, id")
            .fetch_all(&state.db)
            .await
            .map_err(db_err)?;
    Ok(Outcome::xml(document::render_info(
        &statuses,
        &pay_systems,
        &deliveries,
    )))
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use sqlx::PgPool;

    use super::*;
    use crate::exchange::http::tests::{
        Client, exchange_user, exchange_user_with, login, make_zip, upload,
    };
    use crate::exchange::sale::settings::SaleSettings;
    use crate::test_support::{order_fixture, place_order, test_state};

    const XML: &str = "application/xml; charset=windows-1251";
    const OTHER_KIND: &str = "failure\nСессия открыта для другого типа обмена";

    async fn sale_user(db: &PgPool, login: &str) -> i64 {
        exchange_user_with(db, login, "exchange.1c.sale").await
    }

    fn cp1251(bytes: &[u8]) -> String {
        let (text, _, bad) = encoding_rs::WINDOWS_1251.decode(bytes);
        assert!(!bad);
        text.into_owned()
    }

    /// `query` сессии: Ид выгруженных документов заказа.
    async fn query_ids(c: &Client, token: &str) -> Vec<i64> {
        let (ct, body) = c.get_bytes(token, "mode=query").await;
        assert_eq!(ct, XML);
        let text = cp1251(&body);
        assert!(
            text.starts_with(r#"<?xml version="1.0" encoding="windows-1251"?>"#),
            "{text}"
        );
        text.split("<Документ>")
            .skip(1)
            .map(|d| {
                let id = d
                    .split("<Ид>")
                    .nth(1)
                    .unwrap()
                    .split("</Ид>")
                    .next()
                    .unwrap();
                id.parse().unwrap()
            })
            .collect()
    }

    async fn log(db: &PgPool) -> Vec<(String, String, String, serde_json::Value)> {
        sqlx::query_as("SELECT kind, mode, result, stats FROM exchange_log ORDER BY id")
            .fetch_all(db)
            .await
            .unwrap()
    }

    async fn log_len(db: &PgPool) -> usize {
        log(db).await.len()
    }

    #[sqlx::test]
    async fn sale_permission_is_separate(db: PgPool) {
        exchange_user(&db, "cat").await;
        sale_user(&db, "sale").await;
        let state = test_state(db.clone());
        let sale = Client::with_type(&state, "sale");
        let catalog = Client::new(&state);
        let denied = "failure\nДоступ запрещён";
        assert_eq!(sale.checkauth("cat", "password123").await, denied);
        assert_eq!(catalog.checkauth("sale", "password123").await, denied);
        let sale_token = login(&sale, "sale").await;
        assert!(sale.get(&sale_token, "mode=init").await.starts_with("zip="));
        let token = login(&catalog, "cat").await;
        assert!(catalog.get(&token, "mode=init").await.starts_with("zip="));

        // Право отозвано у живой сессии
        sqlx::query("DELETE FROM group_permissions WHERE permission = 'exchange.1c.sale'")
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(sale.get(&sale_token, "mode=init").await, denied);
        assert!(catalog.get(&token, "mode=init").await.starts_with("zip="));
    }

    #[sqlx::test]
    async fn init_with_and_without_version(db: PgPool) {
        sale_user(&db, "sale").await;
        let c = Client::with_type(&test_state(db.clone()), "sale");
        let token = login(&c, "sale").await;
        assert_eq!(
            c.get(&token, "mode=init").await,
            "zip=yes\nfile_limit=209715200"
        );
        assert_eq!(
            c.get(&token, "mode=init&version=2.08").await,
            format!("zip=yes\nfile_limit=209715200\nsessid={token}\nversion=2.09")
        );
        let state: serde_json::Value = sqlx::query_scalar("SELECT state FROM exchange_sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(state["type"], "sale");
        assert_eq!(state["version"], "2.08");
    }

    #[sqlx::test]
    async fn query_success_cycle(db: PgPool) {
        sale_user(&db, "sale").await;
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        let c = Client::with_type(&test_state(db.clone()), "sale");
        let token = login(&c, "sale").await;
        c.get(&token, "mode=init&version=2.08").await;

        let (ct, body) = c.get_bytes(&token, "mode=query").await;
        assert_eq!(ct, XML);
        let text = cp1251(&body);
        assert!(
            text.contains(r#"<КоммерческаяИнформация ВерсияСхемы="2.08""#),
            "{text}"
        );
        assert!(text.contains(&format!("<Ид>{id}</Ид>")), "{text}");
        assert!(text.contains("<НомерВерсии>"), "{text}");
        // query без success отдаёт то же
        assert_eq!(query_ids(&c, &token).await, [id]);

        assert_eq!(c.get(&token, "mode=success").await, "success");
        assert!(query_ids(&c, &token).await.is_empty());
        // Подтверждённая метка — время изменения заказа
        let updated: DateTime<Utc> = sqlx::query_scalar("SELECT updated_at FROM orders")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(
            SaleSettings::load(&db).await.unwrap().last_export_committed,
            Some(updated)
        );
        // Пустой query тоже подтверждается
        assert_eq!(c.get(&token, "mode=success").await, "success");
        assert_eq!(
            SaleSettings::load(&db).await.unwrap().last_export_committed,
            Some(updated)
        );

        let log = log(&db).await;
        let rows: Vec<(&str, &str, &str)> = log
            .iter()
            .map(|l| (l.0.as_str(), l.1.as_str(), l.2.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("sale", "query", "success"),
                ("sale", "query", "success"),
                ("sale", "success", "success"),
                ("sale", "query", "success"),
                ("sale", "success", "success"),
            ]
        );
        assert_eq!(log[0].3["orders"], 1);
        assert_eq!(log[3].3["orders"], 0);
    }

    #[sqlx::test]
    async fn query_without_version_is_schema_205(db: PgPool) {
        sale_user(&db, "sale").await;
        let f = order_fixture(&db).await;
        let a = place_order(&db, &f, None).await;
        let b = place_order(&db, &f, None).await;
        let c = Client::with_type(&test_state(db.clone()), "sale");
        let token = login(&c, "sale").await;
        c.get(&token, "mode=init").await;
        let (_, body) = c.get_bytes(&token, "mode=query").await;
        let text = cp1251(&body);
        // Без version — схема 2.05: без НомерВерсии
        assert!(text.contains(r#"ВерсияСхемы="2.05""#), "{text}");
        assert!(!text.contains("<НомерВерсии>"), "{text}");
        assert_eq!(query_ids(&c, &token).await, [a, b]);
    }

    #[sqlx::test]
    async fn success_without_query_is_error(db: PgPool) {
        sale_user(&db, "sale").await;
        let c = Client::with_type(&test_state(db.clone()), "sale");
        let token = login(&c, "sale").await;
        assert_eq!(c.get(&token, "mode=success").await, "error");
        c.get(&token, "mode=init&version=2.08").await;
        assert_eq!(c.get(&token, "mode=success").await, "error");
        // Повторный success после подтверждения — тоже без выгрузки
        c.get_bytes(&token, "mode=query").await;
        assert_eq!(c.get(&token, "mode=success").await, "success");
        assert_eq!(c.get(&token, "mode=success").await, "error");
        // init начинает обмен заново
        c.get_bytes(&token, "mode=query").await;
        c.get(&token, "mode=init&version=2.08").await;
        assert_eq!(c.get(&token, "mode=success").await, "error");
    }

    /// Документ заказа из 1С со статусом.
    fn status_doc(id: i64, version: &str, code: &str) -> String {
        format!(
            r#"
	<Документ>
		<Ид>{id}</Ид>
		<Номер>{id}</Номер>
		<Дата>2026-10-15</Дата>
		<ХозОперация>Заказ товара</ХозОперация>
		<Роль>Продавец</Роль>
		<Валюта>руб.</Валюта>
		<Курс>1</Курс>
		<Сумма>100</Сумма>
		<НомерВерсии>{version}</НомерВерсии>
		<Номер1С>ЗК-{id}</Номер1С>
		<Комментарий>Комментарий менеджера, достаточно длинный, чтобы файл ушёл частями</Комментарий>
		<ЗначенияРеквизитов>
			<ЗначениеРеквизита>
				<Наименование>Статуса заказа ИД</Наименование>
				<Значение>{code}</Значение>
			</ЗначениеРеквизита>
			<ЗначениеРеквизита>
				<Наименование>Отменен</Наименование>
				<Значение>false</Значение>
			</ЗначениеРеквизита>
		</ЗначенияРеквизитов>
	</Документ>"#
        )
    }

    /// Файл обмена из документов.
    fn cml(docs: &[String]) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<КоммерческаяИнформация ВерсияСхемы="2.08" ДатаФормирования="2026-10-16T09:00:00">{}
</КоммерческаяИнформация>"#,
            docs.concat()
        )
    }

    async fn statuses(db: &PgPool) {
        sqlx::query(
            "INSERT INTO order_statuses (code, name, sort, notify) VALUES ('F', 'Выполнен', 200, TRUE),
                 ('P', 'Собирается', 150, FALSE)",
        )
        .execute(db)
        .await
        .unwrap();
    }

    async fn status_of(db: &PgPool, id: i64) -> String {
        sqlx::query_scalar("SELECT status FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn file_and_import_plain_and_zip(db: PgPool) {
        sale_user(&db, "sale").await;
        let f = order_fixture(&db).await;
        statuses(&db).await;
        SaleSettings {
            change_status_from_1c: true,
            ..SaleSettings::default()
        }
        .save(&db)
        .await
        .unwrap();
        let a = place_order(&db, &f, None).await;
        let b = place_order(&db, &f, None).await;
        let state = test_state(db.clone());
        let c = Client::with_type(&state, "sale");
        let token = login(&c, "sale").await;
        c.get(&token, "mode=init&version=2.08").await;

        let plain = cml(&[status_doc(a, "1", "F")]);
        assert!(plain.len() > 1024, "файл должен уйти частями");
        upload(&c, &token, "orders.xml", plain.as_bytes()).await;
        let r = c.get(&token, "mode=import&filename=orders.xml").await;
        assert_eq!(r, "success\nОбновлено: заказы 1");
        assert_eq!(status_of(&db, a).await, "F");

        let zip = make_zip(&[("in/b.xml", cml(&[status_doc(b, "1", "P")]).as_bytes())]);
        upload(&c, &token, "pack.zip", &zip).await;
        let r = c.get(&token, "mode=import&filename=in/b.xml").await;
        assert_eq!(r, "success\nОбновлено: заказы 1");
        assert_eq!(status_of(&db, b).await, "P");

        // Загруженный файл повторно не применяется
        sqlx::query("UPDATE orders SET status = 'N' WHERE id = $1")
            .bind(a)
            .execute(&db)
            .await
            .unwrap();
        let r = c.get(&token, "mode=import&filename=orders.xml").await;
        assert!(r.starts_with("success"), "{r}");
        assert_eq!(status_of(&db, a).await, "N");

        // Ошибки документов — построчно после success
        let bad = cml(&[status_doc(a, "2", "Q"), status_doc(b, "2", "Q")]);
        upload(&c, &token, "bad.xml", bad.as_bytes()).await;
        let r = c.get(&token, "mode=import&filename=bad.xml").await;
        assert_eq!(
            r,
            format!("success\nДокумент {a}: статус Q не найден\nДокумент {b}: статус Q не найден")
        );

        // Нет файла — failure
        let r = c.get(&token, "mode=import&filename=none.xml").await;
        assert_eq!(r, "failure\nФайл для импорта пуст.");
        upload(&c, &token, "empty.xml", b"").await;
        let r = c.get(&token, "mode=import&filename=empty.xml").await;
        assert_eq!(r, "failure\nФайл для импорта пуст.");
        assert_eq!(
            c.get(&token, "mode=bogus").await,
            "failure\nНеизвестная команда."
        );

        let log = log(&db).await;
        let imports: Vec<_> = log.iter().filter(|l| l.1 == "import").collect();
        assert_eq!(imports.len(), 6);
        assert!(imports.iter().all(|l| l.0 == "sale"));
        assert_eq!(imports[0].2, "success");
        assert_eq!(imports[0].3["orders_updated"], 1);
        assert_eq!(imports[4].2, "failure");
        assert_eq!(imports[5].2, "failure");
    }

    #[sqlx::test]
    async fn old_protocol_file_imports_at_once(db: PgPool) {
        sale_user(&db, "sale").await;
        let f = order_fixture(&db).await;
        statuses(&db).await;
        SaleSettings {
            change_status_from_1c: true,
            ..SaleSettings::default()
        }
        .save(&db)
        .await
        .unwrap();
        let a = place_order(&db, &f, None).await;
        let b = place_order(&db, &f, None).await;
        let c = Client::with_type(&test_state(db.clone()), "sale");
        let token = login(&c, "sale").await;
        // Без version — старый протокол: file сразу загружает документы
        c.get(&token, "mode=init").await;

        let data = cml(&[status_doc(a, "1", "F"), status_doc(b, "1", "Q")]);
        let r = c
            .post(
                &token,
                "mode=file&filename=orders.xml",
                data.clone().into_bytes(),
            )
            .await;
        assert_eq!(r, format!("success\nДокумент {b}: статус Q не найден"));
        assert_eq!(status_of(&db, a).await, "F");

        // Повтор того же файла — без изменений
        sqlx::query("UPDATE orders SET status = 'N' WHERE id = $1")
            .bind(a)
            .execute(&db)
            .await
            .unwrap();
        let r = c
            .post(&token, "mode=file&filename=orders.xml", data.into_bytes())
            .await;
        assert!(r.starts_with("success"), "{r}");
        assert_eq!(status_of(&db, a).await, "N");

        // Архив: загружается первый файл архива
        let zip = make_zip(&[("in/b.xml", cml(&[status_doc(b, "2", "P")]).as_bytes())]);
        let r = c.post(&token, "mode=file&filename=pack.zip", zip).await;
        assert_eq!(r, "success\nОбновлено: заказы 1");
        assert_eq!(status_of(&db, b).await, "P");

        // Пустой файл — failure
        let r = c.post(&token, "mode=file&filename=empty.xml", vec![]).await;
        assert_eq!(r, "failure\nФайл для импорта пуст.");

        let log = log(&db).await;
        let rows: Vec<(&str, &str, &str)> = log
            .iter()
            .map(|l| (l.0.as_str(), l.1.as_str(), l.2.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("sale", "file", "success"),
                ("sale", "file", "success"),
                ("sale", "file", "success"),
                ("sale", "file", "failure"),
            ]
        );
        assert_eq!(log[0].3["orders_updated"], 1);

        // С version file только дописывает
        c.get(&token, "mode=init&version=2.08").await;
        let data = cml(&[status_doc(a, "2", "F")]);
        let r = c
            .post(&token, "mode=file&filename=new.xml", data.into_bytes())
            .await;
        assert_eq!(r, "success");
        assert_eq!(status_of(&db, a).await, "N");
        assert_eq!(log_len(&db).await, 4);
    }

    #[sqlx::test]
    async fn import_is_stepwise(db: PgPool) {
        let user = sale_user(&db, "sale").await;
        let f = order_fixture(&db).await;
        statuses(&db).await;
        SaleSettings {
            change_status_from_1c: true,
            ..SaleSettings::default()
        }
        .save(&db)
        .await
        .unwrap();
        let a = place_order(&db, &f, None).await;
        let b = place_order(&db, &f, None).await;
        let state = test_state(db.clone());
        let (s, _) = session::start(&db, user, session::KIND_SALE).await.unwrap();
        let dir = session::dir(&state.config.upload_dir, &s);
        let data = cml(&[status_doc(a, "1", "P"), status_doc(b, "1", "Q")]);
        session::append(&dir, "o.xml", data.as_bytes())
            .await
            .unwrap();

        // Шаг истёк после первой группы: progress, позиция — в сессии
        let r = import(&state, &s, "o.xml", Step::Fixed(Duration::ZERO))
            .await
            .unwrap();
        assert_eq!(String::from_utf8(r.body).unwrap(), "progress");
        assert_eq!(status_of(&db, a).await, "P");
        assert_eq!(status_of(&db, b).await, "N");
        let st: Value = sqlx::query_scalar("SELECT state FROM exchange_sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(st["sale_pos"]["o.xml"], 1);

        // Следующий шаг — с позиции: первый документ не применяется повторно
        let r = import(&state, &s, "o.xml", Step::Fixed(Duration::ZERO))
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            format!("success\nДокумент {b}: статус Q не найден")
        );
        // Итог success — по всему файлу, с первым шагом
        assert_eq!(r.stats["orders_updated"], 1);
        let st: Value = sqlx::query_scalar("SELECT state FROM exchange_sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(st["sale_pos"], json!({}));
        assert_eq!(st["imported"], json!(["o.xml"]));
    }

    #[sqlx::test]
    async fn stepwise_import_reports_errors_of_all_steps(db: PgPool) {
        let user = sale_user(&db, "sale").await;
        let f = order_fixture(&db).await;
        statuses(&db).await;
        SaleSettings {
            change_status_from_1c: true,
            ..SaleSettings::default()
        }
        .save(&db)
        .await
        .unwrap();
        let a = place_order(&db, &f, None).await;
        let b = place_order(&db, &f, None).await;
        let state = test_state(db.clone());
        let (s, _) = session::start(&db, user, session::KIND_SALE).await.unwrap();
        let dir = session::dir(&state.config.upload_dir, &s);
        let data = cml(&[
            status_doc(a, "1", "Q"),
            status_doc(b, "1", "P"),
            status_doc(a, "2", "P"),
        ]);
        session::append(&dir, "o.xml", data.as_bytes())
            .await
            .unwrap();
        let step = || import(&state, &s, "o.xml", Step::Fixed(Duration::ZERO));
        let r = step().await.unwrap();
        assert!(String::from_utf8(r.body).unwrap().starts_with("progress"));
        let r = step().await.unwrap();
        assert!(String::from_utf8(r.body).unwrap().starts_with("progress"));
        let r = step().await.unwrap();
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            format!("success\nДокумент {a}: статус Q не найден")
        );
        assert_eq!(r.stats["orders_updated"], 2);
        assert_eq!(r.stats["errors"].as_array().unwrap().len(), 1);
        let st: Value = sqlx::query_scalar("SELECT state FROM exchange_sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(st["sale_stats"], json!({}));
    }

    #[sqlx::test]
    async fn info_lists_statuses_pay_systems_deliveries(db: PgPool) {
        sale_user(&db, "sale").await;
        sqlx::query(
            "INSERT INTO order_statuses (code, name, sort) VALUES ('N', 'Принят', 100),
                 ('F', 'Выполнен', 300), ('P', 'Собирается <упаковка>', 200)",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO pay_systems (id, name, sort, active, api_type, handler) VALUES
                 (1, 'Наличные', 100, TRUE, 'cash', ''),
                 (2, 'Счёт', 200, TRUE, 'cashless', ''),
                 (3, 'ЮKassa', 50, TRUE, 'other', 'yookassa'),
                 (4, 'Старая', 10, FALSE, 'cash', '')",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO deliveries (id, name, sort, active) VALUES
                 (1, 'Самовывоз', 100, TRUE), (2, 'Курьер', 50, TRUE), (3, 'Почта', 10, FALSE)",
        )
        .execute(&db)
        .await
        .unwrap();
        let c = Client::with_type(&test_state(db.clone()), "sale");
        let token = login(&c, "sale").await;
        let (ct, body) = c.get_bytes(&token, "mode=info").await;
        assert_eq!(ct, XML);
        // «Cтатусы» — с латинской C, как в языковом файле Битрикса
        let expected = r#"<?xml version="1.0" encoding="windows-1251"?>
<Справочник>
	<Cтатусы>
		<Элемент>
			<Ид>N</Ид>
			<Название>Принят</Название>
		</Элемент>
		<Элемент>
			<Ид>P</Ид>
			<Название>Собирается &lt;упаковка&gt;</Название>
		</Элемент>
		<Элемент>
			<Ид>F</Ид>
			<Название>Выполнен</Название>
		</Элемент>
	</Cтатусы>
	<ПлатежныеСистемы>
		<Элемент>
			<Ид>3</Ид>
			<Название>ЮKassa</Название>
			<ТипОплаты>Эквайринговая оплата</ТипОплаты>
		</Элемент>
		<Элемент>
			<Ид>1</Ид>
			<Название>Наличные</Название>
			<ТипОплаты>Наличная оплата</ТипОплаты>
		</Элемент>
		<Элемент>
			<Ид>2</Ид>
			<Название>Счёт</Название>
			<ТипОплаты>Безналичная оплата</ТипОплаты>
		</Элемент>
	</ПлатежныеСистемы>
	<СлужбыДоставки>
		<Элемент>
			<Ид>2</Ид>
			<Название>Курьер</Название>
		</Элемент>
		<Элемент>
			<Ид>1</Ид>
			<Название>Самовывоз</Название>
		</Элемент>
	</СлужбыДоставки>
</Справочник>"#;
        assert_eq!(cp1251(&body), expected);
        // «ё» есть в windows-1251, байт 0xB8
        assert!(body.contains(&0xB8));
    }

    #[sqlx::test]
    async fn cml_version_210_fails(db: PgPool) {
        sale_user(&db, "sale").await;
        let c = Client::with_type(&test_state(db.clone()), "sale");
        let token = login(&c, "sale").await;
        let r = c.get(&token, "mode=init&version=3.1&cmlVersion=2.10").await;
        assert!(r.starts_with("zip=yes"), "{r}");
        let fail = "failure\nСхема обмена cmlVersion ≥ 2.10 не поддерживается";
        assert_eq!(c.get(&token, "mode=query").await, fail);
        upload(&c, &token, "o.xml", b"<x/>").await;
        assert_eq!(c.get(&token, "mode=import&filename=o.xml").await, fail);
        // Меньшая версия — обычный обмен
        c.get(&token, "mode=init&version=3.1&cmlVersion=2.09").await;
        let (ct, _) = c.get_bytes(&token, "mode=query").await;
        assert_eq!(ct, XML);
    }

    #[sqlx::test]
    async fn sale_sessions_are_isolated(db: PgPool) {
        sale_user(&db, "one").await;
        sale_user(&db, "two").await;
        exchange_user(&db, "cat").await;
        let f = order_fixture(&db).await;
        let state = test_state(db.clone());
        let c = Client::with_type(&state, "sale");
        let ta = login(&c, "one").await;
        let tb = login(&c, "two").await;
        c.get(&ta, "mode=init&version=2.08").await;
        c.get(&tb, "mode=init&version=2.08").await;

        let o1 = place_order(&db, &f, None).await;
        assert_eq!(query_ids(&c, &ta).await, [o1]);
        let o2 = place_order(&db, &f, None).await;
        assert_eq!(query_ids(&c, &tb).await, [o1, o2]);
        assert_eq!(c.get(&ta, "mode=success").await, "success");
        // Подтверждение A не трогает выгрузку B: заказ 2 ещё выгружается B
        assert_eq!(query_ids(&c, &tb).await, [o2]);
        assert_eq!(c.get(&tb, "mode=success").await, "success");
        assert!(query_ids(&c, &tb).await.is_empty());

        // Сессия каталога не принимает запросы обмена заказами, и наоборот
        let catalog = Client::new(&state);
        let tc = login(&catalog, "cat").await;
        assert_eq!(c.get(&tc, "mode=query").await, OTHER_KIND);
        assert_eq!(catalog.get(&ta, "mode=init").await, OTHER_KIND);
        // Сессия без типа (до обмена заказами) — каталоговая
        sqlx::query("UPDATE exchange_sessions SET state = '{}'")
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(c.get(&ta, "mode=query").await, OTHER_KIND);
        assert!(catalog.get(&tc, "mode=init").await.starts_with("zip="));
    }
}
