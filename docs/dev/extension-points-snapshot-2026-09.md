# 设计稿：快照预计算式能力扩展（P2）

- **状态**：V1 已按 §5 实现（后端 + YAML + Admin UI；见同一分支的提交）。**§7.2 的台架 A/B 尚未执行**——实现机无 docker daemon，`ab_configs.sh` 需要实验室网关机，未跑就不声称任何 CPU 结论。
- **范围**：控制面字段 + 快照构建 + 上游请求形状。**不改调度、不改流式转发、不改鉴权顺序**。
- **前置约束**：`AGENTS.md` 规则 9（数据面保持空管）、[`docs/ai/agents/wire-boundary.md`](../ai/agents/wire-boundary.md)、[`docs/architecture.md`](../architecture.md)（"No CostAware, CapabilityAware, session stickiness, or strategy enum"）。
- **不包含**：限流/配额、按 header 或模型的路由策略、usage 解析、body 改写、内容审核（见 §6）。

## 0. 结论先行

1. **不要实现运行时插件框架**（WASM / 动态库 / trait object 注册表）。xlite 是单二进制字节透传网关，运行时扩展点在热路径上必然引入间接调用、不透明分配与不可预测延迟，与定位直接冲突；扩展点的真实消费者是本项目自己，不是第三方生态。
2. **把"插件"实现为「catalog 字段 + 快照预计算」**：能力在 `Store::reload()` 构建路由快照时编译成通道上的预算字段，请求路径只读字段。这是唯一能在"加能力"和"不加成本"之间同时成立的形状。
3. **默认未配置时与今天逐字节等价**：新增字段的默认值让快照产出与当前 `authorization_header()` + 无语义头的结果完全一致，可用现有的指针/字节级断言范式锁住。
4. **命名建议：不要叫 plugin**。见 §10。

## 1. 问题

xlite 目前定位是透传网关，能配置的东西只有 provider（base_url + api_key）、model、channel（upstream_model + weight + priority）、api_key（模型白名单）。现实中的上游差异（鉴权头名字不同、需要静态附加头、chat 路径不是 `/v1/chat/completions`）现在**无法表达**，只能改代码。反过来，如果把"策略/插件"做成一个通用框架塞进 `src/proxy.rs`，就会同时违反规则 9 与 wire-boundary。

所以真正的问题是：**哪些能力可以加，加在哪里，每次请求多付多少？**

## 2. 现状（逐条取证）

以下每条都在本机实跑过，命令与输出见附录 A。

| # | 事实 | 证据 |
| --- | --- | --- |
| S1 | 上游鉴权头**硬编码为 `Bearer`**，头名固定 `authorization` | `src/upstream.rs:242-248` `authorization_header()`；`post_h2c` 在 `src/upstream.rs:110-112`、`post_reqwest` 在 `:159-161` 才决定是否写该头 |
| S2 | `endpoint` / `authority` / `path` / `authorization` **已经在快照构建期算好**，请求路径不解析 URL | `src/store.rs:173-176` 调 `chat_completions_url` / `h2c_parts` / `authorization_header`；`src/upstream.rs:80-81` 的注释亦如此声明 |
| S3 | 热路径顺序固定：取 Bearer → 哈希+查表鉴权 → 扫描 model → 取通道 → 循环尝试 → 改名或零拷贝透传 → 转发 | `src/proxy.rs:83-196`（`bearer_key` → `authenticate` → `ModelRef` → `channels_for` → `for channel in channels` → `outbound_body` → `post_stream`） |
| S4 | 同名时**零拷贝**（返回同一底层 buffer），改名才惰性建 DOM | `src/proxy.rs:32-54` `outbound_body`；`src/proxy.rs:319-330` 用 `std::ptr::eq` 断言 |
| S5 | 目前**不存在**任何插件/扩展机制 | `grep -rniE "trait [A-Za-z]*Plugin\|mod plugins\|src/plugins\|wasmtime\|wasmer\|extism\|libloading\|dlopen" src/ Cargo.toml` → 无匹配 |
| S6 | `Cargo.toml` 依赖里没有任何 WASM/动态加载运行时 | 附录 A V2 |
| S7 | 快照重载失败**只告警、保留旧快照**；启动期失败则硬失败 | `src/main.rs:147-149` `tracing::warn!("snapshot reload failed")`；`src/main.rs:131` 启动 `.context(...)?` |
| S8 | Redis 目录是 JSON 反序列化，`ProviderSecret` 加 `#[serde(default)]` 字段可向后兼容 | `src/catalog/redis.rs:16` `struct Dump`、`:69` `serde_json::from_str` |
| S9 | 热路径余量很薄：此前"网关侧转发合批"这个纯优化**被实测证伪**（网关 9.74 vs 9.70 核，−0.5%，低于噪声） | `docs/dev/loadtest-newapi-controlled-2026-09.md` §14.6 |

S9 是本次设计的成本预算依据：**任何"看起来应该更快"的改动都必须用台架证明没有回归，而不是假定有收益**。

## 3. 成本模型：三层，按"每次请求增量"划分

| 层 | 时机 | 每次请求成本 | 本设计 |
| --- | --- | --- | --- |
| **L0 快照预计算** | `Store::reload()`，与请求路径无关 | **0**（只读预算字段） | ✅ 本次实现 |
| **L1 请求期常量级** | 每次请求，O(1) | 1 次分支 / 1 次计数器；**有状态就有争用** | ⏸ 只定义边界，不实现 |
| **L2 流式 hook** | 每个 SSE chunk | 每 chunk 分配/解析 | ❌ 长期禁止 |

判定规则（写进评审 checklist）：

> 一个能力只有在**能表达为「catalog 字段 → 快照构建期算出常量 → 请求期只读」**时才允许进入 L0。
> 一旦需要在请求期做匹配、解析、查询、或跨请求共享可变状态，它就属于 L1，必须单独出设计稿 + 台架对照。
> 一旦需要看 body 或看 chunk，直接拒绝。

## 4. 候选能力清单

按"能不能只花快照的钱"排序。这张表是给评审人挑范围用的。

| 能力 | 编译到 | 每次请求增量 | 建议 |
| --- | --- | --- | --- |
| 上游鉴权方案（Bearer / 原样 / 自定义头名 / 无） | 快照 | 0（复用已预算的 header 值） | **V1** |
| 上游静态附加 header（`HTTP-Referer`、`X-Title`、租户头） | 快照 | O(#headers) 次 `header()` 追加，无分配；默认 0 次 | **V1** |
| 上游 chat 路径覆盖（非 `/v1/chat/completions`） | 快照 | 0 | **V1** |
| 模型改名（已有 `upstream_model`） | 快照 | 0（同名零拷贝） | 已有 |
| 请求头透传 / 剥离白名单 | 快照出白名单，请求期复制 | O(#allowlist) 次 header 拷贝 | V2，需单列 |
| API Key 级限流 / 配额 | 请求期共享计数 | 原子操作 + 共享 map，**有争用** | V2，独立设计稿 |
| 按 header / 模型选通道（策略路由） | — | — | ❌ 违反规则 9 |
| 协议转换（Anthropic/Gemini） | — | — | ❌ 违反 wire-boundary |
| usage / token 计数 | 流式 | 每 chunk 解析 | ❌ 违反 wire-boundary |
| 请求或响应 body 改写 | 流式 | 每 chunk 分配 | ❌ 违反 wire-boundary |
| 内容审核 | 流式 | 每 chunk | ❌ 违反 wire-boundary |

**给"策略都不太能用"的直接回答**：策略类能力不是"还没做"，而是**产品刻意不做**（规则 9 + architecture.md）。本设计不会把它们变可用；它解决的是"上游形状差异"，不是"多租户策略"。

## 5. 建议的第一版（V1）

三件事一起做，因为它们共享同一个机制与同一份字段。

### 5.1 数据模型

SQLite 新增迁移 `migrations/002_provider_upstream_shape.sql`（**已在真实 `001_init.sql` 之上实跑验证**，见附录 A V6）：

```sql
ALTER TABLE providers ADD COLUMN auth_scheme   TEXT NOT NULL DEFAULT 'bearer';
ALTER TABLE providers ADD COLUMN auth_header   TEXT NOT NULL DEFAULT '';
ALTER TABLE providers ADD COLUMN extra_headers TEXT NOT NULL DEFAULT '[]';
ALTER TABLE providers ADD COLUMN chat_path     TEXT NOT NULL DEFAULT '';
```

- `auth_scheme`：`bearer`（默认，等价今天）| `raw`（api_key 原样作为头值）| `none`（不写鉴权头）。
- `auth_header`：头名，空 = `authorization`。
- `extra_headers`：JSON 数组 `[["X-Title","xlite"], ...]`，快照期解析并校验。
- `chat_path`：空 = 沿用 `chat_completions_url()` 的推导；非空 = 覆盖。

存量行自动回落到默认值（实测：`legacy` 行读出 `bearer | [] | [] | []`），因此不需要 backfill。

### 5.2 快照构建（`src/store.rs`）

`to_channel()` 增加**校验 + 预渲染**：

```rust
// Channel 字段变化：把 String 换成已校验的 header 常量
pub auth: Option<(HeaderName, HeaderValue)>,          // 取代 authorization: String
pub extra_headers: Vec<(HeaderName, HeaderValue)>,    // 默认空
```

- `bearer` → `("authorization", "Bearer {key}")`，与今天 `authorization_header()` 的输出**逐字节相同**。
- `raw` → `(auth_header_or_default, key)`。
- `none` 或空 key → `None`。
- `extra_headers` 在**写入时**（admin create/update、YAML import）就校验并拒绝非法名；快照构建期解析失败按 S7 的语义**保留旧快照并告警**，绝不静默丢弃。

`HeaderName` / `HeaderValue` 在快照期解析，请求期不再做字符串校验——这正是"快照预计算"的意义。

### 5.3 热路径增量（逐行）

`src/proxy.rs` 只改参数传递，不加分支：

```rust
let upstream = state.upstream.post_stream(
    &channel.endpoint, &channel.authority, &channel.path,
    channel.auth.as_ref(), &channel.extra_headers, body,
).await
```

`src/upstream.rs` 的 `post_h2c` / `post_reqwest` 各加两处：

```rust
if let Some((name, value)) = auth { builder = builder.header(name, value); }
for (name, value) in extra { builder = builder.header(name, value); }
```

**成本陈述（不含糊）**：

- 默认配置（未设置任何新字段）时 `extra` 为空切片 → 内层循环 **0 次迭代**，`auth` 恒为 `Some(("authorization","Bearer …"))` → 写出的 header 集合与今天**完全相同**（同一次 `HeaderMap` 插入，没有额外的解析或分配）。
- 配置了 N 个附加头时，代价是向 `http::HeaderMap` 多插入 N 个**已解析**的条目——header 名与值在快照期就解析成 `HeaderName` / `HeaderValue`，请求期不做字符串解析，但每次插入本身就是一次常规的 map 写入（**这里原先写的「无分配」不准确，已更正**：HeaderMap 插入会分配，准确的收益是「不解析、不格式化」）。
- **不在请求期做**：URL 解析、头名合法性校验、JSON 解析、锁、await 之外的 I/O。

### 5.4 YAML 与 Redis 兼容

`ProviderYaml` 增加同名字段（都 `#[serde(default)]`）：

```yaml
providers:
  - name: openrouter
    base_url: https://openrouter.ai/api
    api_key: sk-...
    auth_scheme: bearer            # bearer | raw | none
    auth_header: ""                # 空 = authorization
    headers:                       # 静态附加头，保持书写顺序
      - name: HTTP-Referer
        value: https://example.com
      - name: X-Title
        value: xlite
    chat_path: ""                  # 空 = 自动推导 /v1/chat/completions
```

- **与本文初稿的偏差**：初稿写的是 `headers:` 映射（`X-Title: xlite`），实现改成了 `- name/value` 列表——列表与 Admin API、SQLite 的表示完全一致，且不会丢顺序或撞上重复头名。映射写法更短，若评审坚持要，可以在 `ProviderYaml` 上单独做一层映射转换，不影响其余设计。
- **导出必须对默认值 `skip_serializing_if`**，否则现有 catalog 的导出结果会凭空多出四个键，破坏"导出一份、存起来、再导入"的既有工作流（已有单测锁住）。
- Redis：`ProviderSecret` 加 `#[serde(default)]`。旧节点写、新节点读 → 走默认值；新节点写、旧节点读 → serde 默认忽略未知键（S8 已核实 Dump 用 `serde_json::from_str`）。**双向滚动升级安全**。

### 5.5 Admin UI（本 PR 已交付）

严格按 P1 的封闭集合纪律：**只扩展现有「供应商」对话框的字段，不新增页面/导航/过滤维度**。

- 已交付：`auth_scheme` 用 `Select`，`auth_header` / `chat_path` 用 `Input`，`extra_headers` 用「每行一个 `名称: 值`」的 textarea（复用 `settings.tsx` 已有的 textarea 样式，不引入新的交互组件）。对话框改为 `max-h-[85vh]` + 内部滚动，保证底部按钮始终可见。
- 仍然是 V1c（未做、需单独评估）：带增删行的键值对编辑器。textarea 的表达力够用，且不会让对话框随行数增长而抖动。

### 5.6 安全护栏（fail loud）

对 `extra_headers` 与 `auth_header` 做**拒绝名单**校验：在**写入时**（admin create/update、YAML import）命中即返回 400，不静默忽略；快照构建期若仍遇到非法值，按 S7 的语义**保留旧快照并告警**，不把坏配置放进路由。

`host` · `content-length` · `content-type` · `transfer-encoding` · `connection` · `te` · `upgrade` · `authorization`

理由：h2c 路径显式设置 `:authority`/`host`（`src/upstream.rs:108`）与 `content-type`（`:109`）；`authorization` 必须走专用字段以免两个来源冲突。允许覆盖它们等于把协议正确性交给运营输入。

## 6. 明确不做（本次）

- **不做**运行时插件注册表 / WASM / 动态库加载（§0）。
- **不做**限流、配额、并发闸门——这些都是 L1，且"准入排队"被规则 9 明确禁止用于刷 TTFT。**唯一例外（#75）**：明文 HTTP/1.1 上游的每 host 在途上界（复用 `XLITE_UPSTREAM_MAX_CONNS_PER_HOST`，经用户豁免护栏二，见规则 9）。
- **不做**任何策略路由、协议转换、usage 解析、body 改写、内容审核。
- **不做**"插件 API"抽象层。V1 就是把已有概念补全（通道的上游形状），不需要新词汇。

## 7. 验收

### 7.1 默认路径零回归（必须先绿）

- 新增单测：未配置任何新字段时，构造出的出站 header 集合与今天 `post_h2c` / `post_reqwest` 的产物完全一致（沿用 `src/proxy.rs:319-330` 的"同指针/同字节"断言范式）。
- 新增单测：`auth_scheme=none` → 不写鉴权头；`raw` → 头值等于 api_key 原值；非法头名在写入时被拒（400），不进快照。
- `cargo fmt -p xlite --check`、`cargo clippy -p xlite --lib --tests -- -D warnings`、`cargo test --lib`。

### 7.2 有配置时的成本（台架，不许口算）

- 用 `scripts/loadtest/ab_configs.sh`，同一网关二进制、**同一窗口反序交错两轮**，A 臂 = 不配置扩展，B 臂 = 每个 provider 配 3 个附加头 + `raw` 鉴权。
- **接受门槛**：网关 CPU 无一致方向的 ≥3% 回归；TTFT p50 不变；fail 0.00%。方法学直接照抄被证伪 §14.6 的那次实验（正反两序抵消位置效应）。
- 若测不到效应，结论就写"未测到 ≳3% 的效应"，**不要写成"零成本"**——这是本仓一贯的措辞纪律。

### 7.3 冷路径

- 用 24-mock catalog（`examples/mock-24.yaml`）测 `Store::reload()` 耗时，必须远小于重载周期（默认 5s）。
- 快照构建复杂度 O(#providers × #headers)，与请求量无关。

## 8. 风险

| 风险 | 处置 |
| --- | --- |
| 附加头泄漏到错误上游 | 字段挂在 provider 上、快照期绑定到通道；同一 provider 的所有通道共享，语义明确 |
| 运营误配 `host` / `content-type` 破坏协议 | §5.6 拒绝名单 + 写入期 400 |
| Redis 目录跨版本不兼容 | §5.4 `#[serde(default)]` + 默认忽略未知键（已核实） |
| 导出结果漂移破坏 YAML 往返 | `skip_serializing_if` + 新增 export/import 往返单测 |
| "零成本"被当成结论 | §7.2 措辞纪律：只说测到的效应 |
| 机制被当成"通用插件框架"继续扩张 | §3 判定规则 + §9 规则修订把边界写进常设约束 |

## 9. 治理：需要人工批准的两处常设约束修订

本设计的机制若要落地，必须先改常设约束，而不是绕过它们。

**(a) `AGENTS.md` 规则 9** —— 在"禁止组织/项目/智能路由/策略枚举"后补一句：

> **通道级上游形状（鉴权方案、静态请求头、chat 路径）属于快照预计算的数据，不是策略**：允许以 catalog 字段加入；请求路径只读取快照期算好的常量，不得在请求期做匹配、解析、查询或跨请求可变状态。

**(b) [`docs/ai/agents/wire-boundary.md`](../ai/agents/wire-boundary.md)** —— 补一行：

> 不得在 `Body::from_stream` 的每一帧上挂 hook；请求期允许的扩展只有快照期已成常量的字段读取。

这两句的作用是**把"能加什么/不能加什么"从设计稿升格为机械可判的常设约束**，避免下一个 agent 从"有插件机制"推出"可以加策略"。

## 10. 命名建议

**建议不用 `plugin` 一词**，用 **"通道上游形状（channel upstream shape）"** 或 **"catalog 扩展字段"**。理由：

1. `plugin` 天然暗示"可注册、可扩展、可第三方"，会诱导出框架与 ABI 讨论，而本设计要的恰恰是反方向（把能力压成数据）。
2. 本仓元知识层历史上已经出现过对不存在插件能力的幻觉引用（`AGENTS.md` 规则 2 的典型场景）；引入这个词会提高复发概率。
3. 字段名可读：`auth_scheme` / `extra_headers` / `chat_path` 本身就说清了它是什么。

## 11. 开放问题（请审核人裁决）

1. **V1 范围**：是否就是 `auth_scheme` + `extra_headers` + `chat_path` 三项？还是先只做 `extra_headers`（需求最确定、风险最低）？
2. **UI 时机**：V1 只做后端 + YAML（推荐），还是必须同 PR 给「供应商」对话框加字段？
3. **迁移接受度**：新增 `migrations/002` 意味着部署时要跑迁移（sqlx 自动）。可接受吗？还是希望先用 Redis/内存模式验证一轮？
4. **限流/配额是否立项**：这是唯一有真实需求、但属于 L1 的能力。若立项，需要独立设计稿，且必须先回答"计数器放哪、争用多大、集群语义是什么"。
5. **`raw` 鉴权是否必要**：部分上游用 `x-api-key` + 原样值；如果只有 `bearer` 与自定义头名的组合，能否砍掉 `raw` 减少分支？
6. **规则修订**：§9 的 (a)(b) 两句话是否接受？不接受的话，本设计的 L0 机制缺少常设依据，不建议落地。

## 附录 A：验证记录

| 编号 | 命令 | 观察 |
| --- | --- | --- |
| V1 | `grep -rniE "trait [A-Za-z]*Plugin\|mod plugins\|src/plugins\|wasmtime\|wasmer\|extism\|libloading\|dlopen" src/ Cargo.toml` | 无匹配 → 无任何插件机制（S5） |
| V2 | `sed -n '/\[dependencies\]/,/^\[/p' Cargo.toml` | 只有 tokio/axum/reqwest/sqlx/redis 等；无 WASM/动态加载（S6） |
| V3 | `grep -n "bearer_key\|authenticate\|ModelRef\|channels_for\|for channel\|outbound_body\|post_stream\|should_failover" src/proxy.rs` | 命中 `:83 :100 :103 :107 :140 :152 :174 :180 :189` → §2 S3 的固定顺序 |
| V4 | `grep -n "chat_completions_url\|h2c_parts\|authorization_header" src/store.rs` | `:173-176` 三者在 `to_channel` 内调用 → §2 S2 |
| V5 | `sed -n '242,248p' src/upstream.rs` | `authorization_header` 返回 `format!("Bearer {api_key}")` → §2 S1 |
| V6 | `sqlite3 /tmp/p2check.db < migrations/001_init.sql` 后接 §5.1 的四条 `ALTER`，再插一行 legacy provider 并回读 | 001/002 均成功；legacy 行读出 `bearer \| [] \| [] \| []`；重复 apply 报 `duplicate column name`（sqlx 版本表保证只 apply 一次） |
| V7 | `sed -n '/struct Dump/,/^}/p' src/catalog/redis.rs` + `grep serde_json::from_str` | Dump 为 `serde_json::from_str` 反序列化 → §5.4 的兼容结论 |
| V8 | `sed -n '141,154p' src/main.rs` | 重载失败只 `tracing::warn!`，旧快照继续服务；启动期 `:131` 硬失败 → §2 S7 |

未包含 SQL 之外的执行型断言：本文不含 mermaid（刻意用表格/代码块表达结构，避免引入未渲染的图）。
