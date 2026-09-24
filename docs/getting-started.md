# Getting started

## Start

```bash
export ADMIN_PASSWORD=admin
export XLITE_UPSTREAM_H2C=1
cargo run --release
```

Default listen is `0.0.0.0:8080`. Catalog defaults to `sqlite:xlite.db`.

| Need | Set |
| --- | --- |
| Load-test catalog | `DATABASE_URL=memory` |
| Shared cluster catalog | `REDIS_URL=redis://127.0.0.1:6379` |
| Admin UI in dev | `cd admin && npm install && npm run dev` |

## Load a routing catalog

One public model can point at many providers. One API key with `models: []` can use that model.

```bash
xlite import examples/mock-24.yaml --replace
```

The command prints any newly generated API key once. Then:

```bash
curl -H "Authorization: Bearer sk-..." \
  -H "Content-Type: application/json" \
  -d '{"model":"mock","stream":true,"messages":[{"role":"user","content":"hi"}]}' \
  http://127.0.0.1:8080/v1/chat/completions
```

Admin → Settings can import or export the same YAML.

## 24 mock + Redis (Docker)

Do not use a local `cargo` binary. Build images and follow [manual/mock-24-docker.md](manual/mock-24-docker.md):

```bash
docker compose -f docker-compose.mock-24.yml up -d --build
docker compose -f docker-compose.mock-24.yml run --rm xlite \
  import /opt/xlite/examples/mock-24.docker.yaml --replace
```

## Health

- `GET /health` — process is up
- `GET /ready` — catalog (SQLite or Redis) answers
