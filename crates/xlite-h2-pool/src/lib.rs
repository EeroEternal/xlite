//! Native HTTP/2 upstream connection pool — the "option C" from the load-test
//! TTFT root-cause analysis.
//!
//! # Why this crate exists
//!
//! `reqwest` / `hyper-util`'s default HTTP client reuses a single HTTP/2
//! connection per host and, once the peer's `SETTINGS_MAX_CONCURRENT_STREAMS`
//! is exhausted, keeps assigning requests to that same connection so they
//! queue *inside* it before ever reaching the upstream. That queueing shows
//! up as `upstream_ttft` exploding while `queue_wait` stays 0.
//!
//! Go's `x/net/http2` does not do this: when a connection reaches the peer's
//! advertised stream cap it stops using it and dials a fresh connection (up
//! to `MaxConnsPerHost`), only queueing once every connection is full. This
//! crate reproduces that behaviour *exactly*, and additionally reads the
//! **real** `SETTINGS_MAX_CONCURRENT_STREAMS` from the handshake instead of
//! assuming a constant.
//!
//! # Behaviour
//!
//! 1. Each connection carries an atomic in-flight counter and the peer's real
//!    stream cap (read once after the handshake).
//! 2. `Pool::acquire` reserves a slot on the first connection with
//!    `in_flight < cap`; if every connection is full it dials a new one (up
//!    to [`Config::max_conns_per_host`]); at the cap it waits until a stream
//!    frees or a connection closes.
//! 3. Dropping the returned [`Lease`] frees the reserved slot and wakes any
//!    queued acquirer.
//!
//! # Scope
//!
//! The crate is transport-agnostic: it only needs an
//! `AsyncRead + AsyncWrite + Unpin + Send` stream whose peer speaks HTTP/2
//! (h2c prior knowledge or ALPN-negotiated h2). The caller supplies a
//! [`Dial`] closure, which is where TCP/TLS/proxy setup lives.
//!
//! It is the gateway's upstream strategy for `h2c` streaming; HTTP/1.1 and
//! `https://` requests use the gateway's native LIFO HTTP/1.1 pool (`H1Pool`).

mod error;
mod lease;
mod pool;
mod pump;

pub use error::{PoolError, PumpError};
pub use lease::Lease;
pub use pool::{
    BoxIo, CONNECTION_WINDOW_SIZE, Config, DEFAULT_IDLE_TIMEOUT, DEFAULT_STREAM_BUDGET_PER_CONN,
    Dial, DialFuture, Pool, PoolStats, STREAM_WINDOW_SIZE, Transport, WAIT_BUCKETS, client_builder,
};
