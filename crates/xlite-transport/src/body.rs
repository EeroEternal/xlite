//! Response body: raw bytes out, never parsed, with an optional caller-owned
//! inter-chunk watchdog.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;

use crate::error::TransportError;

type Inner = Pin<Box<dyn Stream<Item = Result<Bytes, TransportError>> + Send + 'static>>;

/// The raw response body. `Item = Result<Bytes, TransportError>`.
///
/// **Dropping it cancels the upstream stream** — h2 sends
/// `RST_STREAM(CANCEL)`, h1 closes the socket — so the engine observes the
/// disconnect. An abnormal EOF surfaces as
/// [`TransportError::BodyIncomplete`](crate::TransportError), never as a
/// clean end: a truncated SSE body must be indistinguishable from a failure,
/// not from `[DONE]`.
///
/// The inter-chunk watchdog fires **only** when the transport was built with
/// [`TransportConfig::stall_timeout`](crate::TransportConfig::stall_timeout);
/// with `None` this stream never runs a clock of its own (exactly one
/// observer per attempt — the caller's).
pub struct BodyStream {
    inner: Inner,
    /// Budget for one chunk gap; `None` = no watchdog.
    budget: Option<Duration>,
    /// Armed watchdog; created lazily so a `None` budget costs nothing.
    watchdog: Option<Pin<Box<tokio::time::Sleep>>>,
    terminated: bool,
}

impl BodyStream {
    /// Wrap any raw byte stream. Public so alternative `Transport`
    /// implementations (including tests and callers' own backends) can build
    /// an [`AttemptOutcome`](crate::AttemptOutcome).
    pub fn new(
        inner: impl Stream<Item = Result<Bytes, TransportError>> + Send + 'static,
        stall: Option<Duration>,
    ) -> Self {
        Self {
            inner: Box::pin(inner),
            budget: stall,
            watchdog: None,
            terminated: false,
        }
    }
}

impl std::fmt::Debug for BodyStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BodyStream").finish_non_exhaustive()
    }
}

impl Stream for BodyStream {
    type Item = Result<Bytes, TransportError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }
        if let Some(budget) = self.budget {
            let arm = self
                .watchdog
                .get_or_insert_with(|| Box::pin(tokio::time::sleep(budget)));
            if arm.as_mut().poll(cx).is_ready() {
                self.terminated = true;
                return Poll::Ready(Some(Err(TransportError::Stall { idle: budget })));
            }
        }
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(budget) = self.budget {
                    let arm = self
                        .watchdog
                        .get_or_insert_with(|| Box::pin(tokio::time::sleep(budget)));
                    tokio::time::Sleep::reset(arm.as_mut(), tokio::time::Instant::now() + budget);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(other) => {
                self.terminated = true;
                Poll::Ready(other)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
