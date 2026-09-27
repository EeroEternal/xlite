---
name: ci-local-runner
description: Run the tiered local gates (scripts/ci_local.sh) for xlite. Covers t0 push / t1 PR-to-main / t2 release, path scoping, and the --only single-face selector.
---

# 本地门禁 runner（t0 / t1 / t2）

门禁按**风险事件**分层，全部在本机跑 —— `scripts/ci_local.sh` 是唯一实现，仓库没有
GitHub CI；验证责任在推送者。

| Tier | 事件 | 跑什么 | 平台 |
| --- | --- | --- | --- |
| **t0** | 任意分支 push | **按改动路径裁剪**：只跑被改到的面 | 本机（含 macOS） |
| **t1** | 合入 main 前 | 全部面 + admin build + h2-pool 集成测试 | 本机 |
| **t2** | 打 tag / 发版前 | t1 + `check_release_sync.sh` | 本机 |

```bash
scripts/ci_local.sh              # t0：按改动路径裁剪
scripts/ci_local.sh --all        # t0 但强制所有面
scripts/ci_local.sh --tier t1    # t1 全量（旧写法 --full 仍等价）
scripts/ci_local.sh --tier t2    # t2：t1 + release sync
scripts/ci_local.sh --only rust  # 只跑一个面：rust | admin | meta
```

## 三个面与路径路由

| 面 | 何时触发 | 内容 |
| --- | --- | --- |
| `rust` | `src/**` `crates/**` `tests/**` `migrations/**` `benches/**` `build.rs` `Cargo.toml` `Cargo.lock` `rust-toolchain.toml` | fmt、clippy（两个 crate）、`cargo test --lib`、h2-pool lib 测试；t1 起加 h2-pool 集成测试 |
| `admin` | `admin/**` | `tsc -b --noEmit`、`npm run lint`、`vitest run`；t1 起加 `npm run build` |
| `meta` | `docs/**` `*.md` `.agents/**` `.github/**` `scripts/**` `.mise.toml` | changelog 归属、文档生命周期、改动脚本的 `bash -n`、改动 workflow 的 YAML 解析；t2 起加 release sync |

**跳过只在能点名改动路径时发生**：基线 ref 不可解析（浅克隆、非 git 目录）或相对基线无
改动时，跑**所有**面。改动路径没有任何面命中时，脚本会告警并跑 meta，绝不静默全绿。

## 环境

需要 Rust 工具链（`rust-toolchain.toml`，rustup 会按文件自动装，含 clippy + rustfmt）、
Node 22（`.mise.toml`）、`admin/node_modules`。

`cargo test --lib` 用内存 SQLite，不需要 Postgres。`sqlx` 的 `sqlite` feature 会传递启用
`libsqlite3-sys/bundled`，SQLite **由源码内联编译**（`cargo tree -e features -i libsqlite3-sys`
可见 `feature "bundled"`；注意 `-p <包名>` 得到的是非反向树，看不到这个结论），
因此 Linux 上**不需要**系统 sqlite 开发库 —— `bundled` 会把 SQLite 一起编译，不需要任何系统包。
单机启动默认 `sqlite:xlite.db`；压测 `DATABASE_URL=memory`；集群设 `REDIS_URL`。

## 已知偶发（不是本次改动引入）

`cargo test -p xlite-h2-pool --test native_pool` 的
`a_raised_settings_cap_is_used_without_dialling_again`（`native_pool.rs:660` 的第一次断言）
是**时序型偶发**：对端 `spawn_settings_changing_peer`（`native_pool.rs:568`）先发
`MAX_CONCURRENT_STREAMS=1`、**睡 150 ms** 再发 4，而该用例要求「此刻读到的仍是 1」。
机器一忙、`acquire` 路径超过 150 ms 时，新上限先落地，断言就得到
`left: 4, right: 1`。这是**用例的墙钟假设**，不是产品竞态 —— 产品「热更新上限」正是它要测的能力。

遇到它先复跑一次区分偶发与真回归，别直接当成自己的改动坏了。根治办法是让对端等一个显式
信号而不是 `sleep(150ms)`；那属于独立的测试修复，不要夹带进无关改动。

## 为什么不按平台放宽

静态门禁（fmt/clippy/tsc/lint/vitest）与平台无关，在 macOS 上跳过不会省下任何正确性，
只会把失败推迟到没有 CI 兜底的地方。真正只能 Linux 做的是产物、集成与部署，那些属于 t2。

**但 shell 脚本要反过来小心**：macOS 自带 bash 3.2，仓内脚本被打死过一次（`declare -A`
在 3.2 上直接 `invalid option`）。注意 meta 面的 `bash -n` **只验语法**，抓不到这类差异 ——
实测 `declare -A m=([a]=1)` 在 bash 3.2 下 `bash -n` 仍返回 0。所以改 `scripts/**` 时要在
本机 macOS 上**实跑**一次，或避开 bash 4+ 构造（`declare -A` / `mapfile` / `${v,,}`）。
