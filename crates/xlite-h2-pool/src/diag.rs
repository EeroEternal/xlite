//! Lightweight diagnostics for the data path: the observability half that
//! [`crate::Pool::stats`] never had (per-connection stream distribution is
//! read-side free; these are the write-side counters).
//!
//! Design constraints (plan §4 / GPT-6):
//! * **sampled**: only streams that claim one of [`SLOTS`] progress slots are
//!   tracked per-chunk. A stuck stream holds its slot for as long as it is
//!   stuck, so stalls stay visible while fast streams churn through the rest.
//! * **per-connection / per-slot aggregation**: no per-chunk logging, no global
//!   lock; every hot-path write is one relaxed atomic store on memory the
//!   stream already owns.
//! * **read side is free**: [`Diag::snapshot`] walks the small slot array.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::pool::monotonic_ms;

/// Concurrent streams tracked with per-chunk progress. 64 is plenty for a
/// stall signature and keeps the walk trivial.
pub const SLOTS: usize = 64;

/// One sampled stream's progress. All stores are `Relaxed`: this is telemetry,
/// ordering against other data is not the point.
struct Slot {
    /// 0 = free, 1 = held. Claimed once per stream, released at stream end.
    state: AtomicUsize,
    start_ms: AtomicU64,
    last_data_ms: AtomicU64,
    bytes: AtomicU64,
    /// `0` until the downstream cancel is observed on this stream.
    cancel_ms: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            start_ms: AtomicU64::new(0),
            last_data_ms: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            cancel_ms: AtomicU64::new(0),
        }
    }
}

/// Pool-wide diagnostics shared by every connection task.
pub struct Diag {
    /// `Register`s sent but not yet picked up by a pump — i.e. how deep the
    /// per-connection register channel is.
    registers_pending: AtomicUsize,
    /// Streams a pump currently owns (picked up, not yet finished).
    pumps_active: AtomicUsize,
    /// Streams fully pumped since start.
    streams_pumped: AtomicU64,
    /// Downstream cancels observed (`out.closed()` taken) since start.
    cancels_seen: AtomicU64,
    /// Cancel→end span of the most recent cancelled stream (ms).
    cancel_prop_last_ms: AtomicU64,
    /// Worst cancel→end span seen (ms).
    cancel_prop_max_ms: AtomicU64,
    slots: [Slot; SLOTS],
    /// Round-robin hint so streams do not all claim slot 0 first.
    next_slot: AtomicUsize,
}

/// A claimed progress slot (opaque index).
#[derive(Clone, Copy)]
pub struct SlotHandle(usize);

/// One sampled stream, as reported by [`Diag::snapshot`].
#[derive(Debug)]
pub struct SlotSnap {
    pub start_ms: u64,
    pub last_data_ms: u64,
    pub bytes: u64,
    pub cancel_ms: u64,
}

/// Pool-wide diagnostics snapshot.
#[derive(Debug)]
pub struct DiagSnap {
    pub registers_pending: usize,
    pub pumps_active: usize,
    pub streams_pumped: u64,
    pub cancels_seen: u64,
    pub cancel_prop_last_ms: u64,
    pub cancel_prop_max_ms: u64,
    /// Held slots only (streams still in flight). `None` entries are free.
    pub slots: Vec<SlotSnap>,
}

impl Default for Diag {
    fn default() -> Self {
        Self::new()
    }
}

impl Diag {
    pub fn new() -> Self {
        Self {
            registers_pending: AtomicUsize::new(0),
            pumps_active: AtomicUsize::new(0),
            streams_pumped: AtomicU64::new(0),
            cancels_seen: AtomicU64::new(0),
            cancel_prop_last_ms: AtomicU64::new(0),
            cancel_prop_max_ms: AtomicU64::new(0),
            slots: std::array::from_fn(|_| Slot::new()),
            next_slot: AtomicUsize::new(0),
        }
    }

    /// Claim a progress slot for a stream being registered; `None` when all
    /// slots are held (that stream is simply not sampled).
    pub fn attach(self: &Arc<Self>) -> Option<SlotHandle> {
        let start = self.next_slot.fetch_add(1, Ordering::Relaxed);
        for i in 0..SLOTS {
            let idx = (start.wrapping_add(i)) % SLOTS;
            if self.slots[idx]
                .state
                .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                let now = monotonic_ms();
                self.slots[idx].start_ms.store(now, Ordering::Relaxed);
                self.slots[idx].last_data_ms.store(now, Ordering::Relaxed);
                self.slots[idx].bytes.store(0, Ordering::Relaxed);
                self.slots[idx].cancel_ms.store(0, Ordering::Relaxed);
                return Some(SlotHandle(idx));
            }
        }
        None
    }

    /// A `Register` entered the per-connection channel.
    pub fn on_register_sent(&self) {
        self.registers_pending.fetch_add(1, Ordering::Relaxed);
    }

    /// The `Register` never made it into the channel (connection task gone).
    pub fn on_register_fail(&self, slot: Option<SlotHandle>) {
        self.registers_pending.fetch_sub(1, Ordering::Relaxed);
        if let Some(slot) = slot {
            self.slots[slot.0].state.store(0, Ordering::Release);
        }
    }

    /// A pump picked the `Register` up (channel drained for it).
    pub fn on_register_taken(&self) {
        self.registers_pending.fetch_sub(1, Ordering::Relaxed);
        self.pumps_active.fetch_add(1, Ordering::Relaxed);
    }

    /// Per-chunk progress for a sampled stream (only called when a slot is held).
    pub fn note_chunk(&self, slot: SlotHandle, len: usize) {
        let s = &self.slots[slot.0];
        s.last_data_ms.store(monotonic_ms(), Ordering::Relaxed);
        s.bytes.fetch_add(len as u64, Ordering::Relaxed);
    }

    /// The downstream cancel was observed (`out.closed()` taken). Counted for
    /// every stream; sampled streams also get a timestamp for the span.
    pub fn note_cancel(&self, slot: Option<SlotHandle>) {
        self.cancels_seen.fetch_add(1, Ordering::Relaxed);
        if let Some(slot) = slot {
            // `0` is the "no cancel" sentinel; a cancel observed at clock 0
            // must not collide with it.
            self.slots[slot.0]
                .cancel_ms
                .store(monotonic_ms().max(1), Ordering::Relaxed);
        }
    }

    /// The pump finished a stream: account it and free the slot. The
    /// cancel→end span recorded here is the *internal* propagation cost
    /// (cancel observed → `RecvStream` dropped / RST queued); wire latency is
    /// measured separately in the field certification (§5.4/5.5).
    pub fn note_stream_end(&self, slot: Option<SlotHandle>) {
        self.pumps_active.fetch_sub(1, Ordering::Relaxed);
        self.streams_pumped.fetch_add(1, Ordering::Relaxed);
        if let Some(slot) = slot {
            let s = &self.slots[slot.0];
            let cancel = s.cancel_ms.load(Ordering::Relaxed);
            if cancel > 0 {
                let span = monotonic_ms().saturating_sub(cancel);
                self.cancel_prop_last_ms.store(span, Ordering::Relaxed);
                self.cancel_prop_max_ms.fetch_max(span, Ordering::Relaxed);
            }
            s.state.store(0, Ordering::Release);
        }
    }

    /// Read-side snapshot; walks only the slot array and a few counters.
    pub fn snapshot(&self) -> DiagSnap {
        let mut slots = Vec::new();
        for s in &self.slots {
            if s.state.load(Ordering::Acquire) == 1 {
                slots.push(SlotSnap {
                    start_ms: s.start_ms.load(Ordering::Relaxed),
                    last_data_ms: s.last_data_ms.load(Ordering::Relaxed),
                    bytes: s.bytes.load(Ordering::Relaxed),
                    cancel_ms: s.cancel_ms.load(Ordering::Relaxed),
                });
            }
        }
        DiagSnap {
            registers_pending: self.registers_pending.load(Ordering::Relaxed),
            pumps_active: self.pumps_active.load(Ordering::Relaxed),
            streams_pumped: self.streams_pumped.load(Ordering::Relaxed),
            cancels_seen: self.cancels_seen.load(Ordering::Relaxed),
            cancel_prop_last_ms: self.cancel_prop_last_ms.load(Ordering::Relaxed),
            cancel_prop_max_ms: self.cancel_prop_max_ms.load(Ordering::Relaxed),
            slots,
        }
    }
}

/// The per-connection slice of [`crate::PoolStats`]: read straight off the
/// connection's existing atomics, so it costs the data path nothing.
#[derive(Debug, PartialEq)]
pub struct ConnSnap {
    pub in_flight: usize,
    pub cap: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_claimed_until_full_then_free_on_release() {
        let d = Arc::new(Diag::new());
        let mut held = Vec::new();
        for _ in 0..SLOTS {
            held.push(d.attach().expect("slot available below SLOTS"));
        }
        assert!(d.attach().is_none(), "SLOTS streams are already tracked");
        d.note_stream_end(Some(held.pop().unwrap()));
        assert!(d.attach().is_some(), "released slot is reusable");
    }

    #[test]
    fn chunk_and_cancel_progress_is_reported() {
        let d = Arc::new(Diag::new());
        d.on_register_sent();
        d.on_register_taken();
        let slot = d.attach().unwrap();
        d.note_chunk(slot, 11);
        d.note_chunk(slot, 31);
        d.note_cancel(Some(slot));
        d.note_stream_end(Some(slot));
        let snap = d.snapshot();
        assert_eq!(snap.pumps_active, 0);
        assert_eq!(snap.streams_pumped, 1);
        assert_eq!(snap.cancels_seen, 1, "every cancel is counted");
        assert!(snap.slots.is_empty(), "slots free at stream end");
    }

    #[test]
    fn cancel_span_is_recorded_on_end() {
        let d = Arc::new(Diag::new());
        let slot = d.attach().unwrap();
        d.note_cancel(Some(slot));
        std::thread::sleep(std::time::Duration::from_millis(10));
        d.note_stream_end(Some(slot));
        let snap = d.snapshot();
        assert!(
            snap.cancel_prop_last_ms >= 5,
            "span measured, got {}",
            snap.cancel_prop_last_ms
        );
        assert!(snap.cancel_prop_max_ms >= snap.cancel_prop_last_ms);
    }

    #[test]
    fn untracked_stream_writes_nothing() {
        let d = Arc::new(Diag::new());
        // No slot claimed: the pump must still work, just without progress.
        d.on_register_taken();
        d.note_stream_end(None);
        let snap = d.snapshot();
        assert_eq!(snap.streams_pumped, 1);
        assert_eq!(snap.pumps_active, 0);
    }
}
