//! Проверка паролей: свои хеши (argon2, `password-auth`) и перенесённые из Битрикса —
//! `$6$…` (sha512-crypt) и старый формат `соль(8) + md5(соль + пароль)`, который
//! храним с префиксом [`BITRIX_MD5_PREFIX`]. Хеш Битрикса после успешного входа
//! заменяется на argon2.

use md5::{Digest, Md5};
use sha_crypt::{PasswordVerifier, ShaCrypt};

pub const BITRIX_MD5_PREFIX: &str = "bxmd5$";

pub struct Verified {
    pub ok: bool,
    /// Хеш устаревшего формата — перехешировать после входа.
    pub rehash: bool,
}

/// Хеш Битрикса (`b_user.PASSWORD`) → как храним у себя.
pub fn from_bitrix(hash: &str) -> String {
    if hash.starts_with('$') {
        hash.to_string()
    } else {
        format!("{BITRIX_MD5_PREFIX}{hash}")
    }
}

pub fn verify(password: &str, stored: &str) -> Verified {
    if let Some(legacy) = stored.strip_prefix(BITRIX_MD5_PREFIX) {
        return Verified {
            ok: verify_bitrix_md5(password, legacy),
            rehash: true,
        };
    }
    if stored.starts_with("$6$") || stored.starts_with("$5$") {
        let ok = ShaCrypt::default()
            .verify_password(password.as_bytes(), stored)
            .is_ok();
        return Verified { ok, rehash: true };
    }
    Verified {
        ok: password_auth::verify_password(password, stored).is_ok(),
        rehash: false,
    }
}

/// Старый Битрикс: первые символы — соль, последние 32 — md5(соль . пароль) в hex.
fn verify_bitrix_md5(password: &str, hash: &str) -> bool {
    if hash.len() <= 32 || !hash.is_char_boundary(hash.len() - 32) {
        return false;
    }
    let (salt, expected) = hash.split_at(hash.len() - 32);
    let actual = hex::encode(Md5::digest(format!("{salt}{password}").as_bytes()));
    // Сравнение без раннего выхода
    actual.len() == expected.len()
        && actual
            .bytes()
            .zip(expected.to_ascii_lowercase().bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrix_md5() {
        let salt = "AbCdEfGh";
        let hash = format!(
            "{salt}{}",
            hex::encode(Md5::digest(format!("{salt}secret").as_bytes()))
        );
        let stored = from_bitrix(&hash);
        assert!(stored.starts_with(BITRIX_MD5_PREFIX));
        assert!(verify("secret", &stored).ok);
        assert!(verify("secret", &stored).rehash);
        assert!(!verify("wrong", &stored).ok);
    }

    #[test]
    fn sha512_crypt() {
        // Эталон из спецификации SHA-crypt (Drepper)
        let stored = "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1";
        assert_eq!(from_bitrix(stored), stored);
        assert!(verify("Hello world!", stored).ok);
        assert!(!verify("Hello world", stored).ok);
    }

    #[test]
    fn own_hashes() {
        let stored = password_auth::generate_hash("pass1234");
        let v = verify("pass1234", &stored);
        assert!(v.ok && !v.rehash);
        assert!(!verify("nope", &stored).ok);
    }
}
