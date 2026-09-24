# Module boundaries

xlite stays a single binary plus one transport crate (`xlite-h2-pool`).

- `src/proxy.rs` — request path only.
- `src/store.rs` — snapshot and channel order.
- `src/admin.rs` — control-plane HTTP.
- `src/upstream.rs` — HTTP client.
- `admin/` — React console for the four objects above.

Do not split crates by directory, add routing strategy enums, or join across unrelated tables. New business behavior belongs in a new module only after it has a caller and tests.
