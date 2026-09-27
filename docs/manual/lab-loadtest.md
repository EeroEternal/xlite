# Mock-24 与压测环境

24 路上游 mock、实验室两台机、客户现场拓扑。Compose 操作步骤见 [`deploy/README.md`](../../deploy/README.md)。与 New API V88 抄同一套 k6 喂法见 [`v88-rig.md`](v88-rig.md)。

## 1. 24 mock

对外一个模型名 `mock`，后面挂 24 条通道（端口 `4101`–`4124`），权重 1、优先级 0，同一优先级内按权重随机。一把 API Key：`sk-loadtest`（YAML 名 `loadtest`，`models: []` 不限制模型）。

| 目录 YAML | 上游地址 | 用在哪 |
| --- | --- | --- |
| `examples/mock-24.yaml` | `http://127.0.0.1:4101` … | 本机进程 |
| `examples/mock-24.docker.yaml` | `http://mocks:4101` … | Compose 网络 |
| 实验室 Redis 目录 | `http://10.99.255.102:4101` … | 跨机 h2c |

网关 `XLITE_UPSTREAM_H2C=1`。仓库 `deploy/Dockerfile.mock` 是 Node HTTP/1.1 SSE（`scripts/stream_mock_server.mjs`）。实验室和客户现场用的是 Go `mock-llm:h2c`，同样 24 端口。

| mock 参数 | 冒烟 | B 组（客户脚本） |
| --- | --- | --- |
| chunks / `out_tokens` | 20 | 500 |
| interval | 10ms | 10ms |
| TTFT | 0 | 500ms |
| 墙钟 | ~0.2s | ~5.5s（500×10ms + 500ms） |

客户 k6 组合（`out|token_interval|稳态`）：

| 组 | out | interval | 稳态 | 流长 |
| --- | --- | --- | --- | --- |
| A | 100 | 50ms | 5min | 短 |
| B | 500 | 10ms | 5min | 短 |
| E | 250 | 20ms | 5min | 短 |
| G | 1000 | 5ms | 5min | 短 |
| C | 1200 | 50ms | 8min | 长 |
| D | 6000 | 10ms | 8min | 长 |
| F | 3000 | 20ms | 8min | 长 |
| H | 12000 | 5ms | 8min | 长 |

k6：`KEEP_BODY=false`，`discardResponseBodies`。步进 1000 VU，失败率连续两档 >1% 停。入口 `POST /v1/chat/completions`，`Authorization: Bearer sk-loadtest`。

## 2. 实验室

公网同一 IP，SSH 端口区分两台。都是 128 核 / 1 TiB，Ubuntu 22.04。

| SSH | 主机名 | 内网 | 角色 |
| --- | --- | --- | --- |
| `ssh -p 60000 bodesi@117.160.123.99` | xinference | `10.99.255.101` | xlite `:8080` + Redis `:6379` |
| `ssh -p 60001 bodesi@117.160.123.99` | bodesi-102 | `10.99.255.102` | 24 × `mock-h2c-4101` … `4124` |

```
k6 / curl
    →  10.99.255.101:8080  (xlite, host 网络)
           │  Redis  127.0.0.1:6379   键 xlite:catalog（约 7KB，1 个 key）
           └─h2c──→  10.99.255.102:4101–4124  (mock-llm:h2c, host 网络)
```

网关容器要点（`docker inspect xlite`）：

- 镜像应打当前 `Cargo.toml` 版本；不要用 `xlite:0.2.0-inflight`，也不要设 `XLITE_MAX_IN_FLIGHT`
- `network=host`，`ulimit nofile=1048576`
- `REDIS_URL=redis://127.0.0.1:6379`，`XLITE_UPSTREAM_H2C=1`
- `XLITE_UPSTREAM_MAX_CONNS_PER_HOST=32`，`TOKIO_WORKER_THREADS=8`（与 `deploy/env.example` 的默认一致；**别留运行时默认**，那等于按核数起）
- 可选 `XLITE_UPSTREAM_STREAM_CAP`（默认不设 = 按对端通告值执行，见下文「上游流闸门」）
- 目录：24 provider / 1 model `mock` / 24 channel / 1 key `loadtest`
- 压测前 Redis `connected_clients` 应为 2，`MEMORY USAGE xlite:catalog` 约 7KB

mock 容器：`mock-llm:h2c`，`network=host`，`nofile=1048576`。`curl http://127.0.0.1:4101/health` → `{"status":"ok"}`。

### 上游流闸门（可选：过载段护农场）

mock-llm:h2c 是 Go h2 服务器，对**每条连接**恒定通告 `SETTINGS_MAX_CONCURRENT_STREAMS=250`
（Go 库默认值；静态分析二进制未见任何改向：启动只有 env `PORT`，运行时 `/_config` 字段
不含流上限。现场可用下方 h2probe 复核）。网关对每个 mock 的准入流上限：

```text
min(XLITE_UPSTREAM_STREAM_CAP, 250) × XLITE_UPSTREAM_MAX_CONNS_PER_HOST
```

超出部分在 h2 连接池排队（等待计入 TTFT，不产生错误）——例如 `STREAM_CAP=40 ×
CONNS=25` = 1000 流/mock。这是臂 B 实测否决应用层排队（issue #45）之后规则允许的
**传输层闸门**形态；注意调低预算会促使池拨更多连接，且有上游首字节尾部回归的先例
（见 `crates/xlite-h2-pool` `Config::stream_budget_per_conn` 文档），闸门值应按实测农场
容量定并整轮验证。核对对端通告值：

```bash
cargo run -p xlite-h2-pool --example h2probe -- 10.99.255.102:4101
```

网关机内核：

```bash
sysctl -w net.core.somaxconn=65535
sysctl -w net.core.netdev_max_backlog=65535
sysctl -w net.ipv4.tcp_max_syn_backlog=65535
sysctl -w net.ipv4.ip_local_port_range="1024 65535"
```

进程 listen backlog 是 65535，内核按 `somaxconn` 封顶。mock 机 `somaxconn` 已是 65535；网关机若仍是 4096 要先改。

冒烟：

```bash
curl -sS -m 30 http://10.99.255.101:8080/health
curl -sS -m 30 http://10.99.255.101:8080/ready
curl -sS -N -m 30 -X POST http://10.99.255.101:8080/v1/chat/completions \
  -H 'Authorization: Bearer sk-loadtest' -H 'Content-Type: application/json' \
  -d '{"model":"mock","messages":[{"role":"user","content":"hi"}],"stream":true}'
```

## 3. 客户现场

逻辑角色：

| 机器 | 角色 |
| --- | --- |
| 22 | Postgres + Redis（New API 状态；xlite 只需 Redis） |
| 24 | 网关副本 1 |
| 26 | 网关副本 2 |
| 29 | mock + k6 |
| HAProxy | 代理 24+26，`:8005` 双节点 / `:8006` 单节点 |

`10.56.88.0/24` 上脚本里出现过的地址：HAProxy `10.56.88.31:8005`（也见过 `10.56.88.22:8005`）；后端 `10.56.88.32:8001`、`10.56.88.33:8002`；健康检查 New API 是 `GET /api/status`，xlite 改成 `GET /health`。

HAProxy 参考 `maxconn 200000`，`timeout client/server/tunnel 600s`，`option http-keep-alive`。

xlite 双副本共用同一 Redis（`xlite:catalog` + `xlite:catalog:lock`）。Admin Cookie 无状态签名，两节点 `ADMIN_PASSWORD`（或 `ADMIN_SESSION_SECRET`）一致即可，不必粘性。Postgres 数据面用不到。

## 4. 对标

客户目标：3 万并发连接。New API 现场 Redis `connected_clients` 到过 15596、6 个 key 合计 200MB+。xlite 请求路径不访问 Redis。

和 New API 比 TTFT/dur/fail 时，喂法必须抄 V88，见 [`v88-rig.md`](v88-rig.md)。

B 组、双节点时看（两套试验不要混填）：

| 指标 | 对标 V88（`KEEP_BODY=true`，活跃流约四五千） | 满负荷（看同时在转的流） |
| --- | --- | --- |
| fail | ≤ V88（贴 0%） | < 0.5% |
| duration p50 | ≤ V88（约 6.5s；冒烟 5.5s） | 千兆上会被网卡拉到 ~30s |
| TTFT p50 | ≤ V88（mock 500ms + 网关；勿低于 500ms） | 满负荷会高于 750ms |
| 网关 ESTAB | 与 `qps × dur` 都写进报告 | 若「连接」= 在途 SSE，目标 ≥ 30000 |
| Redis `connected_clients` | 约 2 | 约 2 |
| Redis 内存 | < 1MB | < 1MB |

TCP ESTAB 含 keep-alive 空闲时，think-time 下也可以看到近 3 万 ESTAB、只有几千路在转。带宽账 `30k × 100B × 100 token/s ≈ 2.4 Gbps` 用的是**活跃流**。千兆上 B 组满负荷会切片；那种环境用 E 组或万兆打满负荷，对标 V88 用少喂 rig。
