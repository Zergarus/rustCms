//! Подключение модулей проектов: каждый `projects/<имя>.rs` с функцией
//! `pub fn project() -> Project` становится доступен по `BXAPI_PROJECT=<имя>`.
//! Каталог `projects/` в репозиторий CMS не входит — это код конкретных сайтов.

use std::{env, fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=projects");
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dir = Path::new(&manifest).join("projects");

    let mut modules: Vec<(String, String)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let valid = name.starts_with(|c: char| c.is_ascii_lowercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if !valid {
                println!(
                    "cargo:warning=projects/{name}.rs пропущен: имя должно быть [a-z][a-z0-9_]*"
                );
                continue;
            }
            println!("cargo:rerun-if-changed={}", path.display());
            modules.push((name.to_string(), path.display().to_string()));
        }
    }
    modules.sort();

    let mut out = String::from("// Сгенерировано build.rs из каталога projects/\n");
    for (name, path) in &modules {
        out.push_str(&format!("#[path = {path:?}]\npub mod {name};\n"));
    }
    let names: Vec<String> = modules.iter().map(|(n, _)| format!("{n:?}")).collect();
    out.push_str(&format!(
        "pub const NAMES: &[&str] = &[{}];\n",
        names.join(", ")
    ));
    out.push_str("pub fn by_name(name: &str) -> Option<super::Project> {\n    match name {\n");
    for (name, _) in &modules {
        out.push_str(&format!("        {name:?} => Some({name}::project()),\n"));
    }
    out.push_str("        _ => None,\n    }\n}\n");

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    fs::write(Path::new(&out_dir).join("projects.rs"), out).expect("запись projects.rs");
}
