//! Per-connection driver + stream pump, merged into **one** task.
//!
//! Rationale: an h2 connection's
//! shared state lock is taken both by the *connection driver* (reads the socket,
//! dispatches frames) and by `RecvStream::poll_data` (the pump). Running those in
//! two tasks left ~17% of gateway CPU in that lock. Driving the connection and
//! polling its streams from a single task makes the lock's owner count **one**,
//! while the connection count stays exactly what the pool dialed (no ×8).
//!
//! Enabled by [`Config::pinned_poll`](crate::Config::pinned_poll). The wire
//! layer currently always enables it (`src/upstream.rs` hardcodes
//! `pinned_poll: true`; there is no env override); the pool crate itself has no
//! default and just honours the flag.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::future::poll_fn;
use futures_util::stream::FuturesUnordered;
use h2::RecvStream;
use tokio::sync::{mpsc, watch};

use crate::pool::{BoxIo, ConnEntry, HostState, bump, publish_peer_cap};

/// Chunks a request task may lag behind the pump before the pump applies
/// backpressure: it stops polling that stream, which withholds the h2
/// flow-control release and pushes back to the upstream.
const BODY_CHANNEL: usize = 16;

/// How many streams may be handed over before a caller waits for the pump.
const REGISTER_CHANNEL: usize = 256;

/// Release the h2 flow-control window once this many bytes accumulate. Matches
/// the wire body's `RELEASE_THRESHOLD`.
const RELEASE_THRESHOLD: usize = 8 * 1024;

/// A stream handed to the connection's task, with the channel its bytes go to.
pub(crate) struct Register {
    recv: RecvStream,
    out: mpsc::Sender<Result<Bytes, String>>,
}

/// The channel a connection's request tasks register their `RecvStream`s on.
pub(crate) fn register_channel() -> (mpsc::Sender<Register>, mpsc::Receiver<Register>) {
    mpsc::channel(REGISTER_CHANNEL)
}

/// Hand `recv` to the connection's task; the returned receiver yields its chunks
/// and then ends. A pump-side error arrives as `Err(message)`.
pub(crate) async fn register(
    tx: &mpsc::Sender<Register>,
    recv: RecvStream,
) -> mpsc::Receiver<Result<Bytes, String>> {
    let (out, body) = mpsc::channel(BODY_CHANNEL);
    if let Err(err) = tx.send(Register { recv, out }).await {
        // The connection task is gone. Fail the stream loudly rather than
        // handing back an empty body.
        let reg = err.0;
        let _ = reg
            .out
            .send(Err("native h2 connection task stopped".to_string()))
            .await;
    }
    body
}

/// Drive `conn` **and** pump its streams from one task, then drop the
/// connection from its host.
///
/// The entry is dropped from the host as soon as the driver ends — *before* the
/// pump is waited on. Waiting on both first would deadlock: the pump only exits
/// once every `Register` sender is gone, and the last sender lives in the
/// `ConnEntry` the driver is about to unpublish.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drive_and_pump(
    mut conn: h2::client::Connection<BoxIo, Bytes>,
    cap: Arc<AtomicUsize>,
    cap_updates: Arc<AtomicU64>,
    generation_tx: watch::Sender<u64>,
    registers: mpsc::Receiver<Register>,
    state_weak: std::sync::Weak<HostState>,
    entry_weak: std::sync::Weak<ConnEntry>,
) {
    let driver = async {
        let _ = poll_fn(|cx| {
            let poll = Pin::new(&mut conn).poll(cx);
            if publish_peer_cap(&cap, conn.max_concurrent_send_streams()) {
                cap_updates.fetch_add(1, Ordering::Relaxed);
                bump(&generation_tx);
            }
            poll
        })
        .await;

        // Unpublish now (see the doc comment): this releases the host's strong
        // reference, so the last `Register` sender can drop and the pump can
        // finish. `entry` is dropped at the end of this block.
        if let (Some(state), Some(entry)) = (state_weak.upgrade(), entry_weak.upgrade()) {
            state.retain(|c| !Arc::ptr_eq(c, &entry));
            bump(&state.generation_tx);
        }
    };
    tokio::join!(driver, pump(registers));
}

/// Poll every registered stream to its end, forwarding chunks. Dropping a
/// stream's `out` closes that request task's receiver, ending its body.
async fn pump(mut registers: mpsc::Receiver<Register>) {
    let mut streams: FuturesUnordered<_> = FuturesUnordered::new();
    let mut open = true;
    loop {
        if !open && streams.is_empty() {
            return;
        }
        tokio::select! {
            biased;
            reg = registers.recv(), if open => match reg {
                Some(r) => streams.push(pump_stream(r)),
                None => open = false,
            },
            _ = streams.next(), if !streams.is_empty() => {}
        }
    }
}

async fn pump_stream(reg: Register) {
    let mut recv = reg.recv;
    let out = reg.out;
    let mut pending = 0usize;
    loop {
        let next = recv.next().await;
        match next {
            Some(Ok(bytes)) => {
                pending += bytes.len();
                if pending >= RELEASE_THRESHOLD {
                    let _ = recv.flow_control().release_capacity(pending);
                    pending = 0;
                }
                if out.send(Ok(bytes)).await.is_err() {
                    break; // request task is gone; stop polling this stream
                }
            }
            Some(Err(err)) => {
                let _ = out.send(Err(err.to_string())).await;
                break;
            }
            None => break,
        }
    }
    if pending > 0 {
        let _ = recv.flow_control().release_capacity(pending);
    }
}
