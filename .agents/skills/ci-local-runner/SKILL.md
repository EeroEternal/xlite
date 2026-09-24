---
name: ci-local-runner
description: Run the local CI script (scripts/ci_local.sh) for xlite. Covers Rust fmt/clippy/tests and admin tsc/lint/vitest/build.
---

# 本地 CI runner

```bash
scripts/ci_local.sh        # fmt, clippy, lib tests, admin lint/test/build
scripts/ci_local.sh --full # also run xlite-h2-pool integration tests
```

需要：Rust 工具链（`rust-toolchain.toml`）、Node 22（`.mise.toml`）、`admin/node_modules`。

`cargo test --lib` 用内存 SQLite，不需要 Postgres。单机启动默认 `sqlite:xlite.db`；压测 `DATABASE_URL=memory`；集群设 `REDIS_URL`。
