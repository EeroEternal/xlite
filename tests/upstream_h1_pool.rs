//! Regression tests for cleartext `http://` upstreams: they must go through the
//! native h1 pool. (The reqwest + request-level gate path was removed in the
//! unification; `https` still uses reqwest.)

use bytes::Bytes;
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use xlite::config::Config;
use xlite::upstream::Upstream;

async fn start_mock() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(serve(stream));
        }
    });
    format!("127.0.0.1:{}", addr.port())
}

/// Minimal keep-alive HTTP/1.1 server returning a fixed JSON body.
async fn serve(mut stream: TcpStream) {
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        while !pending.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                return;
            }
            pending.extend_from_slice(&buf[..n]);
        }
        let head_end = pending.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&pending[..head_end]).to_ascii_lowercase();
        pending.drain(..head_end + 4);
        let cl = head
            .split("\r\n")
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while pending.len() < cl {
            let n = stream.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                return;
            }
            pending.extend_from_slice(&buf[..n]);
        }
        pending.drain(..cl);
        let body = b"{\"ok\":true}";
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n",
            body.len()
        )
        .into_bytes();
        resp.extend_from_slice(body);
        if stream.write_all(&resp).await.is_err() || stream.flush().await.is_err() {
            return;
        }
    }
}

fn cfg() -> Config {
    Config {
        database_url: "memory".into(),
        redis_url: None,
        listen: "127.0.0.1:0".into(),
        h2c: false,
        admin_password: "x".into(),
        session_secret: None,
        max_conns_per_host: 4,
        h1_max_conns_per_host: 4,
        upstream_stream_cap: usize::MAX,
        catalog_reload_secs: 0,
        upstream_ttft_header: false,
        upstream_stall_timeout: std::time::Duration::ZERO,
        upstream_header_timeout: std::time::Duration::ZERO,
    }
}

async fn call(up: &Upstream, host: &str) -> (u16, Vec<u8>) {
    let endpoint = format!("http://{host}/v1/chat/completions");
    let resp = up
        .post_stream(
            &endpoint,
            host,
            "/v1/chat/completions",
            None,
            &[],
            Bytes::from_static(b"{}"),
        )
        .await
        .unwrap();
    let status = resp.status;
    let mut body = resp.body;
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    (status, out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleartext_http_goes_through_the_native_h1_pool() {
    let host = start_mock().await;
    let up = Upstream::new(&cfg());

    let (status, body) = call(&up, &host).await;
    assert_eq!(status, 200);
    assert_eq!(&body[..], b"{\"ok\":true}");

    let stats = up.h1_pool_stats();
    assert_eq!(stats.len(), 1, "the h1 pool must report the host it served");
    assert_eq!(stats[0].0, host);
    assert_eq!(stats[0].1.connections, 1);
    assert_eq!(
        stats[0].1.in_flight, 0,
        "the lease is released once the body is read"
    );
}
