//! Снимок схемы всех инфоблоков (свойства, варианты списков, разделы) в памяти.
//! Перечитывается не чаще раза в [`TTL`]: правки в админке видны в API с этой задержкой.
//! Инфоблоков и разделов немного, а нужны они почти каждому запросу — фильтрам по
//! разделам, URL, привязкам между инфоблоками.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use sqlx::PgPool;
use tokio::sync::RwLock;

use crate::collection::{Collection, Field, FieldOption, Section, repo};

const TTL: Duration = Duration::from_secs(5);

pub struct Schema {
    pub collection: Collection,
    pub props: Vec<Field>,
    pub sections: HashMap<i64, Section>,
    /// Разделы, активные вместе со всеми предками (GLOBAL_ACTIVE в Битриксе).
    pub globally_active: HashSet<i64>,
}

impl Schema {
    pub fn prop(&self, snake_code: &str) -> Option<&Field> {
        self.props.iter().find(|p| p.code == snake_code)
    }

    /// Цепочка разделов от корня до `id` включительно.
    pub fn section_chain(&self, id: i64) -> Vec<&Section> {
        let mut chain = Vec::new();
        let mut cursor = self.sections.get(&id);
        while let Some(s) = cursor {
            chain.push(s);
            if chain.len() > 64 {
                break; // защита от цикла в данных
            }
            cursor = s.parent_id.and_then(|p| self.sections.get(&p));
        }
        chain.reverse();
        chain
    }

    /// Разделы-корни и все их потомки.
    pub fn subtree(&self, roots: &[i64]) -> HashSet<i64> {
        let mut ids: HashSet<i64> = roots
            .iter()
            .copied()
            .filter(|id| self.sections.contains_key(id))
            .collect();
        loop {
            let before = ids.len();
            for s in self.sections.values() {
                if s.parent_id.is_some_and(|p| ids.contains(&p)) {
                    ids.insert(s.id);
                }
            }
            if ids.len() == before {
                return ids;
            }
        }
    }
}

/// Формат цены валюты (CCurrencyLang): `# &#8381;`, разделители, знаки после запятой.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Currency {
    pub code: String,
    pub format_string: String,
    pub dec_point: String,
    pub thousands_sep: String,
    pub decimals: i32,
    pub hide_zero: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Store {
    pub id: i64,
    pub name: String,
    pub active: bool,
}

pub struct Snapshot {
    pub by_id: HashMap<i64, Arc<Schema>>,
    by_code: HashMap<String, i64>,
    pub enums: HashMap<i64, FieldOption>,
    pub currencies: HashMap<String, Currency>,
    /// Склады в порядке сортировки.
    pub stores: Vec<Store>,
    loaded_at: Instant,
}

impl Snapshot {
    pub fn by_code(&self, snake_code: &str) -> Option<&Arc<Schema>> {
        self.by_code
            .get(snake_code)
            .and_then(|id| self.by_id.get(id))
    }

    pub fn get(&self, collection_id: i64) -> Option<&Arc<Schema>> {
        self.by_id.get(&collection_id)
    }

    pub fn enum_value(&self, id: i64) -> Option<&FieldOption> {
        self.enums.get(&id)
    }

    /// Варианты свойства-списка в порядке сортировки.
    pub fn property_enums(&self, property_id: i64) -> Vec<&FieldOption> {
        let mut items: Vec<&FieldOption> = self
            .enums
            .values()
            .filter(|e| e.field_id == property_id)
            .collect();
        items.sort_by_key(|e| (e.sort, e.id));
        items
    }
}

#[derive(Default)]
pub struct Registry {
    current: RwLock<Option<Arc<Snapshot>>>,
}

impl Registry {
    pub async fn snapshot(&self, db: &PgPool) -> sqlx::Result<Arc<Snapshot>> {
        if let Some(s) = self.current.read().await.as_ref()
            && s.loaded_at.elapsed() < TTL
        {
            return Ok(s.clone());
        }
        let mut guard = self.current.write().await;
        // Пока ждали блокировку, снимок мог обновить другой запрос
        if let Some(s) = guard.as_ref()
            && s.loaded_at.elapsed() < TTL
        {
            return Ok(s.clone());
        }
        let snapshot = Arc::new(load(db).await?);
        *guard = Some(snapshot.clone());
        Ok(snapshot)
    }
}

async fn load(db: &PgPool) -> sqlx::Result<Snapshot> {
    let iblocks = repo::list_collections(db).await?;
    let mut props: HashMap<i64, Vec<Field>> = HashMap::new();
    for p in sqlx::query_as::<_, Field>(
        "SELECT id, collection_id, code, name, kind, is_required, sort, multiple, link_collection_id,
                user_type
         FROM collection_fields ORDER BY sort, id",
    )
    .fetch_all(db)
    .await?
    {
        props.entry(p.collection_id).or_default().push(p);
    }
    let mut sections: HashMap<i64, HashMap<i64, Section>> = HashMap::new();
    for s in sqlx::query_as::<_, Section>(
        "SELECT id, collection_id, parent_id, code, xml_id, name, active, sort, depth_level,
                description, picture_id, created_at, updated_at
         FROM collection_sections",
    )
    .fetch_all(db)
    .await?
    {
        sections.entry(s.collection_id).or_default().insert(s.id, s);
    }
    let enums: HashMap<i64, FieldOption> = sqlx::query_as::<_, FieldOption>(
        "SELECT id, field_id, value, xml_id, sort, is_default FROM collection_field_options",
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|e| (e.id, e))
    .collect();

    let mut by_id = HashMap::new();
    let mut by_code = HashMap::new();
    for summary in iblocks {
        let iblock = summary.collection;
        let sections = sections.remove(&iblock.id).unwrap_or_default();
        let globally_active = globally_active(&sections);
        by_code.insert(iblock.code.clone(), iblock.id);
        by_id.insert(
            iblock.id,
            Arc::new(Schema {
                props: props.remove(&iblock.id).unwrap_or_default(),
                sections,
                globally_active,
                collection: iblock,
            }),
        );
    }
    let currencies = sqlx::query_as::<_, Currency>(
        "SELECT code, format_string, dec_point, thousands_sep, decimals, hide_zero FROM currencies",
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|c| (c.code.clone(), c))
    .collect();
    let stores =
        sqlx::query_as::<_, Store>("SELECT id, name, active FROM catalog_stores ORDER BY sort, id")
            .fetch_all(db)
            .await?;
    Ok(Snapshot {
        by_id,
        by_code,
        enums,
        currencies,
        stores,
        loaded_at: Instant::now(),
    })
}

fn globally_active(sections: &HashMap<i64, Section>) -> HashSet<i64> {
    sections
        .values()
        .filter(|s| {
            let mut cursor = Some(*s);
            let mut steps = 0;
            while let Some(c) = cursor {
                if !c.active || steps > 64 {
                    return false;
                }
                steps += 1;
                cursor = c.parent_id.and_then(|p| sections.get(&p));
            }
            true
        })
        .map(|s| s.id)
        .collect()
}
