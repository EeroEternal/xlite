//! Native HTTP/1.1 upstream connection pool — the HTTP/1.1 counterpart of
//! `xlite_h2_pool` (a sibling crate), and an **opt-in backup** (see
//! `docs/dev/h1-upstream-pool-architecture.md`).
//!
//! # Why this exists
//!
//! HTTP/1.1 has no multiplexing: one connection serves one request at a time.
//! So on this protocol a per-host **connection** cap *is* a per-host **in-flight**
//! cap, which is exactly what the reqwest path lacks (reqwest's
//! `pool_max_idle_per_host` bounds only idle retained connections). The existing
//! `AdmissionGate` approximates the bound at the request level; this pool makes it
//! a real connection bound — idle connections included — the same way the h2c pool
//! does for HTTP/2.
//!
//! # Scope
//!
//! Transport-agnostic: the caller supplies a [`Dial`] closure (TCP/TLS/proxy live
//! there). `in_flight` per connection is `{0, 1}`. The pool is **not wired into the
//! gateway by default**; see the design doc for the `XLITE_UPSTREAM_H1_POOL` gate.
//!
//! # Not yet implemented
//!
//! Idle-connection reaping (`reclaimed` is reported but stays `0`) and
//! `min_idle_conns` prewarming — see §6 of the design doc.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Full;
use hyper::body::{Body, Frame, Incoming};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex as AsyncMutex, Notify};

/// The transport handed to the pool by [`Dial`].
pub trait BoxIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> BoxIo for T {}

/// Opens a fresh transport to `host` (`host:port`).
pub type DialFuture = Pin<Box<dyn Future<Output = io::Result<Box<dyn BoxIo>>> + Send>>;
pub type Dial = Arc<dyn Fn(String) -> DialFuture + Send + Sync>;

/// Errors surfaced by [`Pool::acquire`] and [`Lease::send_request`].
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// Opening the underlying transport failed (TCP connect, TLS, …).
    #[error("failed to open transport to upstream: {0}")]
    Dial(#[from] io::Error),

    /// The HTTP/1.1 handshake failed.
    #[error("HTTP/1.1 handshake failed: {0}")]
    Handshake(#[source] hyper::Error),

    /// Sending the request (or reading its response head) failed. The connection
    /// is marked non-reusable, so the next `acquire` dials a fresh one.
    #[error("upstream request failed: {0}")]
    Request(#[source] hyper::Error),
}

#[derive(Clone)]
pub struct Config {
    /// Hard cap on concurrently open connections per host — on HTTP/1.1 this is
    /// also the in-flight request cap. `0` is treated as `1`.
    pub max_conns_per_host: usize,
    /// Opens a fresh transport to `host:port`.
    pub dial: Dial,
}

/// Point-in-time per-host pool state (for the control plane).
#[derive(Clone, Copy, Debug, Default)]
pub struct PoolStats {
    /// Currently open connections, idle ones included.
    pub connections: usize,
    /// Connections currently serving a request.
    pub in_flight: usize,
    /// Total successful dials since start.
    pub dials: usize,
    /// Connections closed by the idle reaper (always `0` until that lands).
    pub reclaimed: usize,
}

impl PoolStats {
    pub fn new(connections: usize, in_flight: usize, dials: usize, reclaimed: usize) -> Self {
        Self {
            connections,
            in_flight,
            dials,
            reclaimed,
        }
    }
}

/// The upstream HTTP/1.1 connection pool.
pub struct Pool {
    max_conns_per_host: usize,
    dial: Dial,
    hosts: Mutex<HashMap<String, Arc<HostState>>>,
}

struct HostState {
    /// Connection table plus the number of dials in flight (so concurrent
    /// acquirers cannot collectively overshoot `max_conns_per_host`).
    inner: Mutex<HostInner>,
    /// Woken whenever a connection is released or closed.
    notify: Notify,
    dials: AtomicUsize,
    reclaimed: AtomicUsize,
}

#[derive(Default)]
struct HostInner {
    conns: Vec<Arc<ConnEntry>>,
    dialing: usize,
}

/// One physical upstream connection. `in_flight` is a single flag (not a count):
/// HTTP/1.1 serves one request at a time.
struct ConnEntry {
    sender: AsyncMutex<http1::SendRequest<Full<Bytes>>>,
    in_flight: AtomicBool,
    /// Cleared after a failed request so the entry is dropped from the table.
    reusable: AtomicBool,
}

/// Tracks whether the response body of a lease's current request was read to
/// completion. HTTP/1.1 can only reuse a connection whose previous response body
/// was fully consumed; a lease dropped before [`BodyTracker::mark_complete`] marks
/// its connection non-reusable, so `acquire` drops it instead of handing a dead
/// (peer-closed) connection to the next request. Clone it into the body consumer.
#[derive(Clone, Default)]
pub struct BodyTracker(Arc<AtomicBool>);

impl BodyTracker {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }

    /// Call once the response body has been read to completion (i.e. the stream
    /// returned `None`). Until then, releasing the lease retires the connection.
    pub fn mark_complete(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_complete(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// A reserved connection. Holding a [`Lease`] means "this connection is busy";
/// dropping it frees the slot and wakes a waiter. It must be held until the
/// response body has been consumed (that is what makes the bound cover the whole
/// request lifetime, not just the header exchange).
pub struct Lease {
    entry: Arc<ConnEntry>,
    state: Arc<HostState>,
    tracker: BodyTracker,
    released: bool,
}

impl Lease {
    /// Send one request on this connection and await its response head.
    ///
    /// HTTP/1.1 is serial: callers must finish/consume the previous response
    /// before calling this again on the same lease. A failure marks the
    /// connection non-reusable. The returned body ([`ResponseBody`]) marks the
    /// connection reusable on its own once polled to completion.
    pub async fn send_request(
        &self,
        req: Request<Full<Bytes>>,
    ) -> Result<Response<ResponseBody>, PoolError> {
        self.tracker.reset();
        let mut sender = self.entry.sender.lock().await;
        match sender.send_request(req).await {
            Ok(resp) => {
                let tracker = self.tracker.clone();
                let (parts, body) = resp.into_parts();
                Ok(Response::from_parts(
                    parts,
                    ResponseBody {
                        inner: body,
                        tracker,
                    },
                ))
            }
            Err(err) => {
                self.entry.reusable.store(false, Ordering::Release);
                Err(PoolError::Request(err))
            }
        }
    }

    /// Whether this connection is currently serving a request (always `true`
    /// while the lease is held).
    pub fn in_flight(&self) -> bool {
        self.entry.in_flight.load(Ordering::Acquire)
    }
}

/// The response body of a [`Lease::send_request`], wrapping the hyper stream so
/// that reaching end-of-stream marks the connection reusable. A body dropped
/// before then retires the connection ([`Lease`] drop), because HTTP/1.1 cannot
/// reuse a socket whose previous response was not fully read.
pub struct ResponseBody {
    inner: Incoming,
    tracker: BodyTracker,
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let this = self.get_mut();
        let frame = Pin::new(&mut this.inner).poll_frame(cx);
        if matches!(frame, Poll::Ready(None)) {
            this.tracker.mark_complete();
        }
        frame
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            self.entry.in_flight.store(false, Ordering::Release);
            // A connection whose response body was not read to completion cannot
            // be reused by HTTP/1.1 (hyper will have/ will retire it), so retire
            // it here as well — otherwise the next acquire could hand out a dead
            // connection and fail a request that had no failover left.
            if !self.tracker.is_complete() {
                self.entry.reusable.store(false, Ordering::Release);
            }
            self.state.notify.notify_waiters();
        }
    }
}

enum Action {
    Reuse(Arc<ConnEntry>),
    Dial,
    Wait,
}

impl Pool {
    pub fn new(config: Config) -> Self {
        Self {
            max_conns_per_host: config.max_conns_per_host.max(1),
            dial: config.dial,
            hosts: Mutex::new(HashMap::new()),
        }
    }

    fn host_state(&self, host: &str) -> Arc<HostState> {
        let mut hosts = self.hosts.lock().expect("h1 pool hosts mutex poisoned");
        Arc::clone(hosts.entry(host.to_string()).or_insert_with(|| {
            Arc::new(HostState {
                inner: Mutex::new(HostInner::default()),
                notify: Notify::new(),
                dials: AtomicUsize::new(0),
                reclaimed: AtomicUsize::new(0),
            })
        }))
    }

    /// Reserve a connection to `host`, dialing one if there is room, otherwise
    /// waiting until a connection is released or closed.
    pub async fn acquire(&self, host: &str) -> Result<Lease, PoolError> {
        let state = self.host_state(host);
        loop {
            // Register for wake-up *before* re-checking state, so a release that
            // happens between the check and the await is not lost.
            let notified = state.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let action = {
                let mut inner = state.inner.lock().expect("h1 pool inner mutex poisoned");
                // Forget connections that a failed request marked non-reusable.
                inner.conns.retain(|c| c.reusable.load(Ordering::Acquire));
                let reuse = inner
                    .conns
                    .iter()
                    .find(|c| {
                        c.in_flight
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                    })
                    .map(Arc::clone);
                if let Some(entry) = reuse {
                    Action::Reuse(entry)
                } else if inner.conns.len() + inner.dialing < self.max_conns_per_host {
                    inner.dialing += 1;
                    Action::Dial
                } else {
                    Action::Wait
                }
            };

            match action {
                Action::Reuse(entry) => {
                    return Ok(Lease {
                        entry,
                        state: Arc::clone(&state),
                        tracker: BodyTracker::new(),
                        released: false,
                    });
                }
                Action::Dial => {
                    let dialed = self.dial(host).await;
                    let mut inner = state.inner.lock().expect("h1 pool inner mutex poisoned");
                    inner.dialing -= 1;
                    match dialed {
                        Ok(entry) => {
                            entry.in_flight.store(true, Ordering::Release);
                            inner.conns.push(Arc::clone(&entry));
                            state.dials.fetch_add(1, Ordering::Relaxed);
                            return Ok(Lease {
                                entry,
                                state: Arc::clone(&state),
                                tracker: BodyTracker::new(),
                                released: false,
                            });
                        }
                        Err(err) => {
                            drop(inner);
                            state.notify.notify_waiters();
                            return Err(err);
                        }
                    }
                }
                Action::Wait => notified.await,
            }
        }
    }

    async fn dial(&self, host: &str) -> Result<Arc<ConnEntry>, PoolError> {
        let io = (self.dial)(host.to_string()).await?;
        let (sender, conn) = http1::handshake(TokioIo::new(io))
            .await
            .map_err(PoolError::Handshake)?;
        // The connection future must be driven for any I/O to happen; it ends
        // when the peer closes or the handshake fails.
        tokio::spawn(async move {
            if let Err(err) = conn.await {
                tracing::debug!(error = %err, "h1 upstream connection closed");
            }
        });
        Ok(Arc::new(ConnEntry {
            sender: AsyncMutex::new(sender),
            in_flight: AtomicBool::new(false),
            reusable: AtomicBool::new(true),
        }))
    }

    /// Point-in-time stats for one host (`0` if it was never dialed).
    pub fn stats(&self, host: &str) -> PoolStats {
        let state = self
            .hosts
            .lock()
            .expect("h1 pool hosts mutex poisoned")
            .get(host)
            .cloned();
        match state {
            Some(state) => {
                let inner = state.inner.lock().expect("h1 pool inner mutex poisoned");
                PoolStats {
                    connections: inner.conns.len(),
                    in_flight: inner
                        .conns
                        .iter()
                        .filter(|c| c.in_flight.load(Ordering::Acquire))
                        .count(),
                    dials: state.dials.load(Ordering::Relaxed),
                    reclaimed: state.reclaimed.load(Ordering::Relaxed),
                }
            }
            None => PoolStats::default(),
        }
    }

    /// Stats for every host that has ever been dialed, sorted by host.
    pub fn all_stats(&self) -> Vec<(String, PoolStats)> {
        let mut hosts: Vec<String> = self
            .hosts
            .lock()
            .expect("h1 pool hosts mutex poisoned")
            .keys()
            .cloned()
            .collect();
        hosts.sort();
        hosts
            .into_iter()
            .map(|host| {
                let stats = self.stats(&host);
                (host, stats)
            })
            .collect()
    }
}
