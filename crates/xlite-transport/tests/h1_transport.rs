//! The h1 backend against a scripted raw HTTP/1.1 server on localhost TCP
//! (`H1Stream` owns real `TcpStream`s, so unlike the h2 tests these cannot
//! run on in-memory duplex pairs).
//!
//! Pins the issue #96 acceptance criteria for HTTP/1.1: one `execute()` = one
//! upstream request, the stale-keep-alive race is typed and redialled exactly
//! once by the helper, `HeaderTimeout` is never replayed, truncation is an
//! error, and dropping the body closes the socket upstream.

#![cfg(feature = "h1")]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Method, Uri};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use xlite_transport::{
    Attempt, Backend, BodyStream, H1Transport, Transport, TransportConfig, TransportError,
};

struct Fixture {
    transport: H1Transport,
    addr: std::net::SocketAddr,
    /// Upstream requests actually seen.
    requests: Arc<AtomicUsize>,
    /// Set when `/stream` observed its socket die (cancellation).
    cancel_seen: Arc<AtomicBool>,
    /// Raw bytes of the last request head the server received.
    last_head: Arc<Mutex<Vec<u8>>>,
}

async fn fixture(header_timeout: Option<Duration>) -> Fixture {
    let mut cfg = TransportConfig::cleartext();
    cfg.header_timeout = header_timeout;
    fixture_with(cfg).await
}

async fn fixture_with(cfg: TransportConfig) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let cancel_seen = Arc::new(AtomicBool::new(false));
    let last_head = Arc::new(Mutex::new(Vec::new()));
    {
        let requests = requests.clone();
        let cancel_seen = cancel_seen.clone();
        let last_head = last_head.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let requests = requests.clone();
                let cancel_seen = cancel_seen.clone();
                let last_head = last_head.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(stream, requests, cancel_seen, last_head).await;
                });
            }
        });
    }

    Fixture {
        transport: H1Transport::new(cfg),
        addr,
        requests,
        cancel_seen,
        last_head,
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

async fn handle_conn(
    mut stream: TcpStream,
    requests: Arc<AtomicUsize>,
    cancel_seen: Arc<AtomicBool>,
    last_head: Arc<Mutex<Vec<u8>>>,
) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let mut conn_requests = 0usize;
    loop {
        let head_end = loop {
            if let Some(end) = find_head_end(&buf) {
                break end;
            }
            let mut tmp = [0u8; 4096];
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Ok(()); // client dropped its socket
            }
            buf.extend_from_slice(&tmp[..n]);
        };
        {
            let mut guard = last_head.lock().expect("fixture head mutex poisoned");
            guard.clear();
            guard.extend_from_slice(&buf[..head_end]);
        }
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let _ = buf.drain(..head_end); // `find_head_end` already includes the CRLFCRLF
        let path = head
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_ascii_lowercase();
        let cl = head
            .split("\r\n")
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while buf.len() < cl {
            let mut tmp = [0u8; 4096];
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let _ = buf.drain(..cl);
        conn_requests += 1;
        requests.fetch_add(1, Ordering::SeqCst);

        match path.as_str() {
            "/ok" => {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nx-trace: a\r\nx-trace: b\r\n\r\npong",
                    )
                    .await?;
            }
            "/slow" => {
                // Never sends a response byte; the caller's budget decides.
                tokio::time::sleep(Duration::from_secs(3600)).await;
                return Ok(());
            }
            "/partial" => {
                // Announces 100 bytes, sends 5, closes the socket.
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n12345")
                    .await?;
                return Ok(());
            }
            "/stream" => {
                stream.write_all(b"HTTP/1.1 200 OK\r\n\r\npart1").await?;
                // Keep writing until the peer's close lands: that is how a
                // downstream cancellation becomes visible upstream on h1.
                for _ in 0..100 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    if stream.write_all(b"x").await.is_err() {
                        cancel_seen.store(true, Ordering::SeqCst);
                        return Ok(());
                    }
                }
                return Ok(());
            }
            "/hold" => {
                // Hold the exchange open so two concurrent attempts are
                // really in flight at the same time.
                tokio::time::sleep(Duration::from_millis(100)).await;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong")
                    .await?;
            }
            "/chunks" => {
                // A body delivered in odd-sized, flushed pieces: SSE frames
                // arrive at arbitrary chunk boundaries and must reassemble
                // byte-exactly.
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 32\r\n\r\n")
                    .await?;
                let pieces: [&[u8]; 7] = [
                    b"0",
                    b"12",
                    b"345",
                    b"6789",
                    b"abcde",
                    b"fghijk",
                    b"lmnopqrstuv",
                ];
                for piece in pieces {
                    stream.write_all(piece).await?;
                    stream.flush().await?;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            "/fail-second" => {
                if conn_requests == 1 {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .await?;
                } else {
                    // Read the request, then close without a response: the
                    // classic stale-keep-alive race, deterministically on the
                    // second request over this socket.
                    return Ok(());
                }
            }
            _ => {
                stream
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await?;
            }
        }
    }
}

fn attempt(fx: &Fixture, path: &str) -> Attempt {
    let uri: Uri = format!("http://{}{path}", fx.addr).parse().unwrap();
    Attempt {
        method: Method::POST,
        uri,
        headers: HeaderMap::new(),
        body: Bytes::from_static(b"{}"),
    }
}

async fn collect(mut body: BodyStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        out.extend_from_slice(&chunk.expect("body chunk"));
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn one_execute_one_request_roundtrip_reuse_and_stats() {
    let fx = fixture(None).await;

    let outcome = fx.transport.execute(attempt(&fx, "/ok")).await.unwrap();
    assert_eq!(outcome.status, http::StatusCode::OK);
    assert_eq!(outcome.version, http::Version::HTTP_11);
    let traces: Vec<_> = outcome.headers.get_all("x-trace").iter().collect();
    assert_eq!(traces.len(), 2, "duplicate response headers preserved");
    assert_eq!(collect(outcome.body).await, b"pong");
    assert!(!outcome.telemetry.reused, "first attempt dialled");
    assert!(outcome.telemetry.dial.is_some(), "fresh dial is timed");
    assert_eq!(
        outcome.telemetry.pool_wait, None,
        "h1 has no queue to park in"
    );
    assert_eq!(
        outcome.telemetry.in_flight_at_acquire, 0,
        "h1 does not multiplex"
    );
    assert_eq!(
        outcome.telemetry.headers_rewritten,
        vec!["content-length", "host"],
        "exactly the mechanical rewrites"
    );
    assert_eq!(
        fx.requests.load(Ordering::SeqCst),
        1,
        "acceptance: one execute = one upstream request"
    );

    // Warm checkout: no second connection.
    let outcome = fx.transport.execute(attempt(&fx, "/ok")).await.unwrap();
    assert!(outcome.telemetry.reused);
    assert_eq!(outcome.telemetry.dial, None, "reuse means no dial");
    assert_eq!(collect(outcome.body).await, b"pong");
    assert_eq!(fx.requests.load(Ordering::SeqCst), 2);

    let stats = fx.transport.stats();
    stats.assert_invariants();
    let host = &stats.hosts[0];
    assert_eq!(host.authority, fx.addr.to_string());
    assert_eq!(host.backend, Backend::H1);
    assert_eq!(host.connections, 1, "the keep-alive socket is pooled");
    assert_eq!(host.dials, 1);
    assert_eq!(host.reused, 1);
    assert_eq!(host.evicted, 0);
    assert_eq!(host.evicted_stale, 0);
    assert_eq!(host.in_flight, 0, "h1 reports 0: no multiplexing");
    assert_eq!(host.waiting, 0);
    assert_eq!(host.wait_ms_p50, None, "h1 does not measure queue wait");
    assert_eq!(host.wait_samples, 0);
    assert_eq!(
        host.reaped_idle,
        Some(0),
        "h1 counts reclamation from day one"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_keepalive_failure_is_typed_and_redialled_exactly_once() {
    let fx = fixture(None).await;

    // conn1 serves one request and is pooled; the server closes right after
    // reading the *next* request on that socket — the classic stale race.
    // The body must be read to completion for the socket to be recycled:
    // dropping an unparsed body is a cancellation and closes the socket.
    let warm = fx.transport.execute(attempt(&fx, "/ok")).await.unwrap();
    assert_eq!(collect(warm.body).await, b"pong");
    assert_eq!(fx.requests.load(Ordering::SeqCst), 1);
    assert_eq!(fx.transport.stats().hosts[0].connections, 1, "pooled");

    // The helper's first attempt lands on the warm socket and fails as the
    // stale class; its single redial gets a fresh connection, on which the
    // server answers the first request.
    let outcome = fx
        .transport
        .execute_redial_once(attempt(&fx, "/fail-second"))
        .await
        .unwrap();
    assert_eq!(
        outcome.telemetry.redials, 1,
        "exactly one redial on the stale class"
    );
    assert!(
        outcome.telemetry.redial_reason.is_some(),
        "the redial is reported, never silent"
    );
    assert_eq!(
        fx.requests.load(Ordering::SeqCst),
        3,
        "warmup + stale attempt + the helper's single redial"
    );
    assert_eq!(fx.transport.stats().hosts[0].evicted_stale, 1);
    // Read it out so conn2 is recycled: the *next* bare attempt must land on
    // it (same as the helper's first attempt did) and fail stale.
    assert_eq!(collect(outcome.body).await, b"ok");

    // A *bare* execute never redials: the same stale failure surfaces typed
    // and stops there.
    let err = fx
        .transport
        .execute(attempt(&fx, "/fail-second"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, TransportError::StaleConnection { .. }),
        "second request on a warm socket must be the stale class, got {err:?}"
    );
    assert_eq!(fx.requests.load(Ordering::SeqCst), 4, "no hidden retry");
    let host = &fx.transport.stats().hosts[0];
    assert_eq!(host.evicted_stale, 2, "every stale discard is counted");
    assert_eq!(
        host.evicted, host.evicted_stale,
        "stale is a subset of evicted"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn header_timeout_is_never_redialled() {
    let fx = fixture(Some(Duration::from_millis(200))).await;

    let err = fx
        .transport
        .execute(attempt(&fx, "/slow"))
        .await
        .unwrap_err();
    assert!(matches!(err, TransportError::HeaderTimeout { .. }));
    assert_eq!(fx.requests.load(Ordering::SeqCst), 1);

    // The helper must NOT replay a POST at a live-but-slow server.
    let err = fx
        .transport
        .execute_redial_once(attempt(&fx, "/slow"))
        .await
        .unwrap_err();
    assert!(matches!(err, TransportError::HeaderTimeout { .. }));
    assert_eq!(
        fx.requests.load(Ordering::SeqCst),
        2,
        "two attempts, one request each — never a redial"
    );

    let host = &fx.transport.stats().hosts[0];
    assert_eq!(host.evicted, 2, "each timed-out socket was discarded");
    assert_eq!(host.evicted_stale, 0, "a timeout is not the stale class");
    assert_eq!(host.connections, 0, "no poisoned socket stays pooled");
}

#[tokio::test(flavor = "multi_thread")]
async fn truncated_body_is_an_error_not_a_clean_end() {
    let fx = fixture(None).await;
    let mut outcome = fx
        .transport
        .execute(attempt(&fx, "/partial"))
        .await
        .unwrap();
    let mut total = 0usize;
    let mut err = None;
    while let Some(item) = outcome.body.next().await {
        match item {
            Ok(bytes) => total += bytes.len(),
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    assert_eq!(total, 5, "the bytes that did arrive are delivered");
    assert!(
        matches!(err, Some(TransportError::BodyIncomplete)),
        "announced 100, sent 5: got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_body_cancels_upstream() {
    let fx = fixture(None).await;
    let outcome = fx.transport.execute(attempt(&fx, "/stream")).await.unwrap();
    let mut body = outcome.body;
    let first = body.next().await.unwrap().unwrap();
    assert_eq!(&first[..], b"part1");

    drop(body);
    let cancelled = tokio::time::timeout(Duration::from_secs(6), async {
        while !fx.cancel_seen.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(
        cancelled.is_ok(),
        "dropping the body must close the socket upstream"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_attempt_never_opens_a_socket() {
    let fx = fixture(None).await;

    // No scheme: rejected by validation before anything else.
    let mut bad = attempt(&fx, "/ok");
    bad.uri = "/v1/completions".parse().unwrap();
    let err = fx.transport.execute(bad).await.unwrap_err();
    assert!(matches!(err, TransportError::InvalidRequest(_)));

    // Contradictory content-length on a *dead port*: had any I/O happened
    // before validation, this would surface as Connect instead.
    let mut bad = attempt(&fx, "/ok");
    bad.uri = "http://127.0.0.1:9/v1/completions".parse().unwrap();
    bad.headers.insert(
        http::header::CONTENT_LENGTH,
        HeaderValue::from_static("999"),
    );
    let err = fx.transport.execute(bad).await.unwrap_err();
    assert!(matches!(err, TransportError::InvalidRequest(_)));

    // transfer-encoding is caller-encoded on this backend: rejected before
    // any I/O, not silently dropped.
    let mut te = attempt(&fx, "/ok");
    te.headers.insert(
        http::header::TRANSFER_ENCODING,
        HeaderValue::from_static("chunked"),
    );
    let err = fx.transport.execute(te).await.unwrap_err();
    assert!(matches!(err, TransportError::InvalidRequest(_)));

    assert_eq!(
        fx.requests.load(Ordering::SeqCst),
        0,
        "zero upstream requests"
    );
    assert!(
        fx.transport.stats().hosts.is_empty(),
        "no host was ever created"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn arbitrary_chunk_splits_reassemble_byte_exact() {
    const EXPECTED: &[u8] = b"0123456789abcdefghijklmnopqrstuv";
    let fx = fixture(None).await;

    let outcome = fx.transport.execute(attempt(&fx, "/chunks")).await.unwrap();
    assert_eq!(outcome.status, http::StatusCode::OK);
    assert_eq!(
        collect(outcome.body).await,
        EXPECTED,
        "odd-sized TCP writes must reassemble byte-exactly"
    );
    assert_eq!(
        fx.requests.load(Ordering::SeqCst),
        1,
        "one execute = one upstream request"
    );

    // The exchange completed cleanly: keep-alive survived the chunking.
    let outcome = fx.transport.execute(attempt(&fx, "/ok")).await.unwrap();
    assert!(outcome.telemetry.reused, "the socket was recycled");
}

#[tokio::test(flavor = "multi_thread")]
async fn request_headers_pass_through_verbatim() {
    let fx = fixture(None).await;

    let uri: Uri = format!("http://{}/ok", fx.addr).parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.append("x-trace", HeaderValue::from_static("a"));
    headers.append("x-trace", HeaderValue::from_static("b"));
    headers.insert("authorization", HeaderValue::from_static("Bearer sk-42"));
    // Non-UTF-8 obs-text must survive byte-for-byte in both directions.
    headers.insert("x-binary", HeaderValue::from_bytes(&[0x80, 0xff]).unwrap());
    let attempt = Attempt {
        method: Method::POST,
        uri,
        headers,
        body: Bytes::from_static(b"{}"),
    };

    let outcome = fx.transport.execute(attempt).await.unwrap();
    assert_eq!(outcome.status, http::StatusCode::OK);
    assert_eq!(
        outcome.telemetry.headers_rewritten,
        vec!["content-length", "host"],
        "only the two mechanical rewrites, nothing else touched"
    );

    let head = fx
        .last_head
        .lock()
        .expect("fixture head mutex poisoned")
        .clone();
    let seen = |needle: &[u8]| head.windows(needle.len()).any(|w| w == needle);
    assert!(seen(b"POST /ok HTTP/1.1\r\n"), "request line");
    assert!(seen(b"x-trace: a\r\n"), "first duplicate preserved");
    assert!(seen(b"x-trace: b\r\n"), "second duplicate preserved");
    assert!(seen(b"authorization: Bearer sk-42\r\n"));
    assert!(seen(b"x-binary: \x80\xff\r\n"), "non-UTF-8 value verbatim");
    assert!(seen(b"content-length: 2\r\n"), "synthesized from the body");
    assert!(seen(b"host: "), "synthesized from the authority");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_authorities_one_transport_separate_pools() {
    let a = fixture(None).await;
    let b = fixture(None).await;

    // One transport, two upstreams: the pool keys on each Attempt's runtime
    // authority — no catalog, no host list, no routing table in between.
    let out = a.transport.execute(attempt(&a, "/ok")).await.unwrap();
    assert_eq!(collect(out.body).await, b"pong");
    let out = a.transport.execute(attempt(&b, "/ok")).await.unwrap();
    assert_eq!(collect(out.body).await, b"pong");

    // Warm both: each authority keeps its own socket.
    let out = a.transport.execute(attempt(&a, "/ok")).await.unwrap();
    assert!(out.telemetry.reused);
    assert_eq!(collect(out.body).await, b"pong");
    let out = a.transport.execute(attempt(&b, "/ok")).await.unwrap();
    assert!(out.telemetry.reused);
    assert_eq!(collect(out.body).await, b"pong");

    let stats = a.transport.stats();
    stats.assert_invariants();
    assert_eq!(stats.hosts.len(), 2, "two authorities, one pool");
    let mut authorities: Vec<String> = stats.hosts.iter().map(|h| h.authority.clone()).collect();
    authorities.sort();
    let mut expected = vec![a.addr.to_string(), b.addr.to_string()];
    expected.sort();
    assert_eq!(authorities, expected);
    for host in &stats.hosts {
        assert_eq!(
            host.connections, 1,
            "{} keeps exactly one socket",
            host.authority
        );
        assert_eq!(host.dials, 1);
        assert_eq!(host.reused, 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_reclaim_closes_expired_sockets_and_counts_them() {
    let mut cfg = TransportConfig::cleartext();
    cfg.pool_idle_reclaim = Some(Duration::from_millis(150));
    let fx = fixture_with(cfg).await;

    let outcome = fx.transport.execute(attempt(&fx, "/ok")).await.unwrap();
    assert_eq!(
        collect(outcome.body).await,
        b"pong",
        "body read out so the socket is pooled"
    );

    tokio::time::sleep(Duration::from_millis(400)).await;

    let outcome = fx.transport.execute(attempt(&fx, "/ok")).await.unwrap();
    assert!(
        !outcome.telemetry.reused,
        "the expired socket was reaped, not reused"
    );
    assert!(outcome.telemetry.dial.is_some(), "a fresh dial happened");
    assert_eq!(collect(outcome.body).await, b"pong");

    let host = &fx.transport.stats().hosts[0];
    assert_eq!(host.dials, 2);
    assert_eq!(
        host.reaped_idle,
        Some(1),
        "the reclamation is counted, not silent"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_pool_cap_retains_one_warm_socket() {
    let mut cfg = TransportConfig::cleartext();
    cfg.h1_max_conns_per_host = 1;
    let fx = fixture_with(cfg).await;

    // Two exchanges in flight at once: neither can reuse, both must dial.
    let (first, second) = tokio::join!(
        fx.transport.execute(attempt(&fx, "/hold")),
        fx.transport.execute(attempt(&fx, "/hold")),
    );
    assert_eq!(collect(first.unwrap().body).await, b"pong");
    assert_eq!(collect(second.unwrap().body).await, b"pong");

    let host = &fx.transport.stats().hosts[0];
    assert_eq!(
        host.dials, 2,
        "nothing warm to reuse while both were in flight"
    );
    assert_eq!(
        host.connections, 1,
        "the older socket was closed when the idle pool hit h1_max_conns_per_host"
    );
    assert_eq!(host.evicted, 0, "a capped put is reclamation, not distrust");
}

#[tokio::test(flavor = "multi_thread")]
async fn dial_gate_bounds_concurrency_per_host() {
    use xlite_transport::h1::H1Pool;

    // The gate is per host: different authorities have different accept
    // queues (xlite#96 — a `listen(5)` server drops a 16-way SYN burst and
    // every dropped SYN costs ~1s of kernel retransmit).
    let pool = H1Pool::new(4, Duration::from_secs(90), 4);
    let host_a = pool.host_pool(false, "a.example:80");
    let host_b = pool.host_pool(false, "b.example:80");

    // Fill host A's gate: exactly `max_parallel_dials` slots.
    let held: Vec<_> = futures_util::future::join_all((0..4).map(|_| host_a.acquire_dial())).await;
    assert_eq!(held.len(), 4, "the first four slots resolve immediately");

    // The fifth dial on A must wait…
    assert!(
        tokio::time::timeout(Duration::from_millis(100), host_a.acquire_dial())
            .await
            .is_err(),
        "host A's gate must hold at its limit"
    );

    // …while host B is unaffected: the limit is per host, not global.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), host_b.acquire_dial())
            .await
            .is_ok(),
        "host B must not be throttled by host A"
    );

    // Releasing a slot lets the waiting dial proceed.
    drop(held);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), host_a.acquire_dial())
            .await
            .is_ok(),
        "the fifth dial must proceed once a slot frees"
    );
}
