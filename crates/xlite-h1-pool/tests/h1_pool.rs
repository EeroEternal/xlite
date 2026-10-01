//! Integration tests for the HTTP/1.1 pool against a minimal real TCP server.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http::Request;
use http_body_util::{BodyExt, Full};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use xlite_h1_pool::{BoxIo, Config, Dial, Pool};

fn tcp_dial() -> Dial {
    Arc::new(|host: String| {
        Box::pin(async move {
            let stream = TcpStream::connect(host.as_str()).await?;
            let _ = stream.set_nodelay(true);
            Ok(Box::new(stream) as Box<dyn BoxIo>)
        })
    })
}

/// Start a keep-alive HTTP/1.1 server; returns `host:port` and a counter of
/// accepted TCP connections.
async fn start_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(serve(stream));
        }
    });
    (format!("127.0.0.1:{}", addr.port()), accepted)
}

async fn serve(mut stream: TcpStream) {
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        // Read a full request head.
        while !pending.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                return;
            }
            pending.extend_from_slice(&buf[..n]);
        }
        let head_end = pending.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head: Vec<u8> = pending[..head_end].to_vec();
        pending.drain(..head_end + 4);
        // Drain the body.
        let cl = content_length(&head);
        while pending.len() < cl {
            let n = stream.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                return;
            }
            pending.extend_from_slice(&buf[..n]);
        }
        pending.drain(..cl);
        // Respond.
        let body = b"{\"ok\":true}";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        );
        if stream.write_all(resp.as_bytes()).await.is_err() {
            return;
        }
        if stream.flush().await.is_err() {
            return;
        }
    }
}

fn content_length(head: &[u8]) -> usize {
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    for line in text.split("\r\n") {
        if let Some(v) = line.strip_prefix("content-length:") {
            return v.trim().parse().unwrap_or(0);
        }
    }
    0
}

async fn wait_count(counter: &AtomicUsize, want: usize) {
    for _ in 0..200 {
        if counter.load(Ordering::SeqCst) == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        want,
        "server never reached the expected connection count"
    );
}

fn make_req(host: &str) -> Request<Full<Bytes>> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("host", host)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from_static(b"{}")))
        .unwrap()
}

fn pool(max_conns: usize) -> Pool {
    Pool::new(Config {
        max_conns_per_host: max_conns,
        dial: tcp_dial(),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_request_succeeds() {
    let (host, _accepted) = start_server().await;
    let pool = pool(4);

    let lease = pool.acquire(&host).await.unwrap();
    let resp = lease.send_request(make_req(&host)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"{\"ok\":true}");

    let stats = pool.stats(&host);
    assert_eq!(stats.connections, 1);
    assert_eq!(stats.in_flight, 1);
    assert_eq!(stats.dials, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_is_reused() {
    let (host, accepted) = start_server().await;
    let pool = pool(4);

    for _ in 0..3 {
        let lease = pool.acquire(&host).await.unwrap();
        let resp = lease.send_request(make_req(&host)).await.unwrap();
        let _ = resp.into_body().collect().await.unwrap();
        drop(lease); // release before the next acquire
    }

    // keep-alive: three sequential requests must reuse one connection.
    wait_count(&accepted, 1).await;
    assert_eq!(pool.stats(&host).connections, 1);
    assert_eq!(pool.stats(&host).in_flight, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cap_bounds_connections_and_parks_the_rest() {
    let (host, accepted) = start_server().await;
    let pool = Arc::new(pool(1));

    let held = pool.acquire(&host).await.unwrap();

    let p2 = Arc::clone(&pool);
    let h2 = host.clone();
    let waiter = tokio::spawn(async move { p2.acquire(&h2).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !waiter.is_finished(),
        "with cap=1 and the only slot held, a second acquire must park"
    );

    drop(held);
    let lease = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("releasing the slot must wake the waiter")
        .unwrap()
        .unwrap();
    drop(lease);

    // cap=1 ⇒ at most one TCP connection is ever opened for this host.
    wait_count(&accepted, 1).await;
    assert_eq!(pool.stats(&host).connections, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosts_are_independent() {
    let (host_a, accepted_a) = start_server().await;
    let (host_b, accepted_b) = start_server().await;
    let pool = pool(1);

    let a = pool.acquire(&host_a).await.unwrap();
    let b = pool.acquire(&host_b).await.unwrap();
    wait_count(&accepted_a, 1).await;
    wait_count(&accepted_b, 1).await;
    drop(a);
    drop(b);

    let hosts: Vec<String> = pool.all_stats().into_iter().map(|(h, _)| h).collect();
    assert_eq!(hosts.len(), 2);
}
