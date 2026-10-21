# Getting started

## Start

**Docker (recommended):**

```bash
docker run -d --name xlite -p 8080:8080 \
  -e ADMIN_PASSWORD=admin \
  -v xlite-data:/opt/xlite/data \
  -e DATABASE_URL=sqlite:/opt/xlite/data/xlite.db \
  ghcr.io/eeroeternal/xlite:latest
```

**Pre-built binary (or from source via `cargo build --release`):**

```bash
ADMIN_PASSWORD=admin ./xlite
```

Default listen is `0.0.0.0:8080` (serving `/v1/chat/completions`, `/v1/messages`, `/v1/responses`, `/v1/embeddings`, `/v1/models`, and `/api/*`). Catalog defaults to `sqlite:xlite.db`.

| Need | Set |
| --- | --- |
| Load-test / stateless catalog | `DATABASE_URL=memory://` |
| Shared cluster catalog | `REDIS_URL=redis://127.0.0.1:6379` |
| Plaintext HTTP/2 (`h2c`) upstreams | `XLITE_UPSTREAM_H2C=1` |
| Async usage & billing webhook | `XLITE_USAGE_WEBHOOK_URL=http://hub:3000/api/gateway/usage` |

## Load a routing catalog

Import [`examples/catalog.yaml`](../examples/catalog.yaml) via the CLI or push it over HTTP (`POST /api/catalog/import` with `Authorization: Bearer <ADMIN_PASSWORD>`):

```bash
xlite import examples/catalog.yaml --replace
```

The command prints any newly generated API key once. Then:

```bash
curl -H "Authorization: Bearer sk-..." \
  -H "Content-Type: application/json" \
  -d '{"model":"mock","stream":true,"messages":[{"role":"user","content":"hi"}]}' \
  http://127.0.0.1:8080/v1/chat/completions
```

## Docker Compose (`xlite` + Redis)

Follow [`deploy/README.md`](../deploy/README.md):

```bash
cd deploy
cp env.example .env
docker compose up -d --build
docker compose run --rm xlite import /opt/xlite/examples/catalog.yaml --replace
```

## Health

- `GET /health` — process is up
- `GET /ready` — catalog (SQLite or Redis) answers
