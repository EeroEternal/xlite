//! The [`Lease`] handle returned by [`crate::Pool::acquire`].

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use bytes::Bytes;
use h2::SendStream;
use h2::client::{ResponseFuture, SendRequest};
use http::Request;

use crate::pool::{ConnEntry, HostState, bump};

/// A reserved stream slot on a specific connection.
///
/// Holding a [`Lease`] counts as one in-flight stream on the connection (it
/// was reserved atomically by [`crate::Pool::acquire`]); dropping it frees
/// the slot and wakes any queued acquirer.
pub struct Lease {
    entry: Arc<ConnEntry>,
    state: Weak<HostState>,
    send: SendRequest<Bytes>,
    reused: bool,
}

impl Lease {
    pub(crate) fn new(entry: Arc<ConnEntry>, state: Weak<HostState>, reused: bool) -> Self {
        let send = entry.send.clone();
        Self {
            entry,
            state,
            send,
            reused,
        }
    }

    /// Whether this lease was checked out from an already-established pooled
    /// connection (`true`) rather than freshly dialed for this acquire (`false`).
    pub fn is_reused(&self) -> bool {
        self.reused
    }

    /// Immediately evict this lease's connection from the host pool so no
    /// subsequent acquire reserves a stream on a connection that has just
    /// failed with a transport/GOAWAY error.
    pub fn evict(&self) {
        if let Some(state) = self.state.upgrade() {
            let removed = state.retain(|c| !Arc::ptr_eq(c, &self.entry));
            if removed > 0 {
                // Visible in `/api/pool`: a rising count means the pool keeps finding
                // sockets it can no longer trust, not that housekeeping is running.
                state.note_eviction();
            }
            bump(&state.generation_tx);
        }
    }

    /// Whether this connection polls its streams in its own task
    /// ([`crate::Config::pinned_poll`]). When false the caller polls the
    /// `RecvStream` itself.
    pub fn pinned_poll(&self) -> bool {
        self.entry.pump.is_some()
    }

    /// Hand a response body to the connection's task, which polls it — no other
    /// task polls this connection's streams. Only call when [`Self::pinned_poll`].
    pub async fn register(
        &self,
        recv: h2::RecvStream,
    ) -> tokio::sync::mpsc::Receiver<Result<Bytes, String>> {
        match self.entry.pump.as_ref() {
            Some(tx) => crate::pump::register(tx, recv).await,
            None => {
                let (out, rx) = tokio::sync::mpsc::channel(1);
                let _ = out
                    .send(Err("lease is not in pinned-poll mode".to_string()))
                    .await;
                rx
            }
        }
    }

    /// Send a request head on this connection's next free stream.
    ///
    /// `end_of_stream = true` for a body-less request; otherwise send the body
    /// over the returned [`SendStream`].
    pub fn send_request(
        &mut self,
        request: Request<()>,
        end_of_stream: bool,
    ) -> Result<(ResponseFuture, SendStream<Bytes>), h2::Error> {
        self.send.send_request(request, end_of_stream)
    }

    /// The peer's **live** `SETTINGS_MAX_CONCURRENT_STREAMS` for this
    /// connection. Refreshed whenever the connection is polled, so a peer that
    /// changes it mid-connection is reflected here too.
    pub fn max_concurrent_streams(&self) -> usize {
        self.entry.cap.load(Ordering::Acquire)
    }

    /// Streams currently in flight on this connection (including this lease).
    pub fn in_flight(&self) -> usize {
        self.entry.in_flight.load(Ordering::Relaxed)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let previous = self.entry.in_flight.fetch_sub(1, Ordering::Relaxed);
        // The connection just went idle: stamp it so the reaper can reclaim it.
        if previous == 1 {
            self.entry
                .idle_since_ms
                .store(crate::pool::monotonic_ms(), Ordering::Release);
        }
        bump(&self.entry.generation_tx);
    }
}
