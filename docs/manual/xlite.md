# xlite operator notes

Current version is the `version` field in the repo-root `Cargo.toml`. It is returned by `GET /health`
(unauthenticated — use this to verify which build a deployment actually runs) and by
`GET /api/overview` (Admin login required).

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `DATABASE_URL` | `sqlite:xlite.db` | SQLite file, or `memory` |
| `REDIS_URL` | unset | If set, catalog is Redis (`xlite:catalog`) |
| `PORT` / `LISTEN_HOST` | `8080` / `0.0.0.0` | Listen address |
| `ADMIN_PASSWORD` | `admin` | Console login |
| `ADMIN_SESSION_SECRET` | unset | Overrides the key used to sign Admin session cookies; set the same on every node |
| `XLITE_UPSTREAM_H2C` | off | Plaintext HTTP/2 to `http://` upstreams |
| `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` | `32` (h2c) / `128` (h1) | Per-host connection cap; unset ⇒ h2c `32`, cleartext HTTP/1.1 `128`; an explicit value applies to both verbatim |
| `XLITE_UPSTREAM_STREAM_CAP` | peer's `SETTINGS` | Client-side cap on concurrent streams per upstream HTTP/2 connection |
| `XLITE_CATALOG_RELOAD_SECS` | `5` | Snapshot reload period; `0` freezes the snapshot (load test) |
| `XLITE_UPSTREAM_TTFT_HEADER` | off | Stamp `x-xlite-upstream-ttft-ms` (diagnostic; delays response headers until the upstream's first byte) |
| `XLITE_UPSTREAM_STALL_TIMEOUT_SECS` | `0` (disabled) | End a response body with `TimedOut` after this many seconds of silence between chunks |
| `XLITE_UPSTREAM_HEADER_TIMEOUT_SECS` | `0` (disabled) | Timeout waiting for upstream response headers after sending the request |

Listen backlog is 65535 (kernel-clamped by `net.core.somaxconn`). For 30k concurrent clients set `net.core.somaxconn = 65535` on the gateway host.

`TOKIO_WORKER_THREADS` is a runtime variable: bare `#[tokio::main]` spawns one worker per core, which can cause excess spinlock contention on high-core-count machines under I/O-bound streaming loads. The bundled `deploy/env.example` defaults to **8** (or **24** for high-chunk-rate streaming workloads). Always set `TOKIO_WORKER_THREADS` explicitly in production.

`XLITE_UPSTREAM_MAX_CONNS_PER_HOST` is one knob with two defaults, because one connection means different things to the two pools. Under **h2c** it is a connection cap that multiplexes streams onto a connection (`32` by default). Under **cleartext HTTP/1.1** (the native `xlite-h1-pool`, used when `XLITE_UPSTREAM_H2C` is off) one connection carries **one** request at a time, so the h1 pool defaults to **128** when the env is unset. Setting the variable explicitly overrides both pools with the one number.

## Routing

There is no strategy field. For a public model, xlite picks a channel in the lowest `priority` group by `weight`, then fails over before the first byte on 5xx / 429 / connection errors.

## YAML

See `examples/catalog.yaml`. Commands: `xlite import <file.yaml>` (optional `--replace`) and `xlite export [file.yaml]`. Export omits user API key plaintext.

24 mock + Redis in Docker: [`deploy/README.md`](../../deploy/README.md). Use `examples/mock-24.docker.yaml` (`http://mocks:4101` …), not the loopback file.
