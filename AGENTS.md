# Repository Rules for Agents (`AGENTS.md`)

## 1. Language Policy (Strict English-Only)

- **All repository documentation (`README.md`, `docs/**/*.md`, `deploy/**/*.md`, `crates/**/README.md`), code comments, commit messages, PR descriptions, and CLI/log strings MUST be written strictly in English.**
- **Do NOT create Chinese or bilingual documentation files** (e.g., `README.zh-CN.md`, `README_zh.md`, or language toggle links in `README.md`).

## 2. Documentation Architecture & Lifecycle Gates

- Keep `README.md` focused as the high-signal entry point: positioning, benchmark summary, technical deep-dive overview (linking into `docs/`), gateway comparison (`xlite` vs. `new-api` vs. `api7/aisix`), 5-minute quick start, workspace layout, and configuration reference.
- Detailed technical designs, benchmark tables, and operator runbooks belong under `docs/`:
  - [`docs/architecture.md`](docs/architecture.md) — Data plane, `ArcSwap` snapshot, routing/failover, `xlite-h2-pool`, `StallGuard`, and companion `xLB` topology.
  - [`docs/benchmarks.md`](docs/benchmarks.md) — Single-node 36-stage streaming benchmarks and transport root-cause ablations.
  - [`docs/manual/xlite.md`](docs/manual/xlite.md) — Operator environment variables, connection pool tuning, and YAML catalog operations.
  - [`docs/changelog.md`](docs/changelog.md) — Release history (`## [Unreleased]` must appear exactly once; new changelog entries must be placed under `## [Unreleased]`).
- Every relative link across `README.md`, `docs/**/*.md`, and `deploy/**/*.md` is checked mechanically by `bash scripts/check_docs_lifecycle.sh`. Always run `bash scripts/ci_local.sh --only meta` after editing any Markdown file.

## 3. Codebase & Verification Gates

- Data-plane scope: Headless multi-protocol AI traffic gateway supporting OpenAI `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings`, `/v1/models`, and Anthropic `/v1/messages`, with weighted-random routing within the lowest `priority` tier, pre-first-byte failover, zero-parse `Bytes` fast path on `/v1/chat/completions`, and stateless Bearer / YAML control-plane configuration.
- Before finishing any change, run the local verification gate:
  ```bash
  bash scripts/ci_local.sh
  ```
