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
| `ADMIN_PASSWORD` | `admin` | Control-plane token (`Authorization: Bearer <ADMIN_PASSWORD>` / `x-admin-token`) and login password |
| `ADMIN_SESSION_SECRET` | unset | Overrides the key used to sign Admin session cookies; set the same on every node |
| `XLITE_USAGE_WEBHOOK_URL` | unset | Optional HTTP(S) webhook URL receiving async post-request JSON usage & billing payloads (`model`, `upstream_model`, `api_key_name`, `provider_name`, `input_tokens`, `output_tokens`, `cache_hit_tokens`, `tokens`, `latency`, `status`) |
| `XLITE_USAGE_WEBHOOK_TOKEN` | unset | Optional Bearer token sent in `Authorization: Bearer <token>` when calling `XLITE_USAGE_WEBHOOK_URL` |
| `XLITE_UPSTREAM_H2C` | off | Plaintext HTTP/2 (`h2c`) to `http://` upstreams via `xlite-h2-pool` |
| `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` | `32` | Per-host HTTP/2 connection cap (`xlite-h2-pool`) and minimum idle pool size for the native HTTP/1.1 pool (`H1Pool`) |
| `XLITE_UPSTREAM_STREAM_CAP` | peer's `SETTINGS` | Client-side cap on concurrent streams per upstream HTTP/2 connection |
| `XLITE_CATALOG_RELOAD_SECS` | `5` | Snapshot reload period; `0` freezes the snapshot (load test) |
| `XLITE_UPSTREAM_STALL_TIMEOUT_SECS` | `0` (disabled) | End a response body with `TimedOut` after this many seconds of silence between chunks |
| `XLITE_UPSTREAM_HEADER_TIMEOUT_SECS` | `0` (disabled) | Timeout waiting for upstream response headers after sending the request |
| `XLITE_SSE_KEEPALIVE_SECS` | `15` | Emit `: keep-alive\n\n` SSE comment heartbeats during silent upstream pauses on `text/event-stream` responses (`0` disables) |
| `XLITE_MAX_BODY_BYTES` | `67108864` (64 MiB) | Maximum request body size for `/v1/chat/completions` (for long-context and multimodal prompts) |

Listen backlog is 65535 (kernel-clamped by `net.core.somaxconn`). For 30k concurrent clients set `net.core.somaxconn = 65535` on the gateway host.

`TOKIO_WORKER_THREADS` is a runtime variable: bare `#[tokio::main]` spawns one worker per core, which can cause excess spinlock contention on high-core-count machines under I/O-bound streaming loads. The bundled `deploy/env.example` defaults to **8** (or **24** for high-chunk-rate streaming workloads). Always set `TOKIO_WORKER_THREADS` explicitly in production.

For high-concurrency HTTP/1.1 connection pooling, active/passive `/health` probing, TLS termination, and pre-first-byte HTTP status rerouting (`402`/`404`/`429`/`5xx`, enabling multiple `xlite` instances to serve disjoint model catalogs without shared storage), pair `xlite` with [**xLB**](https://github.com/EeroEternal/xlb).

## Routing

There is no strategy field. For a public model, xlite picks a channel in the lowest `priority` group by `weight`, then fails over before the first byte on 5xx / 429 / 402 / connection errors.

## YAML & Control Plane API (Language-Agnostic)

See `examples/catalog.yaml`. Commands: `xlite import <file.yaml>` (optional `--replace`) and `xlite export [file.yaml]`. Export omits user API key plaintext.

Any external application (Node.js, Python, Go, Java, etc.) can manage `xlite` over standard HTTP using `Authorization: Bearer <ADMIN_PASSWORD>`:

```http
POST /api/catalog/import HTTP/1.1
Host: xlite:8080
Authorization: Bearer <ADMIN_PASSWORD>
Content-Type: application/json

{
  "replace": true,
  "yaml": "version: 1\nproviders:\n  - name: openai:primary\n    base_url: https://api.openai.com/v1\n    api_key: sk-upstream-...\n    enabled: true\nmodels:\n  - name: gpt-5.4\n    enabled: true\n    channels:\n      - provider: openai:primary\n        upstream_model: gpt-5.4\n        weight: 100\n        priority: 0\n        enabled: true\napi_keys:\n  - name: user-key:u_12345\n    key: sk-client-...\n    enabled: true\n    models:\n      - gpt-5.4\n"
}
```

## Async Usage & Billing Webhook (`XLITE_USAGE_WEBHOOK_URL`)

When `XLITE_USAGE_WEBHOOK_URL` is set (for example `http://my-app:3000/webhooks/xlite-usage`), `xlite` automatically extracts token usage from both non-streaming JSON and streaming SSE responses across `/v1/chat/completions`, `/v1/messages`, `/v1/responses`, and `/v1/embeddings`, and asynchronously sends an HTTP `POST` request after each completed response:

- **Method**: `POST`
- **Headers**:
  - `Content-Type: application/json`
  - `Authorization: Bearer <XLITE_USAGE_WEBHOOK_TOKEN>` (only sent when `XLITE_USAGE_WEBHOOK_TOKEN` is configured)
- **JSON Body**:

```json
{
  "model": "gpt-5.4",
  "upstream_model": "gpt-5.4",
  "api_key_name": "user-key:u_12345",
  "provider_name": "openai:primary",
  "input_tokens": 120,
  "output_tokens": 340,
  "cache_hit_tokens": 64,
  "tokens": 460,
  "latency": 1420,
  "status": 200
}
```

Docker Compose (`xlite` + Redis): [`deploy/README.md`](../../deploy/README.md).
