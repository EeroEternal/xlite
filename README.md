# xlite

Thin OpenAI-compatible AI gateway. Simple channel routing, streaming passthrough.

Scheduling matches New API: weighted random inside the lowest priority group, then failover before the first response byte.

## Run

```bash
export DATABASE_URL=postgres://localhost/xlite
export ADMIN_PASSWORD=admin
export XLITE_UPSTREAM_H2C=1   # for plaintext HTTP/2 mocks
cargo run --release
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

## Gates

```bash
cargo fmt -p xlite --check
cargo clippy -p xlite --lib --tests -- -D warnings
cargo test --lib
cd admin && npx tsc -b --noEmit && npm run lint && npx vitest run
```
