# 24 Upstream Mocks + Redis Deployment (Docker Compose)

This directory is the operational entry point for the **24 upstream HTTP/2 SSE mocks, shared Redis catalog, and `xlite` gateway**. It runs entirely from container images without compiling or invoking host binaries.

There is no PostgreSQL dependency. Providers, models, channels, and API keys are stored in the Redis key `xlite:catalog`. The request hot path reads only the gateway process's lock-free in-memory snapshot (refreshed from Redis every 5 seconds by default).

| File | Purpose |
| :--- | :--- |
| `README.md` | This guide |
| `docker-compose.yml` | `redis` + `mocks` + `xlite` (plus optional `xlite-2` under `--profile ha`) |
| `env.example` | Configurable environment template; copy to `.env` |

The gateway image is built from the repository root (`Dockerfile`); the mock image is built from `deploy/Dockerfile.mock` (referenced directly by Compose). Use `examples/mock-24.docker.yaml` for the catalog import (upstream hostname is `mocks`).

Current image tags: `xlite:0.6.0`, `xlite-mock:0.1.0`.

---

## 1. Topology

```text
curl / k6 / Browser
        |  Host :8080
        v
     xlite container  --cleartext HTTP/2 (h2c)-->  mocks container :4101 … :4124
        |
        +-- redis://redis:6379   key: xlite:catalog
```

Compose services:

| Service | Image | Host Port | Internal Container Address |
| :--- | :--- | :--- | :--- |
| `redis` | `redis:7-alpine` | `6379` | `redis:6379` |
| `mocks` | `xlite-mock:0.1.0` | `4101–4124` | `http://mocks:4101` … `http://mocks:4124` |
| `xlite` | `xlite:0.6.0` | `8080` | Gateway node 1 |
| `xlite-2` (`--profile ha`) | `xlite:0.6.0` | `8081` | Gateway node 2 (shares the same Redis) |

Routing behavior: public model `mock` maps to 24 channels (`weight = 1`, `priority = 0`) using weighted-random selection within the lowest priority tier and pre-first-byte failover on `5xx`, `429`, or connect/header errors. A single API key (`sk-loadtest`) with `models: []` grants access to all public models.

> **Important**: Do **not** import `examples/mock-24.yaml` (`127.0.0.1`) into the containerized gateway—inside the container `127.0.0.1` refers to the gateway container itself, not the `mocks` container. Always use `examples/mock-24.docker.yaml`.

---

## 2. Environment Parameters

Copy the environment template first:

```bash
cd deploy
cp env.example .env
```

`docker compose` automatically reads `.env` in this directory. After editing `.env`, re-run `docker compose up -d` for the affected service.

### 2.1 Gateway (`xlite` / `xlite-2`)

| Variable | Default | 24-Mock Value | Description |
| :--- | :--- | :--- | :--- |
| `REDIS_URL` | *(unset)* | `redis://redis:6379` *(set in compose)* | When set, stores catalog in Redis and ignores SQLite |
| `XLITE_UPSTREAM_H2C` | `false` | `1` | Use cleartext HTTP/2 (`h2c`) for `http://` upstreams |
| `ADMIN_PASSWORD` | `admin` | Change in production | Admin Console login password |
| `ADMIN_SESSION_SECRET` | *(unset)* | Optional | HMAC signing secret for stateless Admin cookies; derived from `ADMIN_PASSWORD` if unset |
| `PORT` | `8080` | `8080` | Container listen port |
| `LISTEN_HOST` | `0.0.0.0` | `0.0.0.0` | Bind address inside container |
| `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` | `32` | `32` (raise if saturated) | Max `h2c` connections per upstream host |
| `DATABASE_URL` | `sqlite:xlite.db` | *(omit when using Redis)* | Ignored when `REDIS_URL` is set |

Accepted truthy values for `XLITE_UPSTREAM_H2C`: `1`, `true`, `yes`, `on` (case-insensitive).

The gateway listen backlog is `65535` (clamped by the kernel's `net.core.somaxconn`). Before running 30,000-connection load tests on a Linux host, run:

```bash
sysctl -w net.core.somaxconn=65535
sysctl -w net.core.netdev_max_backlog=65535
sysctl -w net.ipv4.tcp_max_syn_backlog=65535
sysctl -w net.ipv4.ip_local_port_range="1024 65535"
```

**Container `ulimit nofile` must be at least `1048576`** (`docker-compose.yml` sets `soft = hard = 1048576` for `mocks`, `xlite`, and `xlite-2`). Under high concurrency, a default `nofile=1024` causes silent upstream dial exhaustion (`EMFILE`) and a ~10× throughput drop. Verify after startup:

```bash
docker compose exec xlite sh -c "grep -i 'open files' /proc/1/limits"   # expect 1048576
docker compose exec mocks sh -c "grep -i 'open files' /proc/1/limits"
```

With 24 mocks × 32 connections/host = 768 connections × 250 streams/connection (`SETTINGS_MAX_CONCURRENT_STREAMS`), the pool supports up to ~192,000 concurrent HTTP/2 streams.

### 2.2 Mock Farm (`mocks` service — 24 listeners in one container)

| Variable | Smoke Test | Long-Stream Load Test | Description |
| :--- | :--- | :--- | :--- |
| `MOCK_MODEL` | `mock` | `mock` | Matches `upstream_model` in YAML |
| `MOCK_CHUNKS` | `20` | `500` | Number of SSE chunks per stream |
| `MOCK_INTERVAL_MS` | `10` | `10` | Interval between chunks in ms |
| `MOCK_TTFT_MS` | `0` | `0` (or `500`) | Extra delay before the first response byte in ms |
| `MOCK_BATCH` | `1` | `1` | Coalesce every `N` chunks into one socket write without changing payload bytes |
| `MOCK_LISTEN` | `0.0.0.0` | `0.0.0.0` | Must be `0.0.0.0` for cross-container access |

Each mock listener also supports live runtime reconfiguration via `POST /_config` (no query string = read current JSON; query string = update and return updated JSON).

### 2.3 Redis

Compose runs `redis:7-alpine` with `--save "" --appendonly no` (in-memory only for load tests). Keys used:

- `xlite:catalog`: Full catalog JSON
- `xlite:catalog:lock`: Short-lived distributed write lock

Admin login sessions use **stateless HMAC-SHA256 signed cookies** and are never stored in Redis. Any replica sharing the same `ADMIN_PASSWORD` (or `ADMIN_SESSION_SECRET`) validates session cookies without sticky routing.

---

## 3. Start the Stack

From `deploy/`:

```bash
cd deploy
cp -n env.example .env
docker compose up -d --build
docker compose ps
```

Wait until `redis`, `mocks`, and `xlite` report `healthy`.

To build images manually from the repository root without starting services:

```bash
docker build -t xlite:0.6.0 .
docker build -f deploy/Dockerfile.mock -t xlite-mock:0.1.0 .
```

> **Offline & Mirror Build Notes**:
> - If `.cargo/` and `vendor/` are present in the build context (generated via `scripts/prepare_offline_vendor.sh`), `Dockerfile` automatically runs `cargo build --offline`. Otherwise it fetches crates online.
> - To use upstream official registries instead of the default mirrors during `docker build`, pass empty build args:
>   ```bash
>   docker build -t xlite:0.6.0 --build-arg NPM_REGISTRY= --build-arg CRATES_MIRROR= .
>   ```

---

## 4. Import the 24-Channel Catalog

Once the containers are running, import the 24-channel catalog into Redis:

```bash
cd deploy
docker compose run --rm xlite import /opt/xlite/examples/mock-24.docker.yaml --replace
```

The bundled YAML pre-configures the client API key `sk-loadtest`:

```yaml
api_keys:
  - name: loadtest
    key: sk-loadtest
    models: []
```

Verify that all 24 providers are stored in Redis:

```bash
docker compose exec redis redis-cli GET xlite:catalog | grep -c mock-41
```

Within 5 seconds (`XLITE_CATALOG_RELOAD_SECS`), the gateway snapshot syncs automatically.

---

## 5. Smoke Verification

```bash
export KEY=sk-loadtest

curl -sS http://127.0.0.1:8080/health
# {"status":"ok","gateway":"xlite","version":"0.6.0"}

curl -sS http://127.0.0.1:8080/ready
# {"status":"ready"}

curl -sS -H "Authorization: Bearer $KEY" http://127.0.0.1:8080/v1/models
# lists "id": "mock"

curl -N -sS \
  -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"mock","stream":true,"messages":[{"role":"user","content":"hi"}]}' \
  http://127.0.0.1:8080/v1/chat/completions
```

Open `http://127.0.0.1:8080` in a browser and sign in with `ADMIN_PASSWORD` from `.env` to inspect the Admin Console.

---

## 6. High-Availability Replica (`xlite-2`)

Start a second gateway instance backed by the same Redis catalog:

```bash
cd deploy
docker compose --profile ha up -d xlite-2
curl -sS -H "Authorization: Bearer $KEY" http://127.0.0.1:8081/v1/models
```

---

## 7. Equivalent Standalone `docker run` Commands

```bash
docker network create xlite-net

docker run -d --name xlite-redis --network xlite-net -p 6379:6379 redis:7-alpine

docker run -d --name xlite-mocks --network xlite-net --network-alias mocks \
  --ulimit nofile=1048576:1048576 \
  -p 4101-4124:4101-4124 \
  -e MOCK_MODEL=mock -e MOCK_CHUNKS=20 -e MOCK_INTERVAL_MS=10 -e MOCK_LISTEN=0.0.0.0 \
  xlite-mock:0.1.0

docker run --rm --network xlite-net \
  -e REDIS_URL=redis://xlite-redis:6379 \
  xlite:0.6.0 import /opt/xlite/examples/mock-24.docker.yaml --replace

docker run -d --name xlite-gw --network xlite-net \
  --ulimit nofile=1048576:1048576 \
  -p 8080:8080 \
  -e REDIS_URL=redis://xlite-redis:6379 \
  -e XLITE_UPSTREAM_H2C=1 \
  -e ADMIN_PASSWORD=admin \
  -e LISTEN_HOST=0.0.0.0 \
  xlite:0.6.0
```

---

## 8. Troubleshooting

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| `502 Bad Gateway` on `/v1/chat/completions` | Imported `mock-24.yaml` (`127.0.0.1`) instead of `mock-24.docker.yaml` | Re-import `/opt/xlite/examples/mock-24.docker.yaml --replace` |
| `/ready` returns `503` | `REDIS_URL` points to `127.0.0.1` inside container | Use `redis://redis:6379` |
| `401 Unauthorized` | Missing or mismatched Bearer key | Use `Authorization: Bearer sk-loadtest` |
| Throughput drops ~10× with flood of `502`s at ≥10k VUs | Container `ulimit nofile` clamped at `1024` | Pass `--ulimit nofile=1048576:1048576` and verify `/proc/1/limits` |

---

## 9. Stop & Clean Up

```bash
cd deploy
docker compose --profile ha down
```
