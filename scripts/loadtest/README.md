# scripts/loadtest — xlite 压测 rig

喂法规则、以及「哪一份 k6.js 真正跑了」的判据见
[`docs/manual/v88-rig.md`](../../docs/manual/v88-rig.md)。

| 文件 | 用途 |
| --- | --- |
| `k6/k6.js` | 喂负载的脚本（下面第一节） |
| `run_levels.sh` + `report.py` | **对标 New API** 的定档位压测 + 三道闸门（指纹 / 活跃流 / 链路预算） |
| `link_budget.py` | 链路预算预检（`--selftest` 自证；退出码 2 = LINK-BOUND） |
| `ab_configs.sh` | **同一份代码里某个旋钮/改动值不值**的 A/B harness（判决指标含网关 CPU）；`CONFIGS` 的短键 `TWT/CONNS/RELOAD` 会映射成真实 env 名，未知键直接非零退出 |
| `e2e_reload_swap.sh` | **端到端**验证「reload → 换快照 → 读者看到新快照」（`ab_configs.sh` 在目录冻结时走不到那条路径） |
| `ab_topology.sh` | 判**瓶颈落在网关 / mock 农场 / k6 哪一组**：三组进程分别采 CPU；把三组放同机 loopback 可把 1 GbE 从路径上拿掉。实测结论见 `../../docs/dev/loadtest-newapi-controlled-2026-09.md` §13（**农场是网关的约 5 倍，瓶颈是农场**）—— ⚠️ 那组数是 **`batch=1` 口径**：§14 实测其中约七成是台架每 chunk 一次 syscall 的开销，换 `batch=10` 后农场降到约 **2.7 倍**、且台架不再是天花板。用 `MOCK_BATCH` 选口径 |
| `lib_rig.sh` | 三个 harness 共用的台架互斥锁（`lab_lock`）+ mock 端口展开 `expand_ports` + 农场原值记账 `farm_remember`。锁绑 fd，进程退出/崩溃/`kill -9` 均由内核释放，不留僵尸锁 —— **但 `kill -9` 下 `trap ... EXIT` 不会执行**：孤儿网关/k6 容器会留下，**被改过的 mock `batch` 也不会被还原**（实测写完 `batch=7` 后 `kill -9`，农场就停在 7）。所以每轮都**回读并记录农场口径**（`progress.log` / `env.txt`），那才是「上一轮到底跑的是什么口径」的凭据。除 `ab_topology.sh` 外只用 bash 3.2 可用的特性（macOS 自带 `/bin/bash`） |

**对标表只报 `ttft / dur / fail / qps / ESTAB / 活跃流`。**
活跃流用**闭式** `VUS × dur/iter`（见 [`docs/manual/v88-rig.md`](../../docs/manual/v88-rig.md) §2）；
`qps × dur` 被排空段稀释、系统性低报，只在拿不到 `iteration_duration` 时当**下界**用。
下面的响应头是自家归因工具，**不进对标表**。

## k6/k6.js

默认值按**客户对标喂法**设定（客户机上那份文件的来龙去脉与三个指纹见
[`docs/manual/v88-rig.md`](../../docs/manual/v88-rig.md) §1.1 / §1.2）：

| 档 | env | checks 比 | 用途 |
| --- | --- | --- | --- |
| 对标 V88 | `KEEP_BODY=true KEEP_SAMPLE=1` | **2.00** | 与客户对标那批数据同喂法：每次迭代全量解析，`chunk_interval_ms` / `gateway_overhead_ms` 出数 |
| 省内存 / 客户 memopt | `KEEP_BODY=true KEEP_SAMPLE=0.1` | **1.10** | 客户后来的 v3.2-memopt 版；长流 + 高 VU、k6 内存吃紧时用 |
| 满负荷 3 万连接 | `KEEP_BODY=false` | **1.00** | 不解析 body，只留状态码校验 |

> ⚠️ **`KEEP_SAMPLE` 必须显式传，不要靠文件默认。** 本仓库的 `k6.js` 默认是 **1**，而客户机器上
> 挂的那份 memopt 副本默认是 **0.1** —— 「不传」等于把喂法交给**跑的是哪份文件**决定，于是同名
> 不同喂法：`scripts/loadtest/ab_configs.sh` 出过这个坑（日志写着 `KEEP_BODY=true`、实际跑 0.1，
> 拿到 601 qps 且**看起来一切正常**）。三个 harness（`run_levels.sh` / `ab_configs.sh` /
> `ab_topology.sh`）现在都**永远显式传**并打印生效值与文件自带默认。
>
> ⚠️ **`KEEP_BODY=true` 会让 k6 自己成为瓶颈：全量解析的实测上限约 315 qps**（与该档 VU 数无关，
> 是客户端 CPU 上限）。所以 **1000 VU（~155 qps）安全，4000 VU（要 ~600 qps）量到的是 k6 的墙**。
> 要 `gw_oh` 又要高载 ⇒ `KEEP_BODY=true KEEP_SAMPLE=0.1`（实测 604 qps）；纯产能/CPU ⇒ `KEEP_BODY=false`。
> `ab_configs.sh` 会把这条变成可见：按 `实测 qps ÷ (VUS ÷ dur_p50)` 判定，低于 60% 直接告警
> （诊断报文与踩坑记录见 [`docs/dev/loadtest-newapi-controlled-2026-09.md`](../../docs/dev/loadtest-newapi-controlled-2026-09.md) §13.6）。

不传 `PICK_INTERVAL` 时走 `sleep(0)`；`PICK_INTERVAL` **不是间隔旋钮** —— 任何 >0 的值都只走
`sleep(0)`，保留它只为与客户原件逐行同构。`GRACEFUL_STOP` 默认 600s（k6 会打印成 `10m0s`），
也是与客户原件同构；`run_levels.sh` 会把它透传给 k6 **并**作为 report.py 的指纹期望值。

```bash
# 对标 V88：KEEP_BODY=true 且**显式** KEEP_SAMPLE=1
k6 run --compatibility-mode=base \
  -e TARGET_URL=http://127.0.0.1:8080/v1/chat/completions -e TOKEN=sk-loadtest \
  -e MODEL=mock -e STREAM=true -e IN_TOKENS=100 -e OUT_TOKENS=500 \
  -e VUS=1000 -e DURATION=300s -e MAX_VUS=1000 -e WARMUP=30 \
  -e KEEP_BODY=true -e KEEP_SAMPLE=1 -e STREAM_TIMEOUT=600000 \
  scripts/loadtest/k6/k6.js

# 满负荷 ESTAB：KEEP_BODY=false（ERR_SAMPLE 默认 1% 仍会给失败响应体采样，供 429 归因）
k6 run --compatibility-mode=base \
  -e TARGET_URL=http://127.0.0.1:8080/v1/chat/completions -e TOKEN=sk-loadtest \
  -e MODEL=mock -e STREAM=true -e IN_TOKENS=100 -e OUT_TOKENS=500 \
  -e VUS=31000 -e DURATION=300s -e MAX_VUS=31000 -e WARMUP=30 \
  -e KEEP_BODY=false -e STREAM_TIMEOUT=600000 \
  scripts/loadtest/k6/k6.js
```

与客户原件的差异只有三处，都不改 `ttft / dur / fail / qps` 的口径：
`ERR_SAMPLE` 429 归因计数、`x-xlite-upstream-ttft-ms` / `retry-after` 响应头采集、
`KEEP_SAMPLE` 默认 1（客户 memopt 版默认 0.1）—— **正因为这一处不同，喂法必须显式传**。

## mock 的 chunk 合批（`MOCK_BATCH`）

农场（`mock-llm:h2c`）的 `_config?batch=N` 决定**每 N 个 chunk 一次 flush**。
它**不改载荷一个字节**（响应 100 734 B、`data:` 行 502 两档完全一致），但改**台架自身**的 CPU：

| | 农场 | 网关 | k6 | qps | 首字（ttft=500 基线） |
| --- | ---: | ---: | ---: | ---: | ---: |
| `batch=1` | 22.52 | 7.57 | 20.33 | 621.1 | 529 ms |
| `batch=10` | **6.83** | **2.62** | **5.00** | 630.6 | **722 ms** |

（4000 VU、同机 loopback、单臂 `A2`、120s。）所以它和 `KEEP_BODY` 是**同一条分界线**：

- **对标 / 延迟类** → 保持 `1`（**客户就是 1**，TTFT 是头条指标）；
- **产能 / 天花板类** → 用 `10`：实测 16000 档从"跑不完"变成 2518.8 qps、
  **31000 档 4732.1 qps 且 fail 0.00%**。

`run_levels.sh` 的 `MOCK_BATCH` 默认 **1**；`ab_topology.sh` / `ab_configs.sh` 的默认是**留空**
（= 完全不动农场配置，保持原行为），给值才下发**并回读校验**。数据见
[`docs/dev/loadtest-newapi-controlled-2026-09.md`](../../docs/dev/loadtest-newapi-controlled-2026-09.md) §14。

**四条硬约束**（`batch` 是口径，不是性能旋钮；完整版见
[`docs/manual/v88-rig.md`](../../docs/manual/v88-rig.md) §4）：

1. **同一张表 / 同一批数据里严禁混 batch** —— 不能拼在一起算比值（跨口径混算已犯过三次）。
2. **要改就两侧同口径重跑** —— 否则比的是两种农场成本，不是两个网关。
3. **batch≠1 的结论必须标「台架口径」** —— 真实上游是每 chunk 一次 flush = `batch=1`，
   所以**产品相关的绝对核数永远是 batch=1 那一组**。
4. **报告上看不到 batch 就不许进对标表** —— `env.txt` 记 `MOCK_BATCH` / `MOCK_BATCH_BEFORE`，
   `report.py` 把它打在**报告正文第一段**（≠1 时标 🧪 台架口径 + 首字代价）。

> **1 GbE 现场：改 batch 换不来档位。** 链路容量 ≈5900 活跃流先绑死，而 4000 VU 下农场
> batch=1 只要 ~24 核。batch 的收益只在**把链路拿掉的台架**（同机 loopback / 10 GbE）兑现。
> 想保口径又不够 CPU ⇒ **加农场机器**（`MOCKS=` 收 `host:port` 列表），别改 batch。

## 报告闸门（`run_levels.sh` + `report.py`）

`run_levels.sh` 跑完调用 `report.py`，三道闸门**机械判定**（契约见
[`docs/manual/v88-rig.md`](../../docs/manual/v88-rig.md) §1.3）：

| 闸门 | 判据 | 不过时 |
| --- | --- | --- |
| preflight | `k6.js` 是仓库版（打印 md5/sha256 + `KEEP_SAMPLE` / `GRACEFUL_STOP` 特征） | exit 3，未开跑 |
| 每档指纹 | `checks/http_reqs` = 2.00（±0.01）**且**日志头 `gracefulStop` = 期望值 | 该档标 **`INVALID`**；exit 3 |
| 链路预算 | 闭式活跃流过 `link_budget.py`，看是否 `LINK-BOUND` | **REFUSED**；exit 2 |

**不过 = REFUSED = 不写 `meta.json`**（并删除同目录遗留的旧 `meta.json`），只打印带 `INVALID` 标记的诊断表；用 `--allow-*` 绕行时仍会写 `meta.json`（报告头标 `🧪 DIAGNOSTIC`）。标了 `INVALID` 的档
不进对标表、也不参与链路预算闸门。诊断需要时用 `--allow-linkbound` /
`--allow-fingerprint-mismatch` 显式绕过（报告头会标注）。

改了 `GRACEFUL_STOP` 就跑 A/B（例如 30s）时，期望值必须跟着走：`run_levels.sh` 自动透传
`--expect-graceful-stop`，手工调 `report.py` 时自己加 `--expect-graceful-stop 30s`。

`<LEVEL>.acceptq`（accept 队列峰值）需要 Linux 的 `ss`；没有 `ss`（macOS 开发机）或跨机跑时
记 `unknown`，表里原样显示，**不会记成 0**。

## 响应头（诊断用，默认关）

| 头 | 网关侧开关 | 说明 |
| --- | --- | --- |
| `x-xlite-upstream-ttft-ms` | `XLITE_UPSTREAM_TTFT_HEADER=1` | 发上游请求 → 上游第一个 body 字节的耗时。用来回答「高负载下 TTFT 涨的那一截是在上游还是网关」。 |

两条硬约束：

1. **默认关，压测对标时不要打开。** 开启后网关会等上游第一个 body 字节再发响应头；
   对「先发头、后吐字」的上游会把客户端首字推迟到上游首字。
2. **不进对标表。** New API 不发任何计时头（其 71 MB 二进制里 `x-*-ms` 形态 0 命中），
   客户原版脚本也不读响应头，所以这一列只能单边填。
   rig **不采集** `x-xlite-queue-wait-ms`：xlite 没有准入闸门，排队恒 0，是死列（脚本里已删）。

三段分解用现有列就够：

```
首字前开销 = ttft p50 − mock_ttft
首字后膨胀 = dur p50 − ttft p50 − 流式理想
rig 侧空档 = iteration p50 − dur p50
```
