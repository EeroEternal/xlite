//! The per-host connection pool.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Poll;
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, ArcSwapOption};
use bytes::Bytes;
use h2::client::SendRequest;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use crate::error::PoolError;
use crate::lease::Lease;

/// A boxed async transport stream (`AsyncRead + AsyncWrite + Unpin + Send`).
///
/// A supertrait is used because Rust forbids combining multiple non-auto
/// traits directly in a `dyn` object; `Box<dyn Transport>` is automatically
/// `Unpin`.
pub trait Transport: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Transport for T {}

pub type BoxIo = Box<dyn Transport>;

/// A boxed future that opens a transport stream.
pub type DialFuture = Pin<Box<dyn Future<Output = io::Result<BoxIo>> + Send>>;

/// Opens a new transport stream to `host`.
///
/// The pool does not care whether the stream is raw TCP, TLS, or a proxied
/// socket; it only requires the peer to speak HTTP/2 (h2c prior knowledge or
/// ALPN-negotiated h2). The caller plugs its dialling strategy in here.
pub type Dial = Arc<dyn Fn(String) -> DialFuture + Send + Sync>;

/// Pool configuration.
pub struct Config {
    /// Upper bound on live connections per host — Go's `MaxConnsPerHost`.
    /// Growth stops at this bound; further requests wait for a stream to free.
    pub max_conns_per_host: usize,
    /// Client-side cap on streams per connection, applied **on top of** the
    /// peer's live `SETTINGS_MAX_CONCURRENT_STREAMS`: a connection is full at
    /// `min(peer_settings, stream_budget_per_conn)`. `usize::MAX` (the default
    /// `build_native_h2` passes when `XROUTER_UPSTREAM_STREAM_CAP` is unset) means
    /// "let the peer decide" — the pre-existing behaviour.
    ///
    /// Lowering it below the peer's value makes the pool dial more connections
    /// and spread the in-flight streams across them. That is not only a
    /// concurrency knob: every stream on a connection shares that connection's
    /// internal state lock, so packing hundreds of streams onto one connection
    /// turns the lock into a cross-worker contention point (measured at ~30–50%
    /// of gateway CPU in `docs/dev/loadtest-linkbound-2026-09.md` §十二). Note
    /// that the same experiment showed lowering it also costs an upstream
    /// first-byte **tail** regression (p90 ×4–5), so it is a lever, not a fix.
    pub stream_budget_per_conn: usize,
    /// Poll this authority's `RecvStream`s from the connection's own task — the
    /// same task that drives the connection — instead of from whichever request
    /// task owns the body. That makes the connection's shared lock have exactly
    /// one owner, without adding connections
    /// (see `docs/dev/h2-pinned-poll-2026-09.md` §十二).
    pub pinned_poll: bool,
    /// Opens the underlying transport for a host.
    pub dial: Dial,
    /// Close a connection that has had no in-flight stream for this long,
    /// keeping at least one per host. Mirrors reqwest's `pool_idle_timeout`;
    /// without it a long-lived pool keeps every connection it ever dialed, up
    /// to `max_conns_per_host` per host.
    pub idle_timeout: Duration,
}

/// Default [`Config::idle_timeout`] — matches reqwest's `pool_idle_timeout`.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Default [`Config::stream_budget_per_conn`] — Go's `net/http2` default
/// `SETTINGS_MAX_CONCURRENT_STREAMS`, i.e. "let the peer decide".
pub const DEFAULT_STREAM_BUDGET_PER_CONN: usize = 250;

/// One live HTTP/2 connection to an upstream host.
pub(crate) struct ConnEntry {
    pub(crate) send: SendRequest<Bytes>,
    /// The peer's **live** `SETTINGS_MAX_CONCURRENT_STREAMS`: seeded from the
    /// handshake and refreshed on every poll of the connection, because a peer
    /// may change it at any time (see [`publish_peer_cap`]).
    ///
    /// Behind its own `Arc` so the connection's driver task can refresh it
    /// without holding a strong reference to this entry.
    pub(crate) cap: Arc<AtomicUsize>,
    /// Streams currently reserved on this connection.
    pub(crate) in_flight: AtomicUsize,
    /// [`monotonic_ms`] at which `in_flight` last became 0, used by the idle
    /// reaper. Seeded at creation so a never-used connection is reclaimable.
    pub(crate) idle_since_ms: AtomicU64,
    /// Bumped whenever a slot on this host may have freed, so queued acquirers
    /// re-scan.
    pub(crate) generation_tx: watch::Sender<u64>,
    /// When [`Config::pinned_poll`] is on: the channel this connection's task
    /// listens on for the `RecvStream`s it polls (merged driver + pump).
    pub(crate) pump: Option<tokio::sync::mpsc::Sender<crate::pump::Register>>,
}

pub(crate) struct HostState {
    /// Live connections as an atomically-swapped snapshot: readers (the acquire
    /// hot path) never take a lock, only writers do (see `mutate`).
    conns: ArcSwap<Vec<Arc<ConnEntry>>>,
    /// Serializes the writers — dial-push, close-remove, idle-reap. All rare.
    mutate: Mutex<()>,
    /// `0` = no dial in flight, `1` = one dial in flight. At most one dial per
    /// host, so a burst waits for that dial instead of each opening its own
    /// connection (Go's `Transport` dedupes the same way).
    dialing: AtomicUsize,
    /// Monotonic generation counter. `watch::Receiver::changed()` resolves if
    /// the value changed since subscribe, so a slot freed just before a waiter
    /// subscribes is never lost (unlike a bare `Notify` + `notify_waiters`,
    /// which does not store a permit).
    pub(crate) generation_tx: watch::Sender<u64>,
    /// Set once the per-host idle reaper has been spawned.
    reaper_started: AtomicBool,
    /// Connections successfully dialled.
    dials: AtomicU64,
    /// Idle connections closed by the reaper.
    reclaimed: AtomicU64,
    /// Dials that could not read the peer's stream cap and used the fallback.
    settings_fallbacks: AtomicU64,
    /// Times a connection here observed the peer change its stream cap
    /// mid-connection. Shared with every [`ConnEntry`] (the connection's driver
    /// task, not the host, is the one that samples it).
    cap_updates: Arc<AtomicU64>,
}

impl HostState {
    fn new() -> Self {
        let (generation_tx, _generation_rx) = watch::channel(0u64);
        Self {
            conns: ArcSwap::from_pointee(Vec::new()),
            mutate: Mutex::new(()),
            dialing: AtomicUsize::new(0),
            generation_tx,
            reaper_started: AtomicBool::new(false),
            dials: AtomicU64::new(0),
            reclaimed: AtomicU64::new(0),
            settings_fallbacks: AtomicU64::new(0),
            cap_updates: Arc::new(AtomicU64::new(0)),
        }
    }

    fn connections(&self) -> usize {
        self.conns.load().len()
    }

    /// Append a connection; returns the new count. Writers only.
    fn push(&self, entry: Arc<ConnEntry>) -> usize {
        let _guard = self.mutate.lock().unwrap();
        let mut next = (**self.conns.load()).clone();
        next.push(entry);
        let len = next.len();
        self.conns.store(Arc::new(next));
        len
    }

    /// Drop the connections for which `keep` is false. Writers only.
    pub(crate) fn retain(&self, keep: impl Fn(&Arc<ConnEntry>) -> bool) {
        let _guard = self.mutate.lock().unwrap();
        let mut next = (**self.conns.load()).clone();
        next.retain(|c| keep(c));
        self.conns.store(Arc::new(next));
    }
}

/// Point-in-time snapshot of one pool's state (for `/metrics` / diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStats {
    /// Live connections.
    pub connections: usize,
    /// Streams currently reserved across those connections.
    pub in_flight: usize,
    /// Connections dialled over this pool's lifetime.
    pub dials: u64,
    /// Idle connections closed by the reaper.
    pub reclaimed: u64,
    /// Dials that used the stream-cap fallback instead of the peer's SETTINGS.
    pub settings_fallbacks: u64,
    /// Times a connection observed the peer change
    /// `SETTINGS_MAX_CONCURRENT_STREAMS` mid-connection. A rising value means the
    /// pool is re-sizing connections live (and that a handshake-time cap would
    /// no longer be the truth for this upstream).
    pub cap_updates: u64,
}

/// Most-recently-used host, consulted without taking the map lock.
struct LastHost {
    host: String,
    state: Arc<HostState>,
}

/// Client-side flow-control windows (bytes).
///
/// h2's spec defaults are 64 KiB for both the connection and each stream. That
/// is far too small here: the connection window is shared by **every** stream on
/// the connection, and this pool deliberately multiplexes many streams onto one
/// connection — at ~100 streams/connection the spec window leaves each stream
/// only ~600 B of credit, so the peer stalls waiting for `WINDOW_UPDATE` and the
/// stream pace stretches. hyper's client uses 5 MiB / 2 MiB for the same reason;
/// match it.
pub const CONNECTION_WINDOW_SIZE: u32 = 5 * 1024 * 1024;
pub const STREAM_WINDOW_SIZE: u32 = 2 * 1024 * 1024;

/// Build the h2 client the pool uses: windows sized for many streams per
/// connection, and no server push (this pool never accepts pushed streams).
pub fn client_builder() -> h2::client::Builder {
    let mut builder = h2::client::Builder::new();
    builder
        .initial_connection_window_size(CONNECTION_WINDOW_SIZE)
        .initial_window_size(STREAM_WINDOW_SIZE)
        .enable_push(false);
    builder
}

/// A per-host native HTTP/2 connection pool.
pub struct Pool {
    max_conns_per_host: usize,
    stream_budget_per_conn: usize,
    /// When [`Config::pinned_poll`] is on: each connection's task drives the
    /// connection *and* polls its streams.
    pinned_poll: bool,
    dial: Dial,
    idle_timeout: Duration,
    hosts: Mutex<HashMap<String, Arc<HostState>>>,
    /// Lock-free fast path for the hot path: the gateway builds one pool per
    /// upstream authority, so after the first request this always hits and the
    /// per-acquire `String` allocation + map lock disappear.
    last_host: ArcSwapOption<LastHost>,
}

/// Signal every waiter on this host that the slot landscape changed.
pub(crate) fn bump(tx: &watch::Sender<u64>) {
    // No receivers is fine: nobody is waiting, and the freed slot is still
    // visible to any future acquire via the in-flight counters.
    tx.send_modify(|generation| *generation += 1);
}

impl Pool {
    /// Create a pool with the given configuration.
    pub fn new(config: Config) -> Self {
        Self {
            max_conns_per_host: config.max_conns_per_host.max(1),
            stream_budget_per_conn: config.stream_budget_per_conn.max(1),
            pinned_poll: config.pinned_poll,
            dial: config.dial,
            idle_timeout: config.idle_timeout,
            hosts: Mutex::new(HashMap::new()),
            last_host: ArcSwapOption::empty(),
        }
    }

    /// Number of live connections currently held for `host`.
    pub fn connection_count(&self, host: &str) -> usize {
        let hosts = self.hosts.lock().unwrap();
        hosts
            .get(host)
            .map(|state| state.connections())
            .unwrap_or(0)
    }

    /// Acquire a lease on a connection that has room for one more stream.
    ///
    /// - Reserves a slot on the first connection with
    ///   `in_flight < min(peer_cap, stream_budget_per_conn)`.
    /// - Dials a fresh connection when every live connection is full and the
    ///   count is below [`Config::max_conns_per_host`].
    /// - Waits when every connection is full and the cap is reached.
    pub async fn acquire(&self, host: &str) -> Result<Lease, PoolError> {
        let state = self.host_state(host);

        loop {
            // Hot path: fully lock-free — a snapshot load, a `&str`-free scan
            // and one CAS on the connection's in-flight counter. No host mutex,
            // no watch lock.
            if let Some(entry) = Self::reserve_existing(&state, self.stream_budget_per_conn) {
                return Ok(Lease::new(entry));
            }

            // All full: dial another connection if there is room and none is
            // already being dialled.
            if state.connections() < self.max_conns_per_host
                && state
                    .dialing
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                // Release the dial slot on every exit path — including when
                // this future is dropped mid-dial — so a cancelled acquire
                // never leaks a slot.
                let slot = DialSlot { state: &state };
                let entry = self.dial_and_insert(host, &state).await?;
                drop(slot);

                // `dial_and_insert` already reserved this request's slot while
                // the connection was still invisible to other acquirers, so a
                // waiter woken by the insert can never take the last free slot
                // before us (which is what produced `old == cap` here).
                return Ok(Lease::new(entry));
            }

            // Slow path. Subscribe *after* the failed scan, then re-scan: a
            // release that raced the first scan is caught by the re-scan, and
            // anything after it bumps the generation this receiver observes — so
            // no wakeup is lost while the hot path pays for no watch lock.
            let mut generation = state.generation_tx.subscribe();
            if let Some(entry) = Self::reserve_existing(&state, self.stream_budget_per_conn) {
                return Ok(Lease::new(entry));
            }
            if generation.changed().await.is_err() {
                // Sender dropped: the pool is gone.
                return Err(PoolError::Closed);
            }
        }
    }

    /// Try to reserve a slot on an existing connection with room to spare.
    ///
    /// Lock-free: reads an atomic snapshot of the connection list. A connection
    /// reaped concurrently stays alive for this lease (the snapshot holds an
    /// `Arc`), it is simply no longer offered to later acquirers.
    fn reserve_existing(state: &HostState, stream_budget: usize) -> Option<Arc<ConnEntry>> {
        for entry in state.conns.load().iter() {
            if try_reserve_stream(&entry.cap, &entry.in_flight, stream_budget) {
                return Some(Arc::clone(entry));
            }
        }
        None
    }

    /// Open and register a new connection to `host`.
    async fn dial_and_insert(
        &self,
        host: &str,
        state: &Arc<HostState>,
    ) -> Result<Arc<ConnEntry>, PoolError> {
        let io = (self.dial)(host.to_string()).await?;
        let (send, mut conn) = client_builder().handshake::<_, Bytes>(io).await?;
        // The whole point of this crate: use the peer's real cap.
        let (cap, fell_back) = read_peer_stream_cap(&mut conn).await;
        let cap = cap.max(1);
        if fell_back {
            state.settings_fallbacks.fetch_add(1, Ordering::Relaxed);
        }
        state.dials.fetch_add(1, Ordering::Relaxed);

        let cap_gate = Arc::new(AtomicUsize::new(cap));
        let (pump_tx, pump_rx) = if self.pinned_poll {
            let (tx, rx) = crate::pump::register_channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let entry = Arc::new(ConnEntry {
            send,
            cap: Arc::clone(&cap_gate),
            // Reserve this request's slot *now*, before the connection is
            // pushed into the pool and `bump()` wakes waiters. Otherwise a
            // waiter woken by the insert can `reserve_existing` the fresh
            // connection first, filling it to `cap` before the dialing request
            // reserves its own — the dialer would then queue inside the very
            // connection it opened (and `in_flight` would exceed `cap`).
            in_flight: AtomicUsize::new(1),
            idle_since_ms: AtomicU64::new(monotonic_ms()),
            generation_tx: state.generation_tx.clone(),
            pump: pump_tx,
        });

        // Drive the connection to completion; when it closes, drop it from the
        // host and wake any waiters.
        //
        // The task holds only *weak* references. Holding strong ones would keep
        // the `SendRequest` alive, and an h2 connection only closes once every
        // `SendRequest` handle is dropped — so an abandoned pool (e.g. a
        // `ProviderWire` rebuilt after a cache miss) would leak its sockets
        // forever. Weak refs let the pool's drop close the connection, which
        // then completes this task.
        {
            let entry_weak = Arc::downgrade(&entry);
            let state_weak = Arc::downgrade(state);
            // Three handles that do *not* own the connection: the cap cell, the
            // host's update counter and the host's wake-up sender.
            let cap_gate = Arc::clone(&cap_gate);
            let cap_updates = Arc::clone(&state.cap_updates);
            let generation_tx = state.generation_tx.clone();
            tokio::spawn(async move {
                match pump_rx {
                    // Merged: drive the connection and poll its streams from one
                    // task, so the connection's shared lock has one owner.
                    Some(rx) => {
                        crate::pump::drive_and_pump(
                            conn,
                            cap_gate,
                            cap_updates,
                            generation_tx,
                            rx,
                            state_weak,
                            entry_weak,
                        )
                        .await;
                    }
                    None => {
                        drive_connection(&mut conn, &cap_gate, &cap_updates, &generation_tx).await;
                        if let (Some(state), Some(entry)) =
                            (state_weak.upgrade(), entry_weak.upgrade())
                        {
                            state.retain(|c| !Arc::ptr_eq(c, &entry));
                            bump(&state.generation_tx);
                        }
                    }
                }
            });
        }

        // Register for future acquires and wake anyone queued on a full pool.
        let connections = state.push(Arc::clone(&entry));
        tracing::debug!(
            host = %host,
            cap,
            connections,
            max = self.max_conns_per_host,
            "native h2 upstream connection established"
        );
        bump(&state.generation_tx);
        self.start_idle_reaper(state);
        Ok(entry)
    }

    /// Start the per-host idle reaper at most once. It periodically drops
    /// connections with no in-flight stream for longer than `idle_timeout`,
    /// keeping one so the next request does not have to dial.
    fn start_idle_reaper(&self, state: &Arc<HostState>) {
        if state.reaper_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(state);
        let idle_timeout = self.idle_timeout;
        // Check twice per timeout (with a floor) so reclamation is timely
        // without busy-looping.
        let interval = (idle_timeout / 2).max(Duration::from_millis(50));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                // Weak: a dropped pool must not be kept alive by its reaper.
                let Some(state) = weak.upgrade() else {
                    break;
                };
                reap_idle(&state, idle_timeout);
            }
        });
    }

    fn host_state(&self, host: &str) -> Arc<HostState> {
        // Hot path: no `String` allocation, no map lock.
        if let Some(last) = self.last_host.load_full()
            && last.host == host
        {
            return Arc::clone(&last.state);
        }
        let state = {
            let mut hosts = self.hosts.lock().unwrap();
            hosts
                .entry(host.to_string())
                .or_insert_with(|| Arc::new(HostState::new()))
                .clone()
        };
        self.last_host.store(Some(Arc::new(LastHost {
            host: host.to_string(),
            state: Arc::clone(&state),
        })));
        state
    }

    /// Point-in-time stats for one host (used by `/metrics`).
    pub fn stats(&self, host: &str) -> PoolStats {
        let hosts = self.hosts.lock().unwrap();
        let Some(state) = hosts.get(host) else {
            return PoolStats {
                connections: 0,
                in_flight: 0,
                dials: 0,
                reclaimed: 0,
                settings_fallbacks: 0,
                cap_updates: 0,
            };
        };
        let (connections, in_flight) = {
            let conns = state.conns.load();
            let in_flight = conns
                .iter()
                .map(|c| c.in_flight.load(Ordering::Acquire))
                .sum();
            (conns.len(), in_flight)
        };
        PoolStats {
            connections,
            in_flight,
            dials: state.dials.load(Ordering::Relaxed),
            reclaimed: state.reclaimed.load(Ordering::Relaxed),
            settings_fallbacks: state.settings_fallbacks.load(Ordering::Relaxed),
            cap_updates: state.cap_updates.load(Ordering::Relaxed),
        }
    }
}

/// Milliseconds since a process-wide monotonic origin.
pub(crate) fn monotonic_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Drop connections with no in-flight stream for longer than `idle_timeout`,
/// keeping at least one per host so the next request does not have to dial.
///
/// Runs under the host lock, so it cannot race a concurrent acquire reserving a
/// slot (that path takes the same lock).
/// Drop connections with no in-flight stream for longer than `idle_timeout`,
/// keeping at least one per host so the next request does not have to dial.
///
/// Takes the writers' lock (`mutate`), so it cannot interleave with a dial-push
/// or a close-remove. An acquire may reserve a slot lock-free while this runs;
/// that connection then stays alive for the lease and is simply not offered to
/// later acquirers.
fn reap_idle(state: &HostState, idle_timeout: Duration) {
    const KEEP_MIN: usize = 1;
    let idle_ms = idle_timeout.as_millis() as u64;
    let now = monotonic_ms();

    let guard = state.mutate.lock().unwrap();
    let current = state.conns.load();
    if current.len() <= KEEP_MIN {
        return;
    }

    let mut survivors = Vec::with_capacity(current.len());
    let mut idle = Vec::new();
    for entry in current.iter() {
        let is_idle = entry.in_flight.load(Ordering::Acquire) == 0
            && now.saturating_sub(entry.idle_since_ms.load(Ordering::Acquire)) >= idle_ms;
        if is_idle {
            idle.push(Arc::clone(entry));
        } else {
            survivors.push(Arc::clone(entry));
        }
    }
    // Keep at least one (preferring a live one, else the most recent idle).
    while survivors.len() < KEEP_MIN {
        match idle.pop() {
            Some(entry) => survivors.push(entry),
            None => break,
        }
    }
    let removed = idle.len();
    if removed == 0 {
        return;
    }
    let remaining = survivors.len();
    state.conns.store(Arc::new(survivors));
    drop(current);
    drop(guard);

    state.reclaimed.fetch_add(removed as u64, Ordering::Relaxed);
    bump(&state.generation_tx);
    tracing::debug!(removed, remaining, "reclaimed idle upstream h2 connections");
}

/// Releases a reserved dial slot when dropped.
struct DialSlot<'a> {
    state: &'a HostState,
}

impl Drop for DialSlot<'_> {
    fn drop(&mut self) {
        self.state.dialing.store(0, Ordering::Release);
        bump(&self.state.generation_tx);
    }
}

/// Reserve one stream slot on a connection, against the peer's **live** cap
/// clamped by the client's own budget.
///
/// The cap is read on every attempt instead of being cached at handshake time,
/// so a mid-connection `SETTINGS` update takes effect on the next acquire. Two
/// acquirers racing for the last slot is possible (the load and the add are not
/// one atomic step) — the loser rolls its reservation back immediately, so the
/// counter never stays above the cap.
fn try_reserve_stream(cap: &AtomicUsize, in_flight: &AtomicUsize, stream_budget: usize) -> bool {
    let limit = cap.load(Ordering::Acquire).min(stream_budget);
    let previous = in_flight.fetch_add(1, Ordering::AcqRel);
    if previous < limit {
        return true;
    }
    in_flight.fetch_sub(1, Ordering::AcqRel);
    false
}

/// Publish a freshly sampled peer stream cap onto a connection.
///
/// Two samples are deliberately ignored:
///
/// - `usize::MAX` means h2 has not (yet) processed a `SETTINGS` frame. Storing
///   it would read as "unlimited streams" and quietly defeat the gate — the
///   whole point of which is to never queue inside a full connection.
/// - an unchanged value, which is the overwhelming majority of polls.
///
/// Returns `true` only when the effective cap actually changed.
pub(crate) fn publish_peer_cap(cap: &AtomicUsize, sampled: usize) -> bool {
    if sampled == usize::MAX || cap.load(Ordering::Acquire) == sampled {
        return false;
    }
    // Sole writer (this connection's driver task), so load/compare/store is
    // enough — and unlike `swap` it does not dirty the line on every poll.
    cap.store(sampled, Ordering::Release);
    true
}

/// Drive an h2 connection to completion, keeping its stream cap in step with
/// the peer's live `SETTINGS_MAX_CONCURRENT_STREAMS`.
///
/// Takes the cap cells individually rather than the owning [`ConnEntry`]: this
/// task must **not** hold a strong reference to the entry, because that would
/// keep the `SendRequest` alive and an h2 connection only closes once every
/// handle is dropped (which is what leaked sockets before). None of these three
/// handles owns the connection.
///
/// Sampling after every poll is free in practice: polling is what applies an
/// inbound `SETTINGS` frame in the first place, so a changed cap is always seen
/// on the poll that processed it (no timer, no extra wakeup).
async fn drive_connection(
    conn: &mut h2::client::Connection<BoxIo, Bytes>,
    cap: &AtomicUsize,
    cap_updates: &AtomicU64,
    generation_tx: &watch::Sender<u64>,
) {
    // `Connection` yields `Result<(), h2::Error>`; the pool only cares that it
    // ended, exactly as the previous `let _ = conn.await;` did.
    let _ = poll_fn(|cx| {
        let poll = Pin::new(&mut *conn).poll(cx);
        if publish_peer_cap(cap, conn.max_concurrent_send_streams()) {
            cap_updates.fetch_add(1, Ordering::Relaxed);
            // The number of streams this connection accepts just moved: queued
            // acquirers must re-scan instead of waiting on the old value. A
            // lowered cap wakes them too — they re-scan, find less room than
            // they assumed, and park again.
            bump(generation_tx);
        }
        poll
    })
    .await;
}

/// Drive the freshly-handshaken connection until the peer's
/// `SETTINGS_MAX_CONCURRENT_STREAMS` is applied.
///
/// h2 reports `usize::MAX` until the server's SETTINGS frame has actually been
/// processed, which happens on the first polls of the connection *after* the
/// handshake future resolves. We await that (via [`poll_fn`], which parks the
/// task on the socket rather than busy-spinning) instead of guessing, so a
/// late-arriving SETTINGS on a real TCP link is still read correctly.
async fn read_peer_stream_cap(conn: &mut h2::client::Connection<BoxIo, Bytes>) -> (usize, bool) {
    // Whether we had to guess instead of reading the peer's SETTINGS.
    let fell_back = std::sync::atomic::AtomicBool::new(false);
    let cap = tokio::time::timeout(
        Duration::from_millis(500),
        poll_fn(|cx| {
            let known = conn.max_concurrent_send_streams();
            if known != usize::MAX {
                return Poll::Ready(known);
            }
            match Pin::new(&mut *conn).poll(cx) {
                // Closed before advertising a cap: fall back below.
                Poll::Ready(_) => {
                    fell_back.store(true, Ordering::Relaxed);
                    Poll::Ready(250)
                }
                Poll::Pending => Poll::Pending,
            }
        }),
    )
    .await;
    match cap {
        Ok(cap) => (cap, fell_back.load(Ordering::Relaxed)),
        // Timed out: the peer never advertised a cap.
        Err(_) => (250, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atomics(cap: usize, in_flight: usize) -> (AtomicUsize, AtomicUsize) {
        (AtomicUsize::new(cap), AtomicUsize::new(in_flight))
    }

    /// Most tests exercise the peer's cap alone; `usize::MAX` means "no
    /// client-side budget in the way".
    const NO_BUDGET: usize = usize::MAX;

    #[test]
    fn reserves_only_while_below_the_cap() {
        let (cap, in_flight) = atomics(2, 0);
        assert!(try_reserve_stream(&cap, &in_flight, NO_BUDGET));
        assert!(try_reserve_stream(&cap, &in_flight, NO_BUDGET));
        assert!(
            !try_reserve_stream(&cap, &in_flight, NO_BUDGET),
            "cap reached"
        );
        assert_eq!(
            in_flight.load(Ordering::Relaxed),
            2,
            "a refused attempt must roll its reservation back"
        );
    }

    /// The point of the client budget: a connection is full at
    /// `min(peer_cap, budget)`, so a budget below the peer's cap is what makes
    /// the pool dial another connection instead of packing more streams here.
    #[test]
    fn a_client_budget_caps_below_the_peer_cap() {
        let (cap, in_flight) = atomics(250, 0);
        assert!(try_reserve_stream(&cap, &in_flight, 2));
        assert!(try_reserve_stream(&cap, &in_flight, 2));
        assert!(
            !try_reserve_stream(&cap, &in_flight, 2),
            "budget reached although the peer still allows 250"
        );
        assert_eq!(in_flight.load(Ordering::Relaxed), 2);
        // A freed stream makes the slot usable again.
        in_flight.fetch_sub(1, Ordering::AcqRel);
        assert!(try_reserve_stream(&cap, &in_flight, 2));
    }

    /// The peer's cap still governs when it is *lower* than the client budget.
    #[test]
    fn the_peer_cap_still_wins_when_below_the_budget() {
        let (cap, in_flight) = atomics(1, 1);
        assert!(
            !try_reserve_stream(&cap, &in_flight, 32),
            "peer allows 1 in flight; a bigger client budget must not raise it"
        );
    }

    #[test]
    fn a_raised_cap_takes_effect_without_a_new_connection() {
        let (cap, in_flight) = atomics(1, 1);
        assert!(
            !try_reserve_stream(&cap, &in_flight, NO_BUDGET),
            "full at the handshake-time cap"
        );
        // The peer raised MAX_CONCURRENT_STREAMS mid-connection.
        assert!(publish_peer_cap(&cap, 5));
        assert!(
            try_reserve_stream(&cap, &in_flight, NO_BUDGET),
            "the live cap must be used, not the handshake one"
        );
    }

    #[test]
    fn a_lowered_cap_stops_admissions_on_the_same_connection() {
        let (cap, in_flight) = atomics(250, 3);
        assert!(publish_peer_cap(&cap, 3));
        assert!(
            !try_reserve_stream(&cap, &in_flight, NO_BUDGET),
            "3 in flight and the peer now allows only 3: nothing more may be admitted"
        );
    }

    #[test]
    fn sampling_ignores_an_unadvertised_cap_and_reports_only_changes() {
        let cap = AtomicUsize::new(7);
        assert!(
            !publish_peer_cap(&cap, usize::MAX),
            "usize::MAX means no SETTINGS processed yet"
        );
        assert_eq!(
            cap.load(Ordering::Relaxed),
            7,
            "an unadvertised cap must never overwrite a known one"
        );
        assert!(!publish_peer_cap(&cap, 7), "unchanged sample");
        assert!(publish_peer_cap(&cap, 4));
        assert_eq!(cap.load(Ordering::Relaxed), 4);
    }
}
