//! Подключение модуля проекта: если есть `project/mod.rs` с функцией
//! `pub fn project() -> Project`, он собирается вместе с CMS.
//! Каталог `project/` в репозиторий CMS не входит — это код конкретного сайта.

use std::{env, fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=project");
    // sqlx::migrate! встраивает миграции при сборке: новая миграция — пересборка
    println!("cargo:rerun-if-changed=migrations");
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let module = Path::new(&manifest).join("project").join("mod.rs");

    let mut out = String::from("// Сгенерировано build.rs из каталога project/\n");
    if module.is_file() {
        println!("cargo:rerun-if-changed={}", module.display());
        out.push_str(&format!(
            "#[path = {:?}]\nmod site;\npub fn project() -> Option<super::Project> {{\n    Some(site::project())\n}}\n",
            module.display().to_string()
        ));
    } else {
        out.push_str("pub fn project() -> Option<super::Project> {\n    None\n}\n");
    }

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    fs::write(Path::new(&out_dir).join("project.rs"), out).expect("запись project.rs");
}
