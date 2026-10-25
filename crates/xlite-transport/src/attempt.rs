//! One upstream attempt, fully described by the caller.

use bytes::Bytes;
use http::{HeaderMap, Method, Uri};

use crate::error::TransportError;

/// Exactly what one upstream attempt will send. Prepared once by the caller
/// and reusable across its retry loop ([`Attempt`] is `Clone`; cloning is a
/// cheap `Bytes` refcount bump).
#[derive(Clone, Debug, Default)]
pub struct Attempt {
    pub method: Method,
    /// Absolute URI. Supplies scheme + authority (the pool key) + path/query.
    /// The transport never rewrites it.
    pub uri: Uri,
    /// Full header set, duplicates preserved. Passed through verbatim except
    /// for the mechanical rewrites listed in
    /// [`AttemptTelemetry::headers_rewritten`](crate::AttemptTelemetry::headers_rewritten).
    pub headers: HeaderMap,
    /// Request body. Its length is the authority for `content-length`.
    pub body: Bytes,
}

impl Attempt {
    /// Validate everything that can be validated without touching a socket.
    ///
    /// Called by `execute()` before any dial/acquire/TLS, so an
    /// [`TransportError::InvalidRequest`] provably never reaches the upstream
    /// and is never redial-eligible.
    pub fn validate(&self) -> Result<(), TransportError> {
        match self.uri.scheme_str() {
            Some("http") | Some("https") => {}
            Some(other) => {
                return Err(TransportError::InvalidRequest(scheme_hint(other)));
            }
            None => return Err(TransportError::InvalidRequest("uri has no scheme")),
        }
        if self.uri.authority().is_none() {
            return Err(TransportError::InvalidRequest("uri has no authority"));
        }

        // `content-length` is caller-authoritative: allowed only if it agrees
        // with the body, and never together with `transfer-encoding`.
        let lengths: Vec<_> = self
            .headers
            .get_all(http::header::CONTENT_LENGTH)
            .iter()
            .collect();
        let te = self.headers.contains_key(http::header::TRANSFER_ENCODING);
        if !lengths.is_empty() && te {
            return Err(TransportError::InvalidRequest(
                "content-length and transfer-encoding must not both be set",
            ));
        }
        for value in &lengths {
            let parsed: usize = value
                .to_str()
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .ok_or(TransportError::InvalidRequest(
                    "content-length is not a valid usize",
                ))?;
            if parsed != self.body.len() {
                return Err(TransportError::InvalidRequest(
                    "content-length disagrees with body length",
                ));
            }
        }
        Ok(())
    }
}

/// `Box::leak` is unacceptable here; keep the message static by mapping the
/// few possible scheme strings.
fn scheme_hint(scheme: &str) -> &'static str {
    match scheme {
        "http2c" | "h2c" => "unsupported scheme (use `http` for cleartext h2)",
        _ => "unsupported scheme (only http/https)",
    }
}

/// Headers the transport may strip mechanically on HTTP/2 (RFC 9113 §8.2.2
/// forbids connection-specific fields there). Anything not in this list is
/// passed through byte-for-byte, including duplicates and non-UTF-8 values.
#[allow(dead_code)] // used by the feature-gated backends
pub(crate) const CONNECTION_SPECIFIC: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
];
