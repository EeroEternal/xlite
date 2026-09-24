# 与 New API V88 对标的压测 rig

客户现场比分用的是同一套 k6 阶梯（`run_full_88.sh`）。要对齐 V88 的 TTFT / duration / fail，**先对齐喂法**，再比网关。实验室拓扑、24 mock、3 万 ESTAB 满负荷见 [`lab-loadtest.md`](lab-loadtest.md)。

权威脚本：`bench/0924/deploy_origin/loadtest/run_full_88.sh` + `loadtest/k6/k6.js`。

## 1. V88 原脚本怎么喂

`run_full_88.sh`（2026-09-17 与需求方确认的参数）：

| 项 | 值 |
| --- | --- |
| 入口 | `MODE=single` → `:8006`；`MODE=dual` → `:8005` |
| `KEEP_BODY` | **写死 `true`**（`-e KEEP_BODY=true`） |
| `KEEP_SAMPLE` | 不传，k6 默认 **0.1**（约 10% 迭代解析 body） |
| think-time / `PICK_INTERVAL` | **不传**。k6 默认走进 `sleep(0)`，没有 30s 睡眠 |
| VU | **1000 起，+1000**，上限 60000 |
| 停测 | 连续两档 `http_req_failed > 1%` |
| 稳态 | 短流 300s / 长流 640s |
| 收尾 | `gracefulStop` 默认 600s |
| 流 | `STREAM=true`，`stream: true` |
| mock | 24 路，`ttft=500`，组 B `out=500 interval=10ms`（墙钟底 ≈ 5.5s） |
| k6 | docker `-m 45g`，`--compatibility-mode=base` |

k6 每个 VU 一圈：`http.post` → `check` → `sleep(0)`。`ttft_ms` 是 `res.timings.waiting`（第一个 HTTP 字节）。`discardResponseBodies: !KEEP_BODY`，V88 全局留 body；单次只有抽中采样的才 `responseType: 'text'`。

圈间空档**不是**显式 think-time。31k VU + `KEEP_BODY=true` 的 JS/GC 把 `iteration_duration` 拉长，两次 POST 之间 VU 经常不在打网关。

## 2. 名义 VU ≠ 在途流

稳态近似：

```
真实在途 ≈ qps × http_req_duration
```

k6 的 `VUS=31000` 只是协程数。V88 单节点 `KEEP_BODY=true` 组 B 实测：VU 从 5k 涨到 58k，`qps × dur` 仍在约 **3000–4000**，QPS 封在 ~600。组 B 31000 VU：qps 584.5 × dur 6.55s ≈ **3828** 路在途。

组 E 20000→21000 **换了 k6 脚本**，不是同一脚本突然能吃了：`checks_total / http_reqs` 从 **2.00**（每次迭代 `status` + `body ok`，全量解析 body）变成 **1.10**（`KEEP_SAMPLE=0.1`）。VU 只多 5%，qps 从 1033 到 1912，收尾连接数从 1.0 万到 2.0 万。一喂满：TTFT p50 1747ms / p99 21s；22000 档 k6 空指标。B_31000 的 checks 比也是 2.00，iteration − http 约 45s。

协议：对客户端 HTTP/1.1（k6 + HAProxy `mode http`），对 mock `h2c`。xlite 同样。

## 3. 如何做到：不 cap、不 RST、不 429、超时 600s

这些是**配置和「不要加代码」**，不是新功能。0.3.0 已经按这个形态发了。

### 不要 `XLITE_MAX_IN_FLIGHT`

`21fa7a7` 已从主干拿掉。0.3.0 的 `src/config.rs` **没有**这个环境变量；设了也不会读。启动时不要带 `XLITE_MAX_IN_FLIGHT=…`。实验室曾用过的 `xlite:0.2.0-inflight` 镜像不要再用。

过载时不要自己加信号量/429。吞吐被网卡钉住时，排队只会把等待从 duration 搬进 TTFT（臂 B：dur 仍 ~33s，TTFT 1.4s → 27.7s）。

### 过载不 RST、不 429

数据面 **不会**对过载返回 429。`src/proxy.rs` 给客户端的是：鉴权失败 401、坏 JSON 400、没有模型 404、通道都失败才 502。`should_failover` 里的 429 是**上游**返回 429 时换下一条通道，不是网关自己限流。

RST 来自套接字打满，不是业务码：

| 点 | 做法 |
| --- | --- |
| listen backlog | 0.3.0 已是 65535（内核按 `somaxconn` 封顶） |
| 主机 | `sysctl net.core.somaxconn=65535`，容器 `ulimit nofile=1048576` |
| 入站 | 不设短 `header_read` / 请求超时；axum 默认不砍长流 |
| 上游 h2 | idle 90s **只收没有在飞流的连接**，不会在 31k 稳态里掐活流 |
| 上游 HTTP/1 回退 | reqwest `timeout` 已是 **600s** |
| HAProxy（客户现场） | `timeout client/server/tunnel 600s`，`option http-keep-alive`，`httpchk GET /health` |

0.2.0 现场 RST：backlog 还是 libc 128，爬坡丢 SYN。换 0.3.0 + 上面内核参数。仍出现「活 150–345s 再 reset」时查 HAProxy/中间设备，不要用 429 挡。

### 不要提前写 200 / SSE ping

k6 TTFT 是 `timings.waiting`（第一个字节）。空头会把排队从 waiting 挪到 receiving：报表 TTFT 可到 ~1ms（**低于 mock 500ms**），duration 不变，`gateway_overhead_ms` 会暴涨。V88 低负载 `gw_oh` 约 **−200ms**（晚发头）。先回 200 还破坏 failover 和状态码透传，违反 `docs/ai/agents/wire-boundary.md`。不改这条规则。

### 超时 600s 清单

与 V88 k6 `STREAM_TIMEOUT=600000`、`gracefulStop` 600s 对齐：

- k6：`-e STREAM_TIMEOUT=600000`
- xlite 上游 reqwest：已 600s
- HAProxy：`timeout client 600s` / `timeout server 600s` / `timeout tunnel 600s`
- 不要给 axum 加更短的全局 request timeout

## 4. 两套试验分开跑

| | 对标 V88（抄原 rig） | 满负荷 3 万 ESTAB |
| --- | --- | --- |
| 目的 | 同一喂法比 TTFT / dur / fail | 证明同时挂 3 万条 TCP |
| `KEEP_BODY` | **`true`**（与 `run_full_88.sh` 一致） | `false` 也可，以网关 `ss` ESTAB 为准 |
| think-time | **不要设**（原脚本没有） | 不要设 |
| VU 起点 | **1000**，+1000 | 可从高 VU 打，但必须看 ESTAB |
| 过载 | 不 RST、不 429；超时 600s | 同左 |
| 看什么 | k6 的 ttft / dur / fail / qps | `:8080` ESTAB、Redis `connected_clients` |
| 验收 | 与 V88 同档表可贴 | ESTAB ≥ 30000 且 fail 低 |

不要用对标 rig 的 TTFT 去填「3 万连接」那一格。对标 rig 下活跃流只有几千，数字好看是少喂。

实验室同一 xlite、同一 31000 VU（组 B）只改喂法：

| 臂 | 喂法 | 活跃流 | TTFT p50 | dur p50 | fail |
| --- | --- | --- | --- | --- | --- |
| A | 满负荷，无 sleep | ~28893 | 1.6s | 31.4s | 0% |
| B | `XLITE_MAX_IN_FLIGHT=6000` | — | **27.7s** | 33.3s | 0% |
| C | think-time 30s | ~5053 | 524ms | 6.29s | 0% |
| V88 原 rig | `KEEP_BODY=true`，无 sleep | ~4074 | 742ms | 6.55s | 0% |

臂 C 的 iteration − http ≈ 30s，等于人为睡眠，用来**解释** V88 的圈间空档；对标交付仍用原脚本的 `KEEP_BODY=true` + `sleep(0)`，不要改成 30s sleep。臂 B 说明限流只把难看从 dur 搬到 TTFT，不要恢复 `XLITE_MAX_IN_FLIGHT`。

「3 万连接」要问清是 **TCP ESTAB**（含 keep-alive 空闲）还是 **同时在转的 SSE**。`KEEP_BODY` 自锁 / think-time 下 ESTAB 可以仍高、活跃流只有几千。带宽账（`30k × token 率`）用的是活跃流。对标表填 TTFT/dur/fail；ESTAB 与 `qps × dur` 两列都写。

## 5. 压测时库与控制面（可调）

数据面规则不改：不准入排队、不提前 200、请求路径不碰库。压测只收控制面：

| | 单节点 lab | 双节点对标 |
| --- | --- | --- |
| 目录 | `DATABASE_URL=memory`，不必 Redis/Postgres | `REDIS_URL` 一份 `xlite:catalog`，**不要 Postgres** |
| 快照 | `XLITE_CATALOG_RELOAD_SECS=0` 冻结 | 同左（压测窗口目录不变） |
| 探活 | HAProxy / 脚本用 `GET /health` | `/ready` 会 `PING` Redis，压测不要打这条 |
| Admin | 可开着，压测流量不要打 `/api/*` | 两节点同一 `ADMIN_PASSWORD` |

New API 热路径打 Postgres/Redis；xlite 压测把库停在快照之外，比的是转发，不是目录。

## 6. 对标时 xlite 怎么跑

网关用 **0.3.0 及以上**（listen backlog 65535、入站 `TCP_NODELAY`、无 inflight cap）。过载只表现为 duration/TTFT 变长，不断 TCP。

k6 环境与 `run_full_88.sh` 对齐即可，核心是：

```bash
-e KEEP_BODY=true \
-e STREAM=true \
-e WARMUP=30 \
-e STREAM_TIMEOUT=600000 \
# 不传 PICK_INTERVAL、不传 XLITE_MAX_IN_FLIGHT
# START_VUS=1000，步进 1000
```

网关加 `-e XLITE_CATALOG_RELOAD_SECS=0`。组序、mock `_config`、token/model 与现场 V88 包一致（客户模型名可能是 `mock-model`，实验室 YAML 是 `mock`）。HAProxy 健康检查用 `GET /health`。

## 7. 不要当成优化的手段

- **圈间空档 / think-time / `KEEP_BODY=true`**：少喂，TTFT 变好是活跃流降到管道以内。只用于对标。
- **提前写 `200` 或 SSE ping**：只缩短 k6 `timings.waiting`（可低于 mock 500ms），duration 不变；`KEEP_BODY` 下 `gateway_overhead_ms` 会暴露。V88 没有先写空头。
- **后进先出**：p50 好看，栈底饿到 600s，fail 约 5%，过不了客户 0.5% 线。
- **`XLITE_MAX_IN_FLIGHT`**：排队进 TTFT。实验室 31000 VU cap=8000 时 TTFT p50 从 1.4s 到 26s。

热路径不读 Redis。满负荷时 Redis `connected_clients` 应仍约 2。客户 1GbE 预算上组 B 约 5900 路才不切片；对标 rig 的四五千在途正好落在这个区间，所以 duration 能贴近 5.5–7s。
