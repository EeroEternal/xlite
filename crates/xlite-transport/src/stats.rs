//! Pool telemetry: transport-owned snapshot, exact where the backend can be
//! exact and `None` where it cannot — unknown is never reported as zero.

/// Which backend serves one authority.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    H1,
    H2,
}

/// One authority's connection-pool state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostStats {
    pub authority: String,
    /// The backend this row was projected from (h2 and h1 are separate
    /// backends; a host can briefly have both during ALPN fallback).
    pub backend: Backend,
    /// Live connections.
    pub connections: u64,
    /// Streams currently reserved.
    pub in_flight: u64,
    /// Connections dialled over this host's lifetime.
    pub dials: u64,
    /// Acquires whose dial failed (the transport never retries internally;
    /// this is a fact about the upstream, not an attempt counter).
    pub failed_dials: u64,
    /// Acquires served from an already-established connection.
    pub reused: u64,
    /// Connections the pool stopped trusting (header timeout, transport error
    /// on a reused connection, end while a stream was still in flight).
    pub evicted: u64,
    /// Subset of `evicted` that was the stale-before-first-byte class —
    /// invariant: `evicted_stale <= evicted`.
    pub evicted_stale: u64,
    /// Idle connections reclaimed by the backend's reaper. `None` = the
    /// backend does not count reclamation (h1 reaps lazily on checkout/put in
    /// Phase 1) — not zero.
    pub reaped_idle: Option<u64>,
    /// Acquirers parked on a full pool right now.
    pub waiting: u64,
    /// Percentiles of the time acquires actually spent parked (a fresh,
    /// immediate acquire is measured as `Some(0)` on the lease but is not a
    /// queue-wait sample). `None` = nothing has parked yet, or the backend
    /// does not measure wait — `wait_samples` on the same row is what tells
    /// those apart for h2; h1 reports `None` in Phase 1 (not measured).
    pub wait_ms_p50: Option<u64>,
    pub wait_ms_p99: Option<u64>,
    /// Acquires that were sampled for the percentiles (had to park at least
    /// once). `0` with `wait_ms_p50: None` = measured, nothing to report.
    pub wait_samples: u64,
}

/// A point-in-time snapshot across every host this transport has touched.
///
/// Pure in-process data: no self-hosted HTTP endpoint. Collected at the
/// caller's request; the caller stamps its own wall clock when exporting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransportStats {
    pub hosts: Vec<HostStats>,
}

impl TransportStats {
    /// Invariant shared by every backend: the stale subset never exceeds the
    /// whole. Backed by a test per backend; cheap enough to assert on export.
    pub fn assert_invariants(&self) {
        for host in &self.hosts {
            assert!(
                host.evicted_stale <= host.evicted,
                "host {}: evicted_stale {} > evicted {}",
                host.authority,
                host.evicted_stale,
                host.evicted
            );
        }
    }
}

/// Percentile from a power-of-two bucketed histogram (`xlite-h2-pool`'s
/// `WAIT_BUCKETS` layout: bucket `i` covers `[2^i, 2^(i+1))` ms).
#[allow(dead_code)] // used by the feature-gated backends
pub(crate) fn bucket_percentile(buckets: &[u64], pct: u64) -> Option<u64> {
    let total: u64 = buckets.iter().sum();
    if total == 0 {
        return None;
    }
    let rank = total * pct / 100;
    let mut seen = 0u64;
    for (i, &count) in buckets.iter().enumerate() {
        seen += count;
        if seen > rank {
            return Some(if i == 0 { 0 } else { 1u64 << i });
        }
    }
    Some(1u64 << (buckets.len() - 1))
}
