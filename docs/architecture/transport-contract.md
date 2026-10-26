# `xlite-transport` — the transport contract

`crates/xlite-transport` is the in-process, server-side transport client that the standalone xlite gateway and [Nebula](https://github.com/EeroEternal/nebula) share as their upstream data plane ([issue #95](https://github.com/EeroEternal/xlite/issues/95) / [issue #96](https://github.com/EeroEternal/xlite/issues/96)). It replaces Nebula's hand-rolled `reqwest` forward path and keeps the gateway's own `H1Pool` / `xlite-h2-pool` wiring as **one** implementation of HTTP/1.1 and HTTP/2 upstreams.

This page is the contract: what the public API is, what it promises across versions, and — most importantly — what it will **never** do. The code lives in [`crates/xlite-transport`](../../crates/xlite-transport/src/lib.rs).

## The rules the API encodes

1. **`Transport::execute()` is exactly one upstream attempt.** No retry, no failover, no reroute, no cooldown, no fallback provider. The caller's loop is the only retry loop in the stack; a hidden retry would multiply the caller's attempt budget and its metrics. [`execute_redial_once()`](../../crates/xlite-transport/src/transport.rs) is the single, visible, countable exception: at most one redial, only for the `StaleConnection` class (a reused socket that died before the first response byte), and it reports itself through `telemetry.redials` / `redial_reason`.
2. **The URI is per call.** The pool keys on the runtime `scheme://authority` of each `Attempt`; the caller picks the replica for every attempt. There is no catalog channel, no global host list, no routing table in this crate.
3. **Zero parsing.** Request `Bytes` in, `Bytes` stream out. No JSON DOM, no SSE framing, no usage extraction, no model rewrite. Protocol translation is a different crate's job (Phase 2 of #95).
4. **Config is passed in, environment is not read.** No `*_ENV` globals, no config files, no database, no admin API. `cargo tree -p xlite-transport --no-default-features` is gated by [`scripts/check_transport_deps.sh`](../../scripts/check_transport_deps.sh): never `sqlx`, `axum`, `redis`, `serde_yaml`, or the `xlite` binary crate itself.

Four semantics sit on top of those rules:

- **Dropping `BodyStream` cancels upstream** — `RST_STREAM(CANCEL)` on h2, socket close on h1 — so a client abort reaches the engine.
- **Abnormal EOF is an error, never end-of-stream.** A truncated body surfaces as `BodyIncomplete`; a caller can never mistake it for a clean `[DONE]`.
- **Headers pass through verbatim**, duplicates and non-UTF-8 values included, in both directions. The only touches are mechanical (synthesized `content-length`/`host`, RFC 9113 connection-specific strips on h2) and each one is listed per attempt in `telemetry.headers_rewritten`.
- **One failure, one observer.** `TransportConfig::header_timeout` / `stall_timeout` default to `None`: a caller that owns its own first-byte/stall budgets sets them to `None` so the transport never double-fires a clock the caller already gave up on.

## Public API

| Type | Role |
| :--- | :--- |
| `TransportConfig` | Everything the transport may do: connect/header/stall budgets, per-host caps, idle reclaim, `max_parallel_dials`, caller-supplied `rustls::ClientConfig` (or `None` for cleartext). `TransportConfig::cleartext()` is the sane in-process default. |
| `Attempt` | One attempt, fully described: `method`, absolute `uri`, full `HeaderMap`, `body: Bytes`. `Clone`-able (a refcount bump), so the caller prepares it once and reuses it across its retry loop. `Attempt::validate()` rejects bad URIs and contradictory `content-length` **before any socket exists**. |
| `Transport` (trait) | `execute()` — one attempt; `execute_redial_once()` — the sanctioned stale-class exception; `stats()` — a pure-data snapshot, no self-hosted HTTP endpoint. |
| `AttemptOutcome` | `status`, verbatim `headers`, `version`, `body: BodyStream`, `telemetry`. |
| `AttemptTelemetry` | Measured facts only: `dial` (`None` on reuse), `reused`, `pool_wait` (`None` = not measured), `in_flight_at_acquire`, `redials` / `redial_reason`, `headers_rewritten`. |
| `BodyStream` | `Stream<Item = Result<Bytes, TransportError>>`. Drop = cancel. Runs an inter-chunk watchdog **only** if the caller configured `stall_timeout`. `BodyStream::new()` is public so callers can build outcomes in their own `Transport` implementations (tests, alternate backends). |
| `TransportError` | The only failure surface. Typed, `#[non_exhaustive]`, reachable via `Error::source()` — never match on `to_string()`. |
| `TransportStats` / `HostStats` / `Backend` | Per-authority pool telemetry: `connections`, `dials`, `failed_dials`, `reused`, `evicted`, `evicted_stale <= evicted`, `reaped_idle`, `waiting`, wait percentiles. `TransportStats::assert_invariants()` is cheap enough to call on export. |
| `H1Transport` (feature `h1`) | The contract over the extracted HTTP/1.1 keep-alive pool. |
| `H2Transport` (feature `h2`) | The contract over `xlite-h2-pool`; also accepts a caller-supplied `Dial` (`with_dial`) for unix sockets, proxies, test doubles. |
| `configure_tcp_socket`, `tls_host_from_authority` | Shared socket policy (NODELAY + the 30 s keepalive probe budget from #93) and authority → TLS `ServerName` helper, used by both the crate's backends and the gateway. |

### Error taxonomy

| Variant | Meaning | Redial-eligible? |
| :--- | :--- | :--- |
| `InvalidRequest(&'static str)` | The `Attempt` itself is invalid; produced before any socket exists. | never |
| `Dns` / `Connect` / `Tls` / `Io` | Dial-level failure (tagged by `io::ErrorKind`, never string-matched). | never |
| `HeaderTimeout { waited }` | The caller's header budget elapsed. Only when `header_timeout` was set. | never (a live-but-slow server must not see a replayed POST) |
| `Stall { idle }` | The caller's inter-chunk budget elapsed. Only when `stall_timeout` was set. | never |
| `StaleConnection { kind }` | A **reused** connection failed before any response byte. | **yes — the only class `execute_redial_once()` replays, exactly once** |
| `BodyIncomplete` | The stream ended before its announced length. | never |
| `Reset { code }` | The peer reset the stream/connection after response bytes were seen. | never |
| `Cancelled` | The caller dropped the body. | never |
| `Overflow` / `Closed` | A local resource bound was hit / the transport shut down; the attempt never reached the peer. | caller's call |
| `TransportError` itself | Anything else transport-level. | caller's call |

## Telemetry honesty

- **`None` means unknown, never `0`.** A backend that cannot measure reports `None` (`h1`'s `pool_wait`, pre-Phase-1 `reaped_idle`); a backend that measured "nothing happened" reports `Some(0)` / `0` with `wait_samples: 0`. `HostStats` documents which is which per field.
- **Every death path is counted.** `connections` comes from a drop-gauge on the h1 stream handle, so idle reaping, liveness-probe rejection, mid-body close and caller discard all decrement it.
- **`evicted_stale <= evicted`** is an invariant, asserted by `TransportStats::assert_invariants()` and backed by a test per backend.

## Backend differences (same contract, different physics)

| | h1 (`H1Transport`) | h2 (`H2Transport`) |
| :--- | :--- | :--- |
| One connection carries | one exchange per connection (keep-alive reuse) | many multiplexed streams |
| `pool_wait` | `None` — nothing ever parks (keep pool-wait out of any h1 verdict) | `Some(..)` — percentiles over parked acquires |
| `in_flight_at_acquire` | always `0` | streams reserved on the connection |
| `reaped_idle` | `Some(..)` — counted from day one (lazy reap on checkout/put) | `Some(..)` — the pool reaper |
| `max_parallel_dials` | per-authority SYN-burst gate (default 4) | the pool already serialises dials per host |
| `h1_max_conns_per_host` / `h2_max_conns_per_host` | bounds **retained idle** sockets per authority; in-flight exchanges are not capped by it | a real **connection cap**: acquires park at the cap |

## Feature flags

- `h1` — the HTTP/1.1 keep-alive pool.
- `h2` — the HTTP/2 pool (`xlite-h2-pool`).
- `tls-rustls` — accepts a caller-supplied `rustls::ClientConfig`; the crate never builds one itself (roots, ALPN, key logging stay the caller's decision).

`default-features = false` is a supported downstream pin (that is what Nebula uses); each single-backend configuration runs the full transport suite in `scripts/ci_local.sh`, not just a compile.

## Semver & stability promise

The crate is `0.1.0` (pre-1.0). What callers may rely on:

- **`TransportError`, `StaleKind`, `Backend` are `#[non_exhaustive]`.** Variants may be *added* in a minor release; existing variants are never renamed or repurposed. Callers match with a `_` arm and never parse `Display` output — `Display` text is not a contract, `Error::source()` is.
- **The four rules above are the contract.** `execute()` gaining a hidden retry, a routing decision, or a parse would be a breaking change regardless of type signatures; such a change requires an issue discussion and a changelog entry, not a quiet minor bump.
- **Telemetry semantics are stable:** `None` stays "unknown", the `evicted_stale <= evicted` invariant holds, and any new measured field arrives documented and tested.
- Public structs (`TransportConfig`, `Attempt`, `AttemptOutcome`, `AttemptTelemetry`, `HostStats`) expose plain fields; fields are **added**, not changed in meaning, and additions are called out in [`docs/changelog.md`](../changelog.md). Destructure with `..` to stay forward-compatible.

## What this crate will never do

- **Retry, failover, reroute, cool down, or circuit-break.** One `execute()` = one upstream request on the wire, always — including under `execute_redial_once()`, which is one extra request at most and says so in its telemetry.
- **Choose an authority.** The URI is per call; the caller owns replica selection, placement, admission, and budgets.
- **Parse anything.** No JSON, no SSE, no usage/token accounting, no model rewriting, no tool-pause detection.
- **Read the environment or the disk.** No env vars, no config files, no catalog, no SQLite/Redis, no admin API, no billing.
- **Host an HTTP endpoint.** `stats()` is a snapshot the caller exports on its own clock and in its own format.
- **Translate protocols.** Responses/Messages/Embeddings adapters are Phase 2 of #95, in a different crate.
- **Rewrite headers silently.** Anything other than byte-for-byte passthrough appears in `headers_rewritten`; `transfer-encoding` is rejected as `InvalidRequest` rather than dropped.
- **Fire a watchdog the caller already owns.** With `header_timeout: None` / `stall_timeout: None` the transport runs no clock of its own.

## Example

[`crates/xlite-transport/examples/transport-retry-outside.rs`](../../crates/xlite-transport/examples/transport-retry-outside.rs) is Nebula's loop shape end to end: build one `Attempt` → `execute_redial_once()` → first-chunk deadline → **commit the status** → stream the body, with the retry loop visibly *outside* the crate and no retry at all after the commit point.

## Related

- [`docs/architecture.md`](../architecture.md) — gateway data plane, routing/failover, `xlite-h2-pool`.
- [`architecture/module-boundaries.md`](module-boundaries.md) — crate and module rules.
- [`docs/manual/xlite.md`](../manual/xlite.md) — operator-facing pool knobs (`XLITE_UPSTREAM_MAX_CONNS_PER_HOST`, timeouts).
- [`docs/changelog.md`](../changelog.md) — the #96 delivery history.
