//! Transport construction: everything passed in, nothing read from env or disk.

#[cfg(feature = "tls-rustls")]
use std::sync::Arc;
use std::time::Duration;

/// How one [`crate::Transport`] instance behaves. No field is optional in the
/// `Duration::ZERO`/`None` senses documented per field; the crate never reads
/// environment variables or config files (rule: the caller owns its own env).
#[derive(Clone, Debug)]
pub struct TransportConfig {
    /// Bound on TCP connect + TLS handshake.
    pub connect_timeout: Duration,
    /// Optional budget for request→response-headers. `None` (the recommended
    /// setting for callers that own their own first-byte budget) means this
    /// transport never fires a header clock of its own — there is exactly one
    /// observer per attempt.
    pub header_timeout: Option<Duration>,
    /// Optional inter-chunk idle budget. `None` = the transport never fires a
    /// stall clock. (Protocol-level stall handling — SSE keep-alive comments,
    /// framing — is deliberately *not* this crate's job.)
    pub stall_timeout: Option<Duration>,
    /// Upper bound of concurrent upstream connections per authority on the h1
    /// backend. Should exceed the caller's per-replica concurrency.
    pub h1_max_conns_per_host: usize,
    /// Upper bound of concurrent upstream connections per authority on the h2
    /// backend (streams are multiplexed within them).
    pub h2_max_conns_per_host: usize,
    /// Client-side cap applied on top of the peer's
    /// `SETTINGS_MAX_CONCURRENT_STREAMS`.
    pub h2_max_streams_per_conn: usize,
    /// Close an h1 keep-alive connection (h2: an idle pooled connection) after
    /// this long without an in-flight stream. `None` = never reclaim.
    pub pool_idle_reclaim: Option<Duration>,
    /// Caller-supplied TLS configuration. `None` = this transport only serves
    /// cleartext `http://` authorities. The crate never builds a
    /// `ClientConfig` itself, so key logging, roots and ALPN stay the caller's
    /// decision.
    #[cfg(feature = "tls-rustls")]
    pub tls: Option<Arc<rustls::ClientConfig>>,
}

impl TransportConfig {
    /// A sane default for cleartext-only, in-process upstreams (vLLM /
    /// SGLang on a private network): no internal watchdogs, caller owns all
    /// deadlines.
    pub fn cleartext() -> Self {
        Self {
            connect_timeout: Duration::from_secs(3),
            header_timeout: None,
            stall_timeout: None,
            h1_max_conns_per_host: 128,
            h2_max_conns_per_host: 8,
            h2_max_streams_per_conn: 250,
            pool_idle_reclaim: Some(Duration::from_secs(90)),
            #[cfg(feature = "tls-rustls")]
            tls: None,
        }
    }
}
