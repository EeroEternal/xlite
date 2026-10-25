//! The h2 backend against a scripted h2 server on in-memory duplex pairs.
//!
//! These pin the issue #96 acceptance criteria that are testable without a
//! socket: one `execute()` = one upstream request, the redial helper is
//! bounded and race-proof, `HeaderTimeout` is never replayed, truncated
//! bodies are errors, and dropping the body reaches the upstream.

#![cfg(feature = "h2")]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Method, Uri};
use xlite_transport::{
    Attempt, Backend, BoxIo, Dial, H2Transport, Transport, TransportConfig, TransportError,
};

const URI_BASE: &str = "http://upstream";

/// One transport plus the counters its scripted server maintains.
struct Fixture {
    transport: H2Transport,
    /// Upstream requests actually seen.
    requests: Arc<AtomicUsize>,
    /// Connections actually opened.
    dials: Arc<AtomicUsize>,
    /// Set when the `/stream` route observed its stream die (cancellation).
    cancel_seen: Arc<AtomicBool>,
    /// The server task of the first dial (abortable: simulates a peer close).
    server: tokio::sync::oneshot::Receiver<tokio::task::JoinHandle<()>>,
}

fn fixture(header_timeout: Option<Duration>) -> Fixture {
    let requests = Arc::new(AtomicUsize::new(0));
    let dials = Arc::new(AtomicUsize::new(0));
    let cancel_seen = Arc::new(AtomicBool::new(false));
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let dial: Dial = Arc::new({
        let requests = requests.clone();
        let dials = dials.clone();
        let cancel_seen = cancel_seen.clone();
        move |_key: String| {
            let requests = requests.clone();
            let dials = dials.clone();
            let cancel_seen = cancel_seen.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                let handle = spawn_server(server_io, requests, cancel_seen);
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(handle);
                }
                dials.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(client_io) as BoxIo)
            })
        }
    });

    let mut cfg = TransportConfig::cleartext();
    cfg.header_timeout = header_timeout;
    Fixture {
        transport: H2Transport::with_dial(cfg, dial),
        requests,
        dials,
        cancel_seen,
        server: rx,
    }
}

fn spawn_server(
    io: tokio::io::DuplexStream,
    requests: Arc<AtomicUsize>,
    cancel_seen: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let conn = h2::server::Builder::new()
            .max_concurrent_streams(20u32)
            .handshake::<_, Bytes>(io)
            .await;
        let mut conn = match conn {
            Ok(c) => c,
            Err(_) => return,
        };
        while let Some(Ok((req, mut respond))) = conn.accept().await {
            requests.fetch_add(1, Ordering::SeqCst);
            let path = req.uri().path().to_string();
            let cancel_seen = cancel_seen.clone();
            tokio::spawn(async move {
                let _req = req;
                match path.as_str() {
                    "/ok" => {
                        // Duplicate response headers must survive verbatim.
                        let resp = http::Response::builder()
                            .status(200)
                            .header("x-trace", "a")
                            .header("x-trace", "b")
                            .header("content-length", "4")
                            .body(())
                            .unwrap();
                        let mut sender = respond.send_response(resp, false).unwrap();
                        let _ = sender.send_data(Bytes::from_static(b"pong"), true);
                    }
                    "/slow-headers" => {
                        // Never sends headers; the caller's budget decides.
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                    }
                    "/partial" => {
                        // Announces 100 bytes, sends 5, ends the stream clean.
                        let resp = http::Response::builder()
                            .status(200)
                            .header("content-length", "100")
                            .body(())
                            .unwrap();
                        let mut sender = respond.send_response(resp, false).unwrap();
                        let _ = sender.send_data(Bytes::from_static(b"12345"), true);
                    }
                    "/reset" => {
                        let resp = http::Response::builder().status(200).body(()).unwrap();
                        let mut sender = respond.send_response(resp, false).unwrap();
                        let _ = sender.send_data(Bytes::from_static(b"hello"), false);
                        // Let bytes land first: this is about the pump ending
                        // on a typed reset, not winning a race.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        sender.send_reset(h2::Reason::NO_ERROR);
                    }
                    "/stream" => {
                        let resp = http::Response::builder().status(200).body(()).unwrap();
                        let mut sender = respond.send_response(resp, false).unwrap();
                        let _ = sender.send_data(Bytes::from_static(b"part1"), false);
                        // Hold the stream open; the next send failing is how a
                        // downstream cancel becomes visible upstream.
                        for _ in 0..100 {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            if sender.send_data(Bytes::from_static(b"x"), false).is_err() {
                                cancel_seen.store(true, Ordering::SeqCst);
                                return;
                            }
                        }
                    }
                    _ => {
                        let resp = http::Response::builder().status(404).body(()).unwrap();
                        let _ = respond.send_response(resp, true);
                    }
                }
            });
        }
    })
}

fn attempt(path: &str) -> Attempt {
    let uri: Uri = format!("{URI_BASE}{path}").parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-caller", HeaderValue::from_static("kept"));
    Attempt {
        method: Method::POST,
        uri,
        headers,
        body: Bytes::from_static(b"{}"),
    }
}

async fn collect(mut body: xlite_transport::BodyStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        out.extend_from_slice(&chunk.expect("body chunk"));
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn one_execute_one_request_verbatim_roundtrip_and_reuse() {
    let fx = fixture(None);

    let outcome = fx.transport.execute(attempt("/ok")).await.unwrap();
    assert_eq!(outcome.status, http::StatusCode::OK);
    assert_eq!(outcome.version, http::Version::HTTP_2);
    let traces: Vec<_> = outcome.headers.get_all("x-trace").iter().collect();
    assert_eq!(traces.len(), 2, "duplicate response headers preserved");
    assert_eq!(collect(outcome.body).await, b"pong");
    assert!(!outcome.telemetry.reused, "first attempt dialed");
    assert!(outcome.telemetry.dial.is_some(), "fresh dial is timed");
    assert!(
        outcome.telemetry.pool_wait.is_some(),
        "h2 pool always measures wait"
    );
    assert_eq!(
        outcome.telemetry.headers_rewritten,
        vec!["content-length", "host"],
        "exactly the mechanical rewrites, nothing else"
    );
    assert_eq!(
        fx.requests.load(Ordering::SeqCst),
        1,
        "acceptance: one execute = one upstream request"
    );

    // The pooled connection is reused; no second dial happened.
    let outcome = fx.transport.execute(attempt("/ok")).await.unwrap();
    assert!(outcome.telemetry.reused);
    assert_eq!(outcome.telemetry.dial, None, "reuse means no dial");
    assert_eq!(fx.requests.load(Ordering::SeqCst), 2);
    assert_eq!(fx.dials.load(Ordering::SeqCst), 1);

    let stats = fx.transport.stats();
    stats.assert_invariants();
    let host = &stats.hosts[0];
    assert_eq!(host.authority, "upstream");
    assert_eq!(host.backend, Backend::H2);
    assert_eq!(host.dials, 1);
    assert_eq!(host.reused, 1);
    assert_eq!(host.evicted, 0);
    assert_eq!(host.evicted_stale, 0);
    assert!(
        host.wait_ms_p50.is_none(),
        "percentiles are over parked acquires; nothing ever parked here"
    );
    assert!(host.reaped_idle.is_some(), "h2 counts idle reaping");
}

#[tokio::test(flavor = "multi_thread")]
async fn redial_once_survives_a_peer_close_and_stays_bounded() {
    let fx = fixture(None);
    fx.transport.execute(attempt("/ok")).await.unwrap();
    assert_eq!(fx.requests.load(Ordering::SeqCst), 1);

    // Kill the pooled connection's server side. Whether the client driver has
    // already noticed (fresh dial) or not (stale class + one redial), the
    // helper must succeed with at most one redial.
    fx.server.await.unwrap().abort();

    let outcome = fx
        .transport
        .execute_redial_once(attempt("/ok"))
        .await
        .expect("the helper recovers from a stale pooled connection");
    assert!(outcome.telemetry.redials <= 1, "at most one redial");
    assert!(
        fx.requests.load(Ordering::SeqCst) <= 2,
        "acceptance: the helper never produces a third upstream request"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn header_timeout_is_never_redialled() {
    let fx = fixture(Some(Duration::from_millis(200)));

    let err = fx
        .transport
        .execute(attempt("/slow-headers"))
        .await
        .unwrap_err();
    assert!(matches!(err, TransportError::HeaderTimeout { .. }));
    assert_eq!(fx.requests.load(Ordering::SeqCst), 1);

    // The helper must NOT replay: a live-but-slow server would otherwise see
    // a duplicated chat POST.
    let err = fx
        .transport
        .execute_redial_once(attempt("/slow-headers"))
        .await
        .unwrap_err();
    assert!(matches!(err, TransportError::HeaderTimeout { .. }));
    assert_eq!(
        fx.requests.load(Ordering::SeqCst),
        2,
        "two attempts, one request each — never a redial"
    );

    // Each timed-out attempt evicted its socket: the distrust is visible.
    let host = &fx.transport.stats().hosts[0];
    assert_eq!(host.evicted, 2);
    assert_eq!(host.evicted_stale, 0, "a timeout is not the stale class");
    assert_eq!(host.connections, 0, "no poisoned socket stays pooled");
}

#[tokio::test(flavor = "multi_thread")]
async fn truncated_body_is_an_error_not_a_clean_end() {
    let fx = fixture(None);
    let outcome = fx.transport.execute(attempt("/partial")).await.unwrap();
    let mut body = outcome.body;
    // The `h2` client library resets a stream whose DATA ends short of its
    // announced content-length, and may discard the short data — so the
    // contract to pin is "an error, never a clean end", not which bytes
    // happen to survive.
    let mut total = 0usize;
    let mut err = None;
    while let Some(item) = body.next().await {
        match item {
            Ok(bytes) => total += bytes.len(),
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    assert!(
        matches!(err, Some(TransportError::BodyIncomplete)),
        "announced 100, sent 5: got {err:?}"
    );
    assert!(total <= 5, "only the announced-short bytes can appear");
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_reset_is_typed() {
    let fx = fixture(None);
    let outcome = fx.transport.execute(attempt("/reset")).await.unwrap();
    let mut body = outcome.body;
    let first = body.next().await.unwrap().unwrap();
    assert_eq!(&first[..], b"hello");
    let err = body.next().await.unwrap().unwrap_err();
    assert!(
        matches!(err, TransportError::Reset { code: Some(0) }),
        "RST_STREAM(NO_ERROR) must be typed, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_body_cancels_upstream() {
    let fx = fixture(None);
    let outcome = fx.transport.execute(attempt("/stream")).await.unwrap();
    let mut body = outcome.body;
    let first = body.next().await.unwrap().unwrap();
    assert_eq!(&first[..], b"part1");

    // Drop mid-stream: the upstream must see the disconnect as a reset.
    drop(body);
    let cancelled = tokio::time::timeout(Duration::from_secs(6), async {
        while !fx.cancel_seen.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(cancelled.is_ok(), "drop must reach the upstream as a reset");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_attempt_never_opens_a_socket() {
    let fx = fixture(None);
    let mut bad = attempt("/ok");
    bad.uri = "/v1/completions".parse().unwrap();
    let err = fx.transport.execute(bad).await.unwrap_err();
    assert!(matches!(err, TransportError::InvalidRequest(_)));
    assert_eq!(
        fx.dials.load(Ordering::SeqCst),
        0,
        "zero upstream connections"
    );
    assert_eq!(fx.requests.load(Ordering::SeqCst), 0);
}
