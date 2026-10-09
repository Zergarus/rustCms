//! Права доступа: группы → права на разделы + уровни доступа к инфоблокам.
//!
//! Суперадминистратор (`users.is_admin`) проходит любые проверки. Остальные
//! получают объединение прав всех своих групп.

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;
use sqlx::PgPool;

use crate::{
    auth::User,
    error::{AppError, AppResult},
};

pub const ADMIN_ACCESS: &str = "admin.access";
pub const IBLOCKS_MANAGE: &str = "collections.manage";
pub const USERS_MANAGE: &str = "users.manage";
pub const SHOP_MANAGE: &str = "shop.manage";
pub const ORDERS_MANAGE: &str = "orders.manage";

#[derive(Debug, Clone, Copy, Serialize)]
pub struct PermissionDef {
    pub code: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

pub const PERMISSIONS: &[PermissionDef] = &[
    PermissionDef {
        code: ADMIN_ACCESS,
        name: "Вход в админку",
        description: "Без этого права остальные не действуют",
    },
    PermissionDef {
        code: IBLOCKS_MANAGE,
        name: "Управление инфоблоками",
        description: "Создание и настройка инфоблоков и свойств, изменение элементов во всех инфоблоках",
    },
    PermissionDef {
        code: USERS_MANAGE,
        name: "Управление пользователями",
        description: "Создание, редактирование и блокировка пользователей (кроме суперадминистраторов)",
    },
    PermissionDef {
        code: SHOP_MANAGE,
        name: "Управление магазином",
        description: "Склады, корзины покупателей, типы цен, валюты, настройки каталога и оформления заказа",
    },
    PermissionDef {
        code: ORDERS_MANAGE,
        name: "Работа с заказами",
        description: "Просмотр заказов, смена статуса, оплата, отмена, правка свойств и состава",
    },
];

pub fn is_known_permission(code: &str) -> bool {
    PERMISSIONS.iter().any(|p| p.code == code)
}

/// Уровень доступа к элементам инфоблока.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    None,
    Read,
    Write,
}

impl Level {
    pub fn from_db(s: &str) -> Level {
        match s {
            "write" => Level::Write,
            "read" => Level::Read,
            _ => Level::None,
        }
    }

    pub fn as_db(self) -> Option<&'static str> {
        match self {
            Level::None => None,
            Level::Read => Some("read"),
            Level::Write => Some("write"),
        }
    }
}

/// Эффективные права текущего пользователя. В шаблонах доступен как `user`
/// (поля пользователя + `permissions`, `can_see_iblocks`).
#[derive(Debug, Clone, Serialize)]
pub struct Access {
    #[serde(flatten)]
    pub user: User,
    /// Для суперадминистратора — все права.
    pub permissions: BTreeSet<String>,
    /// Показывать ли раздел «Инфоблоки» в меню.
    pub can_see_iblocks: bool,
    #[serde(skip)]
    iblock_levels: HashMap<i64, Level>,
}

impl Access {
    pub async fn load(db: &PgPool, user: User) -> sqlx::Result<Access> {
        if user.is_admin {
            return Ok(Access {
                user,
                permissions: PERMISSIONS.iter().map(|p| p.code.to_string()).collect(),
                can_see_iblocks: true,
                iblock_levels: HashMap::new(),
            });
        }
        let permissions: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT gp.permission
             FROM group_permissions gp JOIN user_groups ug ON ug.group_id = gp.group_id
             WHERE ug.user_id = $1",
        )
        .bind(user.id)
        .fetch_all(db)
        .await?;
        let levels: Vec<(i64, String)> = sqlx::query_as(
            "SELECT a.collection_id, a.level
             FROM collection_access a JOIN user_groups ug ON ug.group_id = a.group_id
             WHERE ug.user_id = $1",
        )
        .bind(user.id)
        .fetch_all(db)
        .await?;

        let permissions: BTreeSet<String> = permissions.into_iter().map(|(p,)| p).collect();
        let mut iblock_levels = HashMap::new();
        for (collection_id, level) in levels {
            // из нескольких групп берётся максимальный уровень
            let level = Level::from_db(&level);
            let entry = iblock_levels.entry(collection_id).or_insert(Level::None);
            *entry = (*entry).max(level);
        }
        let can_see_iblocks = permissions.contains(IBLOCKS_MANAGE)
            || iblock_levels.values().any(|l| *l >= Level::Read);
        Ok(Access {
            user,
            permissions,
            can_see_iblocks,
            iblock_levels,
        })
    }

    pub fn is_super(&self) -> bool {
        self.user.is_admin
    }

    pub fn can(&self, permission: &str) -> bool {
        self.is_super() || self.permissions.contains(permission)
    }

    /// Может ли пользователь вообще войти в админку.
    pub fn can_enter_admin(&self) -> bool {
        self.user.active && self.can(ADMIN_ACCESS)
    }

    pub fn iblock_level(&self, collection_id: i64) -> Level {
        if self.can(IBLOCKS_MANAGE) {
            Level::Write
        } else {
            self.iblock_levels
                .get(&collection_id)
                .copied()
                .unwrap_or(Level::None)
        }
    }

    pub fn require(&self, permission: &str) -> AppResult<()> {
        if self.can(permission) {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }

    pub fn require_super(&self) -> AppResult<()> {
        if self.is_super() {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }

    pub fn require_iblock(&self, collection_id: i64, level: Level) -> AppResult<()> {
        if self.iblock_level(collection_id) >= level {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(is_admin: bool, perms: &[&str], levels: &[(i64, Level)]) -> Access {
        Access {
            user: User {
                id: 1,
                login: "u".into(),
                email: None,
                is_admin,
                active: true,
            },
            permissions: perms.iter().map(|s| s.to_string()).collect(),
            can_see_iblocks: false,
            iblock_levels: levels.iter().copied().collect(),
        }
    }

    #[test]
    fn super_admin_can_everything() {
        let a = access(true, &[], &[]);
        assert!(a.can(USERS_MANAGE));
        assert_eq!(a.iblock_level(42), Level::Write);
    }

    #[test]
    fn group_rights() {
        let a = access(
            false,
            &[ADMIN_ACCESS],
            &[(1, Level::Read), (2, Level::Write)],
        );
        assert!(a.can_enter_admin());
        assert!(!a.can(USERS_MANAGE));
        assert!(a.require_iblock(1, Level::Read).is_ok());
        assert!(a.require_iblock(1, Level::Write).is_err());
        assert!(a.require_iblock(2, Level::Write).is_ok());
        assert!(a.require_iblock(3, Level::Read).is_err());
    }

    #[test]
    fn iblocks_manage_grants_write_everywhere() {
        let a = access(false, &[ADMIN_ACCESS, IBLOCKS_MANAGE], &[]);
        assert_eq!(a.iblock_level(99), Level::Write);
    }

    #[test]
    fn no_admin_access_no_entry() {
        let a = access(false, &[USERS_MANAGE], &[]);
        assert!(!a.can_enter_admin());
    }
}
