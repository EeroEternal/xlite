# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- **Admin 会话改为无状态签名 Cookie**：`xlite_session` 变为 HMAC-SHA256 签名的 `exp.nonce`（密钥取自 `ADMIN_SESSION_SECRET`，未设则用 `ADMIN_PASSWORD`）。任意网关节点都能独立校验，HAProxy/负载均衡后无需会话粘性，也不再依赖进程内内存；集群各节点设置同一个 `ADMIN_PASSWORD`（或显式 `ADMIN_SESSION_SECRET`）即可共用登录态。
- **示例 YAML 写死压测 Key**：`examples/mock-24*.yaml` 与 `catalog.yaml` 的 `api_keys.loadtest.key` 为 `sk-loadtest`，import 后无需打开 Admin。
- **24 mock Docker 台架**：`Dockerfile` / `Dockerfile.mock`；Compose 与操作说明在 `deploy/`。配置走 Redis；上游 YAML 为 `examples/mock-24.docker.yaml`（主机名 `mocks`）。

## [0.1.0] - 2026-09-24

- **YAML 导入/导出路由目录**：供应商、模型（含通道权重/优先级）、API Key 可用一份 YAML 合并或整表替换。设置页导入导出；命令行 `xlite import file.yaml` / `xlite export`。导出不含用户 API Key 明文；导入未写 `key` 时会生成并把明文返回一次。示例见 `examples/catalog.yaml`、`examples/mock-24.yaml`。
- **配置存储改为 SQLite / 内存 / Redis**：不再使用 PostgreSQL。默认 `DATABASE_URL=sqlite:xlite.db`；压测设 `DATABASE_URL=memory` 走进程内 SQLite；集群设 `REDIS_URL` 后供应商、模型、通道、API Key 写进 Redis（`xlite:catalog`），各节点共享。数据面仍只读内存快照。
- **xlite 初版**：独立仓库的轻量 OpenAI 网关。数据面只做 API Key 鉴权、按权重/优先级选通道、SSE 字节透传；h2c 走原生连接池（对端 `SETTINGS_MAX_CONCURRENT_STREAMS` 满了再拨新连接）。控制面只有供应商、模型与通道、API Key、设置。调度与 New API 相同：同一优先级内按权重随机，首字节前才 failover。
