//! Компиляция и запуск Rust-скриптов супер-администратора.
//!
//! Код скрипта — тело `async fn main`; генерируется временный crate с `sqlx`,
//! `serde_json`, `chrono`, `uuid` и `tokio`. Сборка и запуск идут отдельными
//! процессами с таймаутами и общим `CARGO_TARGET_DIR`, поэтому зависимости
//! компилируются один раз. Одновременно выполняется только один скрипт.
//!
//! Это не песочница: код получает `DATABASE_URL` и права процесса CMS.

use std::{
    env,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Mutex,
    time::timeout,
};

/// Сколько байт stdout и stderr показываем и храним (каждый поток).
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Таймаут сборки по умолчанию, секунды.
const DEFAULT_BUILD_TIMEOUT_SECS: u64 = 180;
/// Таймаут выполнения по умолчанию, секунды.
const DEFAULT_RUN_TIMEOUT_SECS: u64 = 30;
/// Общий каталог сборки по умолчанию (относительно каталога проекта).
const DEFAULT_TARGET_DIR: &str = "target/rust-scripts";
/// Имя пакета временного crate; оно же имя бинарника.
const CRATE_NAME: &str = "cms-rust-script";

/// Генерируемый `Cargo.toml`; версии совпадают с проектом.
const CARGO_TOML: &str = r#"[package]
name = "cms-rust-script"
version = "0.1.0"
edition = "2021"

[dependencies]
sqlx = { version = "0.9", default-features = false, features = ["runtime-tokio", "postgres", "chrono", "json", "uuid"] }
serde_json = "1"
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1", features = ["v4"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
"#;

/// Один скрипт за раз: процессор и каталог сборки общие.
static RUN_LOCK: Mutex<()> = Mutex::const_new(());

/// В тестах долгие запуски выполняются по очереди: иначе второй получит
/// ожидаемую ошибку «уже выполняется» и тест станет нестабильным.
#[cfg(test)]
pub(crate) static TEST_RUN_LOCK: Mutex<()> = Mutex::const_new(());

/// Итог успешного запуска.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    /// Хотя бы один поток обрезан по [`MAX_OUTPUT_BYTES`].
    pub truncated: bool,
}

/// Генерирует `src/main.rs` для пользовательского кода.
pub fn wrapper(user_code: &str) -> String {
    format!(
        "#[tokio::main]\n\
         async fn main() -> Result<(), Box<dyn std::error::Error>> {{\n\
         \x20   let url = std::env::var(\"DATABASE_URL\")?;\n\
         \x20   let pool = sqlx::PgPool::connect(&url).await?;\n\
         {user_code}\n\
         \x20   Ok(())\n\
         }}\n"
    )
}

/// Собирает и запускает код в отдельном временном crate.
///
/// `Err` — toolchain недоступен, идёт другой запуск, ошибка сборки/запуска или
/// таймаут. Текст ошибки содержит вывод cargo/скрипта.
pub async fn run(user_code: &str) -> Result<RunOutput, String> {
    let build_limit = parse_seconds(
        env::var("RUST_SCRIPT_BUILD_TIMEOUT").ok().as_deref(),
        DEFAULT_BUILD_TIMEOUT_SECS,
    );
    let run_limit = parse_seconds(
        env::var("RUST_SCRIPT_RUN_TIMEOUT").ok().as_deref(),
        DEFAULT_RUN_TIMEOUT_SECS,
    );
    run_with_limits(user_code, build_limit, run_limit).await
}

/// [`run`] с явными таймаутами; используется тестами.
pub(crate) async fn run_with_limits(
    user_code: &str,
    build_limit: Duration,
    run_limit: Duration,
) -> Result<RunOutput, String> {
    let _guard = acquire_lock()?;
    let database_url = env::var("DATABASE_URL")
        .map_err(|_| "DATABASE_URL не задан: скрипт не сможет подключиться к БД.".to_string())?;
    if let Some(error) = toolchain_error() {
        return Err(error);
    }

    let started = Instant::now();
    let cwd = env::current_dir()
        .map_err(|error| format!("Не удалось определить текущий каталог: {error}"))?;
    let raw_target = env::var("RUST_SCRIPT_TARGET_DIR").ok();
    let target_dir = target_dir_from(raw_target.as_deref(), &cwd);
    tokio::fs::create_dir_all(&target_dir)
        .await
        .map_err(|error| {
            format!(
                "Не удалось создать каталог сборки {}: {error}",
                target_dir.display()
            )
        })?;

    let crate_dir = env::temp_dir()
        .join("cms-rust")
        .join(uuid::Uuid::new_v4().to_string());
    let result = run_crate(
        &crate_dir,
        user_code,
        &database_url,
        &target_dir,
        build_limit,
        run_limit,
    )
    .await;
    let _ = tokio::fs::remove_dir_all(&crate_dir).await;

    result.map(|mut output| {
        output.duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        output
    })
}

/// Готовит временный crate, собирает и запускает бинарник.
async fn run_crate(
    crate_dir: &Path,
    user_code: &str,
    database_url: &str,
    target_dir: &Path,
    build_limit: Duration,
    run_limit: Duration,
) -> Result<RunOutput, String> {
    let src = crate_dir.join("src");
    tokio::fs::create_dir_all(&src)
        .await
        .map_err(|error| format!("Не удалось создать временный каталог: {error}"))?;
    tokio::fs::write(crate_dir.join("Cargo.toml"), CARGO_TOML)
        .await
        .map_err(|error| format!("Не удалось записать Cargo.toml: {error}"))?;
    tokio::fs::write(src.join("main.rs"), wrapper(user_code))
        .await
        .map_err(|error| format!("Не удалось записать main.rs: {error}"))?;

    let mut build = command("cargo", crate_dir, target_dir, database_url);
    build.arg("build").arg("--quiet");
    let build = run_process(build, build_limit).await?;
    if build.status.is_none() {
        return Err(timeout_error("Сборка", build_limit, &build));
    }
    if !build.success() {
        return Err(process_error("Сборка не удалась", &build));
    }

    let binary = target_dir.join("debug").join(exe_name(CRATE_NAME));
    let exec = command(&binary, crate_dir, target_dir, database_url);
    let exec = run_process(exec, run_limit).await?;
    if exec.status.is_none() {
        return Err(timeout_error("Скрипт", run_limit, &exec));
    }
    if !exec.success() {
        return Err(process_error("Скрипт завершился с ошибкой", &exec));
    }

    Ok(RunOutput {
        stdout: exec.stdout,
        stderr: exec.stderr,
        duration_ms: 0,
        truncated: exec.truncated,
    })
}

/// Команда с минимальным окружением: только разрешённые переменные.
fn command(
    program: impl AsRef<OsStr>,
    dir: &Path,
    target_dir: &Path,
    database_url: &str,
) -> Command {
    let mut command = Command::new(program);
    command.current_dir(dir).env_clear();
    for (key, value) in child_env(database_url, target_dir) {
        command.env(key, value);
    }
    command
}

/// Переменные окружения дочернего процесса.
///
/// Секреты приложения (`SESSION_SECRET`, SMTP и т. п.) не передаются. Cargo
/// должен видеть `DATABASE_URL`: `sqlx::query!` проверяет запросы при сборке.
fn child_env(database_url: &str, target_dir: &Path) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME"]
        .into_iter()
        .filter_map(|key| {
            env::var_os(key).map(|value| (key.to_string(), value.to_string_lossy().into_owned()))
        })
        .collect();
    vars.push(("DATABASE_URL".to_string(), database_url.to_string()));
    vars.push((
        "CARGO_TARGET_DIR".to_string(),
        target_dir.to_string_lossy().into_owned(),
    ));
    vars
}

/// Результат одного процесса.
struct ProcessOutput {
    /// `None` — процесс не успел завершиться и был убит по таймауту.
    status: Option<std::process::ExitStatus>,
    stdout: String,
    stderr: String,
    truncated: bool,
}

impl ProcessOutput {
    fn success(&self) -> bool {
        self.status.is_some_and(|status| status.success())
    }

    fn code(&self) -> String {
        self.status
            .and_then(|status| status.code())
            .map_or_else(|| "signal".to_string(), |code| code.to_string())
    }
}

/// Запускает процесс с таймаутом, убивая его при превышении.
async fn run_process(mut command: Command, limit: Duration) -> Result<ProcessOutput, String> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Не удалось запустить {program}: {error}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Не удалось читать stdout процесса".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Не удалось читать stderr процесса".to_string())?;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_truncated = false;
    let mut stderr_truncated = false;

    let wait = async {
        let (out, err, status) = tokio::join!(
            read_capped(&mut stdout, &mut stdout_bytes, &mut stdout_truncated),
            read_capped(&mut stderr, &mut stderr_bytes, &mut stderr_truncated),
            child.wait(),
        );
        out.and(err)
            .map_err(|error| format!("Ошибка чтения вывода: {error}"))?;
        status.map_err(|error| format!("Ошибка ожидания процесса: {error}"))
    };

    let status = match timeout(limit, wait).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            None
        }
    };

    Ok(ProcessOutput {
        status,
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        truncated: stdout_truncated || stderr_truncated,
    })
}

/// Читает поток до конца, сохраняя не больше [`MAX_OUTPUT_BYTES`].
async fn read_capped<R: AsyncRead + Unpin>(
    reader: &mut R,
    output: &mut Vec<u8>,
    truncated: &mut bool,
) -> std::io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let room = MAX_OUTPUT_BYTES.saturating_sub(output.len());
        let take = room.min(read);
        output.extend_from_slice(&chunk[..take]);
        if take < read {
            *truncated = true;
        }
    }
    Ok(())
}

/// Обрезает текст до лимита и добавляет пометку.
fn truncate_output(text: String) -> (String, bool) {
    if text.len() <= MAX_OUTPUT_BYTES {
        return (text, false);
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut clipped = text[..end].to_string();
    clipped.push_str("\n… вывод обрезан: лимит 64 КБ\n");
    (clipped, true)
}

/// Текст ошибки сборки/запуска с выводом процесса.
fn process_error(prefix: &str, output: &ProcessOutput) -> String {
    let mut message = format!("{prefix} (код {}).", output.code());
    let output = combined_output(&output.stdout, &output.stderr);
    if !output.is_empty() {
        message.push_str("\n\n");
        message.push_str(&output);
    }
    truncate_output(message).0
}

/// Текст ошибки таймаута; сохраняет уже полученный вывод.
fn timeout_error(what: &str, limit: Duration, output: &ProcessOutput) -> String {
    let mut message = format!(
        "{what} не завершился за {} с — процесс убит.",
        limit.as_secs()
    );
    let output = combined_output(&output.stdout, &output.stderr);
    if !output.is_empty() {
        message.push_str("\n\n");
        message.push_str(&output);
    }
    truncate_output(message).0
}

fn combined_output(stdout: &str, stderr: &str) -> String {
    let stdout = stdout.trim_end();
    let stderr = stderr.trim_end();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_string(),
        (true, false) => stderr.to_string(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

/// Занимает единственный слот выполнения.
fn acquire_lock() -> Result<tokio::sync::MutexGuard<'static, ()>, String> {
    RUN_LOCK
        .try_lock()
        .map_err(|_| "Уже выполняется другой скрипт — дождитесь завершения.".to_string())
}

fn toolchain_error() -> Option<String> {
    let path = env::var_os("PATH");
    toolchain_error_for(&missing_tools(path.as_deref()))
}

fn missing_tools(path: Option<&OsStr>) -> Vec<&'static str> {
    ["cargo", "rustc"]
        .into_iter()
        .filter(|tool| !on_path(path, tool))
        .collect()
}

fn on_path(path: Option<&OsStr>, tool: &str) -> bool {
    let Some(path) = path else {
        return false;
    };
    let file = format!("{tool}{}", env::consts::EXE_SUFFIX);
    env::split_paths(path).any(|dir| dir.join(&file).is_file())
}

fn toolchain_error_for(missing: &[&str]) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "Не найден {}. Запуск Rust-скриптов невозможен: установите Rust toolchain \
         (rustup) и повторите.",
        missing.join(" и ")
    ))
}

fn parse_seconds(raw: Option<&str>, default: u64) -> Duration {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map_or_else(|| Duration::from_secs(default), Duration::from_secs)
}

fn target_dir_from(raw: Option<&str>, cwd: &Path) -> PathBuf {
    let raw = raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_TARGET_DIR);
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn exe_name(name: &str) -> String {
    format!("{name}{}", env::consts::EXE_SUFFIX)
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsStr,
        path::Path,
        path::PathBuf,
        time::{Duration, Instant},
    };

    use super::{
        MAX_OUTPUT_BYTES, acquire_lock, child_env, missing_tools, parse_seconds, read_capped, run,
        run_with_limits, target_dir_from, toolchain_error_for, truncate_output, wrapper,
    };

    #[test]
    fn wrapper_embeds_code_into_async_main() {
        let generated = wrapper("println!(\"ok\");");

        assert!(generated.contains("#[tokio::main]"), "{generated}");
        assert!(generated.contains("async fn main"), "{generated}");
        assert!(generated.contains("std::env::var(\"DATABASE_URL\")"));
        assert!(generated.contains("sqlx::PgPool::connect"));
        assert!(generated.contains("println!(\"ok\");"), "{generated}");
        assert!(generated.contains("Ok(())"), "{generated}");
    }

    #[test]
    fn wrapper_keeps_user_code_between_connect_and_ok() {
        let generated = wrapper("let answer = 42;");
        let main = generated.find("async fn main").expect("нет main");
        let connect = generated.find("PgPool::connect").expect("нет connect");
        let code = generated.find("let answer = 42;").expect("нет кода");
        let ok = generated.rfind("Ok(())").expect("нет Ok");

        assert!(main < connect && connect < code && code < ok);
    }

    #[test]
    fn truncation_is_marked_and_stays_utf8() {
        let text = "ё".repeat(MAX_OUTPUT_BYTES);
        let (clipped, truncated) = truncate_output(text);

        assert!(truncated);
        assert!(clipped.contains("обрезан"), "{clipped}");
        assert!(clipped.len() < MAX_OUTPUT_BYTES + 100, "{}", clipped.len());
        assert!(clipped.is_char_boundary(clipped.len()));
    }

    #[test]
    fn short_output_is_not_marked() {
        let (clipped, truncated) = truncate_output("ok".to_string());

        assert!(!truncated);
        assert_eq!(clipped, "ok");
    }

    #[test]
    fn child_env_contains_only_allowed_variables() {
        let vars = child_env(
            "postgres://user:pass@localhost/db",
            Path::new("/tmp/cms-rust-target"),
        );
        let allowed = [
            "DATABASE_URL",
            "PATH",
            "HOME",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "CARGO_TARGET_DIR",
        ];
        for (key, _) in &vars {
            assert!(
                allowed.contains(&key.as_str()),
                "в дочерний процесс попала лишняя переменная {key}"
            );
        }
        let value = |name: &str| {
            vars.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(
            value("DATABASE_URL"),
            Some("postgres://user:pass@localhost/db")
        );
        assert_eq!(value("CARGO_TARGET_DIR"), Some("/tmp/cms-rust-target"));
    }

    #[test]
    fn only_one_run_is_allowed_at_a_time() {
        let guard = acquire_lock().expect("первый запуск не получил mutex");
        let error = acquire_lock().expect_err("второй запуск прошёл");
        assert!(error.contains("выполняется"), "{error}");

        drop(guard);
        assert!(acquire_lock().is_ok(), "mutex не освободился");
    }

    #[test]
    fn missing_toolchain_is_explained() {
        let missing = missing_tools(Some(OsStr::new("/nonexistent-cms-rust")));
        assert_eq!(missing, vec!["cargo", "rustc"]);

        let error = toolchain_error_for(&missing).expect("нет ошибки");
        assert!(error.contains("cargo"), "{error}");
        assert!(error.contains("rustc"), "{error}");
        assert!(error.contains("установите"), "{error}");
        assert!(toolchain_error_for(&[]).is_none());
    }

    #[test]
    fn timeouts_use_defaults_and_positive_overrides() {
        assert_eq!(parse_seconds(None, 180), Duration::from_secs(180));
        assert_eq!(parse_seconds(Some("5"), 180), Duration::from_secs(5));
        assert_eq!(parse_seconds(Some("0"), 180), Duration::from_secs(180));
        assert_eq!(parse_seconds(Some("bad"), 30), Duration::from_secs(30));
    }

    #[test]
    fn target_dir_is_absolute() {
        let cwd = Path::new("/project");
        assert_eq!(
            target_dir_from(None, cwd),
            PathBuf::from("/project/target/rust-scripts")
        );
        assert_eq!(
            target_dir_from(Some("cache/rust"), cwd),
            PathBuf::from("/project/cache/rust")
        );
        assert_eq!(
            target_dir_from(Some("/var/cache/cms-rust"), cwd),
            PathBuf::from("/var/cache/cms-rust")
        );
    }

    #[tokio::test]
    async fn reading_stops_at_the_limit_and_marks_truncation() {
        let data = vec![b'x'; MAX_OUTPUT_BYTES + 5];
        let mut reader: &[u8] = &data;
        let mut output = Vec::new();
        let mut truncated = false;

        read_capped(&mut reader, &mut output, &mut truncated)
            .await
            .expect("чтение вывода");

        assert_eq!(output.len(), MAX_OUTPUT_BYTES);
        assert!(truncated);
    }

    #[tokio::test]
    #[ignore = "собирает временный Rust-крейт (долго), нужен toolchain и DATABASE_URL"]
    async fn runtime_timeout_kills_script() {
        let _serial = super::TEST_RUN_LOCK.lock().await;
        let started = Instant::now();
        let error = run_with_limits("loop {}", Duration::from_secs(180), Duration::from_secs(1))
            .await
            .expect_err("таймаут не сработал");
        println!("{error}");

        assert!(error.contains("не завершился за 1 с"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "скрипт не был убит по таймауту"
        );
    }

    #[tokio::test]
    #[ignore = "собирает временный Rust-крейт (долго), нужен toolchain и DATABASE_URL"]
    async fn real_script_prints_ok() {
        let _serial = super::TEST_RUN_LOCK.lock().await;
        let output = run("println!(\"ok\");").await.expect("запуск не удался");
        println!(
            "stdout={:?} stderr={:?} duration_ms={}",
            output.stdout, output.stderr, output.duration_ms
        );

        assert!(output.stdout.contains("ok"), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        assert!(output.duration_ms > 0, "{output:?}");
    }

    #[tokio::test]
    #[ignore = "собирает временный Rust-крейт (долго), нужен toolchain и DATABASE_URL"]
    async fn compile_error_is_reported() {
        let _serial = super::TEST_RUN_LOCK.lock().await;
        let error = run("this is not rust")
            .await
            .expect_err("ошибка не найдена");
        println!("{error}");

        assert!(error.contains("Сборка не удалась"), "{error}");
    }
}
