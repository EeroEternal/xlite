# HTTP/1.1 上游连接池（`xlite-h1-pool`）设计稿 —— 备选方案

- **状态**：Draft / **备选（默认关闭、未压测验证）**
- **日期**：2026-10-01
- **定位**：给走 reqwest 的 **HTTP/1.1 上游**一个与 h2c 池（`crates/xlite-h2-pool`）同构的连接池，把上游控制粒度从「在途请求级」提升到**「物理连接级」**（含空闲连接）。**本方案不默认启用**；待压测与整体成熟后再验证，再决定是否转正。
- **关联**：
  - 参考实现：`crates/xlite-h2-pool`（h2c 池）
  - 接入点：`src/upstream.rs`
  - 边界决策：`docs/dev/loadtest-backpressure-strategy.md`（方案 A/B/C）
  - 前置近似：PR #75（**已合并** `989c5ad`，用应用层信号量做「在途请求上界」+ `/api/pool` 的 `http1_gates[]` 可见性；规则同步见 #77）——本设计是其「更细粒度」的演进

> ⚠️ **规则边界（同 PR #75）**：本方案仍是「网关侧连接治理」，与 `loadtest-backpressure-strategy.md` §2「护栏二：杜绝应用层阻塞排队」及 `AGENTS.md` 规则 9 存在张力。它被定位为**默认关闭的备选**，正是因为该议题尚未收敛；转正前必须重新走一次设计/评审，而不是默默启用。

---

## 1. 背景与问题

xlite 的上游分流只有两条路（`src/upstream.rs` 的 `post_stream()`）：

```rust
if self.h2c && endpoint.starts_with("http://") {
    self.post_h2c(...)      // 走 crates/xlite-h2-pool（HTTP/2 连接池）
} else {
    self.post_reqwest(...)  // 走 reqwest
}
```

- **h2c 路径**有真正的连接池：每 host 连接数受 `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` 约束（`crates/xlite-h2-pool` 的 `Config::max_conns_per_host`），连接满时在池内等待，且 `/api/pool` 可观测。
- **reqwest 路径**（明文 HTTP/1.1、以及所有 `https://`）**没有任何连接池上界**：reqwest 的 `pool_max_idle_per_host(64)`（`src/upstream.rs` 的 reqwest `Client::builder()`）只限制**空闲保留**连接数，不限制并发连接数。HTTP/1.1 一请求一连接 ⇒ N 并发就开 N 条连接。

PR #75（**已合并** `989c5ad`，规则同步见 #77）用应用层信号量把「在途请求数」限到 `N`，从而近似限制连接数，并把 gate 的 `permits/in_flight/waiting/waiting_max` 接进了 `/api/pool` 的 `http1_gates[]`（`Upstream::http1_gate_stats()`；`/api/pool` 见 `src/admin.rs`）。但它仍有已知的粒度缺口：

| 缺口 | 现象 | 影响 |
| --- | --- | --- |
| **idle 连接不计入** | permit 释放后 keep-alive 连接仍在，占用 fd | 严格上界是 `N + 64`，不是 `N` |
| **连接会重建** | 客户端取消/超时后 reqwest 关闭并新建连接 | 累计连接数 > 峰值在途（实测 10 个断开客户端留下 11 个源端口） |
| **只到「在途」级，不是连接级** | `http1_gates[]` 报的是在途/排队，**不含 idle 连接** | 「打开的物理连接数」仍不可见 |

## 2. 目标 / 非目标

**目标**
1. 对明文 HTTP/1.1 上游提供**连接级**上界：同一 host 打开的物理连接数 ≤ `XLITE_UPSTREAM_MAX_CONNS_PER_HOST`（含 idle）。
2. 与 h2c 池**同构**的接口与语义（`acquire` → `Lease`，连接满则等待），使两条路径的心智模型一致。
3. 连接级可观测：`/api/pool` 能看到 HTTP/1.1 上游的连接数、在途、拨号/回收计数。

**非目标（本设计不覆盖）**
- 不改 `https://` 上游（TLS/ALPN/重定向/压缩等仍由 reqwest 处理；见 §6）。
- 不做智能路由 / 协议转换 / 内容改写（`AGENTS.md` 规则 9）。
- 不默认启用；不替换 PR #75 的近似方案（两者是并存的可选项）。

## 3. 现状（可验证的证据）

| 断言 | 证据命令 | 结果 |
| --- | --- | --- |
| 分流只认 `H2C=1 + http://` | `rg -n "h2c && endpoint" src/upstream.rs` | `post_stream()` 内唯一命中 |
| reqwest 只限 idle 保留 | `rg -n "pool_max_idle_per_host" src/upstream.rs` | 唯一命中（值为 `64`） |
| 连接数上界只在 h2 池 | `rg -n "max_conns_per_host" crates/xlite-h2-pool/src/pool.rs` | 全部命中在 h2 池内部 |
| HTTP/1.1 gate 已在 main（#75） | `rg -n "fn http1_gate_stats" src/upstream.rs` | `Upstream::http1_gate_stats()`；`/api/pool` 见 `src/admin.rs` 的 `http1_gates` |
| reqwest 未开 h2 prior knowledge | `rg -n "\.http2_prior_knowledge\(" src` | 无调用（`http2_prior_knowledge` 只在一行 doc 注释里出现）⇒ 明文 `http://` **确定**走 HTTP/1.1 |
| hyper 已含 http1 | `rg -n "^hyper" Cargo.toml` | `features = ["http1", "http2"]` |

## 4. 设计

### 4.1 新增 crate：`crates/xlite-h1-pool`

与 `xlite-h2-pool` 并列，接口刻意对齐：

```rust
pub struct Config {
    pub max_conns_per_host: usize,   // 连接级硬上界
    pub idle_timeout: Duration,      // 空闲连接回收阈值（reaper）
    pub min_idle_conns: usize,       // reaper 保底空闲连接（默认 1）
    pub dial: Dial,                  // 与 h2 池同一形状：TcpStream → BoxIo
}

impl Pool {
    pub async fn acquire(&self, host: &str) -> Result<Lease, PoolError>;
    pub fn stats(&self, host: &str) -> PoolStats;
    pub fn all_stats(&self) -> Vec<(String, PoolStats)>;
}

pub struct Lease { /* 一条连接的一次独占使用权 */ }
impl Lease {
    pub async fn send_request(&self, req: Request<Full<Bytes>>)
        -> Result<Response<ResponseBody>, PoolError>;
}
```

> 上述签名已与实现同步。`idle_timeout` 由后台 reaper 使用（默认 90s，同 h2 池 / reqwest）；`min_idle_conns`（默认 1）让 reaper 不回收低于该数量的空闲连接；拨号带 `DIAL_RETRIES`（2 次，仅瞬时错误，10ms 退避）。`ResponseBody` 包装 hyper 的 `Incoming`，poll 到 `None` 才把连接标为可复用（未读完就释放则退役该连接）。

### 4.2 连接生命周期

HTTP/1.1 **无多路复用**，一条连接同一时刻只能服务一个请求 ⇒ 连接的 `in_flight ∈ {0, 1}`，与 h2 的「按流计数」不同。

```
新建: TCP connect → hyper::client::conn::http1::handshake → ConnEntry{ in_flight: 0, reusable: true }
复用: acquire 命中 in_flight==0 且 reusable 的连接 → 置 1 → 发请求 → body 读完/释放 → 置 0 → 唤醒 waiter
错误: 连接返回 broken / read EOF → 标记 reusable=false，从表中移除（换新连接）
回收: 后台 reaper 周期扫描，关闭「空闲 ≥ idle_timeout」的连接（保留至多 `min_idle_conns`，见 §6 未决项）
```

### 4.3 `acquire` 语义（与 h2 池一致）

1. 遍历该 host 连接表，返回第一条 `in_flight == 0 && reusable` 的连接（对应 h2 的 first-fit）。
2. 没有空闲且连接数 `< max_conns_per_host` ⇒ **拨号一条新连接**（每 host 拨号可并发，但总连接数受上界约束）。
3. 连接数已达上界 ⇒ **在池内等待**（订阅释放通知），直到有连接空闲或关闭。
4. 与 h2 池相同的已知硬边界：**若一条连接都没有且拨号持续失败，等待可能永不返回**（`rfc-adaptive-h2-pool-architecture.md` §6.4）。本设计必须显式处理（有界重试 / 失败即返回 `PoolError`），**不得**原样复刻这个洞。

### 4.4 观测

`PoolStats` 字段对齐 h2 池，追加 HTTP/1.1 特有项：

```rust
pub struct PoolStats {
    pub connections: usize,       // 当前打开的物理连接数（含 idle）
    pub in_flight: usize,         // 忙连接数（0/1 per conn，求和）
    pub dials: usize,
    pub reclaimed: usize,         // reaper 关闭的空闲连接数（reaper 未做，恒 0）
}
```

> `connections` 含「已被标记不可复用、但尚未被下一次 `acquire` 的 `retain` 清掉」的连接，故瞬时值可能略高于有效连接数 —— 它们是真实仍打开的连接，不是伪造数字。

`GET /api/pool` 现状：`hosts[]` 只覆盖 h2c 池；`http1_gates[]`（#75）覆盖 HTTP/1.1 的**在途/排队**，但**不含**物理连接数与 idle。启用本池后，新增顶层 `h1_pool[]`（每 host：`connections`（含 idle）/`in_flight`/`dials`/`reclaimed`）；此时 `http1_gates[]` 保持为空（gate 对本池接管的 host 不再 `acquire`）—— 两个机制各报各的、不共用同一字段，避免语义漂移。

### 4.5 接入与开关

- 新 env：**`XLITE_UPSTREAM_H1_POOL=1`**（默认 **关**）。
- 生效条件：`XLITE_UPSTREAM_H1_POOL=1` **且** `endpoint` 是明文 `http://`（`https://` 仍走 reqwest）**且** `H2C` 关闭（若 `H2C=1`，明文 `http://` 仍归 h2c 池，避免两条池争同一 host）。
- 与 #75 的关系：本池启用时，`AdmissionGate` 对该 host **不生效**（由连接级上界取代；`/api/pool` 的 `h1_pool[]` 承载本池 stats，`http1_gates[]` 为空，见 §4.4）；**未启用时行为与 main 完全一致**（`AdmissionGate` + `http1_gates[]` 照旧）。

## 5. 与 h2-pool 的关系

两个池的**形状**相同（per-host 连接表 + acquire/lease + reaper + stats），差异只在：

| 维度 | h2 池 | h1 池 |
| --- | --- | --- |
| 每连接并发 | 多流（`SETTINGS_MAX_CONCURRENT_STREAMS`） | 1（串行） |
| 上界 | `min(CAP, peer) × max_conns` | `max_conns`（每连接恒 1） |
| 握手 | h2c prior knowledge / ALPN | HTTP/1.1 request line |
| body 背压 | h2 流控 | TCP 背压（读多少推多少） |

**统一可能性**：抽出 `Transport` trait（`dial` 返回的 IO + `handshake` → `SendRequest`），让 `Pool` 泛化到 h1/h2 两种「每连接并发数」模型。这是本设计的**长期目标**，但会让改动面从「新增一个 crate」扩大到「重构 h2 池」，**本期不做**，单列 §6 未决项。

## 6. 边界与风险（诚实清单）

1. **TLS/ALPN 不覆盖**：只做明文 `http://`。`https://` 上游继续走 reqwest ⇒ 「HTTPS + 只支持 HTTP/1.1」的上游仍无连接上界（与已合并的 #75 相同的边界）。
2. **复刻自研池风险**：h2 池已记录过拨号失败、等待不返回、幽灵流等代价（其 RFC §6）。h1 池必须逐条处理，否则只是把 h2 池的坑再挖一遍。
3. **维护两套池**：新增一个 crate 的长期维护成本；转正前应论证它带来的粒度收益值这个成本。
4. **body 流式透传**：必须保持「字节透传、不做协议转换」（`wire-boundary.md`）；背压靠 TCP，不得在网关缓冲整个响应。
5. **未决项**：`min_idle_conns` 预热是否需要；是否要把两池统一到 `Transport` trait；idle 回收与 DNS 变更/连接漂移的关系。
6. **拨号失败无重试**：h2 池有 `DIAL_RETRIES` + 退避，h1 池 `dial` 失败直接返回 `PoolError`，由 `src/proxy.rs` 的跨 channel failover 兜底（单 channel 时即 502）。语义不一致但可接受；若需要重试，作为独立改动（不在本稿 checklist）。

## 7. 验证计划（压测与整体成熟后执行）

1. **连接级上界**：`max_conns=4`、50 并发 ⇒ 上游 `ss` 看到的**打开连接数**（含 idle）≤ 4（对照 PR #75 的「在途 ≤ 4」）。
2. **idle 回收**：低载后 idle 连接在 `idle_timeout` 内被关闭，`reclaimed` 计数增长，fd 回落。
3. **取消/超时/错误路径**：连接不泄漏（类似 PR #75 的 T5/T7）。
4. **性能对照**：与 reqwest 路径在低载/高载下的 TTFT、吞吐、CPU 对比（不得 regression）。
5. **可观测**：`/api/pool` 能同时列出 h2c 与 h1 上游。
6. **压测台架**：复用 `docs/manual/v88-rig.md` 的 24 口 mock 农场，按 `loadtest-backpressure-strategy.md` 的方案 C 口径（不设应用层 429/排队）对比。

## 8. 回滚

本方案默认关闭：不设 `XLITE_UPSTREAM_H1_POOL` 时，代码路径与 main 逐字节一致。删除 crate + 接入点即可回滚；不影响 h2c 池与 PR #75 的近似方案。

## 9. 收敛计划（压测后，用户已决定）

**决定**：压测验证本池后，**移除 reqwest 的明文 HTTP/1.1 路径与 `AdmissionGate`（#75）**，让明文 `http://` 只走本池。理由：两条路径并存（近似 gate vs 连接级池）维护成本高、观测语义易混（`http1_gates[]` vs `h1_pool[]`），集中在一种。

届时（**独立 PR**，不是本 PR）：
- `src/upstream.rs`：删 `AdmissionGate` / `http1_gates` 采集 / `BodyKind::Reqwest` 的 HTTP/1.1 用法；`post_reqwest` 只保留 `https`；本池对明文 `http://` 转默认生效（去掉 env 开关或默认 `true`）。
- `src/admin.rs`：`/api/pool` 去掉 `http1_gates[]`，`h1_pool[]` 成为 HTTP/1.1 的唯一来源。
- 规则同步：`AGENTS.md` 规则 9 / `loadtest-backpressure-strategy.md` §2 / `extension-points` §6 的例外条款改为「明文 HTTP/1.1 走 `crates/xlite-h1-pool`」（不再是「#75 的 gate + 备选实现」）。
- `advanced/pool_sizing_probe.sh`：读取 `h1_pool[]`。
- `docs/changelog.md`：这是一次**默认行为变更**（明文上游从「reqwest + 在途上界」改为「自研连接池」），需显著标注。

**前置条件**：§7 的压测验证通过（尤其「与 reqwest 路径的性能对照不得 regression」），且 idle reaper 等留白已清。

## 10. 压测记录（2026-10-01，实验室 101/102）

**台架**：k6（101）→ xlite（101）→ 24 口 Go mock（102，明文 HTTP/1.1，`8081-8104`）。constant-VU / 30s，`http_req_waiting` ≈ TTFT。网关 `nofile=1048576`（首轮 1000 VU 的 98% fail 是漏设 fd 所致，已排除）。对照：`XLITE_UPSTREAM_H1_POOL=1` vs 默认 gate+reqwest，均 `MAX_CONNS_PER_HOST=32`。

| 负载 | 指标 | h1pool | gate |
| --- | --- | --- | --- |
| 200 VU | qps / ttfb 中位 / fail | 33.4 / 510ms / 0% | 33.4 / 497ms / 0% |
| 1000 VU | qps / ttfb 中位 / fail | 105.2 / 717ms / 0% | 115.0 / 837ms / 0% |
| 2000 VU（修复前） | ttfb p90 / p95 / max | **18.5 / 24.3~28.8 / 46.8s** | 12.2 / 12.4 / 17.8s |
| 2000 VU（修复后） | ttfb p90 / p95 / max | **12.2 / 12.4 / 17.8s** | 12.2 / 12.4 / 17.9s |

**发现并修复**：初版用 `Notify::notify_waiters()` 唤醒等待者（惊群：每口 `waiting_max` 37–91，释放一次唤醒全部、只有一个赢、其余重新 park），2000 VU 下 ttfb p90 比 gate 高 ~51%、max 高 ~2.6×（3 次重复稳定）。改为每 host `tokio::sync::Semaphore`（permits = `max_conns_per_host`，公平 FIFO，permit 随 `Lease` 释放）后，**p90/p95/max 与 gate 对齐、qps 相同、0 fail**。

**结论**：修复后 h1 池与 reqwest 路径**性能等价**（200/1000/2000 三档）；且 h1 池的连接上界**精确可见**（`/api/pool` 每口 `connections=32`、`dials=32`，无多余拨号），这是 gate 的 `http1_gates[]` 给不出的连接级信息。§7 的「性能对照不得 regression」**通过**。

**reaper 验证（修复 churn）**：长时 2000 VU × 300s 后 `dials` 稳定在 **32~33/口**（对照无 reaper 时为 46–71）、`connections` 24×32、0 fail；压测结束后过一轮 `idle_timeout`（90s，扫描 45s），**`connections` 全部回落到 0、`reclaimed` = 32/口（共 768）**。即 reaper 在高载惰性、低载回收。

**未做**：跨机（非 loopback）稳定性未测；`min_idle_conns` 仅作 reaper 保底，未做主动预热（突建）。

---

## 附：实现进度（本 PR 跟踪）

> 本稿已在 #75（`989c5ad`）、#77（`e42166a`）、#78（`fba7b4e`）合入后 **rebase 到最新 main**；现状引用一律用**符号**（函数 / 字段名）而非行号，避免每次 main 前进都漂移；`http1_gates[]` 是 #75 已交付的观测，§4.4/§4.5 说明本池转正后如何接续它。

- [x] `crates/xlite-h1-pool`：Config / Pool / Lease / PoolError / PoolStats
- [x] 连接表 + `acquire`（复用 / 拨号 / 排队 + `Notify` 唤醒）+ 连接级 `in_flight`
- [x] 集成测试（真实 TCP 回环，**5 项**）：单请求、keep-alive 复用、`cap=1` 连接上界与排队唤醒、跨 host 独立、**半读连接退役**（响应体未读完就释放 ⇒ 连接不复用、下次重新拨号）
- [x] xlite 级接入回归测试（`tests/upstream_h1_pool.rs`）：`h1_pool=1` 走池且 `h1_pool_stats` 非空 / `http1_gates` 空；默认走 gate 且反之
- [x] 空闲连接回收（reaper）：`idle_timeout`（默认 90s，同 h2 池 / reqwest）+ 后台周期扫描（间隔 `idle_timeout/2`）；低载回收后 `connections` 回落、`reclaimed` 计数
- [x] `min_idle_conns`（reaper 保底）+ `DIAL_RETRIES`（瞬时拨号失败有界重试，退避 10ms）
- [x] `src/upstream.rs` 接入（`XLITE_UPSTREAM_H1_POOL=1`，默认关；§4.5）
- [x] `/api/pool` 新增 `h1_pool[]`（不接 gate 字段，避免两个机制混用；§4.4）
- [ ] 探针（`advanced/pool_sizing_probe.sh`）读取 `h1_pool[]`：启用 h1 池后 API 模式观测留白（`hosts[]`/`http1_gates[]` 皆空 ⇒ 回落 `ss`）
- [ ] 压测验证（§7）

**当前状态**：crate + `src/upstream.rs` 接入 + `/api/pool` 的 `h1_pool[]` + **半读连接退役** + **接入回归测试**已实现；**默认关**（不设 `XLITE_UPSTREAM_H1_POOL` 时行为与 main 逐字节一致）。剩余：idle reaper（`reclaimed` 恒 `0`）、压测验证。
