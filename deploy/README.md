# Docker Compose Deployment (`xlite` + Redis)

[`deploy/docker-compose.yml`](docker-compose.yml) runs a production-style `xlite` gateway backed by a shared Redis catalog (`redis:8.8`), with an optional second replica (`--profile ha`) for stateless multi-node clustering.

## 1. Quick Start

```bash
cd deploy
cp env.example .env
docker compose up -d --build
```

Open the Web Console at **`http://127.0.0.1:8080`**, or seed the shared Redis catalog from [`examples/catalog.yaml`](../examples/catalog.yaml):

```bash
docker compose run --rm xlite import /opt/xlite/examples/catalog.yaml --replace
```

Verify readiness:

```bash
curl -s http://127.0.0.1:8080/ready
```

## 2. Multi-Replica High Availability (`--profile ha`)

Because `xlite` stores its routing catalog in Redis (`REDIS_URL=redis://redis:6379`) and signs Admin sessions with a stateless HMAC cookie derived from `ADMIN_PASSWORD` (or `ADMIN_SESSION_SECRET`), you can scale horizontally without sticky sessions:

```bash
docker compose --profile ha up -d
```

- `xlite` listens on `127.0.0.1:8080`
- `xlite-2` listens on `127.0.0.1:8081`

Catalog edits made in the Web Console on either node propagate to all replicas via `XLITE_CATALOG_RELOAD_SECS` (default `2` seconds).

## 3. File Descriptor Budget (`ulimits.nofile`)

Both `xlite` services in [`docker-compose.yml`](docker-compose.yml) set `ulimits.nofile` to `1048576`. Do not remove this block when running high-concurrency streaming workloads (10,000+ concurrent SSE streams).
