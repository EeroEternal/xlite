# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
