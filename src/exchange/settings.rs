//! Настройки обмена с 1С: таблица `options`, модуль `exchange`.

use std::collections::HashMap;

use sqlx::PgPool;

pub const MODULE: &str = "exchange";

/// Что делать с отсутствующим в выгрузке: значения как в Битриксе.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Deactivate,
    Delete,
}

impl Action {
    // Используется настройками в админке (следующая задача плана обмена).
    #[allow(dead_code)]
    pub fn code(self) -> &'static str {
        match self {
            Action::None => "N",
            Action::Deactivate => "A",
            Action::Delete => "D",
        }
    }

    /// Неизвестный код означает «ничего не делать».
    pub fn from_code(code: &str) -> Action {
        match code {
            "A" => Action::Deactivate,
            "D" => Action::Delete,
            _ => Action::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Коллекция товаров, куда попадают записи, если в выгрузке нет своей.
    pub default_collection_id: Option<i64>,
    pub section_action: Action,
    pub element_action: Action,
    pub translit_on_add: bool,
    pub translit_on_update: bool,
    pub use_offers: bool,
    pub deactivate_without_price: bool,
    pub interval_secs: u32,
    pub file_limit: u64,
    pub use_zip: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            default_collection_id: None,
            section_action: Action::None,
            element_action: Action::None,
            translit_on_add: true,
            translit_on_update: false,
            use_offers: false,
            deactivate_without_price: false,
            interval_secs: 30,
            file_limit: 209_715_200,
            use_zip: true,
        }
    }
}

// Используется настройками в админке (следующая задача плана обмена).
#[allow(dead_code)]
fn flag(v: bool) -> &'static str {
    if v { "Y" } else { "N" }
}

impl Settings {
    pub async fn load(db: &PgPool) -> sqlx::Result<Settings> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT name, value FROM options WHERE module = $1")
                .bind(MODULE)
                .fetch_all(db)
                .await?;
        let map: HashMap<String, String> = rows.into_iter().collect();
        let d = Settings::default();
        let yn = |name: &str, default: bool| match map.get(name).map(String::as_str) {
            Some("Y") => true,
            Some("N") => false,
            _ => default,
        };
        let action = |name: &str| Action::from_code(map.get(name).map_or("N", String::as_str));
        Ok(Settings {
            default_collection_id: map
                .get("default_collection")
                .and_then(|v| v.parse().ok())
                .filter(|id| *id > 0),
            section_action: action("section_action"),
            element_action: action("element_action"),
            translit_on_add: yn("translit_on_add", d.translit_on_add),
            translit_on_update: yn("translit_on_update", d.translit_on_update),
            use_offers: yn("use_offers", d.use_offers),
            deactivate_without_price: yn("deactivate_without_price", d.deactivate_without_price),
            interval_secs: map
                .get("interval")
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.interval_secs),
            file_limit: map
                .get("file_limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.file_limit),
            use_zip: yn("use_zip", d.use_zip),
        })
    }

    // Используется настройками в админке (следующая задача плана обмена).
    #[allow(dead_code)]
    pub async fn save(&self, db: &PgPool) -> sqlx::Result<()> {
        let values: [(&str, String); 10] = [
            (
                "default_collection",
                self.default_collection_id
                    .map(|id| id.to_string())
                    .unwrap_or_default(),
            ),
            ("section_action", self.section_action.code().into()),
            ("element_action", self.element_action.code().into()),
            ("translit_on_add", flag(self.translit_on_add).into()),
            ("translit_on_update", flag(self.translit_on_update).into()),
            ("use_offers", flag(self.use_offers).into()),
            (
                "deactivate_without_price",
                flag(self.deactivate_without_price).into(),
            ),
            ("interval", self.interval_secs.to_string()),
            ("file_limit", self.file_limit.to_string()),
            ("use_zip", flag(self.use_zip).into()),
        ];
        let mut tx = db.begin().await?;
        for (name, value) in values {
            sqlx::query(
                "INSERT INTO options (module, name, value) VALUES ($1, $2, $3)
                 ON CONFLICT (module, name) DO UPDATE SET value = EXCLUDED.value",
            )
            .bind(MODULE)
            .bind(name)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    #[test]
    fn action_codes() {
        assert_eq!(Action::from_code("N"), Action::None);
        assert_eq!(Action::from_code("A"), Action::Deactivate);
        assert_eq!(Action::from_code("D"), Action::Delete);
        assert_eq!(Action::from_code("?"), Action::None);
        assert_eq!(Action::from_code(""), Action::None);
        assert_eq!(Action::None.code(), "N");
        assert_eq!(Action::Deactivate.code(), "A");
        assert_eq!(Action::Delete.code(), "D");
    }

    #[sqlx::test]
    async fn settings_defaults_and_roundtrip(db: PgPool) {
        let s = Settings::load(&db).await.unwrap();
        assert_eq!(s.default_collection_id, None);
        assert_eq!(s.section_action, Action::None);
        assert_eq!(s.element_action, Action::None);
        assert!(s.translit_on_add);
        assert!(!s.translit_on_update);
        assert!(!s.use_offers);
        assert!(!s.deactivate_without_price);
        assert_eq!(s.interval_secs, 30);
        assert_eq!(s.file_limit, 209_715_200);
        assert!(s.use_zip);

        let changed = Settings {
            default_collection_id: Some(42),
            section_action: Action::Deactivate,
            element_action: Action::Delete,
            translit_on_add: false,
            translit_on_update: true,
            use_offers: true,
            deactivate_without_price: true,
            interval_secs: 90,
            file_limit: 1_048_576,
            use_zip: false,
        };
        changed.save(&db).await.unwrap();
        assert_eq!(Settings::load(&db).await.unwrap(), changed);

        // Повторное сохранение перезаписывает, а сброс коллекции очищает значение
        let cleared = Settings {
            default_collection_id: None,
            ..changed
        };
        cleared.save(&db).await.unwrap();
        assert_eq!(Settings::load(&db).await.unwrap(), cleared);
    }
}
