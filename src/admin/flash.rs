//! Уведомления «после действия»: короткоживущая cookie `admin_flash` для редиректов и
//! заголовок `HX-Trigger` для ответов на HTMX-запросы.

use axum::{
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use axum_extra::extract::CookieJar;
use serde::Serialize;

pub(crate) const COOKIE: &str = "admin_flash";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FlashKind {
    Ok,
    Error,
}

impl FlashKind {
    fn as_str(self) -> &'static str {
        match self {
            FlashKind::Ok => "ok",
            FlashKind::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Flash {
    pub text: String,
    pub kind: FlashKind,
}

fn hex_encode(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Option<String> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    let bytes = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(bytes).ok()
}

/// 303 на `url` с уведомлением, которое покажет следующая полная страница.
pub(crate) fn redirect(url: &str, text: &str, kind: FlashKind) -> Response {
    let value = hex_encode(&format!("{}:{text}", kind.as_str()));
    let cookie = format!("{COOKIE}={value}; Path=/admin; HttpOnly; SameSite=Lax");
    let mut res = (StatusCode::SEE_OTHER, [(header::LOCATION, url)]).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    res
}

/// Испорченное значение даёт `None`.
pub(crate) fn read(jar: &CookieJar) -> Option<Flash> {
    let raw = hex_decode(jar.get(COOKIE)?.value())?;
    let (kind, text) = raw.split_once(':')?;
    let kind = match kind {
        "ok" => FlashKind::Ok,
        "error" => FlashKind::Error,
        _ => return None,
    };
    Some(Flash {
        text: text.to_string(),
        kind,
    })
}

// Пока не используется: подключается страницами с HTMX-ответами.
#[allow(dead_code)]
/// `HX-Trigger: {"toast":{"text":…,"kind":"ok|error"}}`; не-ASCII экранируется, чтобы
/// заголовок остался корректным.
pub(crate) fn toast(text: &str, kind: FlashKind) -> [(HeaderName, HeaderValue); 1] {
    let json = serde_json::json!({"toast": {"text": text, "kind": kind.as_str()}}).to_string();
    let mut ascii = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() {
            ascii.push(c);
        } else {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                ascii.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    [(
        HeaderName::from_static("hx-trigger"),
        HeaderValue::from_str(&ascii).unwrap_or_else(|_| HeaderValue::from_static("")),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jar_from(set_cookie: &str) -> CookieJar {
        let pair = set_cookie.split(';').next().unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(pair).unwrap());
        CookieJar::from_headers(&headers)
    }

    #[test]
    fn flash_roundtrip_cyrillic() {
        let res = redirect("/admin/x", "Заказ сохранён", FlashKind::Ok);
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()[header::LOCATION], "/admin/x");
        let cookie = res.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.contains("Path=/admin") && cookie.contains("HttpOnly"));
        let flash = read(&jar_from(cookie)).unwrap();
        assert_eq!(
            flash,
            Flash {
                text: "Заказ сохранён".into(),
                kind: FlashKind::Ok
            }
        );
    }

    #[test]
    fn flash_malformed_ignored() {
        assert!(read(&jar_from("admin_flash=zz")).is_none());
        assert!(read(&jar_from("admin_flash=ff")).is_none());
    }

    #[test]
    fn toast_header_is_ascii_json() {
        let [(name, value)] = toast("Готово", FlashKind::Error);
        assert_eq!(name, "hx-trigger");
        let v: serde_json::Value = serde_json::from_str(value.to_str().unwrap()).unwrap();
        assert_eq!(v["toast"]["text"], "Готово");
        assert_eq!(v["toast"]["kind"], "error");
    }
}
