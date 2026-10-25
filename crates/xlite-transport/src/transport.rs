//! The [`Transport`] contract: one call, one upstream attempt.

use http::{HeaderMap, StatusCode, Version};

use crate::attempt::Attempt;
use crate::body::BodyStream;
use crate::error::{StaleKind, TransportError};
use crate::stats::TransportStats;

/// Per-attempt observations. Every field is a measured fact, not a policy.
#[derive(Clone, Debug, Default)]
pub struct AttemptTelemetry {
    /// Time spent establishing a new connection for this attempt, or `None`
    /// when a pooled connection was reused (no dial happened).
    pub dial: Option<std::time::Duration>,
    /// Whether the stream ran on an already-established connection.
    pub reused: bool,
    /// Time this acquire was parked behind a full pool. `Some(0)` = acquired
    /// immediately. `None` = the backend does not measure wait.
    pub pool_wait: Option<std::time::Duration>,
    /// Streams reserved on the connection at acquire time. The h1 backend
    /// reports `0`: HTTP/1.1 serves one exchange per connection, there is
    /// nothing multiplexed to count.
    pub in_flight_at_acquire: usize,
    /// How many times the caller's helper redialled before this outcome (0
    /// for a bare [`Transport::execute`]).
    pub redials: u8,
    /// Why the redial happened, when `redials > 0`.
    pub redial_reason: Option<StaleKind>,
    /// Headers the transport added, removed or synthesized on the way out
    /// (e.g. `content-length`, `host`, RFC 9113 connection-specific strips on
    /// h2). Everything not listed here went out byte-for-byte. Empty when the
    /// attempt went out untouched (no allocation).
    pub headers_rewritten: Vec<&'static str>,
}

/// The successful half of one attempt.
#[derive(Debug)]
pub struct AttemptOutcome {
    pub status: StatusCode,
    /// Response headers verbatim, duplicates preserved.
    pub headers: HeaderMap,
    pub version: Version,
    /// Raw body stream; dropping it cancels upstream.
    pub body: BodyStream,
    pub telemetry: AttemptTelemetry,
}

/// A server-side transport client: **exactly one upstream attempt** per
/// `execute()` — no retry, no failover, no reroute, no cooldown. The caller's
/// loop is the only retry loop in the stack.
///
/// Implementations are used generically (the defaulted
/// [`execute_redial_once`](Transport::execute_redial_once) keeps this trait
/// non-object-safe on purpose: the redial policy must stay visible in the
/// caller's code path).
#[allow(async_fn_in_trait)]
pub trait Transport: Send + Sync {
    /// Execute one attempt against the authority in `attempt.uri`.
    async fn execute(&self, attempt: Attempt) -> Result<AttemptOutcome, TransportError>;

    /// Snapshot of the underlying pool(s). Pure data, no HTTP surface.
    fn stats(&self) -> TransportStats;

    /// Like [`execute`](Transport::execute), but with **at most one** redial,
    /// only for the [`TransportError::StaleConnection`] class (a reused
    /// connection that failed before the first response byte), rebuilt from
    /// the original `Attempt` so headers are never double-applied.
    ///
    /// Never redials on `HeaderTimeout` / `Stall` / `Tls` /
    /// `BodyIncomplete` / `Reset`-after-bytes / `InvalidRequest`: those are
    /// either observable by the caller's own budget, non-idempotent to
    /// replay, or a caller bug. `StaleConnection` is the only class where a
    /// pooled-socket race is a known, benign-enough hazard of keep-alive
    /// (matching the standalone gateway's behaviour, made visible by
    /// `redials` / `redial_reason`).
    async fn execute_redial_once(&self, attempt: Attempt) -> Result<AttemptOutcome, TransportError>
    where
        Self: Sized,
    {
        match self.execute(attempt.clone()).await {
            Err(TransportError::StaleConnection { kind }) => {
                tracing::debug!(?kind, "redialing once on stale pooled connection");
                let mut outcome = self.execute(attempt).await?;
                outcome.telemetry.redials = 1;
                outcome.telemetry.redial_reason = Some(kind);
                Ok(outcome)
            }
            other => other,
        }
    }
}
