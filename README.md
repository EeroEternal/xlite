# xlite

Thin OpenAI-compatible AI gateway. Simple channel routing, streaming passthrough.

Current release: **0.1.0**. See [docs/getting-started.md](docs/getting-started.md) and [docs/changelog.md](docs/changelog.md).

Scheduling matches New API: weighted random inside the lowest priority group, then failover before the first response byte.

## Run

```bash
# Single node: SQLite file (default sqlite:xlite.db)
export ADMIN_PASSWORD=admin
export XLITE_UPSTREAM_H2C=1   # for plaintext HTTP/2 mocks
cargo run --release

# Load test: in-memory catalog
# DATABASE_URL=memory cargo run --release

# Cluster: shared catalog in Redis
# REDIS_URL=redis://127.0.0.1:6379 cargo run --release
```

Listen defaults to `0.0.0.0:8080`.

```
POST /v1/chat/completions
GET  /v1/models
GET  /health
GET  /ready
```

Admin API is under `/api/*`. After `cd admin && npm run build`, the binary serves `admin/dist`.

## Admin

`cd admin && npm install && npm run dev` proxies `/api/` to `http://127.0.0.1:8080`.

Pages: overview, providers, models (channels), API keys, settings.

## 24 mock + Redis (Docker)

See [docs/manual/mock-24-docker.md](docs/manual/mock-24-docker.md).

```bash
docker compose -f docker-compose.mock-24.yml up -d --build
docker compose -f docker-compose.mock-24.yml run --rm xlite \
  import /opt/xlite/examples/mock-24.docker.yaml --replace
```

## Catalog YAML

One public model can bind many providers. Import with merge (update by name) or replace (wipe first):

```bash
xlite import examples/mock-24.yaml --replace
xlite export catalog.yaml
```

Admin → Settings also imports and exports the same file. Schema is in `examples/catalog.yaml`.

## Gates

```bash
cargo fmt -p xlite --check
cargo clippy -p xlite --lib --tests -- -D warnings
cargo test --lib
cd admin && npx tsc -b --noEmit && npm run lint && npx vitest run
```
