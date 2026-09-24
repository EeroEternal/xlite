# Data-plane boundary

xlite has no protocol adapter layer.

- `src/proxy.rs` authenticates, selects a channel, and copies bytes.
- `src/upstream.rs` speaks HTTP to the provider. It does not parse SSE events.
- Failover happens only before the first byte is written to the client.

Do not add Anthropic/Gemini conversion, usage parsing, or request-log writes on the hot path.
