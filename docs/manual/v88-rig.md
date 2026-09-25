# 与 New API V88 对标的压测 rig

客户现场比分用的是同一套 k6 阶梯（`run_full_88.sh`）。要对齐 V88 的 TTFT / duration / fail，**先对齐喂法**，再比网关。实验室拓扑、24 mock、3 万 ESTAB 满负荷见 [`lab-loadtest.md`](lab-loadtest.md)。

权威驱动器：客户包里的 `deploy_origin/loadtest/run_full_88.sh`（四个 driver 一律 `-v /root/loadtest/k6:/k6` → `/k6/k6.js`）。客户包在 `bench/` 下、**不进仓库**（`.gitignore`），所以这里只是文献引用；仓库内可达、且已按客户对标喂法对齐的是 [`scripts/loadtest/k6/k6.js`](../../scripts/loadtest/k6/k6.js)（差异见 §1.2）。

**客户机上被挂载执行的永远是 `/root/loadtest/k6/k6.js`，而那份文件在对标那晚被手改过至少四次**；包里两份 `k6.js`（`deploy88/k6.js`、`loadtest/k6/k6.js`）**都不是**跑出对标数据的那一版。对标那批数据（含客户 708ms）的指纹是 **`checks` 比 2.00 + `gracefulStop: 10m0s` + 有 `gateway_overhead_ms`** —— 判据见 §1.1。

## 1. V88 原脚本怎么喂

`run_full_88.sh`（2026-09-17 与需求方确认的参数）：

| 项 | 值 |
| --- | --- |
| 入口 | `MODE=single` → `:8006`；`MODE=dual` → `:8005` |
| 挂载 | 四个 driver 都是 `-v /root/loadtest/k6:/k6`、跑 `/k6/k6.js` —— **挂载路径不区分版本** |
| `KEEP_BODY` | **写死 `true`**（`-e KEEP_BODY=true`） |
| `KEEP_SAMPLE` | **driver 不传**。对标那轮的挂载版没有采样 ⇒ `KEEP_BODY=true` 时**每次迭代都解析 body**（checks 比 2.00）。包里的 `loadtest/k6/k6.js`（v3.2-memopt）默认 0.1（→1.10）；`deploy88/k6.js` 没有这个变量（→2.00） |
| think-time / `PICK_INTERVAL` | **不传**。k6 默认走进 `sleep(0)`，没有 30s 睡眠 |
| VU | **1000 起，+1000**，上限 60000 |
| 停测 | 连续两档 `http_req_failed > 1%` |
| 稳态 | 短流 300s / 长流 640s |
| 收尾 | 对标那轮用 `GRACEFUL_STOP` 默认 **600s**（日志 header 实测 `10m0s`）；包里的 `deploy88/k6.js` 硬编码 **`10s`**，**不是对标那版** |
| 流 | `STREAM=true`，`stream: true` |
| mock | 24 路，`ttft=500`（冒烟自证 `t=5.598s / 502 chunks`），组 B `out=500 interval=10ms`（墙钟底 ≈ 5.5s） |
| k6 | docker `-m 45g`，`--compatibility-mode=base` |

k6 每个 VU 一圈：`http.post` → `check` → `sleep(0)`。`ttft_ms` 是 `res.timings.waiting`（第一个 HTTP 字节），**在解析 body 之前就记录** —— 所以 2.00 / 1.10 / 1.00 的差别只影响 `iteration_duration` 与 qps，**不抬高首字延迟**。`discardResponseBodies: !KEEP_BODY`，对标那轮全局留 body。

圈间空档**不是**显式 think-time（各版都是 `sleep(0)`）。全量解析版在 `KEEP_BODY=true` 下**每次迭代**都要对约 104 KB body 做 `indexOf('[DONE]')` + 正则提 500 个 `ts` + **499 次 `Trend.add()`**，加上 `discardResponseBodies: false` 与 `-m 45g`，把 `iteration_duration` 拉长到 10s~98s（长流 D 组更重：7000 VU 起 k6 直接空指标）。

### 1.1 判据：对标那晚挂的是哪一版（三个指纹）

`run_full_88.sh` 只 `-v /root/loadtest/k6:/k6`，**不校验脚本内容**，任何版本都记在 `/k6/k6.js` 名下。三个可从日志机械提取的指纹：

| 指纹 | 位置 | 读数 |
| --- | --- | --- |
| `checks_total / http_reqs` | k6 汇总 | **2.00** = 每次迭代全量解析；**1.10** = 采样 0.1；**1.00** = 完全不解析 |
| `gracefulStop` | 日志头部 scenario 行 | **`10s`** = 硬编码那一族；**`10m0s`** = `GRACEFUL_STOP` 默认 600s 那一族 |
| `gateway_overhead_ms` | k6 汇总 CUSTOM | **有出数** = 该版用 `res.timings.receiving`（`loadtest` 族）；**没有** = 该版用的是 `res.timings.total` —— 而 k6 v2.x 的 `timings` **根本没有 `total` 字段**（客户离线包 k6 v2.2.0，本地实测 `Object.keys(res.timings)` = `duration, blocked, looking_up, connecting, tls_handshaking, sending, waiting, receiving`），`isFinite(NaN)` 恒假 ⇒ `deploy88/k6.js` **永远出不了 gw_oh** |

对 `deploy_origin/loadtest/results` 全部日志实测（客户机同一晚，09-17 15:30 → 09-18 08:41）：

| 时间 | 结果集 | gs | checks 比 | gw_oh | 判定 |
| --- | --- | --- | --- | --- | --- |
| 15:30 / 15:58 | `v4-run1-mock12`、`v4-run2-mock18-dual` | 10s | 1.00 | 无 | driver 不传 `KEEP_BODY` ⇒ 只留状态码校验；gs=10s ⇒ 硬编码族 |
| 16:18–16:36 | `v4-levels-partial/D_1000`、`D_4000` | 10s | 2.00 | 无 | 全量解析 + 硬编码 10s + `timings.total` 版 gw_oh ⇒ **就是包里 `deploy88/k6.js`** |
| 16:36:51 | `v4-levels-partial/D_21000` | — | 报错 | — | `ReferenceError: GRACEFUL_STOP is not defined at /k6/k6.js:133:21` —— 现场把 `'10s'` 改成 `GRACEFUL_STOP` 却忘了 `var`（列 21 正是 deploy88 那版 133 行的取值位置） |
| 16:56 / 17:06 | `v4-levels-partial/D_5000`、`D_6000` | 10m0s | 2.00 | 无 | 同一份文件补回 `var GRACEFUL_STOP`（默认 600s），gw_oh 仍缺 ⇒ **还是 `deploy88/k6.js` 那一族** |
| 17:29–08:33 | `v88-single` B 组 58 档 + D 组 6 档 + E@1000…20000（共 **84 档，含 708ms**） | 10m0s | 2.00 | **有** | **第三份**：全量解析 + 600s + `receiving` 版 gw_oh。包里没有这一版；用 `loadtest/k6/k6.js + KEEP_SAMPLE=1` 逐项复现（本机实测 2.000 / 10m0s / chunk+gw_oh 全中） |
| 08:34–08:40 | `v88-single/E_21000` | 10m0s | **1.10** | 有 | 与包里 `loadtest/k6/k6.js`（v3.2-memopt，默认 `KEEP_SAMPLE=0.1`）逐项吻合 ⇒ 那晚最后一次换版 |
| 08:41 | `v88-single/E_22000` | — | 空指标 | — | 停测 |

一条演化线（同一份 `/root/loadtest/k6/k6.js` 被手改四次）：
`deploy88/k6.js`（全量 + 硬编码 10s + `timings.total`）→ 补 `GRACEFUL_STOP`（600s）
→ 换 `receiving` 版 gw_oh（= **第三份**，对标那批数据）→ 加 `KEEP_SAMPLE=0.1`
（= 包里 `loadtest/k6/k6.js`，最后一档）。

> ⚠ **别把「文件身份」和「复现方式」读混**：说「客户跑的是第三份」讲的是**文件身份** —— 当时挂在 `/root/loadtest/k6/k6.js` 的字节既不是包里那份 `deploy88/k6.js`，也不是包里那份 `loadtest/k6/k6.js`（driver 不传 `KEEP_SAMPLE`，而包里那份默认 0.1）。说「`loadtest/k6/k6.js + KEEP_SAMPLE=1` 能复现」讲的是**复现方式** —— `KEEP_SAMPLE=1` 时 `analyze = KEEP_BODY && (rand < 1)` 恒真，与 `if (KEEP_BODY)` 逐分支等价，所以四个可观测特征全中。**不能**因此写成「客户跑的就是包里那份 k6.js」。

三条结论：

1. **对标那批数据（含客户 708ms）出自「第三份」：全量解析 + 600s + `receiving` 版 gw_oh —— 包里没有这个文件。** `deploy88/k6.js` 不是它（硬编码 10s，且因 `timings.total` 不存在而根本出不了 gw_oh）；包里 `loadtest/k6/k6.js` 也不是它（默认 0.1 ⇒ 1.10）。复现：`loadtest/k6/k6.js` 加 `KEEP_SAMPLE=1`，或直接用 [`scripts/loadtest/k6/k6.js`](../../scripts/loadtest/k6/k6.js)（`KEEP_SAMPLE` 默认就是 1）。
2. 包里 `loadtest/k6/k6.js` 只对上最后一档 E@21000 的 1.10 —— 它反证了同一份 driver 在 B/D 上会给出 1.10，而实测是 2.00，即中间确实换过文件。
3. 换版发生在 E@20000（08:33）与 E@21000（08:34）之间；那次换版让 qps 从 1033 涨到 1912、TTFT p50 从 714ms 升到 **1747ms** —— 是喂得更满，**不是刷分**（`ttft_ms` 取首字节，也不受 body 解析影响）。

这也是 `run_full_88_xg.sh` 后来要加 preflight 哈希校验的原因：挂载路径恒定、内容可变，只认路径无法复现。

### 1.2 仓库内可达的 rig：`scripts/loadtest/k6/k6.js`

客户包在 `bench/`（`.gitignore` 忽略），所以对标喂法的可执行载体是 [`scripts/loadtest/k6/k6.js`](../../scripts/loadtest/k6/k6.js)。它**按客户对标喂法设默认值**：

| | 值 | 与客户原件的差异 |
| --- | --- | --- |
| `KEEP_BODY=true`（driver 写死） | 每次迭代全量解析 → checks 比 **2.00** | 同对标那版 |
| `KEEP_SAMPLE` | 默认 **1**（全量） | 客户 memopt 版默认 0.1；显式传 `KEEP_SAMPLE=0.1` 即复刻 memopt（1.10），只在内存吃紧时用 |
| `KEEP_BODY=false` | 1.00，且不注册 chunk/gw_oh 指标 | 同客户原件（满负荷「3 万连接」那一臂） |
| `GRACEFUL_STOP` | 默认 **600s** | 同对标那版 |
| 额外诊断 | `ERR_SAMPLE`（429 原因计数）+ `x-xlite-*` 头采集 | 客户原件没有；不参与 `ttft / dur / fail / qps` 计算 |

复现对标喂法（**不要传 `KEEP_SAMPLE`**）：

```bash
k6 run --compatibility-mode=base \
  -e KEEP_BODY=true -e STREAM=true -e WARMUP=30 -e STREAM_TIMEOUT=600000 \
  -e TARGET_URL=… -e TOKEN=… -e MODEL=mock-model -e VUS=1000 … \
  scripts/loadtest/k6/k6.js
# 不传 PICK_INTERVAL、不传 XLITE_MAX_IN_FLIGHT、不传 KEEP_SAMPLE
```

### 1.3 报告契约：三道机械闸门

只认「挂载路径」是这个台架翻过车的地方（§1.1），所以跑数用 [`scripts/loadtest/run_levels.sh`](../../scripts/loadtest/run_levels.sh)（driver）+ [`report.py`](../../scripts/loadtest/report.py)（出报告），三道闸门都是机械判定、失败即**拒绝出报告**：

| 闸门 | 判据 | 失败 |
| --- | --- | --- |
| preflight | `k6.js` 是仓库版（md5 打印 + `KEEP_SAMPLE` / `GRACEFUL_STOP` 特征） | exit 3 |
| 每档指纹 | `checks_total / http_reqs` = **2.00**（±0.01）且日志头 `gracefulStop: 10m0s` | exit 3 |
| 链路预算 | `活跃流 = qps × dur_p50`，[`link_budget.py`](../../scripts/loadtest/link_budget.py) 判 `LINK-BOUND` | exit 2 |

每个结果目录必须自带：`env.txt`（网关镜像、k6 镜像/digest、`k6.js` md5、`TOKIO_WORKER_THREADS`、`somaxconn`、档位）、每档 `<LEVEL>.log` + `<LEVEL>.summary.json`、`meta.json`（含每档活跃流与预算判读）。**没有 `meta.json` 或标了 `INVALID` 的档位不许进对标表。**

```bash
# lab：V88 B 组档位，10 GbE，报告自动带活跃流与预算判读
TARGET=http://10.99.255.101:18080/v1/chat/completions TOKEN=sk-loadtest \
MODEL=mock-model LEVELS="1000 4000 10000 31000" \
LINK_MBPS=10000 GATEWAY_IMAGE=xlite:0.3.0 \
  scripts/loadtest/run_levels.sh

# 预算自查（也可单独跑，退出码 2 = 链路绑死）
scripts/loadtest/link_budget.py --selftest
```

`K6_IMAGE` 建议钉 digest：`grafana/k6:latest` 漂版本会改 qps 一列（同一 xlite、同档位，k6 v2.3.0 vs v0.57.0：TTFT 不变、qps 变）。

## 2. 名义 VU ≠ 在途流

稳态近似：

```
真实在途 ≈ qps × http_req_duration
```

k6 的 `VUS=31000` 只是协程数。下面这组数是**客户 88 集群 New API 的运行日志**（`deploy_origin/loadtest/results/v88-single` ≡ `nohup-full88.log`，TARGET `http://10.56.88.31:8006`，09-17 17:28 → 09-18 08:41），**不是**实验室 xlite；实验室 xlite 的同喂法数据见 §8。V88 单节点 `KEEP_BODY=true` 组 B 实测：VU 从 5k 涨到 58k，`qps × dur` 仍在约 **3000–4000**，QPS 封在 ~600。组 B 31000 VU：qps 584.5 × dur 6.55s ≈ **3828** 路在途。

组 E 20000→21000 **换了 k6 脚本**（判据见 §1.1），不是同一脚本突然能吃了：`checks_total / http_reqs` 从 **2.00**（每次迭代 `status` + `body ok`，全量解析 body）变成 **1.10**（`KEEP_SAMPLE=0.1`）。VU 只多 5%，qps 从 1033 到 1912，收尾连接数从 1.0 万到 2.0 万。一喂满：TTFT p50 1747ms / p99 21s；22000 档 k6 空指标。B_31000 的 checks 比也是 2.00，iteration − http 约 45s。

协议：对客户端 HTTP/1.1（k6 + HAProxy `mode http`），对 mock `h2c`。xlite 同样。

**1 GbE 上组 B 的容量（本节口径，别处引用它）**：每流 payload ≈ **100.7 KB**（`502` 个 `data:` 行，实测），在 `ts` 跨度 ≈ **5.35s** 内送完 ⇒ 每流 ≈ **18.8 KB/s**；1 GbE 实用 ≈ **112 MB/s**（`0.9 × 1000 Mbps`）⇒ **≈5900 路活跃流**。实测锚点：**4000 路不切片**（过网关 5.86s vs 直连农场 5.81s），**8000 路已切片**（7.12s）。§7、`docs/dev/loadtest-newapi-controlled-2026-09.md` §9 与 `scripts/loadtest/link_budget.py` 都用这个数。

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

k6 TTFT 是 `timings.waiting`（第一个字节）。空头会把排队从 waiting 挪到 receiving：报表 TTFT 可到 ~1ms（**低于 mock 500ms**），duration 不变，`gateway_overhead_ms` 会暴涨。客户 V88 日志（New API）低负载 `gw_oh` 约 **−200ms**（晚发头；该列口径见 §1.1）。先回 200 还破坏 failover 和状态码透传，违反 `docs/ai/agents/wire-boundary.md`。不改这条规则。

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

实验室同一 xlite、同一 31000 VU（组 B）只改喂法（A/B/C 是实验室 xlite；最后一行是**客户 V88 日志里的 New API**，列在这里只为对照喂法差异）：

| 臂 | 喂法 | 活跃流 | TTFT p50 | dur p50 | fail |
| --- | --- | --- | --- | --- | --- |
| A | 满负荷，无 sleep | ~28893 | 1.6s | 31.4s | 0% |
| B | `XLITE_MAX_IN_FLIGHT=6000` | — | **27.7s** | 33.3s | 0% |
| C | think-time 30s | ~5053 | 524ms | 6.29s | 0% |
| 客户 V88 原 rig（New API 日志，**非**实验室 xlite） | `KEEP_BODY=true`，无 sleep | ~4074 | 742ms | 6.55s | 0% |

注意：最后一行不是实验室 xlite 的实测；仓库内没有「实验室 xlite 在 31000 VU + `KEEP_BODY=true`」的日志（`bench/0924/xg-dual` 是 `KEEP_BODY=false` 的满负荷臂，checks 比 1.00，不能混用）。要紧的同条件对照见 §8。

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

热路径不读 Redis。满负荷时 Redis `connected_clients` 应仍约 2。客户 1GbE 预算上组 B 约 **5900 路**才不切片（算法与实测锚点见 §2）；对标 rig 的四五千在途正好落在这个区间，所以 duration 能贴近 5.5–7s。

## 8. 受控对照：把 New API 也搬到实验台（2026-09-25）

客户现场的 V88 日志里 New API 是 `ttft p50 = 708ms`，而同喂法下实验室 xlite 是 `502ms`。**这 206ms 是不是网关差异？** 把客户的 `new-api:v8-sentinel` 镜像搬到同一台实验机、同一份 rig、同一批 mock、同一天跑：

| 网关 | 档位 | TTFT p50 | dur p50 | fail | qps |
| --- | --- | --- | --- | --- | --- |
| xlite 0.2.0 | 1000 VU | **501.8ms** | 5.90s | 0.00% | 160.1 |
| New API `v8-sentinel` | 1000 VU | **504.2ms** | 5.84s | 0.00% | 158.8 |

mock 自己的 `ttft = 500ms`：xlite 加 **2ms**，New API 加 **4ms**。

**⇒ 客户日志里的 708ms 不是 New API 的网关开销**，是客户环境特有的（HAProxy 跳数 / 跨机 Postgres / k6 版本 / 主机负载），本轮未定位。候选中**最值得先验的是跨机 Postgres**：客户那套 rig 的稳态活跃流正好是 **3000–4000**，而 New API 每个请求的结算都要和 Postgres 往返一次（见下文高载段），两侧可能互相掩盖。低成本验证：看客户 New API 的 Postgres `pg_stat_activity` 峰值，以及把 PG 换到同机的对照。对标那轮客户挂的 k6 脚本是全量解析版（checks 比 2.00），判据见 §1.1。

**⇒ 因此「xlite 在纯延迟上领先 New API」这个说法不成立**：低载下两者无差别。不要拿延迟优势去对标。

高载段的结论经过一次更正，又经过一次**降级**：按客户配置（`SQL_MAX_OPEN_CONNS=2000` / PG `max_connections=2000`）时 4000 VU fail **12.4%**；把两边都放开（8000 / 10000）后出现过 **0.57%**，但**同一配置复跑又出现过 12.4% / 12.98%**（波动 20×）—— 所以那一档**不能用来归因**：既不能说"就是 DB 连接数配置"，也不能说"就是 New API 的行为"。要下结论先补 run ledger（每轮记 PG `max_connections` / `SQL_MAX_OPEN_CONNS` / fail%），见 [`../dev/loadtest-newapi-controlled-2026-09.md`](../dev/loadtest-newapi-controlled-2026-09.md) §3.1 / §10.4。可说的只有架构那条：放开连接数后瓶颈上移到 **Postgres CPU**（10000 VU 时吃满 74 核，错误为 `failed to connect to postgres`），根因是 **New API 每个请求的结算都要和 Postgres 往返一次**。本轮 New API 仍是 **1 副本 + 无 HAProxy + PG 与网关同机**，客户现场是 **2 副本 + HAProxy + 独占 PG**，**不可外推**。完整数据、部署配方、边界与复现命令见 [`../dev/loadtest-newapi-controlled-2026-09.md`](../dev/loadtest-newapi-controlled-2026-09.md)。
