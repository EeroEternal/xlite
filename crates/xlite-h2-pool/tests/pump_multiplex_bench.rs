//! Micro-bench (ignored by default) for the pinned-poll pump design.
//!
//! Question: on ONE h2 connection carrying N streams, does pinning every
//! `RecvStream` to a single polling task (a "pump") plus a per-stream channel
//! hand-off remove the cross-task `poll_data` contention (measured at ~47% of
//! gateway CPU), and what does the channel hop cost?
//!
//! Modes, identical total work, differing only in who polls the connection:
//!   `direct-1t`  1 task polls all N streams (no contention, no channel) — floor
//!   `direct-Nt`  N tasks poll their own stream (the gateway's current shape)
//!   `pump-Nt`    1 task polls all N, forwards each chunk over an mpsc channel
//!                to N consumer tasks (the proposed shape)
//!
//! `open` (stream setup) and `drain` (the poll loop) are timed separately: only
//! the drain is what the design changes.
//! The mock server runs on its **own** runtime so the measured runtime's workers
//! are all client work.
//!
//! Run:
//!   cargo test -p xlite-h2-pool --release --test pump_multiplex_bench -- --ignored --nocapture

use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;

const STREAMS: usize = 208;
const CHUNKS: usize = 400;
const CHUNK_BYTES: usize = 200;
const WORKERS: usize = 4;

/// A server that streams `CHUNKS` × `CHUNK_BYTES` on every accepted request as
/// fast as backpressure allows.
async fn fast_server(io: tokio::io::DuplexStream) {
    let mut conn = match h2::server::Builder::new()
        .max_concurrent_streams(4096)
        .handshake::<_, Bytes>(io)
        .await
    {
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
            let body = Bytes::from(vec![b'x'; CHUNK_BYTES]);
            for _ in 0..CHUNKS {
                if send.send_data(body.clone(), false).is_err() {
                    return;
                }
                tokio::task::yield_now().await;
            }
            let _ = send.send_data(Bytes::new(), true);
        });
    }
}

/// Open `STREAMS` requests on ONE connection (server on `srv`); returns the
/// response bodies and how long the setup took.
async fn open_streams(srv: &'static tokio::runtime::Runtime) -> (Duration, Vec<h2::RecvStream>) {
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    srv.spawn(fast_server(server_io));

    let (mut send, conn) = xlite_h2_pool::client_builder()
        .handshake::<_, Bytes>(client_io)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let started = Instant::now();
    let mut futures = Vec::with_capacity(STREAMS);
    for _ in 0..STREAMS {
        let req = http::Request::builder()
            .method("GET")
            .uri("http://upstream/")
            .body(())
            .unwrap();
        let (resp, s) = send.send_request(req, true).unwrap();
        drop(s);
        futures.push(resp);
    }
    let mut recvs = Vec::with_capacity(STREAMS);
    for f in futures {
        recvs.push(f.await.unwrap().into_body());
    }
    (started.elapsed(), recvs)
}

/// Drain one stream, releasing flow-control capacity in 8 KiB batches (as the
/// production `H2Body` does). Returns the byte count.
async fn drain(mut recv: h2::RecvStream) -> usize {
    let mut pending = 0usize;
    let mut total = 0usize;
    while let Some(chunk) = recv.next().await {
        let Ok(bytes) = chunk else { break };
        total += bytes.len();
        pending += bytes.len();
        if pending >= 8 * 1024 {
            let _ = recv.flow_control().release_capacity(pending);
            pending = 0;
        }
    }
    if pending > 0 {
        let _ = recv.flow_control().release_capacity(pending);
    }
    total
}

struct Outcome {
    open: Duration,
    drain: Duration,
    bytes: usize,
}

/// One task polls every stream (no contention, no channel).
async fn run_direct_1t(srv: &'static tokio::runtime::Runtime) -> Outcome {
    let (open, recvs) = open_streams(srv).await;
    let mut set = FuturesUnordered::new();
    for r in recvs {
        set.push(drain(r));
    }
    let started = Instant::now();
    let mut bytes = 0;
    while let Some(n) = set.next().await {
        bytes += n;
    }
    Outcome {
        open,
        drain: started.elapsed(),
        bytes,
    }
}

/// N tasks poll their own stream — the gateway's current shape.
async fn run_direct_nt(srv: &'static tokio::runtime::Runtime) -> Outcome {
    let (open, recvs) = open_streams(srv).await;
    let mut handles = Vec::with_capacity(STREAMS);
    for r in recvs {
        handles.push(tokio::spawn(drain(r)));
    }
    let started = Instant::now();
    let mut bytes = 0;
    for h in handles {
        bytes += h.await.unwrap_or(0);
    }
    Outcome {
        open,
        drain: started.elapsed(),
        bytes,
    }
}

/// One task polls every stream and forwards each chunk over a bounded channel to
/// a per-stream consumer — the proposed shape. Returns bytes delivered through
/// the channels (so a broken hand-off shows up as a low total).
async fn run_pump_nt(srv: &'static tokio::runtime::Runtime) -> Outcome {
    let (open, recvs) = open_streams(srv).await;

    let mut senders = Vec::with_capacity(STREAMS);
    let mut consumers = Vec::with_capacity(STREAMS);
    for _ in 0..STREAMS {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<usize>(16);
        senders.push(tx);
        consumers.push(tokio::spawn(async move {
            let mut total = 0usize;
            while let Some(n) = rx.recv().await {
                total += n;
            }
            total
        }));
    }

    // The pump: the *only* poller of every stream on this connection.
    let mut pump = FuturesUnordered::new();
    for (mut recv, tx) in recvs.into_iter().zip(senders) {
        pump.push(async move {
            let mut pending = 0usize;
            while let Some(chunk) = recv.next().await {
                let Ok(bytes) = chunk else { break };
                let len = bytes.len();
                pending += len;
                if pending >= 8 * 1024 {
                    let _ = recv.flow_control().release_capacity(pending);
                    pending = 0;
                }
                if tx.send(len).await.is_err() {
                    break;
                }
            }
            if pending > 0 {
                let _ = recv.flow_control().release_capacity(pending);
            }
            drop(tx);
        });
    }
    let started = Instant::now();
    while pump.next().await.is_some() {}
    let drain = started.elapsed();

    let mut bytes = 0;
    for h in consumers {
        bytes += h.await.unwrap_or(0);
    }
    Outcome { open, drain, bytes }
}

fn time_it(
    label: &str,
    workers: usize,
    srv: &'static tokio::runtime::Runtime,
    make: impl FnOnce(
        &'static tokio::runtime::Runtime,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Outcome>>>,
) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap();
    let out = rt.block_on(make(srv));
    let chunks = out.bytes / CHUNK_BYTES;
    println!(
        "{label:<12} workers={workers}  open={:>7.3?}  drain={:>7.3?}  bytes={:>9}  drain {:.0} chunks/s",
        out.open,
        out.drain,
        out.bytes,
        chunks as f64 / out.drain.as_secs_f64()
    );
}

#[test]
#[ignore = "micro-bench; run explicitly with --ignored --nocapture"]
fn pump_multiplex_bench() {
    let srv: &'static tokio::runtime::Runtime = Box::leak(Box::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
    ));
    println!("STREAMS={STREAMS} CHUNKS={CHUNKS} CHUNK_BYTES={CHUNK_BYTES}");
    time_it("direct-1t", 1, srv, |s| Box::pin(run_direct_1t(s)));
    time_it("direct-Nt", WORKERS, srv, |s| Box::pin(run_direct_nt(s)));
    time_it("pump-Nt", WORKERS, srv, |s| Box::pin(run_pump_nt(s)));
}
