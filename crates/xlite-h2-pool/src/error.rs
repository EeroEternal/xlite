use std::io;

/// Errors surfaced by [`crate::Pool::acquire`].
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// Opening the underlying transport failed (TCP connect, TLS, …).
    #[error("failed to open transport to upstream: {0}")]
    Dial(#[from] io::Error),

    /// The HTTP/2 handshake failed.
    #[error("HTTP/2 handshake failed: {0}")]
    Handshake(#[from] h2::Error),

    /// The pool has been dropped.
    #[error("connection pool is closed")]
    Closed,
}

/// A stream-level failure delivered on the body channel
/// ([`crate::Lease::register`]). Typed so callers never match on display
/// strings; `#[non_exhaustive]` so new observations can be added.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PumpError {
    /// The peer reset this stream (`RST_STREAM`); `code` when one was sent.
    #[error("stream reset by peer (code {code:?})")]
    Reset { code: Option<u32> },
    /// The connection task stopped before the stream ended.
    #[error("native h2 connection task stopped")]
    Closed,
    /// The lease is not in pinned-poll mode; the caller must poll the
    /// `RecvStream` itself.
    #[error("lease is not in pinned-poll mode")]
    NotPinned,
    /// The connection itself failed (socket or protocol error).
    #[error("connection failed: {0}")]
    Io(#[from] io::Error),
}

impl PumpError {
    /// Map the `h2` error a stream poll produced onto the typed taxonomy.
    pub(crate) fn from_h2(err: h2::Error) -> Self {
        if err.is_reset() {
            Self::Reset {
                code: err.reason().map(u32::from),
            }
        } else if err.is_go_away() {
            // The connection is being torn down; the stream died with it.
            Self::Closed
        } else {
            // Socket failures carry the original `io::Error`; protocol /
            // library errors mean "this connection is no longer usable".
            match err.into_io() {
                Some(io) => Self::Io(io),
                None => Self::Closed,
            }
        }
    }
}
