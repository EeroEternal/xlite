//! Native HTTP/1.1 upstream connection pool — the HTTP/1.1 counterpart of
//! `xlite_h2_pool` (a sibling crate).
//!
//! # Why this exists
//!
//! HTTP/1.1 has no multiplexing: one connection serves one request at a time.
//! So on this protocol a per-host **connection** cap *is* a per-host **in-flight**
//! cap, which is exactly what the reqwest path lacks (reqwest's
//! `pool_max_idle_per_host` bounds only idle retained connections). This pool
//! enforces a real connection bound — idle connections included — the same way
//! the h2c pool does for HTTP/2.
//!
//! # Scope
//!
//! Transport-agnostic: the caller supplies a [`Dial`] closure (TCP/TLS/proxy live
//! there). `in_flight` per connection is `{0, 1}`.
//!
//! # Not yet implemented
//!
//! Idle-connection reaping (`reclaimed` is reported but stays `0`) and
//! `min_idle_conns` prewarming.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Full;
use hyper::body::{Body, Frame, Incoming};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore, TryAcquireError};

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
    /// Close connections idle for longer than this. Keeping it below the
    /// upstream's own keep-alive timeout is what avoids handing a peer-closed
    /// socket to the next request (the `dials` churn seen in the long soak run).
    pub idle_timeout: Duration,
    /// Keep at least this many idle connections per host — the reaper will not
    /// retire below it (default 1, matching the h2 pool).
    pub min_idle_conns: usize,
    /// Opens a fresh transport to `host:port`.
    pub dial: Dial,
}

/// Default [`Config::idle_timeout`] — matches the h2 pool / reqwest's
/// `pool_idle_timeout`.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Default [`Config::min_idle_conns`].
pub const DEFAULT_MIN_IDLE_CONNS: usize = 1;

/// Bound on bounded dial retries: a transient connect failure is worth a couple
/// of retries (it would otherwise surface as a 502), resource exhaustion is not.
pub const DIAL_RETRIES: u8 = 2;

/// Backoff before each dial retry; exists to avoid a hot spin when the peer
/// refuses immediately.
pub const DIAL_RETRY_BACKOFF: Duration = Duration::from_millis(10);

/// Whether a failed dial is worth retrying. The retryable set is named
/// explicitly so resource exhaustion (EMFILE/ENFILE → `Uncategorized`) falls
/// out — those are persistent and retrying them only amplifies dial volume.
fn is_retryable_dial_error(err: &PoolError) -> bool {
    match err {
        PoolError::Dial(e) => matches!(
            e.kind(),
            io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::TimedOut
                | io::ErrorKind::Interrupted
        ),
        PoolError::Handshake(_) => true,
        PoolError::Request(_) => false,
    }
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
    /// Connections closed by the idle reaper.
    pub reclaimed: usize,
    /// Acquirers currently parked on this host's semaphore — the queue depth.
    pub waiting: usize,
    /// High-water mark of [`PoolStats::waiting`].
    pub waiting_max: usize,
    /// Acquires that had to park at least once.
    pub queued_total: u64,
    /// Exact worst wait seen, in milliseconds.
    pub wait_millis_max: u64,
}

impl PoolStats {}

/// The upstream HTTP/1.1 connection pool.
pub struct Pool {
    max_conns_per_host: usize,
    dial: Dial,
    hosts: Arc<Mutex<HashMap<String, Arc<HostState>>>>,
}

struct HostState {
    /// Connection table.
    inner: Mutex<HostInner>,
    /// Fair FIFO admission: one permit per in-flight request (on HTTP/1.1 that is
    /// one permit per busy connection). Replaces the old `Notify` fan-out, whose
    /// `notify_waiters` thundering herd cost ~50% on the p90 under saturation
    /// (measured @2000 VU: ttfb p90 18.5s vs 12.2s for the semaphore path).
    sem: Arc<Semaphore>,
    dials: AtomicUsize,
    reclaimed: AtomicUsize,
    /// Acquirers currently parked on this host's semaphore — the queue depth.
    /// A queue that cannot be seen cannot be reasoned about.
    waiting: AtomicUsize,
    /// High-water mark of [`HostState::waiting`].
    waiting_max: AtomicUsize,
    /// Acquires that had to park at least once (fast-path acquires excluded).
    queued: AtomicU64,
    /// Total milliseconds those acquires spent parked.
    wait_millis: AtomicU64,
    /// Exact worst wait seen.
    wait_millis_max: AtomicU64,
}

#[derive(Default)]
struct HostInner {
    conns: Vec<Arc<ConnEntry>>,
}

/// One physical upstream connection. `in_flight` is a single flag (not a count):
/// HTTP/1.1 serves one request at a time.
struct ConnEntry {
    sender: AsyncMutex<http1::SendRequest<Full<Bytes>>>,
    in_flight: AtomicBool,
    /// Cleared after a failed request so the entry is dropped from the table.
    reusable: AtomicBool,
    /// When the connection last became idle (`None` while in flight). The reaper
    /// drops entries idle for longer than `idle_timeout`.
    idle_since: Mutex<Option<Instant>>,
}

/// Tracks whether the response body of a lease's current request was read to
/// completion. HTTP/1.1 can only reuse a connection whose previous response body
/// was fully consumed; a lease dropped before [`BodyTracker::mark_complete`] marks
/// its connection non-reusable, so `acquire` drops it instead of handing a dead
/// (peer-closed) connection to the next request.
///
/// Initial state is `true` ("no request outstanding"): a lease that was acquired
/// but never used must stay reusable. `send_request` flips it to `false`, and only
/// reading the body to `None` flips it back.
#[derive(Clone)]
pub struct BodyTracker(Arc<AtomicBool>);

impl BodyTracker {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
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
    /// The admission permit; dropped with the lease, waking exactly the next FIFO
    /// waiter. Holding it is what keeps `connections ≤ max_conns_per_host`.
    _permit: OwnedSemaphorePermit,
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
            *self
                .entry
                .idle_since
                .lock()
                .expect("h1 pool idle_since poisoned") = Some(Instant::now());
            // A connection whose response body was not read to completion cannot
            // be reused by HTTP/1.1 (hyper will have/ will retire it), so retire
            // it here as well — otherwise the next acquire could hand out a dead
            // connection and fail a request that had no failover left.
            if !self.tracker.is_complete() {
                self.entry.reusable.store(false, Ordering::Release);
            }
            // `_permit` drops here, waking exactly the next FIFO waiter.
        }
    }
}

/// Background task: periodically retire connections idle for longer than
/// `idle_timeout` (mirrors the h2 pool). This keeps the connection count tracking
/// load and stops the pool from handing out sockets the upstream already closed.
async fn reaper_loop(
    hosts: Arc<Mutex<HashMap<String, Arc<HostState>>>>,
    idle_timeout: Duration,
    min_idle: usize,
) {
    let interval = (idle_timeout / 2).max(Duration::from_millis(50));
    loop {
        tokio::time::sleep(interval).await;
        let states: Vec<Arc<HostState>> = {
            let hosts = hosts.lock().expect("h1 pool hosts mutex poisoned");
            hosts.values().cloned().collect()
        };
        let now = Instant::now();
        for state in states {
            let removed = {
                let mut inner = state.inner.lock().expect("h1 pool inner mutex poisoned");
                let before = inner.conns.len();
                // Retire idle-and-expired connections, but never below `min_idle`.
                let mut idle = inner
                    .conns
                    .iter()
                    .filter(|c| !c.in_flight.load(Ordering::Acquire))
                    .count();
                inner.conns.retain(|c| {
                    if c.in_flight.load(Ordering::Acquire) {
                        return true;
                    }
                    let expired = match *c.idle_since.lock().expect("h1 pool idle_since poisoned") {
                        Some(since) => now.duration_since(since) >= idle_timeout,
                        None => false,
                    };
                    if expired && idle > min_idle {
                        idle -= 1;
                        false
                    } else {
                        true
                    }
                });
                before - inner.conns.len()
            };
            if removed > 0 {
                state.reclaimed.fetch_add(removed, Ordering::Relaxed);
            }
        }
    }
}

impl Pool {
    pub fn new(config: Config) -> Self {
        let hosts = Arc::new(Mutex::new(HashMap::new()));
        // Start the idle-connection reaper on the current runtime, if any. A
        // caller outside a runtime just gets a pool without the background task.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(reaper_loop(
                Arc::clone(&hosts),
                config.idle_timeout,
                config.min_idle_conns,
            ));
        }
        Self {
            max_conns_per_host: config.max_conns_per_host.max(1),
            dial: config.dial,
            hosts,
        }
    }

    fn host_state(&self, host: &str) -> Arc<HostState> {
        let mut hosts = self.hosts.lock().expect("h1 pool hosts mutex poisoned");
        Arc::clone(hosts.entry(host.to_string()).or_insert_with(|| {
            Arc::new(HostState {
                inner: Mutex::new(HostInner::default()),
                sem: Arc::new(Semaphore::new(self.max_conns_per_host)),
                waiting: AtomicUsize::new(0),
                waiting_max: AtomicUsize::new(0),
                queued: AtomicU64::new(0),
                wait_millis: AtomicU64::new(0),
                wait_millis_max: AtomicU64::new(0),
                dials: AtomicUsize::new(0),
                reclaimed: AtomicUsize::new(0),
            })
        }))
    }

    /// Reserve a connection to `host`. Waits in a fair FIFO queue (the semaphore)
    /// for a slot, then reuses an idle connection or dials a new one.
    ///
    /// The permit bounds **in-flight leases** (concurrent requests). On HTTP/1.1
    /// one connection serves one request, so `connections ≤ in-flight peak ≤
    /// max_conns_per_host` follows; idle connections hold no permit and are managed
    /// by the reaper (`min_idle_conns`).
    pub async fn acquire(&self, host: &str) -> Result<Lease, PoolError> {
        let state = self.host_state(host);
        // Fast path: a free slot and nobody ahead of us (the semaphore is fair,
        // so a parked acquirer would be served first) ⇒ this request never
        // queues, so it must NOT touch the waiting counters. Counting it would
        // make `waiting_max` report "queued" for a request that never queued.
        let permit = match Arc::clone(&state.sem).try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                // Slow path: actually parking — now the counters are meaningful.
                let start = Instant::now();
                let depth = state.waiting.fetch_add(1, Ordering::Relaxed) + 1;
                state.waiting_max.fetch_max(depth, Ordering::Relaxed);
                let permit = Arc::clone(&state.sem)
                    .acquire_owned()
                    .await
                    .expect("h1 pool semaphore is never closed");
                state.waiting.fetch_sub(1, Ordering::Relaxed);
                state.queued.fetch_add(1, Ordering::Relaxed);
                let ms = start.elapsed().as_millis() as u64;
                state.wait_millis.fetch_add(ms, Ordering::Relaxed);
                state.wait_millis_max.fetch_max(ms, Ordering::Relaxed);
                permit
            }
            Err(TryAcquireError::Closed) => unreachable!("h1 pool semaphore is never closed"),
        };

        {
            let mut inner = state.inner.lock().expect("h1 pool inner mutex poisoned");
            // Forget connections that a failed request marked non-reusable.
            inner.conns.retain(|c| c.reusable.load(Ordering::Acquire));
            // Reuse an idle connection if there is one.
            if let Some(entry) = inner
                .conns
                .iter()
                .find(|c| {
                    if c.in_flight
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        *c.idle_since.lock().expect("h1 pool idle_since poisoned") = None;
                        true
                    } else {
                        false
                    }
                })
                .map(Arc::clone)
            {
                return Ok(Lease {
                    entry,
                    _permit: permit,
                    tracker: BodyTracker::new(),
                    released: false,
                });
            }
        }
        // No idle connection: the permit proves we are below the cap, so dial a
        // fresh one (its count is bounded by the permit count).
        let entry = self.dial(host).await?;
        {
            let mut inner = state.inner.lock().expect("h1 pool inner mutex poisoned");
            entry.in_flight.store(true, Ordering::Release);
            inner.conns.push(Arc::clone(&entry));
            state.dials.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Lease {
            entry,
            _permit: permit,
            tracker: BodyTracker::new(),
            released: false,
        })
    }

    async fn dial(&self, host: &str) -> Result<Arc<ConnEntry>, PoolError> {
        let mut attempt = 0u8;
        loop {
            match self.dial_once(host).await {
                Ok(entry) => return Ok(entry),
                Err(err) => {
                    if attempt >= DIAL_RETRIES || !is_retryable_dial_error(&err) {
                        return Err(err);
                    }
                    attempt += 1;
                    tokio::time::sleep(DIAL_RETRY_BACKOFF).await;
                }
            }
        }
    }

    async fn dial_once(&self, host: &str) -> Result<Arc<ConnEntry>, PoolError> {
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
            idle_since: Mutex::new(None),
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
                    waiting: state.waiting.load(Ordering::Relaxed),
                    waiting_max: state.waiting_max.load(Ordering::Relaxed),
                    queued_total: state.queued.load(Ordering::Relaxed),
                    wait_millis_max: state.wait_millis_max.load(Ordering::Relaxed),
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
