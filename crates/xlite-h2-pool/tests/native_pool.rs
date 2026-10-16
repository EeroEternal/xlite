//! Behavioural tests for the native h2 pool, using in-memory duplex streams
//! so no network is involved.
//!
//! Each `dial` opens a fresh duplex pair and spawns an h2 server on the other
//! end advertising a configurable `SETTINGS_MAX_CONCURRENT_STREAMS`, which is
//! exactly the value [`Pool`] must read and respect.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use xlite_h2_pool::{BoxIo, Config, Dial, Pool};

/// Spawn an h2 server on `io` advertising `max_streams`; `respond` controls
/// whether accepted requests are answered immediately or held open.
fn spawn_h2_server(io: tokio::io::DuplexStream, max_streams: u32, respond: bool) {
    tokio::spawn(async move {
        let conn = h2::server::Builder::new()
            .max_concurrent_streams(max_streams)
            .handshake::<_, Bytes>(io)
            .await;
        let mut conn = match conn {
            Ok(c) => c,
            Err(_) => return,
        };
        while let Some(req) = conn.accept().await {
            let (request, mut sender) = match req {
                Ok(pair) => pair,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let _recv = request; // keep the receive stream alive
                if respond {
                    let _ = sender.send_response(
                        http::Response::builder().status(200).body(()).unwrap(),
                        true,
                    );
                } else {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    let _ = sender.send_response(http::Response::new(()), true);
                }
            });
        }
    });
}

use bytes::Bytes;

fn test_pool(max_conns: usize, max_streams: u32, respond: bool) -> (Pool, Arc<AtomicUsize>) {
    test_pool_budgeted(
        max_conns,
        max_streams,
        respond,
        xlite_h2_pool::DEFAULT_STREAM_BUDGET_PER_CONN,
    )
}

/// Like [`test_pool`], but with an explicit client-side per-connection stream
/// budget (the peer still advertises `max_streams`).
fn test_pool_budgeted(
    max_conns: usize,
    max_streams: u32,
    respond: bool,
    stream_budget: usize,
) -> (Pool, Arc<AtomicUsize>) {
    let dials = Arc::new(AtomicUsize::new(0));
    let dial: Dial = Arc::new({
        let dials = dials.clone();
        move |_host: String| {
            let dials = dials.clone();
            Box::pin(async move {
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                spawn_h2_server(server_io, max_streams, respond);
                dials.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(client_io) as BoxIo)
            })
        }
    });
    let pool = Pool::new(Config {
        max_conns_per_host: max_conns,
        stream_budget_per_conn: stream_budget,
        pinned_poll: false,
        dial,
        idle_timeout: xlite_h2_pool::DEFAULT_IDLE_TIMEOUT,
    });
    (pool, dials)
}

/// Like [`test_pool`], but the first `fail_times` dials fail — the shape a
/// transient upstream outage has (peer restarting, syn-backlog overflow,
/// momentary fd pressure). Every later dial succeeds.
fn test_pool_flaky(
    max_conns: usize,
    max_streams: u32,
    fail_times: usize,
) -> (Pool, Arc<AtomicUsize>) {
    let dials = Arc::new(AtomicUsize::new(0));
    let dial: Dial = Arc::new({
        let dials = dials.clone();
        move |_host: String| {
            let dials = dials.clone();
            Box::pin(async move {
                let attempt = dials.fetch_add(1, Ordering::SeqCst);
                if attempt < fail_times {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionRefused,
                        "transient dial failure",
                    ));
                }
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                spawn_h2_server(server_io, max_streams, false);
                Ok(Box::new(client_io) as BoxIo)
            })
        }
    });
    let pool = Pool::new(Config {
        max_conns_per_host: max_conns,
        stream_budget_per_conn: xlite_h2_pool::DEFAULT_STREAM_BUDGET_PER_CONN,
        pinned_poll: false,
        dial,
        idle_timeout: xlite_h2_pool::DEFAULT_IDLE_TIMEOUT,
    });
    (pool, dials)
}

/// Like [`test_pool_flaky`], but every dial fails with the given raw OS error —
/// used to prove that a *persistent* dial failure is not retried. `24` is
/// `EMFILE` on both Linux and macOS.
fn test_pool_failing_raw(raw_os_error: i32) -> (Pool, Arc<AtomicUsize>) {
    let dials = Arc::new(AtomicUsize::new(0));
    let dial: Dial = Arc::new({
        let dials = dials.clone();
        move |_host: String| {
            let dials = dials.clone();
            Box::pin(async move {
                dials.fetch_add(1, Ordering::SeqCst);
                Err(std::io::Error::from_raw_os_error(raw_os_error))
            })
        }
    });
    let pool = Pool::new(Config {
        max_conns_per_host: 4,
        stream_budget_per_conn: xlite_h2_pool::DEFAULT_STREAM_BUDGET_PER_CONN,
        pinned_poll: false,
        dial,
        idle_timeout: xlite_h2_pool::DEFAULT_IDLE_TIMEOUT,
    });
    (pool, dials)
}

/// What `GET /api/pool` serves, and what the field probe reads: every host the
/// pool has dialed, with its live connection and in-flight counts.
#[tokio::test(flavor = "multi_thread")]
async fn all_stats_reports_every_dialed_host() {
    let (pool, _) = test_pool(4, 3, false);
    assert!(
        pool.all_stats().is_empty(),
        "nothing dialed yet: an empty pool must report nothing, not a phantom host"
    );

    let lease = pool.acquire("upstream").await.unwrap();
    let stats = pool.all_stats();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].0, "upstream");
    assert_eq!(stats[0].1.connections, 1);
    assert_eq!(stats[0].1.in_flight, 1, "the lease holds the slot");
    assert_eq!(stats[0].1.dials, 1);

    // The visibility change: the two inputs of the per-host ceiling are reported,
    // so nobody has to compute `min(cap, peer) × max_conns` by hand.
    assert_eq!(
        stats[0].1.peer_cap,
        Some(3),
        "this fixture's peer advertises SETTINGS_MAX_CONCURRENT_STREAMS=3"
    );
    assert_eq!(
        stats[0].1.stream_budget_per_conn,
        xlite_h2_pool::DEFAULT_STREAM_BUDGET_PER_CONN
    );
    assert_eq!(
        stats[0].1.effective_streams_per_conn(),
        Some(3),
        "min(our 250 budget, the peer's 3): the peer wins"
    );

    // A host whose connections are gone (or that was only ever probed) reports no
    // peer value — the ceiling is unknowable until a handshake, and must not be
    // invented.
    let unknown = pool.stats("never-dialed");
    assert_eq!(unknown.peer_cap, None);
    assert_eq!(unknown.effective_streams_per_conn(), None);

    drop(lease);
    assert_eq!(
        pool.all_stats()[0].1.in_flight,
        0,
        "releasing the lease has to be visible, otherwise the probe would \
         conclude the pool is saturated when it is idle"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_peer_max_concurrent_streams() {
    let (pool, _) = test_pool(4, 3, false);
    let lease = pool.acquire("upstream").await.unwrap();
    assert_eq!(lease.max_concurrent_streams(), 3);
}

/// A single transient dial failure must not reach the client as a 502: the
/// bounded retry in `acquire` recovers it.
#[tokio::test(flavor = "multi_thread")]
async fn a_transient_dial_failure_is_retried_instead_of_returned() {
    let (pool, dials) = test_pool_flaky(4, 3, 1);
    let lease = pool
        .acquire("upstream")
        .await
        .expect("one transient dial failure is retried, not surfaced to the client as a 502");
    assert_eq!(lease.max_concurrent_streams(), 3);
    assert_eq!(
        dials.load(Ordering::SeqCst),
        2,
        "the failing attempt plus exactly one retry"
    );
    drop(lease);
}

/// The retry is bounded. A persistent failure (fd exhaustion is one) has to give
/// up and return the error instead of spinning on the failing dial — the spin is
/// exactly what `DIAL_RETRY_BACKOFF` exists to prevent, because `EMFILE` and
/// `ECONNREFUSED` both return without ever yielding.
#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_dial_failure_gives_up_after_bounded_retries() {
    let (pool, dials) = test_pool_flaky(4, 3, usize::MAX);
    let started = std::time::Instant::now();
    let err = match pool.acquire("upstream").await {
        Ok(_) => panic!("a persistent dial failure has to come back as an error"),
        Err(err) => err,
    };
    assert!(
        matches!(err, xlite_h2_pool::PoolError::Dial(_)),
        "the caller has to receive the dial error unchanged: {err:?}"
    );
    assert_eq!(
        dials.load(Ordering::SeqCst),
        3,
        "one attempt plus DIAL_RETRIES (2), then stop"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(20),
        "the backoff has to actually happen: it is what prevents the hot spin"
    );
    assert_eq!(pool.connection_count("upstream"), 0);
}

/// Resource exhaustion is persistent, not transient: `EMFILE` must not be
/// retried. Retrying it would triple the dial volume in the issue #49 scenario
/// (22.78M → ~68M) while still returning the same 502 to every client.
#[tokio::test(flavor = "multi_thread")]
async fn a_resource_exhaustion_dial_error_is_not_retried() {
    const EMFILE: i32 = 24; // same value on Linux and macOS
    let (pool, dials) = test_pool_failing_raw(EMFILE);
    let err = match pool.acquire("upstream").await {
        Ok(_) => panic!("a persistent dial failure has to come back as an error"),
        Err(err) => err,
    };
    assert!(
        matches!(err, xlite_h2_pool::PoolError::Dial(_)),
        "the caller has to receive the dial error unchanged: {err:?}"
    );
    assert_eq!(
        dials.load(Ordering::SeqCst),
        1,
        "fd exhaustion must not be retried: one attempt, then surface it"
    );
    assert_eq!(pool.connection_count("upstream"), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn dials_new_connection_when_full() {
    let (pool, dials) = test_pool(4, 3, false);
    let mut leases = Vec::new();
    for _ in 0..4 {
        leases.push(pool.acquire("upstream").await.unwrap());
    }
    // 3 streams fit on the first connection, the 4th forces a second dial.
    assert_eq!(dials.load(Ordering::SeqCst), 2);
    drop(leases);
}

/// A client-side stream budget below the peer's advertised cap must make the
/// pool dial another connection instead of packing a second stream onto the
/// first — the mechanism that spreads a connection's state lock across cores.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_stream_budget_dials_more_connections() {
    // Peer advertises 100 streams; a budget of 1 makes every stream its own
    // connection.
    let (pool, dials) = test_pool_budgeted(4, 100, false, 1);
    let first = pool.acquire("upstream").await.unwrap();
    assert_eq!(pool.connection_count("upstream"), 1);
    let second = pool.acquire("upstream").await.unwrap();
    assert_eq!(
        pool.connection_count("upstream"),
        2,
        "budget 1 must dial a second connection instead of sharing the first"
    );
    assert_eq!(dials.load(Ordering::SeqCst), 2);
    drop((first, second));

    // Negative control: without a budget the same peer cap keeps both on one.
    let (unbudgeted, unbudgeted_dials) = test_pool_budgeted(4, 100, false, 100);
    let a = unbudgeted.acquire("upstream").await.unwrap();
    let b = unbudgeted.acquire("upstream").await.unwrap();
    assert_eq!(unbudgeted.connection_count("upstream"), 1);
    assert_eq!(unbudgeted_dials.load(Ordering::SeqCst), 1);
    drop((a, b));
}

/// A server that answers each request with `chunks` data frames, then ends.
fn spawn_chunk_server(io: tokio::io::DuplexStream, chunks: usize) {
    tokio::spawn(async move {
        let mut conn = match h2::server::Builder::new().handshake::<_, Bytes>(io).await {
            Ok(c) => c,
            Err(_) => return,
        };
        while let Some(req) = conn.accept().await {
            let (request, mut respond) = match req {
                Ok(p) => p,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let _recv = request;
                let resp = http::Response::builder().status(200).body(()).unwrap();
                let mut send = match respond.send_response(resp, false) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                for _ in 0..chunks {
                    if send.send_data(Bytes::from_static(b"hello"), false).is_err() {
                        return;
                    }
                }
                let _ = send.send_data(Bytes::new(), true);
            });
        }
    });
}

fn pinned_pool() -> Pool {
    let dial: Dial = Arc::new(move |_host: String| {
        Box::pin(async move {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            spawn_chunk_server(server_io, 8);
            Ok(Box::new(client_io) as BoxIo)
        })
    });
    Pool::new(Config {
        max_conns_per_host: 1,
        stream_budget_per_conn: xlite_h2_pool::DEFAULT_STREAM_BUDGET_PER_CONN,
        pinned_poll: true,
        dial,
        idle_timeout: xlite_h2_pool::DEFAULT_IDLE_TIMEOUT,
    })
}

/// `pinned_poll` must expose a pump, and that pump must actually carry the
/// stream — a connection whose body is polled by the pump, not by the caller.
#[tokio::test(flavor = "multi_thread")]
async fn pinned_poll_forwards_streams_through_the_connection_task() {
    let pool = pinned_pool();
    let mut lease = pool.acquire("upstream").await.unwrap();
    assert!(
        lease.pinned_poll(),
        "pinned_poll must route streams through the connection task"
    );

    let request = http::Request::builder()
        .method("GET")
        .uri("http://upstream/")
        .body(())
        .unwrap();
    let (response, send_stream) = lease.send_request(request, true).unwrap();
    drop(send_stream);
    let recv = response.await.unwrap().into_body();

    let mut rx = lease.register(recv).await;
    let mut total = 0usize;
    while let Some(chunk) = rx.recv().await {
        total += chunk.expect("pump bytes").len();
    }
    assert_eq!(
        total,
        8 * 5,
        "the pump must forward every byte before ending"
    );
}

/// The default keeps direct polling: no pump, so callers poll the `RecvStream`.
#[tokio::test(flavor = "multi_thread")]
async fn default_pool_has_no_pump() {
    let (pool, _) = test_pool(1, 10, false);
    let lease = pool.acquire("upstream").await.unwrap();
    assert!(!lease.pinned_poll());
}

#[tokio::test(flavor = "multi_thread")]
async fn caps_at_max_conns_and_waits() {
    let (pool, dials) = test_pool(2, 3, false);
    let mut leases = Vec::new();
    for _ in 0..6 {
        leases.push(pool.acquire("upstream").await.unwrap());
    }
    assert_eq!(dials.load(Ordering::SeqCst), 2);

    // The 7th acquire must block: both connections are full and at the cap.
    let blocked = tokio::time::timeout(Duration::from_millis(150), pool.acquire("upstream")).await;
    assert!(
        blocked.is_err(),
        "acquire should block once the cap is reached"
    );

    // Freeing one slot unblocks it on an existing connection (no new dial).
    drop(leases.pop());
    let seventh = tokio::time::timeout(Duration::from_secs(5), pool.acquire("upstream"))
        .await
        .expect("acquire should complete after a slot frees")
        .unwrap();
    assert_eq!(dials.load(Ordering::SeqCst), 2);
    drop(seventh);
    drop(leases);
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_and_receives_a_response() {
    let (pool, _) = test_pool(2, 3, true);
    let mut lease = pool.acquire("upstream").await.unwrap();

    let request = http::Request::builder()
        .method("GET")
        .uri("http://upstream/")
        .body(())
        .unwrap();
    let (response_future, send_stream) = lease.send_request(request, true).unwrap();
    drop(send_stream); // end_of_stream already sent

    let response = response_future.await.unwrap();
    assert_eq!(response.status(), 200);
}

/// A concurrent burst must open a *single* connection (then share it), not
/// race the pool up to `max_conns_per_host`.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_on_an_empty_pool_opens_one_connection() {
    // cap 100, so one connection can absorb the whole burst of 20.
    let (pool, dials) = test_pool(8, 100, false);
    let pool = Arc::new(pool);

    let mut handles = Vec::new();
    for _ in 0..20 {
        let pool = Arc::clone(&pool);
        handles.push(tokio::spawn(async move {
            pool.acquire("upstream").await.unwrap()
        }));
    }
    let mut leases = Vec::new();
    for handle in handles {
        leases.push(handle.await.unwrap());
    }
    assert_eq!(
        dials.load(Ordering::SeqCst),
        1,
        "a burst must share one dial, not open one connection per request"
    );
    drop(leases);
}

/// Dropping the pool must close its connections — otherwise an abandoned pool
/// (e.g. a rebuilt `ProviderWire`) leaks its sockets forever, because the h2
/// connection only closes once every `SendRequest` handle is dropped.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_pool_closes_its_connections() {
    let dials = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicUsize::new(0));
    let dial: Dial = Arc::new({
        let dials = dials.clone();
        let closed = closed.clone();
        move |_host: String| {
            let dials = dials.clone();
            let closed = closed.clone();
            Box::pin(async move {
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                tokio::spawn(async move {
                    if let Ok(mut conn) = h2::server::handshake(server_io).await {
                        // Runs until the client closes the connection.
                        while let Some(request) = conn.accept().await {
                            if request.is_err() {
                                break;
                            }
                        }
                    }
                    closed.fetch_add(1, Ordering::SeqCst);
                });
                dials.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(client_io) as BoxIo)
            })
        }
    });
    let pool = Pool::new(Config {
        max_conns_per_host: 4,
        stream_budget_per_conn: xlite_h2_pool::DEFAULT_STREAM_BUDGET_PER_CONN,
        pinned_poll: false,
        dial,
        idle_timeout: xlite_h2_pool::DEFAULT_IDLE_TIMEOUT,
    });
    let lease = pool.acquire("upstream").await.unwrap();

    // Drop the lease and the pool: the connection must close.
    drop(lease);
    drop(pool);
    for _ in 0..100 {
        if closed.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        closed.load(Ordering::SeqCst),
        1,
        "dropping the pool must close its upstream connection"
    );
}

/// Concurrent acquire/release churn must not deadlock: a slot freed while no
/// waiter is subscribed (the `notify_waiters` lost-wakeup race) must still be
/// picked up. With a bare `Notify` + `notify_waiters`, this stress test hangs
/// occasionally; the generation-counter fix makes it deterministic.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_acquire_release_does_not_deadlock() {
    // 4 connections × cap 2 = 8 slots, so 32 tasks churn against a bound.
    let (pool, _dials) = test_pool(4, 2, false);
    let pool = Arc::new(pool);

    let mut handles = Vec::new();
    for _ in 0..32 {
        let pool = Arc::clone(&pool);
        handles.push(tokio::spawn(async move {
            for _ in 0..50 {
                let lease = pool.acquire("upstream").await.unwrap();
                tokio::task::yield_now().await;
                drop(lease);
            }
        }));
    }
    for handle in handles {
        tokio::time::timeout(Duration::from_secs(15), handle)
            .await
            .expect("task must not deadlock")
            .unwrap();
    }
}

/// Idle connections must be reclaimed down to one per host, so a long-lived
/// pool does not keep every connection it ever dialed.
#[tokio::test(flavor = "multi_thread")]
async fn idle_connections_are_reclaimed_to_one() {
    // cap 1 → a burst of 5 concurrent leases opens ~5 connections (max 8).
    let dials = Arc::new(AtomicUsize::new(0));
    let dial: Dial = Arc::new({
        let dials = dials.clone();
        move |_host: String| {
            let dials = dials.clone();
            Box::pin(async move {
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                spawn_h2_server(server_io, 1, false);
                dials.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(client_io) as BoxIo)
            })
        }
    });
    let pool = Pool::new(Config {
        max_conns_per_host: 8,
        stream_budget_per_conn: xlite_h2_pool::DEFAULT_STREAM_BUDGET_PER_CONN,
        pinned_poll: false,
        dial,
        idle_timeout: Duration::from_millis(200),
    });

    let mut leases = Vec::new();
    for _ in 0..5 {
        leases.push(pool.acquire("upstream").await.unwrap());
    }
    assert_eq!(
        pool.connection_count("upstream"),
        5,
        "burst opened 5 connections"
    );
    drop(leases);

    // The reaper wakes every idle_timeout / 2 (= 100ms); poll a few cycles.
    let mut count = 5;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        count = pool.connection_count("upstream");
        if count == 1 {
            break;
        }
    }
    assert_eq!(count, 1, "idle connections must be reclaimed down to one");
}

/// The pool must advertise a receive window far larger than the spec's 64 KiB:
/// the connection window is shared by every multiplexed stream, so the spec
/// default throttles a pool that packs many streams onto one connection.
///
/// Proof: the server pushes 256 KiB on one stream and the client reads it
/// **without ever releasing capacity**. With a 64 KiB window the peer would
/// stall after 64 KiB; with the pool's window the full body arrives.
#[tokio::test(flavor = "multi_thread")]
async fn recv_window_exceeds_the_spec_default() {
    use futures_util::StreamExt;

    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        let conn = h2::server::Builder::new()
            .handshake::<_, bytes::Bytes>(server_io)
            .await;
        let mut conn = match conn {
            Ok(c) => c,
            Err(_) => return,
        };
        while let Some(req) = conn.accept().await {
            let (request, mut respond) = match req {
                Ok(pair) => pair,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let _recv = request;
                let response = http::Response::builder().status(200).body(()).unwrap();
                let mut send = match respond.send_response(response, false) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let body = vec![b'x'; 256 * 1024];
                let _ = send.send_data(bytes::Bytes::from(body), true);
            });
        }
    });

    let (mut send, conn) = xlite_h2_pool::client_builder()
        .handshake::<_, bytes::Bytes>(client_io)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let request = http::Request::builder()
        .method("GET")
        .uri("http://upstream/")
        .body(())
        .unwrap();
    let (response_future, send_stream) = send.send_request(request, true).unwrap();
    drop(send_stream);

    // Deliberately never call `release_capacity`: the received byte count is
    // bounded purely by the window the client advertised.
    let mut recv = response_future.await.unwrap().into_body();
    // A regression to the 64 KiB spec window stalls the peer instead of failing
    // fast, so bound the read rather than hanging the suite.
    let total = tokio::time::timeout(Duration::from_secs(10), async {
        let mut total = 0usize;
        while let Some(chunk) = recv.next().await {
            match chunk {
                Ok(bytes) => {
                    total += bytes.len();
                    if total >= 256 * 1024 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        total
    })
    .await
    .unwrap_or(0);
    assert!(
        total > 64 * 1024,
        "received only {total} bytes without releasing; the recv window is still ~64 KiB"
    );
}

/// `stats()` must report live connections/in-flight and lifetime counters.
#[tokio::test(flavor = "multi_thread")]
async fn stats_track_connections_in_flight_and_dials() {
    use xlite_h2_pool::PoolStats;

    let (pool, dials) = test_pool(4, 3, false);
    let lease = pool.acquire("upstream").await.unwrap();

    let stats = pool.stats("upstream");
    assert_eq!(stats.connections, 1);
    assert_eq!(stats.in_flight, 1);
    assert_eq!(stats.dials, 1);
    assert_eq!(stats.reclaimed, 0);
    assert_eq!(stats.settings_fallbacks, 0);
    assert_eq!(stats.cap_updates, 0);
    assert_eq!(dials.load(Ordering::SeqCst), 1);

    drop(lease);
    assert_eq!(pool.stats("upstream").in_flight, 0);

    // Unknown host: zeroed snapshot, no side effects.
    assert_eq!(
        pool.stats("never-dialled"),
        PoolStats {
            connections: 0,
            in_flight: 0,
            dials: 0,
            reclaimed: 0,
            settings_fallbacks: 0,
            cap_updates: 0,
            stream_budget_per_conn: xlite_h2_pool::DEFAULT_STREAM_BUDGET_PER_CONN,
            peer_cap: None,
            waiting: 0,
            waiting_max: 0,
            queued_total: 0,
            wait_millis: 0,
            wait_millis_max: 0,
            wait_buckets: [0; xlite_h2_pool::WAIT_BUCKETS],
        }
    );
}

/// The queue is where a saturated pool's latency goes, and it used to be invisible:
/// `in_flight` says the pool is full, but nothing said how many requests were parked
/// behind it or how long they waited.
#[tokio::test(flavor = "multi_thread")]
async fn a_saturated_pool_reports_its_queue_depth_and_wait() {
    // One connection carrying exactly one stream: the second acquirer must park.
    let (pool, _dials) = test_pool(1, 1, false);
    let pool = Arc::new(pool);

    let first = pool.acquire("upstream").await.unwrap();
    let before = pool.stats("upstream");
    assert_eq!(before.waiting, 0, "nothing parked yet");
    assert_eq!(before.queued_total, 0);

    let waiter = {
        let pool = Arc::clone(&pool);
        tokio::spawn(async move { pool.acquire("upstream").await.unwrap() })
    };

    // The acquirer has to be *parked* before the release, otherwise this test proves
    // nothing about the counters.
    for _ in 0..200 {
        if pool.stats("upstream").waiting == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let parked = pool.stats("upstream");
    assert_eq!(
        parked.waiting, 1,
        "a parked acquirer has to show up in the queue depth"
    );
    assert!(parked.waiting_max >= 1, "high-water mark has to see it");

    // Hold it a measurable while so the wait timer has something to record.
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(first);

    let second = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("a released slot must wake the parked acquirer")
        .unwrap();

    let after = pool.stats("upstream");
    assert_eq!(after.waiting, 0, "the queue has to drain");
    assert_eq!(after.queued_total, 1, "exactly one acquire had to park");
    assert_eq!(
        after.wait_buckets.iter().sum::<u64>(),
        1,
        "the parked acquire lands in exactly one bucket"
    );
    assert!(
        after.wait_millis_max >= 10,
        "the wait was ~20ms, so the worst wait has to reflect it (got {})",
        after.wait_millis_max
    );
    drop(second);
}

/// A deliberately raw h2 peer that changes its stream cap mid-connection.
///
/// h2's *server* API offers no way to re-advertise
/// `SETTINGS_MAX_CONCURRENT_STREAMS` on a live connection — but the protocol
/// allows a peer to do exactly that at any time, so the frames are written by
/// hand here. This is the only way to exercise the pool's live-cap handling.
fn spawn_settings_changing_peer(
    io: tokio::io::DuplexStream,
    first: u32,
    then: u32,
    mut raise: tokio::sync::watch::Receiver<bool>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    tokio::spawn(async move {
        let (mut reader, mut writer) = tokio::io::split(io);
        // Drain whatever the client sends (its preface, SETTINGS, ACKs) so the
        // duplex buffer can never fill up and stall it.
        tokio::spawn(async move {
            let mut sink = [0u8; 4096];
            while let Ok(read) = reader.read(&mut sink).await {
                if read == 0 {
                    break;
                }
            }
        });

        let _ = writer.write_all(&settings_frame(first)).await;
        let _ = writer.flush().await;

        // Wait for the test to ask for the second frame instead of racing a wall
        // clock: with a fixed `sleep` here, a slow machine can apply the new cap
        // before the test has even asserted the handshake cap, which is exactly
        // how this test used to flake. `wait_for` tests the current value first,
        // so a signal that arrived before this point still counts.
        if raise.wait_for(|raised| *raised).await.is_err() {
            return; // the test dropped the sender; nothing left to serve
        }

        let _ = writer.write_all(&settings_frame(then)).await;
        let _ = writer.flush().await;

        // Keep the connection open so the pool keeps holding this entry.
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
}

/// One SETTINGS frame carrying a single `MAX_CONCURRENT_STREAMS` (id 0x3) entry.
fn settings_frame(max_streams: u32) -> [u8; 15] {
    let mut frame = [0u8; 15];
    frame[..3].copy_from_slice(&6u32.to_be_bytes()[1..]); // payload length
    frame[3] = 0x4; // frame type: SETTINGS
    frame[9..11].copy_from_slice(&0x0003u16.to_be_bytes()); // MAX_CONCURRENT_STREAMS
    frame[11..15].copy_from_slice(&max_streams.to_be_bytes());
    frame
}

/// Returns the pool, the dial counter, and the sender the test uses to release
/// each peer's *second* SETTINGS frame (see [`spawn_settings_changing_peer`]).
fn settings_changing_pool(
    first: u32,
    then: u32,
    max_conns: usize,
) -> (Pool, Arc<AtomicUsize>, tokio::sync::watch::Sender<bool>) {
    let dials = Arc::new(AtomicUsize::new(0));
    let (raise_tx, raise_rx) = tokio::sync::watch::channel(false);
    let dial: Dial = Arc::new({
        let dials = dials.clone();
        move |_host: String| {
            let dials = dials.clone();
            let raise_rx = raise_rx.clone();
            Box::pin(async move {
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                spawn_settings_changing_peer(server_io, first, then, raise_rx);
                dials.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(client_io) as BoxIo)
            })
        }
    });
    let pool = Pool::new(Config {
        max_conns_per_host: max_conns,
        stream_budget_per_conn: xlite_h2_pool::DEFAULT_STREAM_BUDGET_PER_CONN,
        pinned_poll: false,
        dial,
        idle_timeout: xlite_h2_pool::DEFAULT_IDLE_TIMEOUT,
    });
    (pool, dials, raise_tx)
}

/// A peer that lowers `SETTINGS_MAX_CONCURRENT_STREAMS` mid-connection must be
/// obeyed *live*: the cap the pool gates on drops, and the next request past it
/// opens a new connection instead of queueing inside the now-full one — the
/// exact pathology this pool exists to avoid.
#[tokio::test(flavor = "multi_thread")]
async fn a_mid_connection_settings_change_is_applied_live() {
    let (pool, dials, raise) = settings_changing_pool(3, 2, 4);

    let first = pool.acquire("upstream").await.unwrap();
    assert_eq!(first.max_concurrent_streams(), 3, "handshake cap");
    assert_eq!(dials.load(Ordering::SeqCst), 1);

    // Only now tell the peer to lower the cap; it waits for this signal instead
    // of a timer, so the handshake-cap assertions above cannot race it.
    raise.send_replace(true);

    // The peer's second SETTINGS frame is processed on a poll of the
    // connection, which the driver task does not need a timer for.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while first.max_concurrent_streams() != 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the lowered cap was never picked up"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        pool.stats("upstream").cap_updates,
        1,
        "exactly one cap change was observed"
    );

    // Two streams now fill this connection...
    let _second = pool.acquire("upstream").await.unwrap();
    assert_eq!(dials.load(Ordering::SeqCst), 1, "still room for one more");

    // ...so the third must dial instead of queueing beyond the new cap.
    let _third = pool.acquire("upstream").await.unwrap();
    assert_eq!(
        dials.load(Ordering::SeqCst),
        2,
        "past the *live* cap the pool dials, it does not queue inside"
    );
    assert_eq!(pool.connection_count("upstream"), 2);
}

/// A raised cap is used on the same connection: a peer that lifts its limit must
/// not force needless dials.
#[tokio::test(flavor = "multi_thread")]
async fn a_raised_settings_cap_is_used_without_dialling_again() {
    let (pool, dials, raise) = settings_changing_pool(1, 4, 4);

    let first = pool.acquire("upstream").await.unwrap();
    assert_eq!(first.max_concurrent_streams(), 1);
    // At the handshake cap the second request opens its own connection.
    let second = pool.acquire("upstream").await.unwrap();
    assert_eq!(dials.load(Ordering::SeqCst), 2);
    assert_eq!(second.max_concurrent_streams(), 1);

    // Both connections are told the cap is now 4; the pool should notice and
    // stop dialling for what those connections can already carry. The raise is
    // triggered here, *after* both handshake caps were observed, rather than by
    // a timer inside the peer: a slow machine must not be able to apply the new
    // cap before the assertions above have run.
    raise.send_replace(true);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while first.max_concurrent_streams() != 4 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the raised cap was never picked up"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(second);
    let _third = pool.acquire("upstream").await.unwrap();
    assert_eq!(
        dials.load(Ordering::SeqCst),
        2,
        "the raised cap must absorb the request on an existing connection"
    );
}

// ---------------------------------------------------------------------------
// Cancellation propagation regression tests.
//
// The bug these guard: `pump_stream` waited on `recv.next()` and only noticed a
// departed downstream at the *next* `out.send` failure — so when the upstream
// went silent, the cancel never reached it and the stream stayed open. Every
// wait below is bounded; none may rely on the upstream sending again to wake
// anything up.
// ---------------------------------------------------------------------------

/// The pump's per-stream body channel cap (`pump.rs` `BODY_CHANNEL`). A test
/// that claims "the channel was full" checks against this.
const BODY_CHANNEL_CAP: usize = 16;

/// What the fake upstream observed: how many data chunks it managed to send,
/// and — where it waits for one — the peer's reset reason.
struct Seen {
    label: &'static str,
    sent: usize,
    reset: Result<h2::Reason, String>,
}

/// Per-request behaviour by request path:
///   * `/silent` — headers + one chunk, then silence; waits for the peer's
///     reset and reports its reason.
///   * `/flood` — headers + 64 chunks, no end of stream, then waits for the
///     peer's reset: with a cap-16 body channel and no reads, this guarantees
///     the pump is parked in `out.send` (saturated) when the cancel lands.
///   * `/drip` — headers + 8 chunks spaced 20ms apart + end of stream.
///   * `/reset` — headers + one chunk, then an explicit `RST_STREAM`.
///   * `/tail` — headers + one 8191-byte chunk + end of stream (below the
///     8 KiB release threshold, so its credit returns only at stream end).
///   * anything else — headers + 8 chunks + end of stream.
fn spawn_cancel_server(io: tokio::io::DuplexStream, seen: tokio::sync::mpsc::Sender<Seen>) {
    tokio::spawn(async move {
        let mut conn = match h2::server::Builder::new().handshake::<_, Bytes>(io).await {
            Ok(c) => c,
            Err(_) => return,
        };
        while let Some(req) = conn.accept().await {
            let (request, mut respond) = match req {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let path = request.uri().path().to_string();
            let seen = seen.clone();
            tokio::spawn(async move {
                let _recv = request;
                let resp = http::Response::builder().status(200).body(()).unwrap();
                let mut send = match respond.send_response(resp, false) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let mut sent = 0usize;
                let send_chunk =
                    |bytes: &'static [u8], end: bool, send: &mut h2::SendStream<Bytes>| {
                        send.send_data(Bytes::from_static(bytes), end).is_ok()
                    };
                match path.as_str() {
                    "/silent" => {
                        if !send_chunk(b"hello", false, &mut send) {
                            return;
                        }
                        sent = 1;
                        // Silent from here: the cancel must arrive without this
                        // side ever sending again.
                        let reset = std::future::poll_fn(|cx| send.poll_reset(cx)).await;
                        let _ = seen
                            .send(Seen {
                                label: "silent",
                                sent,
                                reset: reset.map_err(|e| e.to_string()),
                            })
                            .await;
                    }
                    "/flood" => {
                        for _ in 0..64 {
                            if !send_chunk(b"x", false, &mut send) {
                                return;
                            }
                            sent += 1;
                        }
                        let reset = std::future::poll_fn(|cx| send.poll_reset(cx)).await;
                        let _ = seen
                            .send(Seen {
                                label: "flood",
                                sent,
                                reset: reset.map_err(|e| e.to_string()),
                            })
                            .await;
                    }
                    "/drip" => {
                        for _ in 0..8 {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            if !send_chunk(b"hello", false, &mut send) {
                                return;
                            }
                        }
                        let _ = send.send_data(Bytes::new(), true);
                    }
                    "/reset" => {
                        let _ = send.send_data(Bytes::from_static(b"hello"), false);
                        // Let headers + data land first: this case is about the
                        // pump ending on a reset, not winning a race.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        send.send_reset(h2::Reason::NO_ERROR);
                    }
                    "/tail" => {
                        let _ = send.send_data(Bytes::from_static(&[b'y'; 8191]), true);
                    }
                    "/big" => {
                        // 3 MiB beats the 2 MiB per-stream window: it can only
                        // arrive if credit is released past the 8 KiB threshold.
                        // Ends with a 500-byte tail below the threshold.
                        let block = Bytes::from_static(&[b'y'; 65536]);
                        let mut sent = 0usize;
                        while sent < 3 * 1024 * 1024 {
                            if send.send_data(block.clone(), false).is_err() {
                                return;
                            }
                            sent += block.len();
                        }
                        let _ = send.send_data(Bytes::from_static(&[b'z'; 500]), true);
                    }
                    _ => {
                        for _ in 0..8 {
                            if !send_chunk(b"hello", false, &mut send) {
                                return;
                            }
                        }
                        let _ = send.send_data(Bytes::new(), true);
                    }
                }
            });
        }
    });
}

/// A pinned-poll pool whose dials serve [`spawn_cancel_server`].
fn cancel_pool(seen: tokio::sync::mpsc::Sender<Seen>, max_conns: usize) -> Pool {
    let dial: Dial = Arc::new(move |_host: String| {
        let seen = seen.clone();
        Box::pin(async move {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            spawn_cancel_server(server_io, seen.clone());
            Ok(Box::new(client_io) as BoxIo)
        })
    });
    Pool::new(Config {
        max_conns_per_host: max_conns,
        stream_budget_per_conn: xlite_h2_pool::DEFAULT_STREAM_BUDGET_PER_CONN,
        pinned_poll: true,
        dial,
        idle_timeout: xlite_h2_pool::DEFAULT_IDLE_TIMEOUT,
    })
}

fn chat_request(path: &str) -> http::Request<()> {
    http::Request::builder()
        .method("GET")
        .uri(format!("http://upstream{path}"))
        .body(())
        .unwrap()
}

/// Register `path` on `lease` and return its pump receiver.
async fn open_stream(
    lease: &mut xlite_h2_pool::Lease,
    path: &str,
) -> tokio::sync::mpsc::Receiver<Result<Bytes, String>> {
    let (response, send_stream) = lease.send_request(chat_request(path), true).unwrap();
    drop(send_stream);
    let recv = response.await.unwrap().into_body();
    lease.register(recv).await
}

/// The P1 regression: a silent upstream must see the downstream's cancel
/// promptly — with the *reason* the peer actually sent — and without the
/// upstream being forced to send another chunk. Before the fix the pump parked
/// in `recv.next()` and the reset only left after the upstream spoke again.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_reaches_a_silent_upstream_without_another_chunk() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let pool = cancel_pool(tx, 2);
    let mut lease = pool.acquire("upstream").await.unwrap();

    let mut body = open_stream(&mut lease, "/silent").await;
    let first = tokio::time::timeout(Duration::from_secs(2), body.recv())
        .await
        .expect("first chunk must arrive")
        .expect("pump channel open")
        .expect("pump bytes");
    assert_eq!(&first[..], b"hello");

    // The downstream walks away while the upstream is silent.
    drop(body);
    drop(lease);

    let seen = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("cancel must reach a silent upstream inside the bound")
        .expect("server task alive");
    assert_eq!(seen.label, "silent");
    assert_eq!(seen.sent, 1, "the upstream must not have sent again");
    assert_eq!(
        seen.reset,
        Ok(h2::Reason::CANCEL),
        "the peer reset must carry CANCEL (dropped RecvStream), not a \
         connection error dressed up as a cancel"
    );
}

/// A cancel while the pump's body channel is saturated must still reach the
/// upstream: the test fills the channel and *waits until it is observably
/// full* before dropping, so the saturated path is the one being exercised.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_with_a_full_body_channel_reaches_the_upstream() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let pool = cancel_pool(tx, 1);
    let mut lease = pool.acquire("upstream").await.unwrap();

    let body = open_stream(&mut lease, "/flood").await;

    // Saturation sync: the pump hands chunks to a cap-16 channel; reading
    // nothing, the channel must fill completely before the cancel.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if body.len() >= BODY_CHANNEL_CAP {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "body channel never filled: the saturated path is not being tested"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(body);
    drop(lease);

    let seen = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("cancel must reach the upstream while its channel is full")
        .expect("server task alive");
    assert_eq!(seen.label, "flood");
    assert_eq!(
        seen.sent, 64,
        "the upstream must have flooded before the cancel"
    );
    assert_eq!(seen.reset, Ok(h2::Reason::CANCEL));

    // The slot is given back locally even though the peer has not acked.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let stats = pool.all_stats();
        if stats.iter().all(|(_, s)| s.in_flight == 0) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "in_flight never drained"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Cancelling one stream must not disturb a *concurrently active* sibling on
/// the same connection: the sibling is mid-drip when the cancel lands and must
/// still complete with every byte.
#[tokio::test(flavor = "multi_thread")]
async fn canceling_one_stream_leaves_its_sibling_intact() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    // One connection for both streams, so they share the driver/pump task.
    let pool = cancel_pool(tx, 1);

    // Both streams are live on the same connection before anything is cancelled.
    let mut noisy = pool.acquire("upstream").await.unwrap();
    let mut noisy_body = open_stream(&mut noisy, "/silent").await;
    let mut quiet = pool.acquire("upstream").await.unwrap();
    let mut quiet_body = open_stream(&mut quiet, "/drip").await;

    let first = tokio::time::timeout(Duration::from_secs(2), noisy_body.recv())
        .await
        .expect("first chunk must arrive")
        .expect("pump channel open")
        .expect("pump bytes");
    assert_eq!(&first[..], b"hello");

    // Cancel the silent stream while the sibling is still dripping.
    drop(noisy_body);
    drop(noisy);

    let mut total = 0usize;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(5), quiet_body.recv())
        .await
        .expect("the sibling must keep flowing after its neighbour is cancelled")
    {
        total += chunk.expect("pump bytes").len();
    }
    assert_eq!(total, 8 * 5, "the sibling must receive every byte");
    drop(quiet);
    drop(quiet_body);

    let seen = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("the cancelled stream's reset must reach the upstream")
        .expect("server task alive");
    assert_eq!(seen.label, "silent");
    assert_eq!(seen.reset, Ok(h2::Reason::CANCEL));
}

/// Clean EOF and an upstream-side reset both terminate the pump: the receiver
/// sees the bytes, then an end (or a pump error) — never a hang.
#[tokio::test(flavor = "multi_thread")]
async fn clean_eof_and_upstream_reset_both_terminate_the_pump() {
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let pool = cancel_pool(tx, 1);

    // Clean end of stream: 8 chunks, then the channel closes.
    let mut lease = pool.acquire("upstream").await.unwrap();
    let mut body = open_stream(&mut lease, "/ok").await;
    let mut total = 0usize;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(5), body.recv())
        .await
        .expect("a clean EOF must end the pump")
    {
        total += chunk.expect("pump bytes").len();
    }
    assert_eq!(total, 8 * 5);
    drop(lease);

    // Upstream-side reset: bytes then an error, then the channel closes.
    let mut lease = pool.acquire("upstream").await.unwrap();
    let mut body = open_stream(&mut lease, "/reset").await;
    let mut saw_error = false;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(5), body.recv())
        .await
        .expect("an upstream reset must end the pump")
    {
        if chunk.is_err() {
            saw_error = true;
        }
    }
    assert!(saw_error, "an upstream reset must surface as a pump error");
    drop(lease);
}

/// Flow-control credit accounting on one connection (5 MiB conn window,
/// 2 MiB stream window, 8 KiB release threshold). What is actually proven:
///   * a 3 MiB body only completes if credit is released **across the
///     threshold** mid-stream — deleting the mid-stream release turns this
///     test red (verified: NEG-A);
///   * 700 bodies of 8191 B (below the threshold) plus a 3 MiB body all
///     complete: large transfers and sequential requests cycle the windows
///     indefinitely.
/// What is NOT proven here: the *tail* release (`pending > 0` at stream end)
/// is not isolated — h2 auto-releases a closed stream's connection credit
/// (proto/streams/streams.rs `drop_stream_ref` → `release_closed_capacity`),
/// so deleting the tail release leaves this test green (verified: NEG-B).
/// The tail release is belt-and-braces (returns credit before the refs drop),
/// not load-bearing.
#[tokio::test(flavor = "multi_thread")]
async fn flow_control_credit_is_released_in_full() {
    let (tx, _rx) = tokio::sync::mpsc::channel::<Seen>(4);
    let pool = cancel_pool(tx, 1);

    // Cross-threshold releases: 3 MiB > 2 MiB stream window.
    let mut lease = pool.acquire("upstream").await.unwrap();
    let mut body = open_stream(&mut lease, "/big").await;
    let mut total = 0usize;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(10), body.recv())
        .await
        .expect("credit must be released across the threshold or this stalls at the window")
    {
        total += chunk.expect("pump bytes").len();
    }
    assert_eq!(total, 3 * 1024 * 1024 + 500);
    drop(lease);

    // Below-threshold bodies: 700 × 8191 B = 5.6 MiB > 5 MiB connection
    // window. Every request recycles both windows through pump + h2's
    // stream-close release; this must stay healthy for the long run.
    for i in 0..700 {
        let mut lease = pool.acquire("upstream").await.unwrap();
        let mut body = open_stream(&mut lease, "/tail").await;
        let mut total = 0usize;
        while let Some(chunk) = tokio::time::timeout(Duration::from_secs(5), body.recv())
            .await
            .unwrap_or_else(|_| panic!("stream {i}: tail credit leaked, connection window ran dry"))
        {
            total += chunk.expect("pump bytes").len();
        }
        assert_eq!(total, 8191, "stream {i} lost bytes");
        drop(lease);
    }

    // The connection is still healthy after 5.6 MiB of window cycling.
    let mut lease = pool.acquire("upstream").await.unwrap();
    let mut body = open_stream(&mut lease, "/ok").await;
    let mut total = 0usize;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(5), body.recv())
        .await
        .expect("follow-up stream must complete")
    {
        total += chunk.expect("pump bytes").len();
    }
    assert_eq!(total, 8 * 5);
}
