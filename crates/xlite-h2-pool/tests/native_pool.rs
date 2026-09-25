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

#[tokio::test(flavor = "multi_thread")]
async fn reads_peer_max_concurrent_streams() {
    let (pool, _) = test_pool(4, 3, false);
    let lease = pool.acquire("upstream").await.unwrap();
    assert_eq!(lease.max_concurrent_streams(), 3);
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
        }
    );
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
