//! Print the `SETTINGS_MAX_CONCURRENT_STREAMS` an h2c upstream advertises.
//!
//! Ground truth for sizing the transport admission gate: the gateway pool
//! executes `min(XLITE_UPSTREAM_STREAM_CAP, advertised)`, so point this at the
//! real mock farm before picking a cap. See `docs/manual/lab-loadtest.md`
//! ("上游流闸门") and `Config::stream_budget_per_conn` in this crate.
//!
//! ```text
//! cargo run -p xlite-h2-pool --example h2probe -- 10.99.255.102:4101
//! ```

use std::future::poll_fn;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use xlite_h2_pool::client_builder;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args()
        .nth(1)
        .ok_or("usage: h2probe <host:port>  (h2c prior knowledge)")?;
    let tcp = tokio::net::TcpStream::connect(&addr).await?;
    tcp.set_nodelay(true)?;
    let (_send, mut conn) = client_builder().handshake::<_, bytes::Bytes>(tcp).await?;

    // Same dance as the pool's `read_peer_stream_cap`: drive the connection
    // until the peer's first SETTINGS lands, then report the advertised cap.
    let cap = tokio::time::timeout(
        Duration::from_secs(2),
        poll_fn(|cx| {
            let known = conn.max_concurrent_send_streams();
            if known != usize::MAX {
                return Poll::Ready(Some(known));
            }
            match Pin::new(&mut conn).poll(cx) {
                // Closed before advertising anything: the pool falls back to 250.
                Poll::Ready(_) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            }
        }),
    )
    .await
    .map_err(|_| format!("{addr}: no SETTINGS frame within 2s"))?;
    match cap {
        Some(n) => println!("{addr}: SETTINGS_MAX_CONCURRENT_STREAMS = {n}"),
        None => println!("{addr}: closed before advertising (pool falls back to 250)"),
    }
    Ok(())
}
