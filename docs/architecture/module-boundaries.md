# Module boundaries

xlite stays a single binary plus one transport crate (`xlite-h2-pool`).

- `src/proxy.rs` — request path and async usage webhook reporting only.
- `src/protocol.rs` — Anthropic `/v1/messages`, OpenAI `/v1/responses`, and `/v1/embeddings` translation.
- `src/store.rs` — snapshot and channel order.
- `src/catalog/` — SQLite or Redis persistence for providers, models, channels, keys; `yaml.rs` is import/export only.
- `src/admin.rs` — headless control-plane HTTP API (`Bearer <ADMIN_PASSWORD>`, `x-admin-token`, or cookie).
- `src/upstream.rs` — HTTP/2 and HTTP/1.1 upstream client pools.

Do not split crates by directory, add routing strategy enums, or join across unrelated tables. New business behavior belongs in a new module only after it has a caller and tests.
