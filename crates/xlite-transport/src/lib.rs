//! `xlite-transport` — an in-process, server-side transport client for AI
//! upstreams (vLLM / SGLang / …), designed as the shared data plane between
//! the standalone xlite gateway and Nebula (issue #95 / #96).
//!
//! The contract, in four rules:
//!
//! 1. **[`Transport::execute`] is exactly one upstream attempt.** No retry,
//!    no failover, no reroute, no cooldown — the caller's loop is the only
//!    retry loop in the stack. [`Transport::execute_redial_once`] is the
//!    opt-in, visible, countable exception for stale keep-alive sockets.
//! 2. **The URI is per call.** The pool keys on the runtime authority; the
//!    caller picks the replica for every attempt.
//! 3. **Zero parsing.** Request body in, [`BodyStream`] out. No JSON DOM, no
//!    SSE framing, no usage extraction. Protocol translation is a different
//!    crate's job.
//! 4. **Config is passed in, environment is not read.** No catalog, no
//!    on-disk config, no database — `default-features = false` must not pull
//!    `sqlx` / `axum` / `redis` / `serde_yaml` (this crate does not depend on
//!    the `xlite` binary crate at all).
//!
//! Feature flags: `h1` (HTTP/1.1 keep-alive pool), `h2` (HTTP/2 pool via
//! `xlite-h2-pool`), `tls-rustls` (caller-supplied [`rustls::ClientConfig`]).

mod attempt;
mod body;
mod config;
mod error;
mod stats;
mod transport;

#[cfg(feature = "h1")]
pub mod h1;
#[cfg(feature = "h2")]
mod h2;

pub use attempt::Attempt;
pub use body::BodyStream;
pub use config::TransportConfig;
pub use error::{StaleKind, TransportError};
#[cfg(feature = "h1")]
pub use h1::H1Transport;
#[cfg(feature = "h2")]
pub use h2::H2Transport;
pub use stats::{Backend, HostStats, TransportStats};
pub use transport::{AttemptOutcome, AttemptTelemetry, Transport};

/// Enable `TCP_NODELAY` and OS-level TCP keepalive on an upstream socket so NAT
/// gateways, cloud load balancers, and firewalls do not silently drop long-context
/// prefill or reasoning streams during prolonged application-layer silence.
///
/// The probe budget is deliberately short (`15s + 3 × 5s = 30s` to declare a peer
/// dead): without an explicit `TCP_KEEPCNT` the kernel default of 9 probes made a
/// blackholed connection take `30s + 9 × 10s = 120s` to fail, always losing the
/// race against the header timeout (issue #93).
#[cfg(any(feature = "h1", feature = "h2"))]
pub fn configure_tcp_socket(stream: &tokio::net::TcpStream) {
    let _ = stream.set_nodelay(true);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(std::time::Duration::from_secs(15))
        .with_interval(std::time::Duration::from_secs(5))
        .with_retries(TCP_KEEPALIVE_RETRIES);
    let _ = socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive);
}

/// Failed probes before the kernel reports the peer dead (see [`configure_tcp_socket`]).
#[cfg(any(feature = "h1", feature = "h2"))]
const TCP_KEEPALIVE_RETRIES: u32 = 3;

/// Strip an optional port (and IPv6 brackets) from an authority, yielding the
/// host part a TLS `ServerName` needs.
#[cfg(any(feature = "h1", feature = "h2"))]
pub fn tls_host_from_authority(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[')
        && let Some((ipv6, _)) = rest.split_once(']')
    {
        return ipv6;
    }
    authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority)
}
/// Re-exported so callers can supply their own dial ([`H2Transport::with_dial`]).
#[cfg(feature = "h2")]
pub use xlite_h2_pool::{BoxIo, Dial};
