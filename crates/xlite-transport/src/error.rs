//! Error taxonomy: a stable, typed enum — never matched by display string.
//!
//! [`TransportError`] is the only failure surface of
//! [`Transport::execute`](crate::Transport::execute). Where an underlying
//! `io` / `rustls` / `h2` error exists it is reachable via
//! [`std::error::Error::source`] so callers can log it without string matching.

use std::time::Duration;

/// What the transport observed about a pooled connection that had just failed.
///
/// Only failures with **zero response bytes** land here; anything observed
/// after the first byte is a [`TransportError::Reset`] /
/// [`TransportError::BodyIncomplete`] and is never redial-eligible.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleKind {
    /// The peer closed a reused idle connection right as the request was written.
    PeerClosed,
    /// The peer reset the connection before any response byte.
    Reset,
    /// Any other transport-level failure before the first response byte.
    Io,
}

/// One attempt's failure. `#[non_exhaustive]`: new variants may be added, so
/// callers must carry a `_` arm for forward compatibility — but must never
/// match on `to_string()`.
#[non_exhaustive]
#[derive(Debug)]
pub enum TransportError {
    /// The `Attempt` itself is invalid (bad URI, contradictory headers).
    /// Produced before any socket exists; never retryable.
    InvalidRequest(&'static str),
    /// DNS resolution for the attempt's authority failed.
    Dns(std::io::Error),
    /// TCP connect failed (refused, unreachable, …).
    Connect(std::io::Error),
    /// TLS handshake failed (feature `tls-rustls`).
    Tls(std::io::Error),
    /// The caller-configured header budget elapsed before response headers.
    /// Only produced when [`TransportConfig::header_timeout`](crate::TransportConfig::header_timeout)
    /// was set.
    HeaderTimeout { waited: Duration },
    /// The caller-configured inter-chunk budget elapsed. Only produced when
    /// [`TransportConfig::stall_timeout`](crate::TransportConfig::stall_timeout) was set.
    Stall { idle: Duration },
    /// A **reused** connection failed before any response byte — the class
    /// [`execute_redial_once`](crate::Transport::execute_redial_once) may
    /// redial exactly once.
    StaleConnection { kind: StaleKind },
    /// The stream ended before the announced length (truncated body).
    BodyIncomplete,
    /// The peer reset the stream/connection after response bytes were seen.
    /// `code` is the RST_STREAM code when one was received.
    Reset { code: Option<u32> },
    /// The caller dropped the body before completion.
    Cancelled,
    /// A local resource bound was hit; the attempt never reached the peer.
    Overflow,
    /// The transport was shut down.
    Closed,
    /// Any other transport-level I/O failure.
    Io(std::io::Error),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(why) => write!(f, "invalid attempt: {why}"),
            Self::Dns(_) => write!(f, "dns resolution failed"),
            Self::Connect(_) => write!(f, "tcp connect failed"),
            Self::Tls(_) => write!(f, "tls handshake failed"),
            Self::HeaderTimeout { waited } => {
                write!(f, "no response headers within {waited:?}")
            }
            Self::Stall { idle } => write!(f, "no response chunk for {idle:?}"),
            Self::StaleConnection { kind } => {
                write!(f, "reused connection failed before first byte ({kind:?})")
            }
            Self::BodyIncomplete => write!(f, "response body ended before its announced length"),
            Self::Reset { code } => write!(f, "stream reset by peer (code {code:?})"),
            Self::Cancelled => write!(f, "cancelled by caller"),
            Self::Overflow => write!(f, "local resource bound hit"),
            Self::Closed => write!(f, "transport closed"),
            Self::Io(_) => write!(f, "transport i/o error"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidRequest(_)
            | Self::BodyIncomplete
            | Self::Cancelled
            | Self::Overflow
            | Self::Closed
            | Self::HeaderTimeout { .. }
            | Self::Stall { .. }
            | Self::StaleConnection { .. }
            | Self::Reset { .. } => None,
            Self::Dns(e) | Self::Connect(e) | Self::Tls(e) | Self::Io(e) => Some(e),
        }
    }
}

impl TransportError {
    /// Whether this failure is the redial-eligible stale-connection class.
    pub fn is_stale_connection(&self) -> bool {
        matches!(self, Self::StaleConnection { .. })
    }
}
