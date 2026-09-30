# RFC: HTTP/2 自适应最小负载连接池架构设计方案（H2 Least-Loaded Adaptive Pool）

- **作者**：Xlite 核心网关团队
- **日期**：2026-09-29
- **状态**：Draft（提交 Grok / GPT-5/6 架构审计）
- **实测状态**：⚠️ [§6](#6-实测反证与修订后的结论2026-09-30-实验室复测) 记录了 2026-09-30 实验室复测：§1 的问题陈述与 §3 的四项处方**均未被实测支持**，请先读 §6 再读正文。
- **目标场景**：超大规模 LLM 生产级网关（45,000~60,000 VU，3,000+ QPS 极限突发）下的 **首字延迟（TTFT）与尾部延迟（p99）极限制约突破**。

---

> ⚠️ **实测状态（2026-09-30 更新）**：本方案 §1 的问题陈述与 §3 的四项处方**均未被实测支持**，请先读 **§6 实测反证与修订后的结论**。速览：把「每连接流数」拉满到 **184**（4~9 倍），首字只变 **+2.8~5.5ms**（方案预估 +100~300ms 的约 1/30）；实测第一个断的点是**拨号**（`PoolError::Dial`）而不是连接内排队。本方案作为「待审计的议题清单」仍有价值，但**不要据此调整现场参数**。

## 1. 现状剖析与问题陈述（Problem Statement）

### 1.1 现状实现（`xlite-h2-pool v0.5.1`）

目前 Xlite 的自研 HTTP/2 上游连接池（`crates/xlite-h2-pool/src/pool.rs`）在 `acquire` 阶段采用的是 **“First-Fit 线性扫描 + 静态硬上限（Hard Stream Cap）”** 策略：

```rust
// 现状代码：pool.rs:330-336
fn reserve_existing(state: &HostState, stream_budget: usize) -> Option<Arc<ConnEntry>> {
    for entry in state.conns.load().iter() {
        if try_reserve_stream(&entry.cap, &entry.in_flight, stream_budget) {
            return Some(Arc::clone(entry));
        }
    }
    None
}
```

### 1.2 现状的物理瓶颈（为什么在高并发下 TTFT 向上翘起？）

1. **“头部连接拥塞”与负载严重倾斜（Head-of-Line Contention）**：
   - 现有的 `for entry in state.conns.load().iter()` 是从索引 `0` 开始线性遍历。
   - 只要第 0 号连接的 `in_flight < stream_budget`（如 50），**所有新到达的请求会一股脑全部硬塞进第 0 号连接**。
   - **后果**：第 0 号连接扛了 49 个流（其 TCP 发送窗口早已吃紧、Socket 写入锁竞争激烈），而后面的第 1~15 号连接可能只有 2~3 个流甚至闲置。
   - 新建请求的第一个 `HEADERS` 帧必须在拥挤的第 0 号连接上排队等待前 49 个流的 `WINDOW_UPDATE`，导致首字延迟（TTFT）直接被拉长 100ms~300ms！
2. **“全满才拨号”的被动冷启动延迟（Reactive Dialing Stall）**：
   - 目前只有当现有连接的 `in_flight` **100% 达到 Cap**（全部塞满）时，第 51 个请求才会触发 `state.dialing` 发起新建 TCP 拨号；
   - 此时第 51 个请求必须同步等待 `dial().await` + `client_builder().handshake().await`（花费 5~15ms 握手时间），直接产生断层式的尾部毛刺。
3. **全局原子行颠簸（Atomic Cache-Line Bouncing）**：
   - 随着 8 个 Tokio Worker 线程并发递增同一条连接的 `entry.in_flight.fetch_add`，多个 CPU 核心的 L1/L2 缓存行被反复无效化（Cache Invalidation），增加了微秒级锁总线延迟。

---

## 2. 设计目标（Design Goals）

1. **消除头部倾斜，实现均衡分摊（Smooth Flow Distribution）**：新请求永远优先路由至当前**在途流最少、TCP 窗口最充裕**的连接，将流完全平摊在所有活跃 TCP 连接上。
2. **连接主动自适应预热（Proactive Pre-dialing）**：引入平滑软阈值（Soft Cap）。当集群平均连接负载达到软水位时，后台异步提前拨号建立新连接，保证任何时刻新请求到达时都有“零等待”的可用通道。
3. **长短流物理隔离（Long/Short Flow Separation）**：长耗时流（`out ≥ 3000`）与短流（`out ≤ 250`）分池，避免秒级/分钟级的长流锁死整条高频 TCP 管道。
4. **保持 Lock-Free 极致特性**：在实现动态调度的同时，绝不引入阻塞式互斥锁（Mutex），保持 O(1) 或无锁轻量 CAS。

---

## 3. 核心架构设计（Architectural Specification）

```
                  Client Request
                        │
                        ▼
            ┌────────────────────────┐
            │   Pool::acquire(host)  │
            └───────────┬────────────┘
                        │
       ┌────────────────┴────────────────┐
       ▼                                 ▼
[Short Flow / Default]           [Long Flow (Heuristic)]
       │                                 │
       ▼                                 ▼
┌──────────────────────┐         ┌──────────────────────┐
│  Primary HostState   │         │  Isolated Long-Pool  │
│ (SoftCap=25,Hard=50) │         │ (SoftCap=10,Hard=20) │
└──────────┬───────────┘         └──────────┬───────────┘
           │                                │
           ▼                                ▼
┌────────────────────────────────────────────────────────┐
│  Round-Robin / Least-Loaded Lock-Free Slot Scan        │
│  - Load snapshot of Arc<Vec<Arc<ConnEntry>>>          │
│  - Select connection with MIN(in_flight)               │
│  - Try atomic CAS reservation                          │
└──────────────────────────┬─────────────────────────────┘
                           │
             ┌─────────────┴─────────────┐
             ▼                           ▼
      [Success: < SoftCap]        [Warning: >= SoftCap]
             │                           │
      Return Lease Immediately    Return Lease & Trigger
                                  Background Async Pre-dial!
```

### 3.1 核心算法：Power-of-Two-Choices (P2C) 最小负载无锁调度

如果每次都在整个 `conns` 列表中全局查找 `min(in_flight)`，需要扫描整个数组，且容易产生羊群效应（多个线程同时选中同一个最小值）。

因此，本方案采用工业级分布式调度中最经典的 **Power-of-Two-Choices (P2C) + 环形轮询（Round-Robin Offset）**：

#### 算法规范：
1. 令连接列表长度为 $N$。每个 Worker 线程维护一个无锁自增的 `cursor`。
2. 随机选取或通过 `cursor` 步进选取两条连接 $C_a$ 与 $C_b$：
   $$idx_a = cursor \pmod N, \quad idx_b = (cursor + \Delta) \pmod N$$
3. 比较其即时在途数：
   $$\text{target} = \begin{cases} C_a, & \text{if } C_a.\text{in\_flight} \le C_b.\text{in\_flight} \\ C_b, & \text{otherwise} \end{cases}$$
4. 对 $\text{target}$ 尝试原子预留：`try_reserve_stream(&target.cap, &target.in_flight, hard_cap)`。
5. 若成功，直接返回；若由于竞态失败，顺延降级全量无锁扫描一次。

**数学证明效益**：P2C 算法在常数时间 $O(1)$ 内即可达到接近全局最优的极度均匀分布，彻底消灭第 0 号连接承载 50 个流而第 15 号连接承载 0 个流的极端畸变！

---

### 3.2 动态自适应伸缩：Soft-Cap 异步预拨号机制

将原来的单一 `stream_budget_per_conn` 升级为**双水位线模型**：

```rust
pub struct PoolConfig {
    /// 软水位（Soft Cap，如 20）：当选中的连接在途流数 >= soft_cap 时，
    /// 说明连接池开始变挤，立即触发后台异步预拨号（Pre-dialing）。
    pub soft_stream_cap: usize,
    /// 硬水位（Hard Cap，如 50）：单连接最大并发流绝对上限。
    pub hard_stream_cap: usize,
    /// 最大连接数（如 32 ~ 64）。
    pub max_conns_per_host: usize,
    /// 最小常驻连接数（如 4 ~ 8）：服务启动后主动预热，永远不关闭。
    pub min_idle_conns: usize,
}
```

#### 预拨号控制流（Zero-Stall Dialing）：
```rust
if current_conns < config.max_conns_per_host {
    let avg_in_flight = total_in_flight / current_conns;
    if avg_in_flight >= config.soft_stream_cap {
        // 尝试获取拨号槽，在后台 task 中拨号，绝不阻塞当前请求！
        if state.dialing.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let state_clone = Arc::clone(&state);
            tokio::spawn(async move {
                let _ = dial_and_insert_background(&state_clone).await;
                state_clone.dialing.store(0, Ordering::Release);
            });
        }
    }
}
```
**效益**：新连接在第 21 个流进入时已经在后台悄悄握手就绪；当第 50 个流到达时，一条全新的、处于最高拥塞窗口的 TCP 连接已经静候多时，**新连接的等待时间由 15ms 骤降至 0ms**！

---

### 3.3 HTTP/2 连接级别的滑动平均 RTT 与健康探测

Go 的 `http2.Transport` 表现平稳的一个隐含原因是底层 TCP 拥塞控制和 RTT 均衡。为了使 Rust 网关具备真正的生产级弹性，在 `ConnEntry` 中增加健康度感知：

```rust
pub(crate) struct ConnEntry {
    pub(crate) send: SendRequest<Bytes>,
    pub(crate) cap: Arc<AtomicUsize>,
    pub(crate) in_flight: AtomicUsize,
    /// 指数加权移动平均 RTT（Microseconds）
    pub(crate) ewma_rtt_us: AtomicU64,
    /// 最近一次发生 TCP Reset 或超时的时间戳
    pub(crate) last_penalty_ms: AtomicU64,
    pub(crate) idle_since_ms: AtomicU64,
    pub(crate) generation_tx: watch::Sender<u64>,
}
```

- **首字反馈与打分（RTT Feedback）**：
  当一个 Lease 收到第一帧响应头（TTFT）时，计算本次实际耗时，更新连接的 `ewma_rtt_us`：
  $$\text{EWMA} = \alpha \times \text{RTT}_{\text{new}} + (1 - \alpha) \times \text{EWMA}_{\text{old}}$$
- **调度打分权重**：在 P2C 决策时，比较的不再仅仅是裸的 `in_flight`，而是综合负载得分：
  $$\text{Score} = \text{in\_flight} \times \sqrt{\text{EWMA\_RTT}}$$
- **效益**：如果单台 Mock 实例或某条特定的 TCP 连接由于宿主机中断干扰发生丢包降速，其 Score 会迅速劣化，网关调度器会自动避开它，把流量平滑引向健康的连接。

---

## 4. 关键指标推演与对标预期（Expected Impact）

基于上述架构改造后，在 45,000 VU（极端高并发）压测场景下的理论预期变化：

| 评估维度 | 当前实现 (Xlite 0.5.1 + Cap=50) | 本设计实现 (H2 Least-Loaded Adaptive) | New API 双节点对标基准 |
| :--- | :---: | :---: | :---: |
| **连接内负载分布** | 极度不均（前几条 50 满载，后面极空） | **绝对平摊（每条连接 ~15-20 流）** | 相对平摊（Go transport 默认机制） |
| **45k VU TTFT p50** | 931.8 ms ~ 1,197.0 ms | **≤ 650.0 ms ~ 720.0 ms** 🏆 | 768.1 ms（*靠卡顿偷懒*） |
| **45k VU TTFT p99** | 4,120.3 ms | **≤ 1,800.0 ms** 🏆 | 2,163.6 ms |
| **交付吞吐 QPS** | 1,136.5 req/s | **1,200.0 ~ 1,400.0 req/s** 🏆 | 779.0 req/s |
| **流式抖动 (chunk p99)** | 29 ms ~ 32 ms | **≤ 30 ms** 🏆 | 411 ms 🚨 |
| **完成请求总量** | 59.2 万个 | **60 万 ~ 65 万个** 🏆 | 27.7 万个 |

---

## 5. 待外部架构专家（Grok / GPT-6）审计的 Checklist

> **审计前请先读 §6**：下面第 1~4 项中的三项，其前提已被 2026-09-30 的实验室实测否证。

请针对本方案的以下关键工程决策进行严格审查：

1. **P2C 算法在 h2 多路复用连接池中的合理性**：相较于全局最小扫描（$O(N)$ 并发争用）与粗粒度 First-Fit（头部拥塞），P2C 是否为兼顾 Lock-Free 与平摊的最优解？
2. **Soft-Cap 异步预拨号的竞态保护**：在超高并发瞬间涌入时，后台异步 `tokio::spawn` 预拨号是否可能引发连接数瞬间突破 `max_conns_per_host`？`dialing` 状态原子的 CAS 屏障是否严密？
3. **EWMA-RTT 引入对数据面的微小开销**：在几十万 QPS 的极端场景下，原子加权统计 RTT 是否会导致不必要的 Cache Miss？是否有更纯粹的无状态方案替代？
4. **与 Go `net/http2` 状态机的等价性与超越性**：本方案在消除 Go GC 停顿的基础上，是否真正还原甚至超越了 Go 原生连接池的传输层优势？

---

## 6. 实测反证与修订后的结论（2026-09-30 实验室复测）

> 本节由 issue [#49](https://github.com/EeroEternal/xlite/issues/49) 的实验记录回填。**结论先行：本方案 §1 的问题陈述与 §3 的四项处方均未被实测支持；唯一被实测证实的方向是「拨号路径的容错与可观测性」，其形态与 §3 的 P2C / 预拨号无关。**

### 6.1 §1 的核心机制被否证

§1 主张：「几百条流挤在一条连接上 ⇒ 新请求的 HEADERS 要排在前面流的 `WINDOW_UPDATE` 之后 ⇒ 首字 +100~300ms」。实测（`xlite` 0.5.0，无闸门；两台 128 核实验机，24 口 mock 农场，mock 侧 `ttft=500ms`）：

| 台架 | 每连接流数 | ttft p50 | 相对基线 |
| --- | --- | --- | --- |
| 24 口 / 16000 VU | 20~45 | 499~502ms | 基线 |
| **5 口 / 16000 VU** | **≈184** | **502.8ms** | **+2.8ms** |
| 5 口 / 8000 VU | ≈184 | 505.5ms | +5.5ms |

⇒ 把「每连接流数」这一轴拉满 4~9 倍，首字只动 **2.8~5.5ms**，比方案预估的 +100~300ms 小 **1.5~2 个数量级**。

**反方向也测了**（2026-09-30 第二轮，干净台架：网关容器与 k6 容器都显式 `--ulimit nofile=1048576:1048576`，并以「每档 `POOL_ERR=0` + 单流预检 `200`」作为可比性闸门；24 口农场、16000 VU、60s、`MAX_CONNS_PER_HOST=128`）：

| `XLITE_UPSTREAM_STREAM_CAP` | 到 mock 连接 | ttft p50 (ms) | ttft p90 | dur p50 | dur p90 | iter/s | 失败 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 不设（随对端 250） | 244 / 248 | **549.5 / 549.0** | 749 / 750 | 7.35 / 7.22s | 12.34 / 12.16s | 1563 / 1584 | 0% |
| 250 | 297 | **549.4** | 750 | 7.40s | 12.44s | 1555 | 0% |
| 30 | 1098 / 1091 | **782.1 / 772.1** | 1055 / 1035 | 7.22 / 7.03s | 10.75 / 10.22s | 1638 / 1689 | 0% |
| 8 | 3548 / 3655 | **1127.4 / 1178.2** | 2927 / 3100 | 7.73 / 7.94s | 9.54 / 9.81s | 1611 / 1561 | 0% |

（每档两轮并排；重复性 **±1~1.3%**。⚠️ 更早一轮的「CAP 阶梯」全部无效 —— 那些档位打到的网关容器 **没有 `--ulimit`**（软上限 1024，实测 8782 万次 `PoolError::Dial` 失败），比的是 fd 状态而不是 CAP，见 issue #49 的更正。）

⇒ **把流摊到更多连接不是「没有收益」，而是「中位首字显著变差」**：CAP=30 ⇒ **+223~232ms**、CAP=8 ⇒ **+578~629ms**（同时**尾部变好**：dur p90 12.3s → 9.5s，吞吐不变）。这与 §1 的预测**方向相反**：该方案把「摊平负载」当作解法，实测这个方向恰恰伤首字。逐层机理尚未归因（候选：每 host 的拨号槽位串行、mock 侧每连接开销、网关侧 epoll/softirq 开销），但**量大且可重复**，足以否决 §3 里任何「降低 CAP / 摊平负载」的处方。

另有一条协议前提也不成立：**RFC 9113 §5.2 的流控只作用于 DATA 帧，HEADERS 不受流控窗口约束** ⇒ 「HEADERS 排队等 `WINDOW_UPDATE`」在协议层无法发生。若确实观测到首帧延迟，成因应在**写路径竞争 / 调度**，与流控窗口无关。

### 6.2 §3 四项处方的逐条判定

| 处方 | 判定 | 依据 |
| --- | --- | --- |
| **P2C 最小负载调度** | ❌ 无收益 | 现状 first-fit + 懒拨号在稳态已把每条连接填到 `min(对端 SETTINGS, CAP)` ⇒ 负载分布本来就是平的，重排不改变任何可观测量。（§6.1 的阶梯测的是**连接数**，不改变本行判定：P2C 只改分布、不改连接数） |
| **Soft-Cap 异步预拨号** | ❌ 与硬顶冲突 | 预拨号会**提前多开连接**；`MAX_CONNS × CAP` 是每口硬顶，撞顶即退化成应用层排队（`docs/dev/loadtest-backpressure-strategy.md` §2 护栏二明确禁止的形态）。且实测的失效点恰是 `PoolError::Dial`（见 §6.3）⇒ 多拨号只会更快撞墙 |
| **长 / 短流分池** | ❌ 无对象 | 对标台架一组一轮、同档 `out_tokens` 一致 ⇒ 没有长短混流可分；若生产中确有混流，正确入口是写调度 / 窗口公平性，而不是连接分池 |
| **EWMA-RTT 打分** | ❌ 无稳态收益 | 同一 mock 口的所有连接等价 ⇒ 打分只增加反馈震荡与原子开销 |

### 6.3 被实测证实的唯一方向：**拨号路径**（形态不是 P2C）

用三处计数插桩（`PoolError` 只有 `Dial` / `Handshake` / `Closed` 三个变体、`dial_and_insert` 只有两个 `?` ⇒ 三处即全集）在 fd 受限的容器里复现：

| 网关容器 `nofile` | `Dial` 失败次数 | `status_502` | 到 mock 已建连接 | qps |
| --- | --- | --- | --- | --- |
| 1024（漏 `--ulimit`） | **22,783,000** | 947,182（**99.49%**） | 39 | — |
| 1,048,576 | **0** | 0.00% | 1,551 | **1,484** |

⇒ **`Dial` 是资源压力下第一个断的点**（22,783,000 ÷ 947,182 = **24.0** = 每请求 24 个 channel 全部拨号失败；补齐 fd 后同一档 `fail=0.00%`、qps 从约 155 涨到 1,484）。这正是本 RFC 意外带来的价值：它把注意力引向了新连接 / 拨号路径。**但修法不是 P2C 重构，而是：**

1. **让失败可归因** —— 已修（PR [#50](https://github.com/EeroEternal/xlite/pull/50)：502 的错误链原先被 `anyhow::Error::to_string()` 截断 ⇒「fd 耗尽」「h2 握手失败」「连接池关闭」在客户端与日志里**长得一模一样**，排障只能猜；本轮评测因此被连续误导两次）。
2. **资源对账** —— 压测 / 部署前核对**链路上每一环**的 `nofile`（k6 / HAProxy / 网关 / 农场），见 `deploy/v88/advanced/CONFIG.md` §3.1 与 PR [#43](https://github.com/EeroEternal/xlite/pull/43)。
3. **`acquire` 的失败降级** —— 未实现，边界见 §6.4。

### 6.4 关于「拨号失败 ⇒ 不要直接 502」的边界（**未决策，勿据此改现场**）

`crates/xlite-h2-pool/src/pool.rs::acquire` 第 2 步的 `dial_and_insert(…).await?` 是唯一让拨号失败**直接冒泡**成 502 的地方 —— 第 3 步「订阅 generation 并等待既有连接释放槽位」本来就是现成的降级路径，只是拨号失败时被 `?` 绕过了。**但不能简单改成回落到第 3 步**：

- 池里**一条连接都没有**时，没有任何一方会 `release` ⇒ 单看这一点，等待像是会**永不返回**；但代码事实相反：`DialSlot::drop`（`pool.rs`）在**拨号失败**时也会 `bump(generation)` ⇒ 唤醒**立刻**到来、循环**立刻**重试 ⇒ 结果是**热自旋**而不是挂死 —— `EMFILE` / `ECONNREFUSED` 都是立即返回、没有真正的挂起点，于是在 `acquire` 第 1~2 步之间空转（tokio 的 coop budget 会周期性让它让出，但 worker 会被烧满核）。**热自旋比 502 糟得多** ⇒ 任何「失败后重试 / 等待」的形态**必须自带退避**。
- 容量**真的没了**时（如 fd 耗尽），降级只是把「快失败」换成「慢失败」：该档只剩 39 条连接在服役，16000 个 VU 排在 `39 × 30` 个槽位后面 ⇒ 单请求首字会涨一个数量级以上 ⇒ 对**压测**比 502 更差，对**真实用户**更优。

⇒ 该项应在「有界重试 / 状态码语义（503 + `Retry-After`）/ 保持现状」之间**显式决策**后再动手；本方案 §3 并未覆盖这条。

### 6.5 修订后的优先级

1. **资源正确性**（`nofile` 对账 + 启动自检）—— 本轮真因，收益最大、风险最低。
2. **可观测性**（错误链、失败分类）—— 已落地一部分（#50）。
3. **拨号容错的有界形态** —— 需先决策边界（§6.4）。
4. ~~P2C / 预拨号 / 分池 / EWMA~~ —— 在给出**实测可观测的收益**之前不动。
5. **CAP 的正确用法**：**≥ 对端通告值**。§6.1 的干净阶梯显示「降 CAP 去摊平」会拿中位首字换尾部（CAP=30 ⇒ +223ms / CAP=8 ⇒ +578ms，尾部变好），所以它是一把**有方向的**双刃刀，不是一个免费的加速旋钮 —— 调之前先明确要优化 p50 还是 p90。

**关联**：issue [#49](https://github.com/EeroEternal/xlite/issues/49)（完整实验记录与三处自我更正）、PR [#50](https://github.com/EeroEternal/xlite/pull/50)（错误链）、PR [#43](https://github.com/EeroEternal/xlite/pull/43)（k6 `--ulimit` + 链路 fd 核对）。
