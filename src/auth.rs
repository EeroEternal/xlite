use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use uuid::Uuid;

const COOKIE_NAME: &str = "xlite_session";
const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone, Default)]
pub struct Sessions {
    inner: Arc<RwLock<HashMap<String, Instant>>>,
}

impl Sessions {
    pub fn issue(&self) -> String {
        let token = Uuid::new_v4().to_string();
        self.inner
            .write()
            .expect("session lock")
            .insert(token.clone(), Instant::now());
        token
    }

    pub fn revoke(&self, token: &str) {
        self.inner.write().expect("session lock").remove(token);
    }

    pub fn valid(&self, token: &str) -> bool {
        let mut map = self.inner.write().expect("session lock");
        match map.get(token).copied() {
            Some(issued) if issued.elapsed() < SESSION_TTL => true,
            Some(_) => {
                map.remove(token);
                false
            }
            None => false,
        }
    }
}

pub fn cookie_value(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix(&format!("{COOKIE_NAME}="))
            .map(|v| v.to_string())
    })
}

pub fn set_cookie_header(token: &str) -> String {
    format!("{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800")
}

pub fn clear_cookie_header() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn session_roundtrip() {
        let sessions = Sessions::default();
        let token = sessions.issue();
        assert!(sessions.valid(&token));
        sessions.revoke(&token);
        assert!(!sessions.valid(&token));
    }

    #[test]
    fn cookie_parse_finds_named_value() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("other=1; xlite_session=abc123; theme=dark"),
        );
        assert_eq!(cookie_value(&headers).as_deref(), Some("abc123"));
    }
}
