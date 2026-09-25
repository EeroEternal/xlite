# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- **`GET /health` 带上 `version`**：免鉴权的探活响应从 `{"status":"ok","gateway":"xlite"}` 变为再带一个 `version`（`CARGO_PKG_VERSION`）字段。此前只有要登录的 `GET /api/overview` 才给版本，启动日志之外**无法用一个 curl 确认现场跑的是哪一版**；对标时反复强调要钉 k6 镜像 digest 与脚本 sha256，网关自己却没有可远程核对的指纹。HAProxy `httpchk` 只判状态码，加字段不影响探活。回归：`health_reports_gateway_and_version`。

- **运维：`TOKIO_WORKER_THREADS` 不要留默认**（实测，见 `docs/dev/loadtest-newapi-controlled-2026-09.md` §12）：裸 `#[tokio::main]` 默认按 `nproc` 起 worker。在 128 核的网关机上实测默认值 **白烧约 40% CPU 且尾部更差** —— 1000 VU（约 2.2 核工作量）下 128 → 2 线程为 `2.400 → 1.430` 核，同时 `p90 −28%`、`p95 −60%`、`p99 −23%`、吞吐与失败率不变（代价仅 `p50 +16%`）。**按负载定，别抄常数**：4000 VU 下 128 → 8 线程省 9%（幅度随载荷变小）。同轮实测另两条：`XLITE_UPSTREAM_MAX_CONNS_PER_HOST` 32/64/128 **零效应**（不用调）；**listen backlog 65535 不是延迟优化而是并发上限保障** —— ≤1000 VU 无延迟差别（accept 队列峰值 17/128），但 **4000 VU 下 backlog 128 直接 34.6% 失败、65535 为 0.00%**。

- **可选的上游首字计时头 `XLITE_UPSTREAM_TTFT_HEADER`（默认关）**：置 1 时流式响应带 `x-xlite-upstream-ttft-ms`（发上游请求 → 上游第一个 body 字节的耗时），用来回答「高负载下 TTFT 涨的那一截在上游还是网关」。**默认关**：HTTP 头先于 body 发出，要报上游首字就必须先等那一个字节，对「先发头、后吐字」的上游会把客户端首字推迟到上游首字；关闭时行为与之前一致（只多一层 `stream::iter(None)` 空适配器，字节与顺序不变）。**这个头不进对标表**：New API 不发任何计时头（其二进制里 `x-*-ms` 形态 0 命中），客户原版脚本也不读响应头，该列只能单边填；`queue-wait` 类头**不提供**（无准入闸门，恒 0，是死列）。同时新增 `scripts/loadtest/`（xlite 自己的 rig，读 `x-xlite-*`；默认值按客户对标喂法设：`KEEP_BODY=true` ⇒ 每次迭代全量解析、checks 比 **2.00**，`KEEP_SAMPLE` 默认 **1**，显式传 0.1 才是客户 v3.2-memopt 的采样版 **1.10**，`KEEP_BODY=false` 时仍是 **1.00**）。回归：`with_head_prepends_without_reordering_or_dropping`。

- **受控对照：把客户的 New API 镜像搬到实验台（`docs/dev/loadtest-newapi-controlled-2026-09.md`）**：用客户给的 `new-api:v8-sentinel` 镜像 + 客户自己的 `init-db.sh` 目录配方（Postgres + Redis + `h2c` 通道），在**同一台实验机、同一份 rig（`KEEP_BODY=true` + `-m 45g`，两侧 checks 比均 2.00）、同一批 24 mock、同一天**与 xlite 0.2.0 对跑。结果：**低载（1000 VU）两者 TTFT 无差别**（xlite 501.8ms / New API 504.2ms，mock 自身 500ms），即客户日志里 New API 的 708ms **不是网关开销**；高载段 New API 在「单副本 + 本地 Postgres」下先崩（4000 VU fail 12.4%、10000 VU 99.8%，签名是每请求结算占一条 DB 连接），但客户现场是 2 副本 + HAProxy + 独占 PG，**不可外推**。**后续更正（含一次降级）**：按客户配置（`SQL_MAX_OPEN_CONNS=2000` / PG `max_connections=2000`）时 4000 VU fail 12.4%；把两边放开到 8000/10000 后出现过 **0.57%**，但**同一配置复跑又出现过 12.4% / 12.98%**（波动 20×）—— 所以那一档**不能用来归因**（既不能说"就是 DB 连接数配置"，也不能说"就是 New API 的行为"），要下结论先补 run ledger（每轮记 PG `max_connections` / `SQL_MAX_OPEN_CONNS` / fail%）；能说的只有架构那条：放开连接数后瓶颈上移到 **Postgres CPU**（10000 VU 时 74 核，错误为 `failed to connect to postgres`），因为 New API 每个请求的结算都要与 Postgres 往返一次。同时更正 `v88-rig.md` 对「客户跑的是哪份 k6 脚本」的归属：客户机上被挂载执行的始终是 `/root/loadtest/k6/k6.js`，但那份文件在对标那晚被手改过至少四次，包里两份 `k6.js`（`deploy88/k6.js` 硬编码 `gracefulStop: '10s'`；`loadtest/k6/k6.js` 采样 0.1 → checks 比 1.10，只对上最后一档 E@21000）**都对不上**跑出对标数据的那一版。对标那批（含客户 708ms）的指纹是 checks 比 **2.00** + `gracefulStop: 10m0s` + 有 `gateway_overhead_ms`，判据见 `v88-rig.md` §1.1。

- **h2c 目标预计算**：通道快照带上 `authority` / `path` / `Authorization`，请求路径不再解析上游 URL、不再 `format!("Bearer …")`。
- **Mock-24 与压测环境手册**：实验室 `117.160.123.99:60000/60001`、客户 22/24/26/29 拓扑、k6 组合与 3 万连接对标，见 `docs/manual/lab-loadtest.md`。
- **V88 对标 rig**：`KEEP_BODY=true`、无 think-time、1000 VU 起跳；不 cap / 不提前 200 / 超时 600s 的落地方式见 `docs/manual/v88-rig.md`。
- **压测可冻结目录快照**：`XLITE_CATALOG_RELOAD_SECS=0` 关掉 5 秒 Redis/SQLite 重载；请求路径仍不碰库。HAProxy 探 `/health`。

## [0.3.0] - 2026-09-24

- **热路径零拷贝透传**：`upstream_model` 与请求 `model` 同名（常规情形）时不再 JSON 反序列化/重序列化，原请求字节直接透传（字节透传边界名副其实）；需要改名时才惰性构建 DOM（每请求至多一次）。上游请求体改传 `Bytes`，去掉 h2c `copy_from_slice` 与 reqwest `to_vec` 第三次拷贝。注意：含重复 `model` 键的请求体现在返回 400（原为末键生效）。
- **快照 Arc 化 + 端点预计算**：路由快照里模型/通道改存 `Arc`，每请求排序只克隆指针不克隆 5 个 `String`；通道的 `chat/completions` URL 在快照构建时算好，不在每请求重拼。5 秒重载在路由未变时跳过写锁交换。
- **Key 哈希与 failover 细节**：API key 哈希改手写 hex（免 `format!` fmt 机器）；failover 前的错误体排空封顶 64 KiB，坏上游不再能拖住重试。
- **入站 listen backlog 与 TCP_NODELAY**：监听套接字 backlog 65535（内核按 `net.core.somaxconn` 封顶）。Linux 上 libc `SOMAXCONN` 是 128，默认 `TcpListener::bind` 会在 3 万 VU 爬坡时丢 SYN。每条入站连接打开 `TCP_NODELAY`，10ms SSE 不被 Nagle 合并。（此前误置在 `[Unreleased]`，实际随本版发出：提交 `4876579` 改了 `src/main.rs`。）

## [0.2.0] - 2026-09-24

- **Admin 会话改为无状态签名 Cookie**：`xlite_session` 变为 HMAC-SHA256 签名的 `exp.nonce`（密钥取自 `ADMIN_SESSION_SECRET`，未设则用 `ADMIN_PASSWORD`）。任意网关节点都能独立校验，HAProxy/负载均衡后无需会话粘性，也不再依赖进程内内存；集群各节点设置同一个 `ADMIN_PASSWORD`（或显式 `ADMIN_SESSION_SECRET`）即可共用登录态。
- **示例 YAML 写死压测 Key**：`examples/mock-24*.yaml` 与 `catalog.yaml` 的 `api_keys.loadtest.key` 为 `sk-loadtest`，import 后无需打开 Admin。
- **24 mock Docker 台架**：`Dockerfile` / `Dockerfile.mock`；Compose 与操作说明在 `deploy/`。配置走 Redis；上游 YAML 为 `examples/mock-24.docker.yaml`（主机名 `mocks`）。

## [0.1.0] - 2026-09-24

- **YAML 导入/导出路由目录**：供应商、模型（含通道权重/优先级）、API Key 可用一份 YAML 合并或整表替换。设置页导入导出；命令行 `xlite import file.yaml` / `xlite export`。导出不含用户 API Key 明文；导入未写 `key` 时会生成并把明文返回一次。示例见 `examples/catalog.yaml`、`examples/mock-24.yaml`。
- **配置存储改为 SQLite / 内存 / Redis**：不再使用 PostgreSQL。默认 `DATABASE_URL=sqlite:xlite.db`；压测设 `DATABASE_URL=memory` 走进程内 SQLite；集群设 `REDIS_URL` 后供应商、模型、通道、API Key 写进 Redis（`xlite:catalog`），各节点共享。数据面仍只读内存快照。
- **xlite 初版**：独立仓库的轻量 OpenAI 网关。数据面只做 API Key 鉴权、按权重/优先级选通道、SSE 字节透传；h2c 走原生连接池（对端 `SETTINGS_MAX_CONCURRENT_STREAMS` 满了再拨新连接）。控制面只有供应商、模型与通道、API Key、设置。调度与 New API 相同：同一优先级内按权重随机，首字节前才 failover。
