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

**对标表只报 `ttft / dur / fail / qps / ESTAB / 活跃流`。**
活跃流用**闭式** `VUS × dur/iter`（见 [`docs/manual/v88-rig.md`](../../docs/manual/v88-rig.md) §2）；
`qps × dur` 被排空段稀释、系统性低报，只在拿不到 `iteration_duration` 时当**下界**用。
下面的响应头是自家归因工具，**不进对标表**。

## k6/k6.js

默认值按**客户对标喂法**设定（客户机上那份文件的来龙去脉与三个指纹见
[`docs/manual/v88-rig.md`](../../docs/manual/v88-rig.md) §1.1 / §1.2）：

| 档 | env | checks 比 | 用途 |
| --- | --- | --- | --- |
| 对标 V88 | `KEEP_BODY=true`（**不传** `KEEP_SAMPLE`） | **2.00** | 与客户对标那批数据同喂法：每次迭代全量解析，`chunk_interval_ms` / `gateway_overhead_ms` 出数 |
| 省内存 / 客户 memopt | `KEEP_BODY=true KEEP_SAMPLE=0.1` | **1.10** | 客户后来的 v3.2-memopt 版；长流 + 高 VU、k6 内存吃紧时用 |
| 满负荷 3 万连接 | 不传 `KEEP_BODY` | **1.00** | 不解析 body，只留状态码校验 |

不传 `PICK_INTERVAL` 时走 `sleep(0)`，`GRACEFUL_STOP` 默认 600s —— 与客户原件同构。

```bash
# 对标 V88：KEEP_BODY=true 且不要传 KEEP_SAMPLE
k6 run --compatibility-mode=base \
  -e TARGET_URL=http://127.0.0.1:8080/v1/chat/completions -e TOKEN=sk-loadtest \
  -e MODEL=mock -e STREAM=true -e IN_TOKENS=100 -e OUT_TOKENS=500 \
  -e VUS=1000 -e DURATION=300s -e MAX_VUS=1000 -e WARMUP=30 \
  -e KEEP_BODY=true -e STREAM_TIMEOUT=600000 \
  scripts/loadtest/k6/k6.js

# 满负荷 ESTAB：不传 KEEP_BODY（ERR_SAMPLE 默认 1% 仍会给失败响应体采样，供 429 归因）
k6 run --compatibility-mode=base \
  -e TARGET_URL=http://127.0.0.1:8080/v1/chat/completions -e TOKEN=sk-loadtest \
  -e MODEL=mock -e STREAM=true -e IN_TOKENS=100 -e OUT_TOKENS=500 \
  -e VUS=31000 -e DURATION=300s -e MAX_VUS=31000 -e WARMUP=30 \
  -e STREAM_TIMEOUT=600000 \
  scripts/loadtest/k6/k6.js
```

与客户原件的差异只有三处，都不改 `ttft / dur / fail / qps` 的口径：
`ERR_SAMPLE` 429 归因计数、`x-xlite-*` 响应头采集、`KEEP_SAMPLE` 默认 1（客户 memopt 版默认 0.1）。

## 响应头（诊断用，默认关）

| 头 | 网关侧开关 | 说明 |
| --- | --- | --- |
| `x-xlite-upstream-ttft-ms` | `XLITE_UPSTREAM_TTFT_HEADER=1` | 发上游请求 → 上游第一个 body 字节的耗时。用来回答「高负载下 TTFT 涨的那一截是在上游还是网关」。 |

两条硬约束：

1. **默认关，压测对标时不要打开。** 开启后网关会等上游第一个 body 字节再发响应头；
   对「先发头、后吐字」的上游会把客户端首字推迟到上游首字。
2. **不进对标表。** New API 不发任何计时头（其 71 MB 二进制里 `x-*-ms` 形态 0 命中），
   客户原版脚本也不读响应头，所以这一列只能单边填。
   `x-xlite-queue-wait-ms` 同理**不提供**：xlite 没有准入闸门，排队恒 0，是死列。

三段分解用现有列就够：

```
首字前开销 = ttft p50 − mock_ttft
首字后膨胀 = dur p50 − ttft p50 − 流式理想
rig 侧空档 = iteration p50 − dur p50
```
