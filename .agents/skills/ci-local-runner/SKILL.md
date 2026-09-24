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

`cargo test --lib` 不连 Postgres。启动网关才需要 `DATABASE_URL`。
