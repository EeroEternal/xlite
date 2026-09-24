# xlite architecture

xlite is a thin OpenAI-compatible gateway. It forwards `/v1/chat/completions` as bytes and uses the same simple channel pick as New API.

## Planes

- Data plane: `src/proxy.rs` authenticates an API key, reads `model`, picks channels, copies the upstream stream.
- Control plane: `src/admin.rs` plus `admin/` for providers, models, channels, and API keys.
- Admin sessions: stateless HMAC-signed cookies (`src/auth.rs`), keyed from `ADMIN_PASSWORD` or `ADMIN_SESSION_SECRET`, so any node validates a cookie with no Redis and no sticky routing.
- Snapshot: `src/store.rs` holds the in-memory routing table reloaded from the catalog.
- Catalog: SQLite file by default, in-memory SQLite when `DATABASE_URL=memory`, Redis when `REDIS_URL` is set (cluster).

## Routing

There is one scheduling rule:

1. Enabled channels for the requested public model, filtered by the key's model allow-list (empty allow-list means all models).
2. Take the lowest `priority` group and pick one channel at random by `weight`.
3. If the chosen channel fails before the first response byte (connect error, 429, or 5xx), try the remaining channels in priority order.

No CostAware, CapabilityAware, session stickiness, or strategy enum.

## Upstream

Cleartext HTTP/2 (`XLITE_UPSTREAM_H2C=1` and `http://` base URL) uses `crates/xlite-h2-pool`: one connection is full at the peer's `SETTINGS_MAX_CONCURRENT_STREAMS`, then the pool dials another (capped by `XLITE_UPSTREAM_MAX_CONNS_PER_HOST`). Other URLs use reqwest.

## Objects

| Object | Role |
| --- | --- |
| Provider | Upstream `base_url` + API key |
| Model | Public name clients send |
| Channel | Model → provider, with weight and priority |
| API Key | SHA-256 hash in the catalog; plaintext shown once on create/rotate |

## Persistence

| Mode | How to enable | Where config lives |
| --- | --- | --- |
| SQLite | default, or `DATABASE_URL=sqlite:xlite.db` | local file |
| Memory | `DATABASE_URL=memory` | process RAM, for load tests |
| Redis | `REDIS_URL=redis://...` | `xlite:catalog` JSON, shared by every node |

The request path never talks to SQLite or Redis. Admin writes update the catalog, then the snapshot reloads. Load tests may freeze the snapshot (`XLITE_CATALOG_RELOAD_SECS=0`) and probe `/health` instead of `/ready`.

## YAML catalog

`src/catalog/yaml.rs` imports and exports providers, models/channels, and API keys. HTTP: `GET/POST /api/catalog/export|import`. CLI: `xlite import` / `xlite export`. There is no strategy field; channels only have `weight` and `priority`.
