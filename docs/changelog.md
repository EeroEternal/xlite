# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.4] - 2026-10-17

- **Lock-free multi-host pool lookup & zero-copy HTTP/1.1 vectored writes (`crates/xlite-h2-pool/src/pool.rs`, `src/upstream.rs`)**: Replaced the single-entry `last_host` cache and `Mutex<HashMap<...>>` in `xlite-h2-pool`, `H1Pool`, and `h1_tls_hosts` with `ArcSwap`-backed maps/sets so concurrent routing across multiple upstream authorities executes with zero mutex contention and zero per-request `String` allocations; switched cleartext HTTP/1.1 request forwarding to `write_vectored` (`writev`) to send request headers and bodies in a single syscall without intermediate buffer copying.
- **Transparent 1x redial on stale reused HTTP/2 connections (`crates/xlite-h2-pool/src/lease.rs`, `src/upstream.rs`)**: Added `Lease::is_reused()` and `Lease::evict()` so that when a warm pooled HTTP/2 connection (`h2c` or `ALPN: h2`) is silently dropped by an upstream or intermediate NAT/firewall before response headers arrive, `xlite` immediately evicts the dead connection and transparently redials once before triggering channel failover or transient cooldown.
- **Channel cooldown preservation across catalog reloads (`src/store.rs`)**: `Store::reload()` now carries over active `cooldown_until_ms` deadlines for channels whose `id`, `base_url`, `api_key`, and `upstream_model` are unchanged, while immediately clearing cooldowns when a channel's target or credentials are updated by an operator.

## [0.6.3] - 2026-10-17

- **Native TLS HTTP/2 (`ALPN: h2`) upstream pooling with zero-waste HTTP/1.1 fallback (`src/upstream.rs`)**: Wired `https://` endpoints (such as DeepSeek `api.deepseek.com` and Xiaomi MiMo `api.xiaomimimo.com` / `token-plan-*.xiaomimimo.com`) into `xlite-h2-pool` via TLS ALPN `["h2", "http/1.1"]` negotiation (including RFC 9113 `content-length` headers for HTTP/2-to-HTTP/1.1 edge proxy compatibility), enabling peer `SETTINGS_MAX_CONCURRENT_STREAMS`-aware stream multiplexing, `RST_STREAM(CANCEL)` propagation, and `/api/pool` visibility over TLS; if an `https://` peer negotiates `http/1.1`, the already-handshaked TLS socket is parked directly into `H1Pool` and reused with zero extra handshake.
- **Lock-free per-channel failure cooldown (`src/store.rs`, `src/proxy.rs`)**: Added an `AtomicU64` monotonic cooldown deadline on `Channel` (`30s` on `401–404` quota/auth/route errors, `5s` on `408`/`429`/`5xx` and transport/header timeouts) so `order_channels` immediately places healthy backup channels ahead of cooling-down channels without paying repeated upstream RTTs, while keeping cooling-down channels at the tail so traffic is never blackholed if all channels are simultaneously in cooldown.
- **Expanded pre-first-byte failover on `HTTP 401 / 402 / 403 / 404 / 408` (`src/upstream.rs`)**: Extended `should_failover` to treat upstream `402 Payment Required` (account balance or credit exhaustion), `401 Unauthorized` / `403 Forbidden` (single-key revocation in a multi-key pool), `404 Not Found` (missing model on a subset of providers), and `408 Request Timeout` as retryable channel failures before the first response byte is sent.
- **Admin UI Xiaomi MiMo preset & Token Plan auto-resolution (`admin/src/lib/inner-providers.ts`, `admin/src/pages/models.tsx`)**: Updated the built-in Xiaomi provider preset with the official OpenAI-compatible pay-as-you-go endpoint (`https://api.xiaomimimo.com/v1`), Token Plan subscription endpoint (`https://token-plan-cn.xiaomimimo.com/v1`, automatically selected when `tp-` / `ttp-` API keys are used), and current 1M-context models (`mimo-v2.6-pro`, `mimo-v2.6-flash`, `mimo-v2.6-pro-ultraspeed`).

## [0.6.2] - 2026-10-17

- **Native HTTP/1.1 + TLS LIFO upstream connection pool and `reqwest` removal (`src/upstream.rs`, `Cargo.toml`)**: Replaced `reqwest` with a built-in HTTP/1.1 + HTTPS (`tokio-rustls` + `webpki-roots`) LIFO keep-alive pool (`H1Pool`) featuring non-blocking `try_read` socket liveness probing on checkout, transparent 1x redial on stale keep-alive close races (`EOF`/`RST` before first response byte), zero-copy `Bytes` streaming for `Content-Length`/`chunked`/EOF-delimited bodies, a 5s TCP/TLS `CONNECT_TIMEOUT`, `TCP_NODELAY` + 30s OS-level `TcpKeepalive`, and removal of `reqwest`'s hardcoded 600s total-stream wall-clock cap.

## [0.6.1] - 2026-10-16

- **Zero-DOM surgical `"model"` byte-splice rewrite for long-context prompts (`src/proxy.rs`)**: Replaced full `serde_json::Value` DOM parsing and re-serialization on channel model renames (`upstream_model != model`) with a depth-1 JSON span scanner (`RewritePlan::Splice`) that splices only the top-level `"model"` string value into a flat buffer, eliminating multi-megabyte AST allocations on 128k–1M token prompts while caching the span across failover attempts.
- **Configurable long-context request body ceiling (`XLITE_MAX_BODY_BYTES`)**: Raised the `/v1/chat/completions` body limit from Axum's 2 MiB default to `64 MiB` (`DEFAULT_MAX_BODY_BYTES`), configurable via `XLITE_MAX_BODY_BYTES`.
- **SSE anti-buffering headers, `: keep-alive` heartbeats (`XLITE_SSE_KEEPALIVE_SECS`), and upstream TCP/H2 keepalive (`src/proxy.rs`, `src/upstream.rs`)**: Automatically emit `Cache-Control: no-cache` and `X-Accel-Buffering: no` on `text/event-stream` responses, send `: keep-alive\n\n` SSE comment heartbeats every `XLITE_SSE_KEEPALIVE_SECS` (default `15s`, `0` to disable) during silent reasoning pauses without resetting `StallGuard`'s upstream stall deadline, and enable `tcp_keepalive(30s)` plus HTTP/2 adaptive flow-control windows and keep-alive pings on the upstream `reqwest` client.
- **Deploy & `xlite-h2-pool` cleanup**: Removed legacy Node.js 24-mock harness files (`scripts/stream_mock_server.mjs`, `scripts/start_mocks.sh`, `deploy/Dockerfile.mock`, `examples/mock-24*.yaml`, `docs/manual/mock-24-docker.md`) in favor of the standalone `mock-llm` repository, simplified `deploy/docker-compose.yml` to a clean `xlite + Redis` deployment stack, and removed the unused `diag.rs` / `examples/h2probe.rs` scaffolding from `crates/xlite-h2-pool`.

## [0.6.0] - 2026-10-16

- **Workspace streamlining & companion `xLB` separation**: Extracted `crates/mock-llm` into a standalone repository, removed `crates/xlite-h1-pool` and load-test-specific diagnostic hooks (`XLITE_UPSTREAM_TTFT_HEADER` and the `/api/pool` ring buffer) in favor of pairing with [**xLB**](https://github.com/EeroEternal/xlb) for L4/Thin-L7 HTTP/1.1 pooling and load balancing, keeping `xlite` strictly focused as a single binary + `crates/xlite-h2-pool`.
- **Documentation, SVG architecture diagram, and 36-stage benchmarks (`README.md`, `docs/assets/architecture.svg`, `docs/architecture.md`, `docs/benchmarks.md`, `AGENTS.md`)**: Condensed `README.md` with a top-level SVG topology diagram, four-way gateway comparison (`xlite` vs. `LiteLLM` vs. `New API` vs. `AISIX OSS`), `xLB` companion architecture, and 36-stage single-node streaming benchmarks (1k–18k VUs, 10.55M requests).

## [0.5.6] - 2026-10-15

- **Data plane: configurable upstream liveness timeouts (`XLITE_UPSTREAM_STALL_TIMEOUT_SECS` and `XLITE_UPSTREAM_HEADER_TIMEOUT_SECS`)**: Added `StallGuard` in `src/proxy.rs` to bound inter-chunk silence on response bodies and error body draining across h2c, HTTP/1.1, and reqwest paths, plus `await_headers` in `src/upstream.rs` to bound request-to-response-headers wait without penalising connection pool queueing. Both default to `0` (disabled).
- **Startup environment validation and logging**: Added `legacy_env_warnings_from_env()` in `src/config.rs` to warn on ignored legacy `XROUTER_*` variables, common `XLITE_*` variable name typos (`XLITE_UPSTREAM_CONNS_PER_HOST`), and unsupported PostgreSQL `DATABASE_URL` values; logged `tokio_worker_threads`, `catalog_reload_secs`, and timeout settings at startup.

## [0.5.5] - 2026-10-11

- **H2 pool lightweight data-plane diagnostics (`GET /api/pool`)**: Added `crates/xlite-h2-pool/src/diag.rs` to report per-connection in-flight/cap counts, pump channel depth, cancellation counts/spans, and 64-slot sampled stream progress using lock-free relaxed atomics.
- **Stream cancellation propagation fix**: Fixed `pump_stream` in `crates/xlite-h2-pool` to select on `out.closed()` while waiting for upstream frames so downstream cancellation immediately drops `RecvStream` and sends `RST_STREAM(CANCEL)` even when the upstream is silent.
- **Admin UI improvements**: Compacted the model creation modal into a scroll-free layout, aligned form control heights and focus rings, and refreshed built-in provider model catalogs.

## [0.5.4] - 2026-10-11

- **Load-test worker tuning utility**: Added `TOKIO_WORKER_THREADS` sizing guidance for high-concurrency streaming workloads.

## [0.5.3] - 2026-10-01

- **Native cleartext HTTP/1.1 connection pool (`crates/xlite-h1-pool`)**: Unified cleartext `http://` upstreams onto the native HTTP/1.1 pool with per-host connection bounding (`H1_DEFAULT_MAX_CONNS_PER_HOST = 128`) and `/api/pool` observability.
- **Snapshot precomputed upstream shapes**: Precomputed custom auth headers (`bearer`, `raw`, `none`), static `extra_headers`, and custom `chat_path` per channel at catalog snapshot build time so the request path performs zero string parsing.

## [0.5.2] - 2026-09-30

- **FD budget startup self-check**: Added `RLIMIT_NOFILE` inspection at startup with warnings when the soft limit is below 65,536, and preserved full `anyhow` error chains on upstream `502` responses.
- **Control-plane pool stats (`GET /api/pool`)**: Exposed per-host connection, stream, and dial counters for the native HTTP/2 pool.

## [0.5.1] - 2026-09-28

- **Native HTTP/2 connection pool enhancements**: Added connection-task pinned polling (`pinned_poll`) and configurable client-side stream caps (`XLITE_UPSTREAM_STREAM_CAP`).

## [0.5.0] - 2026-09-26

- **Initial release of `xlite`**: Thin OpenAI-compatible AI gateway with weighted-random + priority failover routing, lock-free `ArcSwap` catalog snapshots (SQLite, in-memory, or Redis), native h2c multiplexing pool (`crates/xlite-h2-pool`), YAML import/export, and React Admin console.
