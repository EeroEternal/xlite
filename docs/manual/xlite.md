# xlite operator notes

Current version is the `version` field in the repo-root `Cargo.toml`, also returned by `GET /api/overview`.

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `DATABASE_URL` | `sqlite:xlite.db` | SQLite file, or `memory` |
| `REDIS_URL` | unset | If set, catalog is Redis (`xlite:catalog`) |
| `PORT` / `LISTEN_HOST` | `8080` / `0.0.0.0` | Listen address |
| `ADMIN_PASSWORD` | `admin` | Console login |
| `XLITE_UPSTREAM_H2C` | off | Plaintext HTTP/2 to `http://` upstreams |
| `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` | `32` | h2c pool cap per host |

## Routing

There is no strategy field. For a public model, xlite picks a channel in the lowest `priority` group by `weight`, then failovers before the first byte.

## YAML

See `examples/catalog.yaml`. Commands: `xlite import <file.yaml>` (optional `--replace`) and `xlite export [file.yaml]`. Export omits user API key plaintext.
