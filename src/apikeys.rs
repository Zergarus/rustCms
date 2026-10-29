//! API-ключи: выпуск, отзыв, проверка. Формат `cms_<prefix>_<секрет>`; хранится только
//! sha256 секрета.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};

use crate::error::{AppError, AppResult};

/// Доступные scope: код и название.
pub const SCOPES: &[(&str, &str)] = &[
    ("stock.read", "Чтение склада"),
    ("stock.write", "Черновики документов"),
    ("stock.conduct", "Проведение документов"),
];

pub struct NewKey {
    pub user_id: i64,
    pub name: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, FromRow)]
pub struct KeyRow {
    pub id: i64,
    pub name: String,
    pub prefix: String,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub last_used_ip: Option<String>,
    pub revoked_at: Option<DateTime<Utc>>,
}

pub struct KeyOwner {
    // читается API склада (следующие задачи подпроекта)
    #[allow(dead_code)]
    pub key_id: i64,
    pub user_id: i64,
    pub scopes: Vec<String>,
}

const PREFIX_LEN: usize = 8;
const SECRET_LEN: usize = 32;

/// Случайная строка из `alphabet` без перекоса (отбраковка значений вне кратного диапазона).
fn random_string(alphabet: &[u8], len: usize) -> String {
    let limit = 256 - (256 % alphabet.len());
    let mut out = String::with_capacity(len);
    while out.len() < len {
        for b in rand::random::<[u8; 64]>() {
            if (b as usize) < limit && out.len() < len {
                out.push(alphabet[b as usize % alphabet.len()] as char);
            }
        }
    }
    out
}

fn hash_secret(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// Сравнение байтовых срезов без раннего выхода.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= (a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

/// Выпускает ключ; возвращает id и полный ключ (показывается один раз).
pub async fn issue(db: &PgPool, key: &NewKey, by: i64) -> AppResult<(i64, String)> {
    let name = key.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Укажите название ключа".into()));
    }
    if key.scopes.is_empty() {
        return Err(AppError::BadRequest("Выберите хотя бы один scope".into()));
    }
    if let Some(bad) = key
        .scopes
        .iter()
        .find(|s| !SCOPES.iter().any(|(code, _)| code == s))
    {
        return Err(AppError::BadRequest(format!("Неизвестный scope {bad}")));
    }
    let secret = random_string(
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
        SECRET_LEN,
    );
    let hash = hash_secret(&secret);
    // Префикс уникален; при (крайне редкой) коллизии пробуем снова.
    for _ in 0..5 {
        let prefix = random_string(b"abcdefghijklmnopqrstuvwxyz0123456789", PREFIX_LEN);
        let id: Option<i64> = sqlx::query_scalar(
            "INSERT INTO api_keys (user_id, name, prefix, secret_hash, scopes, expires_at, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (prefix) DO NOTHING RETURNING id",
        )
        .bind(key.user_id)
        .bind(name)
        .bind(&prefix)
        .bind(&hash)
        .bind(&key.scopes)
        .bind(key.expires_at)
        .bind(by)
        .fetch_optional(db)
        .await?;
        if let Some(id) = id {
            return Ok((id, format!("cms_{prefix}_{secret}")));
        }
    }
    Err(AppError::BadRequest(
        "Не удалось выпустить ключ, повторите".into(),
    ))
}

/// Отзывает ключ пользователя; false — ключа нет, он чужой или уже отозван.
pub async fn revoke(db: &PgPool, user_id: i64, key_id: i64) -> AppResult<bool> {
    let n = sqlx::query(
        "UPDATE api_keys SET revoked_at = now()
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(key_id)
    .bind(user_id)
    .execute(db)
    .await?
    .rows_affected();
    Ok(n > 0)
}

pub async fn list(db: &PgPool, user_id: i64) -> sqlx::Result<Vec<KeyRow>> {
    sqlx::query_as(
        "SELECT id, name, prefix, scopes, created_at, expires_at, last_used_at, last_used_ip, revoked_at
         FROM api_keys WHERE user_id = $1 ORDER BY id DESC",
    )
    .bind(user_id)
    .fetch_all(db)
    .await
}

/// Разбирает `cms_<prefix>_<секрет>` на (префикс, секрет).
pub fn parse(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.strip_prefix("cms_")?;
    let (prefix, secret) = rest.split_once('_')?;
    let prefix_ok = prefix.len() == PREFIX_LEN
        && prefix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !prefix_ok || secret.is_empty() {
        return None;
    }
    Some((prefix, secret))
}

/// Проверяет ключ: не отозван, не просрочен, хэш совпадает, пользователь активен.
pub async fn authenticate(
    db: &PgPool,
    raw: &str,
    ip: Option<&str>,
) -> sqlx::Result<Option<KeyOwner>> {
    let Some((prefix, secret)) = parse(raw) else {
        return Ok(None);
    };
    let row: Option<(i64, i64, String, Vec<String>)> = sqlx::query_as(
        "SELECT k.id, k.user_id, k.secret_hash, k.scopes
         FROM api_keys k JOIN users u ON u.id = k.user_id
         WHERE k.prefix = $1 AND k.revoked_at IS NULL
           AND (k.expires_at IS NULL OR k.expires_at > now()) AND u.active",
    )
    .bind(prefix)
    .fetch_optional(db)
    .await?;
    let Some((key_id, user_id, hash, scopes)) = row else {
        return Ok(None);
    };
    if !ct_eq(hash.as_bytes(), hash_secret(secret).as_bytes()) {
        return Ok(None);
    }
    sqlx::query(
        "UPDATE api_keys SET last_used_at = now(), last_used_ip = $2
         WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')",
    )
    .bind(key_id)
    .bind(ip)
    .execute(db)
    .await?;
    Ok(Some(KeyOwner {
        key_id,
        user_id,
        scopes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::user_cookie;

    fn new_key(user_id: i64) -> NewKey {
        NewKey {
            user_id,
            name: "WMS".into(),
            scopes: vec!["stock.read".into()],
            expires_at: None,
        }
    }

    #[test]
    fn parse_cases() {
        let secret = "A".repeat(32);
        let raw = format!("cms_ab12cd34_{secret}");
        assert_eq!(parse(&raw), Some(("ab12cd34", secret.as_str())));
        assert_eq!(parse(&format!("xyz_ab12cd34_{secret}")), None);
        assert_eq!(parse(&format!("cms_ab12_{secret}")), None);
        assert_eq!(parse("cms_ab12cd34_"), None);
        assert_eq!(parse(""), None);
    }

    #[sqlx::test]
    async fn issue_stores_only_hash(db: PgPool) {
        let (uid, _) = user_cookie(&db, "k1", false).await;
        let (id, full) = issue(&db, &new_key(uid), uid).await.unwrap();
        assert_eq!(full.len(), 45);
        let (prefix, secret) = parse(&full).unwrap();
        let (p, hash): (String, String) =
            sqlx::query_as("SELECT prefix, secret_hash FROM api_keys WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(p, prefix);
        assert_eq!(hash, hex::encode(Sha256::digest(secret.as_bytes())));
        assert!(!hash.contains(secret));
        let rows = list(&db, uid).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prefix, prefix);
    }

    #[sqlx::test]
    async fn issue_validates(db: PgPool) {
        let (uid, _) = user_cookie(&db, "k2", false).await;
        let mut k = new_key(uid);
        k.name = "  ".into();
        assert!(matches!(
            issue(&db, &k, uid).await,
            Err(AppError::BadRequest(_))
        ));
        let mut k = new_key(uid);
        k.scopes = vec![];
        assert!(matches!(
            issue(&db, &k, uid).await,
            Err(AppError::BadRequest(_))
        ));
        let mut k = new_key(uid);
        k.scopes = vec!["stock.nope".into()];
        assert!(matches!(
            issue(&db, &k, uid).await,
            Err(AppError::BadRequest(_))
        ));
    }

    #[sqlx::test]
    async fn authenticate_ok_and_wrong_secret(db: PgPool) {
        let (uid, _) = user_cookie(&db, "k3", false).await;
        let (id, full) = issue(&db, &new_key(uid), uid).await.unwrap();
        let o = authenticate(&db, &full, Some("1.2.3.4"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((o.key_id, o.user_id), (id, uid));
        assert_eq!(o.scopes, vec!["stock.read".to_string()]);
        let bad = format!("{}{}", &full[..13], "x".repeat(32));
        assert!(authenticate(&db, &bad, None).await.unwrap().is_none());
        assert!(authenticate(&db, "garbage", None).await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn authenticate_revoked_expired_inactive(db: PgPool) {
        let (uid, _) = user_cookie(&db, "k4", false).await;
        let (id, full) = issue(&db, &new_key(uid), uid).await.unwrap();
        assert!(authenticate(&db, &full, None).await.unwrap().is_some());
        assert!(revoke(&db, uid, id).await.unwrap());
        assert!(authenticate(&db, &full, None).await.unwrap().is_none());

        let (id2, full2) = issue(&db, &new_key(uid), uid).await.unwrap();
        sqlx::query("UPDATE api_keys SET expires_at = now() - interval '1 hour' WHERE id = $1")
            .bind(id2)
            .execute(&db)
            .await
            .unwrap();
        assert!(authenticate(&db, &full2, None).await.unwrap().is_none());

        let (_, full3) = issue(&db, &new_key(uid), uid).await.unwrap();
        assert!(authenticate(&db, &full3, None).await.unwrap().is_some());
        sqlx::query("UPDATE users SET active = false WHERE id = $1")
            .bind(uid)
            .execute(&db)
            .await
            .unwrap();
        assert!(authenticate(&db, &full3, None).await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn last_used_throttled(db: PgPool) {
        let (uid, _) = user_cookie(&db, "k5", false).await;
        let (id, full) = issue(&db, &new_key(uid), uid).await.unwrap();
        let get = |db: PgPool| async move {
            sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
                "SELECT last_used_at FROM api_keys WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap()
        };
        authenticate(&db, &full, Some("1.1.1.1")).await.unwrap();
        let first = get(db.clone()).await.unwrap();
        authenticate(&db, &full, Some("2.2.2.2")).await.unwrap();
        assert_eq!(get(db.clone()).await.unwrap(), first);
        sqlx::query(
            "UPDATE api_keys SET last_used_at = last_used_at - interval '2 minutes' WHERE id = $1",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        authenticate(&db, &full, Some("2.2.2.2")).await.unwrap();
        assert!(get(db.clone()).await.unwrap() >= first);
        let ip: Option<String> =
            sqlx::query_scalar("SELECT last_used_ip FROM api_keys WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(ip.as_deref(), Some("2.2.2.2"));
    }

    #[sqlx::test]
    async fn xml_id_unique_nonempty(db: PgPool) {
        let ins = |x: &'static str| {
            let db = db.clone();
            async move {
                sqlx::query("INSERT INTO store_documents (doc_type, xml_id) VALUES ('A', $1)")
                    .bind(x)
                    .execute(&db)
                    .await
            }
        };
        ins("").await.unwrap();
        ins("").await.unwrap();
        ins("X").await.unwrap();
        assert!(ins("X").await.is_err());
    }
}
