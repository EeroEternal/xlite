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

pub use attempt::Attempt;
pub use body::BodyStream;
pub use config::TransportConfig;
pub use error::{StaleKind, TransportError};
pub use stats::{Backend, HostStats, TransportStats};
pub use transport::{AttemptOutcome, AttemptTelemetry, Transport};
