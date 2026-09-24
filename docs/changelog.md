# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- **热路径零拷贝透传**：`upstream_model` 与请求 `model` 同名（常规情形）时不再 JSON 反序列化/重序列化，原请求字节直接透传（字节透传边界名副其实）；需要改名时才惰性构建 DOM（每请求至多一次）。上游请求体改传 `Bytes`，去掉 h2c `copy_from_slice` 与 reqwest `to_vec` 第三次拷贝。
- **快照 Arc 化 + 端点预计算**：路由快照里模型/通道改存 `Arc`，每请求排序只克隆指针不克隆 5 个 `String`；通道的 `chat/completions` URL 在快照构建时算好，不在每请求重拼。5 秒重载在路由未变时跳过写锁交换。
- **Key 哈希与 failover 细节**：API key 哈希改手写 hex（免 `format!` fmt 机器）；failover 前的错误体排空封顶 64 KiB，坏上游不再能拖住重试。
## [0.2.0] - 2026-09-24

- **Admin 会话改为无状态签名 Cookie**：`xlite_session` 变为 HMAC-SHA256 签名的 `exp.nonce`（密钥取自 `ADMIN_SESSION_SECRET`，未设则用 `ADMIN_PASSWORD`）。任意网关节点都能独立校验，HAProxy/负载均衡后无需会话粘性，也不再依赖进程内内存；集群各节点设置同一个 `ADMIN_PASSWORD`（或显式 `ADMIN_SESSION_SECRET`）即可共用登录态。
- **示例 YAML 写死压测 Key**：`examples/mock-24*.yaml` 与 `catalog.yaml` 的 `api_keys.loadtest.key` 为 `sk-loadtest`，import 后无需打开 Admin。
- **24 mock Docker 台架**：`Dockerfile` / `Dockerfile.mock`；Compose 与操作说明在 `deploy/`。配置走 Redis；上游 YAML 为 `examples/mock-24.docker.yaml`（主机名 `mocks`）。

## [0.1.0] - 2026-09-24

- **YAML 导入/导出路由目录**：供应商、模型（含通道权重/优先级）、API Key 可用一份 YAML 合并或整表替换。设置页导入导出；命令行 `xlite import file.yaml` / `xlite export`。导出不含用户 API Key 明文；导入未写 `key` 时会生成并把明文返回一次。示例见 `examples/catalog.yaml`、`examples/mock-24.yaml`。
- **配置存储改为 SQLite / 内存 / Redis**：不再使用 PostgreSQL。默认 `DATABASE_URL=sqlite:xlite.db`；压测设 `DATABASE_URL=memory` 走进程内 SQLite；集群设 `REDIS_URL` 后供应商、模型、通道、API Key 写进 Redis（`xlite:catalog`），各节点共享。数据面仍只读内存快照。
- **xlite 初版**：独立仓库的轻量 OpenAI 网关。数据面只做 API Key 鉴权、按权重/优先级选通道、SSE 字节透传；h2c 走原生连接池（对端 `SETTINGS_MAX_CONCURRENT_STREAMS` 满了再拨新连接）。控制面只有供应商、模型与通道、API Key、设置。调度与 New API 相同：同一优先级内按权重随机，首字节前才 failover。
