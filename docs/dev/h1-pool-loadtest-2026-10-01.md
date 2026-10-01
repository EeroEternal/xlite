# h1 池压测记录（2026-10-01，实验室 101/102）

- **状态**：实测记录（一次性台架，非 CI）
- **被测**：`XLITE_UPSTREAM_H1_POOL=1`（`crates/xlite-h1-pool`）vs 默认 reqwest+`AdmissionGate` vs h2c 池
- **目的**：验证 h1 池对 reqwest 路径**不得 regression**；顺带量化连接数与协议对吞吐的影响
- **配套**：设计稿 [`h1-upstream-pool-architecture.md`](h1-upstream-pool-architecture.md) §10

> ⚠️ 台架是**同机 loopback + 单台 mock 机**的实验室结果，绝对值不外推；只用于**同台架对照**。

## 1. 拓扑

```
k6 (101, docker --network=host) ──HTTP/1.1──→ xlite (101, 各实例 1931x)
                                                   └──HTTP/1.1──→ Go mock ×24 (102, 8081–8104)
```

- 101、102 均为 128 核 / 1 TB；`load average` 空闲 <1。
- 网关 `--ulimit nofile=1048576`（**必须**：漏设时 1000 VU 起就 98% fail，见 §5 教训）。
- mock：102 上的 Go `mock-llm:h2c` 24 口；实测**同时支持 HTTP/1.1 与 h2c**，故 h1/h2c 两组打同一批 mock。

## 2. 实例配置（env）

| 端口 | 池 | `XLITE_UPSTREAM_H2C` | `XLITE_UPSTREAM_H1_POOL` | `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` |
| --- | --- | --- | --- | --- |
| 19310 | h1 池 | 0 | 1 | 32 |
| 19312 | gate + reqwest | 0 | — | 32 |
| 19314 | h1 池 | 0 | 1 | 128 |
| 19316 | h2c 池 | 1 | — | 32 |

其余共同项：`DATABASE_URL=sqlite:…/{h1pool,gate}.db`（catalog 见 §3）、`XLITE_CATALOG_RELOAD_SECS=0`、`TOKIO_WORKER_THREADS=8`、`ADMIN_PASSWORD=x`、`ulimit -n 1048576`。

## 3. mock 与 catalog

- 24 个 provider：`http://10.99.255.102:8081` … `:8104`，单 model `mock`，`weight=1 priority=0`；API key `sk-loadtest`。
- 每请求：mock 默认 `ttft≈500ms` + 500 chunk × 10ms ≈ **5.5 s**（故闭式并发 ≈ `VU × dur/5.5`）。

## 4. k6 脚本（`load.js`）

```js
import http from 'k6/http';
import { Trend } from 'k6/metrics';
const ttfb = new Trend('ttfb_ms', true);
export const options = { vus: Number(__ENV.VUS), duration: __ENV.DUR };
export default function () {
  const res = http.post(`${__ENV.GW}/v1/chat/completions`,
    JSON.stringify({ model: 'mock', stream: true, messages: [{ role: 'user', content: 'hi' }] }),
    { headers: { 'content-type': 'application/json', authorization: 'Bearer sk-loadtest' }, timeout: '600s' });
  ttfb.add(res.timings.waiting);
}
```

运行：`docker run --rm --network=host -v ~/h1lab:/scripts k6-customer:latest run -e VUS=<v> -e DUR=<d> -e GW=<url> /scripts/load.js`

## 5. 数据

### 5.1 h1 池 vs gate（`MAX_CONNS=32`）

| 负载 | 指标 | h1pool | gate(reqwest) |
| --- | --- | --- | --- |
| 200 VU / 30s | qps / ttfb 中位 / fail | 33.4 / 510ms / 0% | 33.4 / 497ms / 0% |
| 1000 VU / 30s | qps / ttfb 中位 / fail | 105.2 / 717ms / 0% | 115.0 / 837ms / 0% |
| 3000 VU / 60s | qps / ttfb p90 / fail | 118.5 / 22.6s / 0% | 118.4 / 22.8s / 0% |
| 5000 VU / 60s | qps / ttfb p90 / fail | 128.0 / 35.2s / 0% | 127.8 / 35.5s / 0% |
| 2000 VU / 300s | qps / ttfb p90 / fail | 124.8 / 16.2s / 0% | — |

**修复前后（2000 VU 尾部，`Notify` 惊群）**：初版 h1pool ttfb p90 **18.5s** / p95 24–29s / max **46.8s**，gate 12.2 / 12.4 / 17.8s；改公平 `Semaphore` 后 h1pool 12.2 / 12.4 / 17.9s（与 gate 对齐，2 次重复）。

### 5.2 连接数是 h1 的最大杠杆

| 配置 | 2000 VU qps | 5000 VU qps | ttfb 中位 | 连接/口 |
| --- | --- | --- | --- | --- |
| h1pool `MAX_CONNS=32` | 111 | — | 10.7s | 32 |
| h1pool `MAX_CONNS=128` | **335** | **411** | 528ms / 6.35s | ~100+ |
| h2c 池 `MAX_CONNS=32` | **333** | **741** | 526ms / 543ms | 32（多流） |

### 5.3 reaper / churn

- 无 reaper：长时 300s 后 `dials` 46–71/口（基線 32），即连接 churn。
- 有 reaper：`dials` 稳定 32~33/口；压测结束后过一轮 `idle_timeout`（90s，扫描 45s），`connections` 全部回落 **0**、`reclaimed` = **32/口（共 768）**。

## 6. 教训（台架坑）

1. **漏设 `nofile`**：默认继承 ssh 的 1024，1000 VU 起两池都 ~98% fail（duration 只有 15ms 的快速失败）。启动前必须 `ulimit -n 1048576`。
2. **`MAX_CONNS` 会掩盖结论**：`32` 时 h1pool 与 gate 都是 111 qps（都被并发上限卡住）——此时"两者相同"是**瓶颈相同**，不是"没有差异"。要比较实现，先确认没有被配置饿住。
3. **k6 的 summary 里 `http_req_waiting` 可能不打印**（该镜像版本），用自定义 `ttfb_ms` Trend。

## 7. 结论

1. h1 池与 reqwest 路径**吞吐/尾部/fail 全面持平**（三档 + 长时），§7 的「不得 regression」通过。
2. **`MAX_CONNS` 是 h1 的最大杠杆**：`32→128` ⇒ qps 3.7×、ttfb 10.7s→528ms。h1 的默认值应**按 fd 预算**给得比 h2c 大得多。
3. **能用 h2c 就用 h2c**：5000 VU h2c 741 qps / 不排队 vs h1(128) 411 / 6.35s。h1 池是「上游只支持 HTTP/1.1」时的退路。
