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
| `XLITE_ATTEMPT_WEBHOOK_URL` | unset | Optional HTTP(S) webhook receiving **one event per upstream attempt**, posted as soon as that attempt finishes (`event`, `request_id`, `model`, `api_key_name`, `attempt`) |
| `XLITE_ATTEMPT_WEBHOOK_TOKEN` | unset | Optional Bearer token sent in `Authorization: Bearer <token>` when calling `XLITE_ATTEMPT_WEBHOOK_URL` |
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

Any external application (Node.js, Python, Go, Java, etc.) can manage `xlite` over standard HTTP using `Authorization: Bearer <ADMIN_PASSWORD>`. Both bodies below are equivalent — a `yaml` string wrapper, or the catalog document itself as JSON:

```http
POST /api/catalog/import HTTP/1.1
Host: xlite:8080
Authorization: Bearer <ADMIN_PASSWORD>
Content-Type: application/json

{
  "replace": true,
  "providers": [{"name": "openai:primary", "base_url": "https://api.openai.com/v1", "api_key": "sk-upstream-...", "enabled": true}],
  "models": [
    {
      "name": "gpt-5.4",
      "enabled": true,
      "channels": [{"provider": "openai:primary", "upstream_model": "gpt-5.4", "weight": 100, "priority": 0, "enabled": true, "max_concurrency": 64}]
    }
  ],
  "api_keys": [{"name": "user-key:u_12345", "key": "sk-client-...", "enabled": true, "models": ["gpt-5.4"]}]
}
```

(The same document with `"yaml": "<catalog text>"` instead of the three arrays is accepted for YAML/YAML-flow callers.)

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
  "status": 200,
  "attempts": [
    {
      "channel_id": 7,
      "provider_id": 3,
      "upstream_model": "gpt-5.4",
      "status": 402,
      "error": "{\"error\":\"quota exhausted\"}",
      "latency_ms": 83,
      "cooled_down": true
    },
    {
      "channel_id": 9,
      "provider_id": 3,
      "upstream_model": "gpt-5.4",
      "status": 200,
      "error": null,
      "latency_ms": 1420,
      "cooled_down": false
    }
  ]
}
```

`request_id` correlates this batch report with the per-attempt events below.

`attempts` lists **every** upstream attempt of the request in order, so a control plane can attribute failures, retries and cooldowns per channel instead of only seeing the final result. `error` is the upstream body (truncated to 512 chars) or the transport error; `cooled_down` is `true` when that failure started a cooldown window.

### Per-attempt events (`XLITE_ATTEMPT_WEBHOOK_URL`)

The batch payload above arrives once the request finished. For live failover
visibility, set `XLITE_ATTEMPT_WEBHOOK_URL` (and optionally
`XLITE_ATTEMPT_WEBHOOK_TOKEN`) and xlite posts one event per attempt immediately:

```json
{
  "event": "attempt",
  "request_id": "6f1c9f1e-...",
  "model": "gpt-5.4",
  "api_key_name": "user-key:u_12345",
  "attempt": {
    "channel_id": 7, "provider_id": 3, "upstream_model": "gpt-5.4",
    "status": 402, "error": "{\"error\":\"quota exhausted\"}",
    "latency_ms": 83, "cooled_down": true
  }
}
```

Events are posted asynchronously, so their relative arrival order with respect to
the batch report is not guaranteed — use `request_id` to join them. Both webhooks
are opt-in: with neither configured, no attempt records are materialised at all.

## Request Priority (`x-xlite-priority`)

Callers can mark a request as bulk traffic:

```http
x-xlite-priority: bulk
```

* **`bulk`** — if that channel's upstream pool would have to park (another request is
  already waiting, or every allowed connection is dialed and every stream slot is
  taken), the request is **shed immediately** instead of queueing: xlite fails over
  to the next channel, and when none is available returns `503` with
  `Retry-After: 1` and `{"error":{"type":"overloaded_error"}}`. A shed channel is
  not cooled down (it was never contacted), and the attempt is reported with
  `status: 503`, `cooled_down: false`.
* **absent / anything else** — latency-sensitive default: wait for a slot as before.

This is admission priority, not reordering: served-in-flight requests are never
preempted, but bulk traffic stops consuming the wait queue that priority traffic
depends on. Combined with `GET /api/channels` (`pool.waiting`, `wait_ms_p99`) it
gives an explicit answer to "who gets the slot when everything is full".

## Per-Provider Timeouts (`header_timeout_secs` / `stall_timeout_secs`)

The global `XLITE_UPSTREAM_HEADER_TIMEOUT_SECS` / `XLITE_UPSTREAM_STALL_TIMEOUT_SECS`
apply to every upstream. A provider may override them on its own:

```yaml
providers:
  - name: flaky-provider
    base_url: https://api.example.com/v1
    api_key: sk-...
    header_timeout_secs: 5
    stall_timeout_secs: 20
```

* `header_timeout_secs` bounds the request → response-headers wait **for that
  provider only**; on expiry the attempt fails over exactly like a transport error
  (the attempt is recorded, and the channel cools down).
* `stall_timeout_secs` bounds the silence between response chunks for that
  provider; a hung stream is terminated cleanly and the lease is released.
* Omitted fields inherit the global value, and `0` means "disabled" exactly as the
  global setting does — so existing catalogs are unchanged.

## Per-Channel Concurrency Cap (`max_concurrency`)

Each channel may declare an in-flight cap; omitted/`null` keeps it bounded only by
the upstream pool:

```yaml
models:
  - name: gpt-5.4
    channels:
      - provider: openai:primary
        upstream_model: gpt-5.4
        max_concurrency: 64
```

* The cap is enforced at admission, **not** by queueing: a request that arrives
  while the channel is full fails over to the next channel, and if every channel
  is full returns `503` + `Retry-After: 1` with `{"error":{"type":"overloaded_error"}}`.
* The slot is held for the lifetime of the response body (a streaming response
  keeps it until the last chunk or until the client disconnects), so the number is
  real in-flight load, not request rate.
* A full channel is **not** cooled down — it is healthy, just busy — and the
  attempt is reported with `status: 503`, `cooled_down: false`.
* Uncapped channels pay nothing: no atomic counter is touched.
* Observability: `GET /api/channels` reports live `in_flight` and the configured
  `max_concurrency` next to the pool view, so the cap can be tuned against the
  queue (`pool.waiting`) instead of guesswork.

## Cooldown Length Follows `Retry-After`

When a failover-eligible response carries `Retry-After: <seconds>`, the cooldown
uses it (clamped to `1..=300s`, never shorter than the status default `401–404` →
30s, otherwise 5s). A provider that says "retry in 20s" is therefore parked for
20s instead of the fixed 5s, and one that says 1h is capped at 5 minutes.

## Channel Cooldown Metrics & Control

Cooldowns are per process and previously invisible. Both endpoints require the usual `Authorization: Bearer <ADMIN_PASSWORD>`:

- `GET /api/channels` — every row also carries `cooling_down` (bool), `cooldown_remaining_ms` (0 = healthy), and — when that upstream has a live pool — `pool`: `connections`, `in_flight`, `waiting` (requests parked on the per-host cap), `effective_ceiling` and `wait_ms_p50` / `wait_ms_p99`. The pool is keyed by upstream authority, so channels that share a base URL report the same numbers; this is the per-channel view of the concurrency signal that `/api/pool` only shows cluster-wide.
- `POST /api/channels/{id}/cooldown` with `{"millis": 5000}` — deprioritize that channel for 5s; `{"millis": 0}` clears the cooldown immediately (useful after fixing a quota/auth problem). Unknown ids return `404`; the response echoes `cooling_down` and `cooldown_remaining_ms`.
- An operator cooldown is stored outside the routing snapshot, so it survives reloads (including ones that change the channel's credential, where automatic carry-over gives up). A channel that exists in the catalog but is not routed right now still accepts the cooldown and returns `"pending_reload": true`; it applies as soon as the channel enters a snapshot. Cooldown state is **per process**: with several replicas, call the endpoint on each one (automatic cooldowns are per replica for the same reason).

Docker Compose (`xlite` + Redis): [`deploy/README.md`](../../deploy/README.md).
