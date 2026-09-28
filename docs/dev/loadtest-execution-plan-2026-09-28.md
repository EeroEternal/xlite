# 实验台对标验证操作方案（2026-09-28）

- **状态**：现场压测操作 SOP / 验证指南
- **日期**：2026-09-28
- **目标**：
  1. 验证低并发（< 10,000 VU）下，上游保护策略与流预算对首字延迟（TTFT）零副作用（保持领先 New API 100~150ms）；
  2. 冲刺 50,000 超大 VU 极限高并发，验证在 `TOKIO_WORKER_THREADS=8` 线程收敛与传输层准入闸门下，彻底消除过载失败（保持 0.00% fail），对比 New API 48k+ 崩溃的现场数据。

---

## 1. 验证流程概览

```
┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
│  Phase 0: 准备   │ ──> │  Phase 1: 验证   │ ──> │  Phase 2: 冲刺   │ ──> │  Phase 3: 归因   │
│  启动与基线确认  │     │ 10k 以下低载影响 │     │ 50k 超大并发极限 │     │ 跨轮对比与产出   │
└─────────────────┘     └─────────────────┘     └─────────────────┘     └─────────────────┘
```

---

## 2. Phase 0：网关启动与基线环境核对

在网关机（双节点配置或单节点）上启动 XG 网关，应用经过充分论证的最佳参数。

### 2.1 网关启动命令（Docker）
```bash
docker run -d \
  --name xlite-gateway \
  --network host \
  --restart unless-stopped \
  -e TOKIO_WORKER_THREADS=8 \
  -e REDIS_URL=redis://10.56.88.22:6379 \
  -e XLITE_UPSTREAM_H2C=1 \
  -e XLITE_UPSTREAM_MAX_CONNS_PER_HOST=4 \
  xlite:0.5.0
```

> **参数核心设计与算术逻辑（为什么直接推荐设为 4）**：
> 1. **为什么 32 对 50k 大并发太大了**：
>    - 单 Mock 准入流上限公式为：$\text{CONNS} \times 250$。
>    - 若设为 `32`，单 Mock 准入高达 $32 \times 250 = \mathbf{8,000 \text{ 流}}$，24 个 Mock 准入高达 **192,000 流**。
>    - 冲到 60,000 VU 时，每个 Mock 涌入 $60,000 / 24 \approx \mathbf{2,500 \text{ 流}}$。但实测单个 Go mock 极限仅能承受约 **~1,200 流**，超出即崩溃吐 RST（这正是历史测试中高并发 fail 抬升的根因）。
> 2. **设为 `4` 带来的“低载零开销 + 高载自动安全阀”效果**：
>    - **低载段（< 24,000 VU）**：每个 Mock 在途流最多几十到几百，根本用不满 4 条连接，**完全惰性生效、零排队、零延迟额外开销**（1k~4k 的 TTFT 与设为 32 完全重叠）；
>    - **超高载（50,000 ~ 60,000 VU）**：每个 Mock 严格被卡在 $4 \times 250 = \mathbf{1,000 \text{ 流}}$（刚好在 1,200 生存线之下），Mock 永远不会崩溃吐 RST；超出的在途请求自动在网关 H2 连接池安全排队，确保**全局 0.00% fail**。
> 3. **对照备份**：
>    - 若现场想观测不设闸门时 mock 的原生表现，可切回 `-e XLITE_UPSTREAM_MAX_CONNS_PER_HOST=32` 对比。

### 2.2 负载均衡 HAProxy 现场完整配置（10.56.88.22）

现场入口 `:8005`（双节点）与 `:8006`（单节点）由 HAProxy 承接。**从 New API 切到 XG 时，必须核对以下配置**（完整配置如下）：

```haproxy
global
    maxconn 200000
    log stdout format raw local0

defaults
    mode http
    log global
    option http-keep-alive
    timeout connect 10s
    timeout client 600s          # SSE 长流：必须是 600s，与 k6/上游对齐，别改小
    timeout server 600s
    timeout tunnel 600s          # 隧道/流式阶段
    timeout http-request 60s     # 仅约束"收完初始 HTTP 请求头"这段

# 双节点入口（端口 8005）
frontend api_dual
    bind *:8005
    default_backend xgw_dual

# 单节点入口（端口 8006，用于单机对照测试）
frontend api_single
    bind *:8006
    default_backend xgw_single

# 双节点后端（同时转发至 10.56.88.24 和 10.56.88.26 两台网关）
backend xgw_dual
    balance roundrobin
    option httpchk GET /health
    http-check expect status 200
    server n24 10.56.88.24:8080 check inter 5s fall 5 rise 2
    server n26 10.56.88.26:8080 check inter 5s fall 5 rise 2

# 单节点后端（仅转发至 10.56.88.24 单台）
backend xgw_single
    balance roundrobin
    option httpchk GET /health
    http-check expect status 200
    server n24 10.56.88.24:8080 check inter 5s fall 5 rise 2
```

> **从客户原版 New API 切换时必改要点**：
> 1. **探活必须改用 `GET /health`**：客户原版配的是 `option httpchk GET /api/status`。XG 没有 `/api/status`，若不改，HAProxy 探活连续失败会把网关全判定为宕机，导致压测直接出 **100% 503 错误**！
> 2. **探活容错建议设为 `fall 5`**：容错窗口由原版的 15s（$5\text{s}\times 3$）放宽到 25s（$5\text{s}\times 5$），避免 50k+ 极端并发下由于网关瞬时排队造成探活毛刺而发生误判摘除（消灭 503 节点摘除风暴）。
> 3. **长连接超时（600s）严禁改小**：`timeout client/server/tunnel` 必须与压测脚本的 `STREAM_TIMEOUT=600000`（10分钟）严格对齐。

### 2.3 启动核验
```bash
# 1. 验证网关工作线程数（除辅助线程外，应有 8 个 tokio-runtime-w 线程）
ps -T -p $(pgrep xlite) | wc -l

# 2. 网关单机健康检查
curl -s http://127.0.0.1:8080/health

# 3. 验证 HAProxy 负载均衡入口健康状态（分别测双节点与单节点）
curl -s http://10.56.88.22:8005/health
curl -s http://10.56.88.22:8006/health
```

---

## 3. Phase 1：低载区验证（< 10,000 VU）与 Batch 影响检验

**核心目的**：
1. 确保在常规工作区间，首字延迟（TTFT）依然全面领先 New API（550ms vs 708ms），验证流预算闸门在低载下完全惰性无副作用。
2. **检验 `MOCK_BATCH=5` 对 10,000 以下并发的影响**：
   - **理论预期**：真实流式 LLM 上游是按 token 推送（即 `batch=1`，单 chunk 独立 flush）；`MOCK_BATCH=5` 是 mock 农场将 5 个 chunk 合批打包成单次传输。
   - **延迟代价**：每合批 5 个 chunk，首包到达时间（TTFT）会有小幅推迟约 `(batch - 1) × 10ms ≈ 40ms`（因为 mock 需缓冲攒够 5 块再发出）；但网络小包（DATA 帧/ACK）数量直接降低 80%，极大减轻了内核软中断负担。
   - **操作策略**：现场按两套口径对照跑，清晰定性。

在压测机进入 `deploy/v88` 目录执行：

### 方案 A：客户严格原版基线（`MOCK_BATCH=1`，对齐 100% 原始口径）
```bash
cd deploy/v88

# 1. 运行常规流 B 组（1000 ~ 4000 密阶梯）
MOCK_BATCH=1 ./run_full_88_xg.sh --combo B --levels 1000,2000,3000,4000

# 2. 运行长流 D 组（1000 ~ 4000 密阶梯）
MOCK_BATCH=1 ./run_full_88_xg.sh --combo D --levels 1000,2000,3000,4000
```
> **验收指标**：TTFT p50 稳定在 **550ms ~ 600ms**（领先 New API 708ms 约 100~150ms），p99 < 1200ms，fail 为 **0.00%**。

### 方案 B：合批优化测试（`MOCK_BATCH=5`，检验网络减负与延迟表现）
```bash
# 运行 B 组与 D 组，观察 batch=5 下的 TTFT 实际偏移
MOCK_BATCH=5 ./run_full_88_xg.sh --combo B --levels 1000,2000,3000,4000
MOCK_BATCH=5 ./run_full_88_xg.sh --combo D --levels 1000,2000,3000,4000
```
> **验收指标**：
> - TTFT p50 预期在 **590ms ~ 640ms** 之间（即使加了 ~40ms 批缓冲延迟，依然显著快于 New API 的 708ms）；
> - 观察网关机系统中断与压测机 softirq 是否显著下降（数据包减少 80%）。

---

## 4. Phase 2：冲刺 50,000 超大 VU（极限高载抗压）

**目的**：对标现场 New API 在 45,000 抖动、49,000 崩溃（fail 16%~27%）的极限拐点，检验 XG 在大并发下的抗压表现。

### 4.1 启动超高并发压测（建议首选 MOCK_BATCH=1 原生口径冲刺，备选 MOCK_BATCH=5）
```bash
cd deploy/v88

# 冲刺 D 组高并发段（建议覆盖关键对比档位）
# 首选 batch=1 验证传输层闸门 (XLITE_UPSTREAM_STREAM_CAP=250) 的原生抗压能力：
MOCK_BATCH=1 ./run_full_88_xg.sh --combo D --levels 15000,25000,35000,45000,50000

# 若 45k+ 因 mock 农场容器自身产生 RST 丢包，切换至 MOCK_BATCH=5 彻底解除 mock 自身算力瓶颈：
# MOCK_BATCH=5 ./run_full_88_xg.sh --combo D --levels 15000,25000,35000,45000,50000
```

### 4.2 仓库自带的伴随诊断与调优脚本（开箱即用）

在 `deploy/v88/advanced/` 目录下，仓库已经准备好了针对现场调优与瓶颈抓取的专用工具：

#### 1. 伴随采样脚本：`sample_three_walls.sh`（推荐全程开启）
在压测进行中，分屏运行此脚本。它采用只读方式，每 5 秒自动采样一次**三面墙**（农场事件循环、k6 客户端 CPU、网关 worker 调度），自动识别系统瓶颈落在哪台机器的哪一层：
```bash
cd deploy/v88/advanced

# 伴随压测运行（0 表示一直采到 Ctrl-C，行首带精确时间戳，与压测日志对齐）
./sample_three_walls.sh --seconds 0 --entries 10.56.88.22:8005
```
* **核心观测产出**：
  * `gw_twt` / `cpu_cores`：网关实际消耗核数；
  * `health_ms`：网关探活响应时间，如果超过 5s 说明网关事件循环排队饱和；
  * `conn_ms`：TCP 建立连接耗时，若升高说明 accept 队列（backlog）积压。

#### 2. 农场物理产能扫描：`farm_ceiling_scan.sh`（调优摸底）
如果想在不跑完整压测阶梯的情况下，快速摸清当前 24 个 Mock 在不同 batch 下的物理极限：
```bash
cd deploy/v88/advanced

# 扫描 D 组在当前 mock 农场下的真实产能天花板
./farm_ceiling_scan.sh --combo D --batch 1
```

---

## 5. 现场内核与网络关键调优项（执行前必验）

按照 `deploy/v88/advanced/CONFIG.md` 规范，压测前必须检查网关宿主机的 Linux 内核参数，避免因系统默认短板（如 `somaxconn=128`）造成非代码级塌方：

```bash
# 网关宿主机执行（确保连接队列与端口范围足够）
sysctl -w net.core.somaxconn=65535
sysctl -w net.core.netdev_max_backlog=65535
sysctl -w net.ipv4.tcp_max_syn_backlog=65535
sysctl -w net.ipv4.ip_local_port_range="1024 65535"
sysctl -w net.ipv4.tcp_tw_reuse=1

# 检查网关容器的 nofile（必须为 1048576，绝不能是 1024 默认值）
docker exec xlite-gateway sh -c "ulimit -n"
```

---

## 6. Phase 3：数据提取与对比报告生成

测试完成后，从生成的 `loadtest/results` 目录提取数据，重点横向对比：

| 评估维度 | XG 实测表现（本轮） | New API 实测基线（`v88-dual` 现场数据） |
| :--- | :--- | :--- |
| **低载 TTFT p50 (1k-4k)** | 预期 550ms ~ 600ms | 708ms ~ 714ms（XG 快 100~150ms） |
| **网关 CPU 消耗 (15k VU)** | 预期 1.5 ~ 2.6 核 | **7.2 核**（PID 176843 占用 720%） |
| **网关内存常驻 (15k VU)** | 预期 150MB ~ 300MB | **6.13 GB**（VIRT 23.5 GB） |
| **高并发失败率 (48k-50k VU)** | **预期 0.00% fail** | **16.01% ~ 27.58% fail**（出现大量断连） |

将提取的数据填充至 `docs/dev/` 下的相关对标报告中。
