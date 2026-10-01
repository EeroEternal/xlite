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
| `XLITE_CATALOG_RELOAD_SECS` | `5` | Snapshot reload period; `0` freezes the snapshot (load test) |
| `XLITE_UPSTREAM_TTFT_HEADER` | off | Stamp `x-xlite-upstream-ttft-ms` (diagnostic; delays response headers until the upstream's first byte — see `scripts/loadtest/README.md`) |

Listen backlog is 65535 (kernel-clamped by `net.core.somaxconn`). For 30k concurrent clients set `net.core.somaxconn = 65535` on the gateway host. Do not set `XLITE_MAX_IN_FLIGHT`; that cap was reverted because it queues streams and inflates duration.

`TOKIO_WORKER_THREADS` is not in the table on purpose: it is a runtime variable, and **leaving it at the default is wrong here**. Bare `#[tokio::main]` spawns one worker per core, but this gateway is I/O-bound — measured on a 128-core host, the default burned **~40% more CPU and produced a worse tail** than a small pool (1000 VU: 128 → 2 threads moved **2.670 → 1.579** cores with `p90 −17%` / `p95 −45%` / `p99 −51%`, throughput and failure rate unchanged, cost `p50 +14%`; at 4000 VU, 128 → 8 threads saves 9%). The bundled `deploy/env.example` defaults to **8**. Size it to the load — roughly **4–8 for a ~1000-stream workload** — and don't copy a constant from another tier: the saving shrinks as real parallelism rises. Full data in `../dev/loadtest-newapi-controlled-2026-09.md` §12.

`XLITE_UPSTREAM_MAX_CONNS_PER_HOST` is one knob with two defaults, because one connection means different things to the two pools. Under **h2c** it is a connection cap that multiplexes: 1000 streams over 24 upstreams ≈ 42 streams/host vs a cap of 32, but measured **zero effect** at 32/64/128 — h2c packs streams onto a connection, so leave it alone. Under **cleartext HTTP/1.1** (the native `xlite-h1-pool`, used only when the upstream speaks no h2/h2c) one connection carries **one** request, so the cap *is* the concurrency: 32 → 128 measured **3.7× qps** (10.7s → 528ms median TTFT; `../dev/h1-pool-loadtest-2026-10-01.md` §5.2), which is why the h1 pool defaults to **128** when the env is unset. Setting the variable explicitly overrides both pools with the one number.

## Routing

There is no strategy field. For a public model, xlite picks a channel in the lowest `priority` group by `weight`, then failovers before the first byte.

## YAML

See `examples/catalog.yaml`. Commands: `xlite import <file.yaml>` (optional `--replace`) and `xlite export [file.yaml]`. Export omits user API key plaintext.

24 mock + Redis in Docker: [`deploy/README.md`](../../deploy/README.md). Use `examples/mock-24.docker.yaml` (`http://mocks:4101` …), not the loopback file. Lab machines and the customer topology: [`lab-loadtest.md`](lab-loadtest.md).
