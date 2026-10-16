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
