//! Nebula's loop shape, with the retry loop **outside** the crate (issue #96).
//!
//! ```text
//! cargo run -p xlite-transport --example transport-retry-outside -- \
//!     http://10.0.0.11:8000 http://10.0.0.12:8000
//! ```
//!
//! Each argument is one replica base URL. The shape a caller must own:
//!
//! 1. build one [`Attempt`] — clone it per replica (cheap: a `Bytes` bump);
//! 2. one [`Transport::execute_redial_once`] per replica — one upstream
//!    request each, at most one stale-socket redial, visible in telemetry;
//! 3. a **first-chunk deadline** before any status reaches the client;
//! 4. **commit**: after the first chunk the status is visible downstream and
//!    this loop must never retry again — a mid-stream failure is reported,
//!    not replayed.
//!
//! The transport itself never retries, never parses, never picks a replica.

use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Method, Uri};
use xlite_transport::{Attempt, H1Transport, Transport, TransportConfig};

/// One attempt's first-byte budget (Nebula's `NEBULA_PROXY_FIRST_BYTE_TIMEOUT_MS`).
const FIRST_CHUNK_DEADLINE: Duration = Duration::from_secs(60);

const BODY: &[u8] =
    br#"{"model":"deepseek-v3","stream":true,"messages":[{"role":"user","content":"ping"}]}"#;

#[tokio::main]
async fn main() {
    let replicas: Vec<String> = std::env::args().skip(1).collect();
    let replicas = if replicas.is_empty() {
        vec!["http://127.0.0.1:8000".to_string()]
    } else {
        replicas
    };

    let transport = H1Transport::new(TransportConfig::cleartext());

    // Prepared once, reused across every attempt: the body is not
    // re-serialised per replica, and duplicate header names survive verbatim.
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "authorization",
        HeaderValue::from_static("Bearer sk-replace-me"),
    );
    headers.append("x-trace", HeaderValue::from_static("a"));
    headers.append("x-trace", HeaderValue::from_static("b"));
    let body = Bytes::from_static(BODY);

    for replica in &replicas {
        let uri: Uri = format!("{replica}/v1/chat/completions")
            .parse()
            .expect("replica must be an absolute http(s) base URL");
        let attempt = Attempt {
            method: Method::POST,
            uri,
            headers: headers.clone(),
            body: body.clone(),
        };

        // ---- one attempt (at most one stale-socket redial, self-reported) ----
        let outcome = match transport.execute_redial_once(attempt).await {
            Ok(outcome) => outcome,
            Err(err) => {
                eprintln!("[{replica}] attempt failed before headers: {err}");
                continue; // this loop decides: try the next replica
            }
        };

        // ---- first-chunk deadline: the status is NOT yet committed ----
        let mut stream = outcome.body;
        let first = match tokio::time::timeout(FIRST_CHUNK_DEADLINE, stream.next()).await {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(Some(Err(err))) => {
                eprintln!("[{replica}] no usable first chunk: {err}");
                continue; // dropping the body cancels upstream
            }
            Ok(None) => Bytes::new(), // headers plus a clean empty body
            Err(_) => {
                eprintln!(
                    "[{replica}] first chunk missed the deadline; dropping the body cancels upstream"
                );
                continue;
            }
        };

        // ==== COMMIT: status + headers go downstream from here. Never retry again. ====
        println!(
            "[{replica}] committed: {} {:?}",
            outcome.status, outcome.version
        );
        for (name, value) in &outcome.headers {
            println!("  {name}: {}", String::from_utf8_lossy(value.as_bytes()));
        }
        print!("{}", String::from_utf8_lossy(&first));
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => print!("{}", String::from_utf8_lossy(&chunk)),
                Err(err) => {
                    eprintln!(
                        "\n[{replica}] mid-stream failure after commit (reported, never replayed): {err}"
                    );
                    stats_snapshot(&transport);
                    std::process::exit(1);
                }
            }
        }
        println!();
        stats_snapshot(&transport);
        return;
    }

    eprintln!("every replica failed before the first chunk");
    stats_snapshot(&transport);
    std::process::exit(1);
}

/// `stats()` is a pure snapshot: the caller stamps its own clock and format.
fn stats_snapshot(transport: &H1Transport) {
    let stats = transport.stats();
    stats.assert_invariants();
    let conns: u64 = stats.hosts.iter().map(|h| h.connections).sum();
    println!(
        "pool: {} host(s), {conns} live connection(s)",
        stats.hosts.len()
    );
}
