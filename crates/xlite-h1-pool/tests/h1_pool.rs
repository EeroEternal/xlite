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
    start_server_with_body(b"{\"ok\":true}".to_vec()).await
}

async fn start_server_with_body(body: Vec<u8>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    let body = Arc::new(body);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(serve(stream, Arc::clone(&body)));
        }
    });
    (format!("127.0.0.1:{}", addr.port()), accepted)
}

async fn serve(mut stream: TcpStream, body: Arc<Vec<u8>>) {
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
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n",
            body.len()
        )
        .into_bytes();
        resp.extend_from_slice(&body);
        if stream.write_all(&resp).await.is_err() {
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
        idle_timeout: Duration::from_secs(90),
        min_idle_conns: 0,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unread_body_retires_the_connection() {
    // Large enough that a single client read cannot exhaust it, so abandoning the
    // body leaves it unread (HTTP/1.1 cannot reuse such a connection).
    let big = vec![b'x'; 512 * 1024];
    let (host, _accepted) = start_server_with_body(big).await;
    let pool = pool(4);

    let lease = pool.acquire(&host).await.unwrap();
    let resp = lease.send_request(make_req(&host)).await.unwrap();
    let mut body = resp.into_body();
    let _ = body.frame().await; // read one frame, then abandon the rest
    drop(body);
    drop(lease);

    // The retired connection must not be handed out again: the next acquire dials
    // a fresh one (dials goes 1 -> 2) instead of reusing the half-read socket.
    let lease2 = pool.acquire(&host).await.unwrap();
    assert_eq!(
        pool.stats(&host).dials,
        2,
        "an unread body must retire its connection instead of being reused"
    );
    drop(lease2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reaper_reclaims_idle_connections() {
    let (host, _accepted) = start_server().await;
    let pool = Pool::new(Config {
        max_conns_per_host: 4,
        idle_timeout: Duration::from_millis(100),
        min_idle_conns: 0,
        dial: tcp_dial(),
    });

    let lease = pool.acquire(&host).await.unwrap();
    let resp = lease.send_request(make_req(&host)).await.unwrap();
    let _ = resp.into_body().collect().await.unwrap();
    drop(lease);
    assert_eq!(pool.stats(&host).connections, 1);

    // The reaper scans every idle_timeout / 2 = 50ms.
    for _ in 0..40 {
        if pool.stats(&host).connections == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        pool.stats(&host).connections,
        0,
        "an idle connection must be reaped"
    );
    assert!(
        pool.stats(&host).reclaimed >= 1,
        "reclaimed must be counted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reaper_keeps_min_idle_connections() {
    let (host, _accepted) = start_server().await;
    let pool = Pool::new(Config {
        max_conns_per_host: 4,
        idle_timeout: Duration::from_millis(80),
        min_idle_conns: 1,
        dial: tcp_dial(),
    });

    let lease = pool.acquire(&host).await.unwrap();
    let resp = lease.send_request(make_req(&host)).await.unwrap();
    let _ = resp.into_body().collect().await.unwrap();
    drop(lease);

    // Well past idle_timeout: the one connection must survive as min_idle.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        pool.stats(&host).connections,
        1,
        "a connection within min_idle_conns must not be reaped"
    );
    assert_eq!(pool.stats(&host).reclaimed, 0);
}

/// The waiting counters have to make the invisible queue visible: with cap=1 a
/// second acquire parks (waiting/waiting_max), and once the slot is released it
/// is counted in queued_total with its wait duration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saturated_pool_reports_its_queue_depth_and_wait() {
    let (host, _accepted) = start_server().await;
    let pool = Arc::new(pool(1));

    let held = pool.acquire(&host).await.unwrap();
    let before = pool.stats(&host);
    assert_eq!(before.waiting, 0);
    assert_eq!(before.queued_total, 0);

    let waiter = {
        let pool = Arc::clone(&pool);
        let h2 = host.clone();
        tokio::spawn(async move { pool.acquire(&h2).await })
    };
    for _ in 0..200 {
        if pool.stats(&host).waiting == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let parked = pool.stats(&host);
    assert_eq!(parked.waiting, 1, "a parked acquirer must show up");
    assert_eq!(parked.waiting_max, 1, "the high-water mark has to see it");
    assert_eq!(parked.in_flight, 1, "the held slot still counts");

    // Hold it a measurable while so the wait timer has something to record.
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(held);

    let lease = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("releasing the slot must wake the waiter")
        .unwrap();
    let after = pool.stats(&host);
    assert_eq!(after.waiting, 0, "the queue has to drain");
    assert_eq!(after.queued_total, 1, "exactly one acquire had to park");
    assert!(
        after.wait_millis_max >= 10,
        "the wait was ~20ms, so the worst wait has to reflect it (got {})",
        after.wait_millis_max
    );
    drop(lease);
}
