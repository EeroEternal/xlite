# 受控对照：xlite vs New API 实验台复现（2026-09-25）

性质：一次对照实验的原始数据与能定性的结论，**不是**发版数据，也**不是**已交付的性能承诺。
喂法规则见 [`../manual/v88-rig.md`](../manual/v88-rig.md)。

目的：客户现场的 V88 日志里 New API `ttft p50 = 708ms`，实验室同喂法下 xlite `502ms`。
把这 206ms 拆开——是**网关差异**还是**环境差异**？做法是把客户的 New API 镜像搬到同一台
实验机、同一份 rig、同一批 mock、同一天跑。

---

## 1. 方法（两侧唯一变量是网关）

| 项 | 值 |
| --- | --- |
| 网关 A | `xlite:0.2.0-inflight`，`XLITE_MAX_IN_FLIGHT` **不设**（= 0.2.0 原始行为），`--network host :18080`，`--ulimit nofile=1048576` |
| 网关 B | **客户镜像 `new-api:v8-sentinel`**（`/new-api`，Go，构建于 2026-09-17 —— 与 0918 那轮同期），`:13000` |
| B 的依赖 | Postgres `postgres:16-alpine` `:8007`（`shared_buffers=8GB`）+ Redis `redis:7-alpine` `:6380` |
| B 的目录 | 用客户自己的 `deploy88/init-db.sh` 配方：`type=1`、`models='mock-model'`、`setting={"http_protocol":"h2c"}`、同一 token、`LogConsumeEnabled=false`、`performance_setting.monitor_enabled=false`、`SelfUseModeEnabled=true` |
| 上游 | 102 上 24 × `mock-llm:h2c`（`:4101-4124`），B 组 `ttft=500 / 500 chunks × 10ms` |
| rig | 102 上客户包的 k6.js（`KEEP_BODY=true`、不传 `KEEP_SAMPLE`）+ `docker grafana/k6:latest` `-m 45g`，`WARMUP=30`，稳态 300s，档位 1000 / 4000 / 10000 / 31000 |
| 拓扑 | 网关在 101，k6 与 mock 在 102 —— 与客户现场同构（SSE 两次穿过链路） |

**口径自证**（两侧都成立，否则数据不可比）：

- body **逐字节一致**：`502` 个 `data:` 行、`100736` 字节
- `checks_total / http_reqs` 两侧均 **2.00** ⇒ 两侧跑的是**同一份全量解析版** k6.js（`KEEP_BODY=true` ⇒ 每次迭代 `status 200` + `body ok`）。这一列的判读、以及客户机上那份文件当晚换过几次，见 [`../manual/v88-rig.md`](../manual/v88-rig.md) §1.1
- 冒烟：xlite `t=5.607s`，New API `t=5.635s`（mock 理想 5.5s）

---

## 2. 结果一（最重要）：低载下两者**没有差别**

| 网关 | TTFT p50 | TTFT p99 | dur p50 | fail | qps |
| --- | ---: | ---: | ---: | ---: | ---: |
| xlite 0.2.0 @1000 VU | **501.8ms** | 509.7 | 5.90s | **0.00%** | 160.1 |
| New API `v8-sentinel` @1000 VU | **504.2ms** | 535.7 | 5.84s | **0.00%** | 158.8 |

mock 自己的 `ttft = 500ms`。**xlite 加 2ms，New API 加 4ms。**

⇒ **客户日志里的 708ms 不是 New API 的网关开销。** 它是客户环境特有的（HAProxy 跳数 /
跨机 Postgres（host 22）/ 客户那代 k6 / 主机负载），本轮**没能定位**是哪一项。

⇒ **纯延迟上 xlite 没有可主张的优势。** 见 §4 的更正。

---

## 3. 结果二：New API 的请求路径每请求打一次 DB（4000 VU 起撞墙，归因待补 ledger）

| 档位 | xlite（TTFT / dur / fail） | New API · **客户配置** | New API · **放开 DB 连接** |
| --- | --- | --- | --- |
| 1000 VU | 501.8ms / 5.90s / **0.00%** | 504.2ms / 5.84s / 0.00% | 504.2ms / 5.83s / **0.00%** |
| 4000 VU | 502.2ms / 5.86s / **0.00%** | 505.6ms / 5.87s / **12.39%** | 505.3ms / 5.87s / **0.57%** |
| 10000 VU | 502.2ms / 5.85s / **0.00%** | 6487ms / 3m47s / **99.81%** | 1235ms / 1.23s / **99.99%** |
| 31000 VU | 507.3ms / 5.90s / **0.00%** | 100%（快失败） | 83%（300s 只完成 **6** 个请求） |

- **客户配置** = `SQL_MAX_OPEN_CONNS=2000` + PG `max_connections=2000`（`init-db.sh` 与文档记录的现场值）。
- **放开 DB 连接** = `SQL_MAX_OPEN_CONNS=8000` + PG `max_connections=10000`，两者都加 `--ulimit nofile=1048576`。

### 3.1 因果链（压测期间实测采样）

| 档位 | pg 连接数 | New API CPU | **PG CPU** | fail |
| --- | ---: | ---: | ---: | ---: |
| 1000 VU | 12–39 | ~10 核 | 35–49% | 0.00% |
| 4000 VU | 29 → **3919** | 10–29 核 | 100% → **1881%** | **0.57%** |
| 10000 VU | ~2900 | 1.3–7.6 核 | **7370%**（74 核） | 99.99% |
| 31000 VU | ~2600 | 0.2–10.9 核 | 1500–2400% | PG 直接失联 |

1. **4000 VU 那堵墙与 DB 连接数有关，但「缺的就是连接数」这条因果链尚未成立。**
   单次观测：PG `max_connections` 2000 → 4000 → 10000 时 fail = 23.33% → 12.39% → 0.57%；
   但**放开后的同一配置（10000 / 8000）在多次运行里又出现过 12.4% 与 12.98%**（见 §10.4）——
   波动 20×，与配置差异同量级。**⇒ 这一档不能用来归因，也不能用来推 New API 的容量。**
   要把它变成结论，先补 **run ledger**：每次运行记 `时间 / PG max_connections /
   SQL_MAX_OPEN_CONNS / 4000 VU fail% / dur / note`。没有 ledger，本节与 §10.4 互相矛盾，
   `changelog` 与 `../manual/v88-rig.md` §8 里那句「主要是 DB 连接数配置」也不成立。
2. **放开连接数之后瓶颈上移到 Postgres 的 CPU**：10000 VU 时 PG 吃满 **74 核**，New API 只剩 1–7 核可干；
   错误分布变成 `failed to connect to postgres` ×**12207**。根因是架构性的：**New API 每个请求的结算
   （billing settlement）都要和 Postgres 往返一次**，所以它的吞吐天花板 = DB 层的吞吐天花板，而不是网关自身的转发能力。
   这一条（每请求打 DB）由失败签名与 `pg_stat_activity` 直接支持，不依赖第 1 条的因果。
3. 31000 VU 时 PG 直接不响应（`server closed the connection unexpectedly`），整个栈挂死。

### 3.2 能说与不能说

- **能说**：New API 的请求路径**每个请求都要打一次 DB**（结算），并发天花板因此由 Postgres 层决定；
  xlite 的请求路径完全不碰 DB/Redis。这是架构差异，可现场验证。
- **不能说**：「New API 在 10k 并发下不行」。本轮 PG 与网关共处一机、被 10000 条并发连接拖垮；
  客户现场 PG 是**独占 host 22**，结论可能完全不同。
- **不能说**：「放开连接数后 4000 VU 就只剩 0.57%」——同一配置复跑出现过 12.4% / 12.98%（§10.4），
  波动 20×。要用这个数必须先补 §3.1 的 run ledger。
- **要验**：客户 New API 的 Postgres 在两轮 v88 压测里的 `pg_stat_activity` 峰值与 CPU。
  若峰值贴着 `max_connections=2000`，他们那套「60000 VU 无失败」也可能同样是在 DB 墙前面停住的
  （客户 rig 的真实活跃流是 3000–4000，见 [`../manual/v88-rig.md`](../manual/v88-rig.md) §2）。

---

## 4. 更正：此前「xlite TTFT 领先 206ms（−29%）」的结论作废

该结论来自「**实验室 xlite 502ms** vs **客户历史日志 New API 708ms**」—— 未控制 k6 版本、
主机负载、HAProxy 跳数、Postgres 拓扑。§2 的受控实验证明：**同条件下 New API 也是 504ms**。

⇒ 对客户**不要主张延迟优势**。客户只要在自己机器上原样跑一次 xlite 就会发现是平局。

---

## 5. New API 实验台部署配方（可复现，含踩过的坑）

```bash
# 1) 镜像（客户给的 OCI tar）
scp new-api-v8.tar.gz bodesi@101:~/
docker load -i ~/new-api-v8.tar.gz          # → new-api:v8-sentinel

# 2) 依赖
docker run -d --name postgres-bench --network host \
  -e POSTGRES_USER=root -e POSTGRES_PASSWORD=123456 -e POSTGRES_DB=new-api \
  postgres:16-alpine -c port=8007 -c max_connections=4000 -c shared_buffers=8GB -c work_mem=16MB
docker run -d --name newapi-redis --network host \
  redis:7-alpine redis-server --port 6380 --save "" --appendonly no

# 3) 网关（--ulimit 必加！）
docker run -d --name newapi-bench --network host --ulimit nofile=1048576 \
  -e SQL_DSN="postgresql://root:123456@127.0.0.1:8007/new-api" \
  -e REDIS_CONN_STRING="redis://127.0.0.1:6380" \
  -e PORT=13000 -e GIN_MODE=release -e TZ=Asia/Shanghai \
  -e SQL_MAX_OPEN_CONNS=2000 -e RELAY_MAX_IDLE_CONNS=100000 \
  -e RELAY_MAX_IDLE_CONNS_PER_HOST=50000 -e STREAM_FLUSH_EVERY=1 \
  -e SESSION_SECRET=benchsecret -e CRYPTO_SECRET=benchsecret \
  new-api:v8-sentinel

# 4) 首次启动建表 → 用 adapt 过的 init-db.sh 灌 24 条 channel（指向 10.99.255.102:4101-4124）→ 重启
```

坑（每一个都真实踩到过）：

| 坑 | 现象 | 处置 |
| --- | --- | --- |
| **容器 `ulimit -n` 默认 1024** | 1000 VU 还能过，4000 VU 起大面积失败 | `docker run --ulimit nofile=1048576`。**两侧必须一致**，否则比的是 fd 上限 |
| **`docker rm postgres` 丢数据** | 重建容器后 channels 归零 → 100% 快失败（14k qps 重试风暴） | 重建后必须重跑 `init-db.sh`；或挂 volume |
| **`max_connections` 打满** | `error settling billing: failed to connect` | 抬高 PG `max_connections`；根治要看 §3 的结论 |
| **101 的 `:3000` 已被占用**（k8s NodePort 上另有一个 New API） | `bind: address already in use`，容器起来又退 | 换 `PORT=13000` |
| **`latest` 未钉版本** | 实验台拉到 `k6 v2.3.0`（镜像建于 09-21），客户 09-18 那轮是更老的 | 比 qps 时要么钉 digest，要么两侧同版本 |

---

## 6. 未做与边界（这些都不能从本文外推）

- New API 只跑 **1 副本、无 HAProxy**；客户是 2 副本 + HAProxy。§3 的高载结论**不可外推**。
- k6 用 `grafana/k6:latest`（跑时已是 **v2.3.0**）。同一 xlite、同一档位、只换 k6 版本
  （v2.3.0 vs v0.57.0），**TTFT 不变**（502.2 vs 502.9ms），**qps 变**（318.8 vs 390.5）——
  即 qps 一列对 rig 版本敏感，延迟一列不敏感。
- 实验台 102 上有别人的 Prometheus（617% CPU、load 15–35），**绝对值不可与客户机器比**。
- 未测：两侧在同机 10/25 GbE 或 loopback 下的真实并发上限；New API 2 副本下的表现。

---

## 7. 复现

原始日志在实验室 **`102:~/labtest/`**（`run1`–`run14`）与 **`101:~/labtest/init-lab-db.sh`**。

```bash
# xlite 侧（客户原 rig 逐字复刻）
OUT=~/labtest/runX bash ~/labtest/sweep4.sh 1000:300s 4000:300s 10000:300s 31000:300s

# New API 侧（同一份 sweep，只换 TARGET/TOKEN/MODEL）
TARGET=http://10.99.255.101:13000/v1/chat/completions \
TOKEN=GVxf1uUl0mnn7ZiLXXgEJsjaZz9a3kMJMNe0AIntZrv38dd7 MODEL=mock-model \
OUT=~/labtest/runY nohup bash ~/labtest/sweep6.sh 1000:300s 4000:300s 10000:300s 31000:300s
```

---

## 8. 可观测的边界：为什么 `x-xgateway-*` 那两个头不进对标表

曾提议给 xlite 加 `x-xgateway-queue-wait-ms` / `x-xgateway-upstream-ttft-ms` 以便对标。
结论：**不进对标表，只作自家归因工具（P3）**。先把归属钉死——这一条容易记错：

| 事实 | 证据 |
| --- | --- |
| 这两个头是 **xgateway（xrouter）自己的**约定，不是对标 rig 的一部分 | `src/adapter/generic/{stream_handler,byte_forward,nonstream,dashscope}.rs` 里的 `.header("x-xgateway-queue-wait-ms", …)` |
| **客户原版脚本不读任何响应头** | 包里两份（`bench/0924/deploy_origin/loadtest/k6/k6.js` 采样版、`deploy_origin/deploy88/k6.js` 无采样版）对 `x-xgateway` / `ERR_SAMPLE` / `queue_wait_ms` **0 命中** |
| 读这两个头的是 **XG 侧我们自己后来加的那版** | `xrouter/scripts/loadtest/k6/k6.js`（含 `ERR_SAMPLE` + header 采集；xlite 侧同源：`scripts/loadtest/k6/k6.js`）。`xg-dual` 日志头部写的就是它：`ERR_SAMPLE=0.01 | k6=/root/loadtest_xg/k6/k6.js` |
| New API **没有任何计时头** | 其 71 MB 二进制里 `x-*-ms` 形态 **0 命中**（只有 `X-Accel-Buffering`，其余全是 AWS S3 SDK 的头） |
| 因此两侧日志里这两列都是空的 | `xg-dual`：`qw p50=- p99=- \| uttft p50=- p99=-`；`v88-single` 连这两列都没有（老脚本） |

**三条推论：**

1. **不能进对标表**：客户原版 rig 读不到；即便是我们自己那版 rig，New API 也不发 → 只能是单边列，客户第一句就会问「为什么只有你有」。
2. **加它不会让任何可比指标变好**：rig 里 `ttftMs.add(res.timings.waiting)` 与 `addHeaderTrend(...)` 是两条独立路径，头只往两个新 Trend 塞样本，**不参与 `ttft / dur / fail / qps` 的计算**。纯增量诊断，不存在「用头刷分」。
3. **`queue-wait-ms` 对 xlite 是死列**（没有准入闸门 → 恒 0）。只有 `upstream-ttft-ms` 有信息量：用来回答「31000 VU 下 TTFT 从 502ms 涨到 1598ms，那 1.1s 是在上游还是网关」。

### 对标表不需要它

三段分解用现有列就能做，`v88-rig.md` §4 那几列已经够：

```
首字前开销 = ttft p50 − mock_ttft（B 组 500ms）
首字后膨胀 = dur p50 − ttft p50 − 流式理想（500 × ti）
rig 侧空档 = iteration p50 − dur p50
```

B 组 @1000 VU 代入本文 §2 的受控数据：

| | 首字前开销 | 首字后膨胀 | rig 侧空档 |
| --- | ---: | ---: | ---: |
| xlite | **+1.8ms** | +398ms | +10ms |
| New API | **+4.2ms** | +336ms | +20ms |

首字前开销 **1.8ms 对 4.2ms** 就是 §2「低载平局」的全部依据——没用一个额外的头。

> 顺带一条待复核的观察：首字后膨胀 xlite 比 New API 高约 **60ms**（+398 vs +336，≈8% vs 6.7%）。量级在噪声边缘，但若可复现，那就是「每 chunk 成本」这条 P1 的直接证据。

---

## 9. 链路上的三个瓶颈，与「最高上限」该怎么取

整条链路上有三个可能的瓶颈（**链路 / mock 农场 / 网关**），前几节只量化了链路与网关。
把农场单独测出来：k6 直连 24 台 mock（loopback，绕开网关与链路），B 组配置。

| 流数 | dur p50 | dur p90 | fail | TTFT p50 | wire | 农场 CPU（24 容器合计） |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4000 | 5.81s | 5.82s | 0.00% | 501.5ms | 63 MB/s | ~13 核 |
| 8000 | 5.94s | 6.01s | 0.00% | 511.0ms | 124 MB/s | ~42 核 |
| 16000 | 6.21s | 9.54s | 0.00% | 515.5ms | 198 MB/s | 47–115 核 |
| **31000** | **8.99s** | 10.3s | **0.00%** | 547.6ms | **303 MB/s** | **~113 核**（宿主 100% busy） |

对照过网关（xlite，1 GbE，同 B 组）：

| 流数 | 过网关 dur p50 | **直连农场 dur p50** | 差异来自 |
| ---: | ---: | ---: | --- |
| 4000 | 5.86s | 5.81s | ≈ 0 |
| 8000 | 7.12s | 5.94s | 链路 |
| 16000 | 11.99s | 6.21s | 链路 |
| 31000 | **31.38s** | **8.99s** | **链路（22.4s）** |

**结论一：1 GbE 台架上先绑的是链路，不是农场也不是网关。** 直连农场的 31000 流只到 1.63×，
而它要 303 MB/s —— 1 GbE 实用上限约 112 MB/s。

**结论二：农场有自己的软天花板**——从 16000 流起 dur 由 6.21s 抬到 8.99s，CPU 到 ~113 核。

| 链路 | B 组链路容量 | 农场软天花板 | **先绑的是** |
| --- | ---: | ---: | --- |
| 1 GbE（客户现在） | **~5900 流**（算法与实测锚点见 [`../manual/v88-rig.md`](../manual/v88-rig.md) §2） | ~16k–31k 流 | **链路** |
| 10 GbE（升级后） | ~45000 流 | ~16k–31k 流 | **农场** |

⇒ **`cap = min(链路, 农场, 网关)`，三个数都必须实测。** 客户的「3 万并发」在 10 GbE 上会先撞**农场**。

### 9.1 「设一个最高上限」的三条硬约束

1. **值必须实测，不能拍脑袋。** 上一轮 cap=6000 而 1 GbE 的 B 组容量约 **5900 流**
   （[`../manual/v88-rig.md`](../manual/v88-rig.md) §2）→ cap 与容量同量级，根本没起到保护作用
   （实测 TTFT 1.6s → 27.7s，`dur` 未改善）。
2. **cap 不提高吞吐。** 四臂实测 goodput 恒定 **676–894 req/s**；cap 只决定难看在哪个指标上：
   不设闸 → `dur` 难看；设闸+无限等 → `TTFT` 难看；设闸+有界预算 → `fail%` 难看。
3. **只有一侧设闸 = 对设闸方不利。** 同一负载下设闸方的 `fail%` 高、不设闸方是 0%
   （New API 没有闸门）。**要设就两侧同策略**，或把「过载语义」写进规格与客户约定。

**诚实版「不至于很难看」**：`cap = 下游容量`；超额 **429 + Retry-After**（不是排队）；
延迟分位**只在成功请求上统计**，`fail%` 单独如实报；且两侧同口径。

### 9.2 边界

- 农场那组在**共享宿主**上测（k6 + prometheus + 24 mock 同机），宿主在 31000 流时已 100% busy；
  `docker stats` 逐容器 CPU 有噪声，**绝对值需在 mocks 独占 / k6 `taskset` 隔离后复测**。
- 直连用的是 HTTP/1.1（每 VU 一条连接），网关到 mock 走 **h2c**（多路复用）。两者的
  **每 chunk 发射成本相同**，但连接管理成本不同，所以这组只能给农场的**量级**，不能当精确容量。

---

## 10. 共享约束下的对标：能分胜负的只有两个指标

对标的前提是**双方共用同一条链路、同一批 mock** —— 所以 `C = min(C_链路, C_农场)` 是**共享的**，
`VU > C` 的档位对两个网关**没有区分度**（双方一起进入链路受限区）。能分胜负的只剩网关自身两项：

1. **TTFT p50**（首字前）
2. **`gateway_overhead_ms`**（首字后；用 mock 自己的 `ts` 抵消 mock 定时抖动）

### 10.1 受控对照（同机 / 同 24 mock / 同 k6 镜像 / 同脚本 / 同档）

B 组 @1000 VU，两侧 `checks/请求` 均 **2.00**（同口径），`KEEP_BODY=true` + `KEEP_SAMPLE=1`，
k6 用**客户自己的镜像 v2.2.0**：

| 指标 | **xlite 0.2.0** | **New API v8-sentinel** | 差 |
| --- | ---: | ---: | --- |
| `chunk_interval_ms` p50/p90/p95/p99 | 11/11/11/12 | 11/11/11/12 | 相同 |
| `chunk_interval_ms` avg | 10.798 | 10.676 | 两侧都是 mock 自身抖动 |
| **TTFT p50** | **501.77ms** | 504.17ms | **xlite −2.4ms** |
| TTFT p99 | 510.35ms | 533.84ms | xlite −23ms |
| **`gateway_overhead_ms` p50** | **0.51ms** | 4.18ms | **xlite −3.7ms（8×）** |
| `gateway_overhead_ms` p99 | 15.52ms | 44.75ms | **xlite −29ms（2.9×）** |
| `gateway_overhead_ms` avg | 1.35ms | 5.49ms | xlite −4.1ms |
| dur p50 | 5.90s | 5.83s | +70ms（**见下**） |
| fail | 0.00% | 0.00% | 相同 |

**关于那 70ms 的 `dur` 差异**：它来自 **mock 自身的定时抖动**（同一份 mock，两次跑的平均 chunk 间隔
10.798ms vs 10.676ms，×500 块 = 61ms）。而 `gw_oh`（`receiving − ts 跨度`）**恰好抵消这个抖动**，
所以在那个指标上 xlite 反而是领先的。**`dur` 不能用来比网关；`gw_oh` 才能。**

### 10.2 客户环境里的 ~205ms 不是 New API 的代码

v88 日志里 New API `TTFT p50 = 708ms`（= mock 500 + **208ms**）且 `gw_oh p50 = −205ms`。
这两者是**同一个延迟的两面**：网关若把客户端可见的流起点推迟 D，则 TTFT `+D`、`receiving` `−D`。

但**同一镜像、同一 k6、同一 mock 配置**在我的实验台上，New API 只有 `TTFT +4.2ms` / `gw_oh +4.2ms`。

⇒ **那 ~205ms 来自客户现场的路径（HAProxy 跳 / 网络），不是 New API 的转发实现。**
⇒ **未验证的推论**：若它是路径上的**常量**延迟，则两侧同加、不影响相对比较；若它与「该网关首块的发送方式」
耦合（HAProxy 缓冲 / 首块 flush 时机），就**不一定**同加。要钉死这一点，只能把 xlite 也放到客户那条路径上测
`gw_oh`（或在实验台上复现同一条 HAProxy 跳），不能靠推断。

### 10.3 rig 层面的更正：`v88-rig.md` §1.1 的归属要收窄

| 事实 | 证据 |
| --- | --- |
| `deploy88/k6.js` 的 `gw_oh` 用 `res.timings.total` | 源码 |
| **这个字段在当前任何 k6 里都不存在** | 客户自带镜像 **v2.2.0** 与实验室 **v0.57.0**，`Object.keys(res.timings)` 均为 `[duration, blocked, looking_up, connecting, tls_handshaking, sending, waiting, receiving]` → `total=undefined` |
| 所以 `deploy88/k6.js` **产不出** `gw_oh` | `isFinite(NaN)` 恒假 |
| 但 v88 日志里 `gw_oh` 有值 ⇒ 那轮跑的是**第三份脚本** | `if (KEEP_BODY)`（全量解析，checks=2.00）**+** `res.timings.receiving`（能出数） |
| **复现口径** | **`loadtest/k6/k6.js` + `KEEP_BODY=true` + `KEEP_SAMPLE=1`**（同时满足两个特征） |

⇒ 归档的两份副本**都对不上实际跑的那份**。对照协议必须**钉 k6 版本 + preflight 校验脚本哈希**，
否则连"哪个指标能出数"都不确定。

### 10.4 关于 New API 容量的边界

同配置（PG `max_connections=10000` / `SQL_MAX_OPEN_CONNS=8000`）下，4000 VU 的 fail 在多次运行中
出现 **0.57% / 12.4% / 12.98%** 的波动。⇒ **不能从实验台推 New API 的容量**；
只有两侧都 0% fail 的档位（如 B@1000）才是可用对照。

⇒ 这一条同时**推翻 §3 那半个因果**（"4000 VU 缺的就是连接数"）：波动幅度与配置差异同量级，
不补 §3.1 的 run ledger 就分不清是配置还是抖动。可说的只剩架构那条：
**New API 每请求打一次 DB**（失败签名 + `pg_stat_activity` 支持），所以它的天花板 = DB 层的天花板。

### 10.5 下一步：0.2.0 vs 0.3.0 的 A/B 口径

先把事实摆正：**零拷贝、入站 `TCP_NODELAY`、listen backlog 65535 三项都已经在 v0.3.0 tag 里**
（tag = 提交 `4876579` = `origin/main`；`[Unreleased]` 里那条是 changelog 条目的放置门禁处理）。
所以这里没有"代码要发"，只有**构建并部署 `xlite:0.3.0` 镜像**一件事 —— 实验室现在用的还是
`xlite:0.2.0-inflight`（§1 网关 A）。

| 项 | 口径 |
| --- | --- |
| 对照 | `xlite:0.2.0-inflight`（不设 cap）vs `xlite:0.3.0`，同机 / 同 rig / 同 mock / 同档 |
| 脚本 | `scripts/loadtest/run_levels.sh`（`KEEP_BODY=true`，`KEEP_SAMPLE` 默认 1 ⇒ checks 比 2.00；自动写 `env.txt` / `meta.json`） |
| 必须钉住 | k6 镜像 digest、`k6.js` md5、`TOKIO_WORKER_THREADS`（裸 `#[tokio::main]` 默认=核数，`deploy/env.example` 写 32） |
| 看什么 | **`gw_oh p50/p90/p99/avg`** + `TTFT p50/p99` + **网关 CPU / 每千活跃流**；`dur` 不比（§10.1） |
| 重复 | A/B 交错 ≥3 轮并报离散度 —— 本文 §10.4 自己就测到过 20× 抖动，单次 A/单次 B 不足以称"领先" |

两条预期要写对：

1. `TCP_NODELAY` 该作用在**尾部**（`p90/p99/avg`，当前 15.5 / 1.35ms），**不是 `p50`**（已经 0.51ms，没有下降空间）。
2. **loopback / 同机台架会掩盖 Nagle + 延迟 ACK** ⇒ 这一臂可能测出"零效果"，那是假阴性。要么在真实链路
   （或过一跳 HAProxy）上跑，要么在报告里标注「此臂对 NODELAY 不敏感」。零拷贝的收益本来也不在 `gw_oh`，
   而在**核数**（每千活跃流），所以核数必须一起报。

> 另外：在 1 GbE 的 C 以内，New API 在 4000 VU 前也不撞共享约束，但它会先撞自己的 DB 层（§3）。
> 要拿「上 10 GbE 后 xlite 更省」这个结论，仍然必须先有**宽链路台架**（P0-1），A/B 取代不了它。
