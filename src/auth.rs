use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const COOKIE_NAME: &str = "xlite_session";
const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const KEY_TAG: &[u8] = b"xlite-session-key-v1";

type HmacSha256 = Hmac<Sha256>;

/// Stateless admin sessions: an HMAC-SHA256-signed `exp.nonce` payload. Any
/// gateway process holding the same key accepts a cookie issued by any other,
/// so Admin works behind a round-robin load balancer without sticky sessions.
#[derive(Clone)]
pub struct Sessions {
    key: [u8; 32],
}

impl Sessions {
    /// Key material is `ADMIN_SESSION_SECRET` when set, otherwise
    /// `ADMIN_PASSWORD`; either way every node must share the same value.
    pub fn new(admin_password: &str, session_secret: Option<&str>) -> Self {
        let material = session_secret
            .map(str::trim)
            .filter(|secret| !secret.is_empty())
            .unwrap_or(admin_password);
        let mut hasher = Sha256::new();
        hasher.update(KEY_TAG);
        hasher.update(material.as_bytes());
        let key: [u8; 32] = hasher.finalize().into();
        Self { key }
    }

    pub fn issue(&self) -> String {
        let exp = now_unix().saturating_add(SESSION_TTL.as_secs() as i64);
        let payload = format!("{exp}.{}", Uuid::new_v4());
        let signature = self.sign(&payload);
        format!("{payload}.{signature}")
    }

    pub fn valid(&self, token: &str) -> bool {
        let Some((payload, signature)) = token.rsplit_once('.') else {
            return false;
        };
        if !constant_time_eq(signature.as_bytes(), self.sign(payload).as_bytes()) {
            return false;
        }
        let Some((exp, _nonce)) = payload.split_once('.') else {
            return false;
        };
        matches!(exp.parse::<i64>(), Ok(exp) if exp > now_unix())
    }

    fn sign(&self, payload: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.key).expect("hmac accepts keys of any length");
        mac.update(payload.as_bytes());
        hex(&mac.finalize().into_bytes())
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
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
        let sessions = Sessions::new("hunter2", None);
        let token = sessions.issue();
        assert!(sessions.valid(&token));
        assert!(!sessions.valid("garbage"));
        assert!(!sessions.valid(""));
    }

    #[test]
    fn tampered_token_is_rejected() {
        let sessions = Sessions::new("hunter2", None);
        let token = sessions.issue();
        let mut bytes = token.into_bytes();
        let last = bytes.last_mut().expect("token is non-empty");
        *last = if *last == b'a' { b'b' } else { b'a' };
        let tampered = String::from_utf8(bytes).expect("utf8");
        assert!(!sessions.valid(&tampered));
    }

    #[test]
    fn cluster_nodes_share_sessions() {
        let node_a = Sessions::new("shared", None);
        let node_b = Sessions::new("shared", None);
        assert!(node_b.valid(&node_a.issue()));
    }

    #[test]
    fn different_secret_rejects_token() {
        let node_a = Sessions::new("shared", None);
        let other = Sessions::new("different", None);
        assert!(!other.valid(&node_a.issue()));
    }

    #[test]
    fn explicit_session_secret_overrides_password() {
        let from_secret_a = Sessions::new("pw-one", Some("cluster-secret"));
        let from_secret_b = Sessions::new("pw-two", Some("cluster-secret"));
        assert!(from_secret_b.valid(&from_secret_a.issue()));
        let from_password = Sessions::new("pw-one", None);
        assert!(!from_password.valid(&from_secret_a.issue()));
    }

    #[test]
    fn expired_token_is_rejected() {
        let sessions = Sessions::new("hunter2", None);
        let payload = "1.00000000-0000-0000-0000-000000000000";
        let token = format!("{payload}.{}", sessions.sign(payload));
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
