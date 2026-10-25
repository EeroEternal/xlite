//! `evicted` semantics: sockets the pool stopped trusting, not routine turnover.
//!
//! A peer may close an idle connection at any moment — that must not count as
//! an eviction (it is neither housekeeping nor a failure). An end that
//! interrupts a stream still in flight must count. Both branches of the
//! driver-exit path are covered: `pinned_poll` off (`pool.rs`) and on
//! (`pump.rs`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use xlite_h2_pool::{BoxIo, Config, Dial, Pool, PoolStats};

/// A pool whose server side can be killed mid-test, plus the dial counter.
/// The receiver yields the server task's handle the first time `dial` runs.
fn closable_pool(
    pinned_poll: bool,
) -> (
    Pool,
    Arc<AtomicUsize>,
    tokio::sync::oneshot::Receiver<tokio::task::JoinHandle<()>>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let dials = Arc::new(AtomicUsize::new(0));
    let dial: Dial = Arc::new({
        let tx = tx.clone();
        let dials = dials.clone();
        move |_host: String| {
            let tx = tx.clone();
            let dials = dials.clone();
            Box::pin(async move {
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                let server = tokio::spawn(async move {
                    let Ok(mut conn) = h2::server::Builder::new()
                        .max_concurrent_streams(10u32)
                        .handshake::<_, bytes::Bytes>(server_io)
                        .await
                    else {
                        return;
                    };
                    // Hold the connection open; the test kills this task.
                    let _ = conn.accept().await;
                });
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(server);
                }
                dials.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(client_io) as BoxIo)
            })
        }
    });
    let pool = Pool::new(Config {
        max_conns_per_host: 4,
        stream_budget_per_conn: xlite_h2_pool::DEFAULT_STREAM_BUDGET_PER_CONN,
        pinned_poll,
        dial,
        idle_timeout: xlite_h2_pool::DEFAULT_IDLE_TIMEOUT,
    });
    (pool, dials, rx)
}

/// Poll until the connection has left the pool (or time out).
async fn wait_until_empty(pool: &Pool) -> PoolStats {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let stats = pool.stats("upstream");
            if stats.connections == 0 {
                break stats;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("connection never left the pool")
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_peer_close_is_not_an_eviction() {
    for pinned_poll in [false, true] {
        let (pool, dials, rx) = closable_pool(pinned_poll);
        let lease = pool.acquire("upstream").await.unwrap();
        drop(lease);
        assert_eq!(
            pool.stats("upstream").connections,
            1,
            "pinned={pinned_poll}"
        );

        // Routine peer close with no stream in flight: turnover, not distrust.
        rx.await.unwrap().abort();

        let stats = wait_until_empty(&pool).await;
        assert_eq!(
            stats.evicted, 0,
            "idle close must not evict, pinned={pinned_poll}"
        );
        assert_eq!(
            stats.reclaimed, 0,
            "the reaper did not run, pinned={pinned_poll}"
        );
        assert_eq!(dials.load(Ordering::SeqCst), 1, "pinned={pinned_poll}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn driver_end_with_stream_in_flight_is_an_eviction() {
    for pinned_poll in [false, true] {
        let (pool, dials, rx) = closable_pool(pinned_poll);
        let lease = pool.acquire("upstream").await.unwrap();

        // Kill the peer while the lease is held: the stream is interrupted.
        rx.await.unwrap().abort();

        let stats = wait_until_empty(&pool).await;
        assert_eq!(
            stats.evicted, 1,
            "interrupted stream must evict, pinned={pinned_poll}"
        );
        assert_eq!(
            stats.reclaimed, 0,
            "the reaper did not run, pinned={pinned_poll}"
        );
        assert_eq!(dials.load(Ordering::SeqCst), 1, "pinned={pinned_poll}");
        drop(lease);
    }
}
