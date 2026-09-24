# xlite operator notes

Current version is the `version` field in the repo-root `Cargo.toml`, also returned by `GET /api/overview`.

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `DATABASE_URL` | `sqlite:xlite.db` | SQLite file, or `memory` |
| `REDIS_URL` | unset | If set, catalog is Redis (`xlite:catalog`) |
| `PORT` / `LISTEN_HOST` | `8080` / `0.0.0.0` | Listen address |
| `ADMIN_PASSWORD` | `admin` | Console login |
| `ADMIN_SESSION_SECRET` | unset | Overrides the key used to sign Admin session cookies; set the same on every node |
| `XLITE_UPSTREAM_H2C` | off | Plaintext HTTP/2 to `http://` upstreams |
| `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` | `32` | h2c pool cap per host |
| `XLITE_CATALOG_RELOAD_SECS` | `5` | Snapshot reload period; `0` freezes the snapshot (load test) |

Listen backlog is 65535 (kernel-clamped by `net.core.somaxconn`). For 30k concurrent clients set `net.core.somaxconn = 65535` on the gateway host. Do not set `XLITE_MAX_IN_FLIGHT`; that cap was reverted because it queues streams and inflates duration.

## Routing

There is no strategy field. For a public model, xlite picks a channel in the lowest `priority` group by `weight`, then failovers before the first byte.

## YAML

See `examples/catalog.yaml`. Commands: `xlite import <file.yaml>` (optional `--replace`) and `xlite export [file.yaml]`. Export omits user API key plaintext.

24 mock + Redis in Docker: [`deploy/README.md`](../../deploy/README.md). Use `examples/mock-24.docker.yaml` (`http://mocks:4101` …), not the loopback file. Lab machines and the customer topology: [`lab-loadtest.md`](lab-loadtest.md).
