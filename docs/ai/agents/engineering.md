# Engineering hard rules

条款只描述**本仓库真实存在的构造**。凡是「将来有 X 才用得上」的约束，这里都写成显式条件
（「若你新增 X，则…」）——不要为了满足一条写不到实处的规则去引入 X。

## 一、禁止夹带变更（Silent Drift）

单个 commit / PR 不得夹带与声明目标无关的修改。特别是以下行为一律视为违规：

1. **调参私货**：以 "remediate" / "refactor" / "format" 名义修改默认值、阈值、超时、并发上限、限流配置等生产参数。
2. **批量重格式化**：`cargo fmt` / `prettier` / `black` 的结果必须单独 commit，不得与逻辑改动混在同一 commit。
3. **`#[allow(...)]` 压制**：用 `#[allow(clippy::xxx)]` 让警告消失，但没说明原因。
4. **跨模块 refactor**：修复 A 模块的 bug 时，不得同时重命名 B 模块的内部结构。

违规 commit 一律要求 `git reset --mixed HEAD~1` 重新拆分。

---

## 二、`git stash`

四条硬性约束见 AGENTS.md 规则 3（诚实命名 / stash 前后各自验证 / 禁 stash `Cargo.toml`
`Cargo.lock` 与构建脚本）；完整步骤与误判症状见 skill
[`git-stash-safe`](../../../.agents/skills/git-stash-safe/SKILL.md)。

---

## 三、后台任务 / daemon / `tokio::spawn`

本仓库除 `src/main.rs` 外没有常驻后台任务。**若你新增** `tokio::spawn` / daemon，以下五条
全部适用（不是「下一轮再补」）：

1. **生命周期显式化**：不得在 `impl Default::default()` / `impl X::new()` 里裸写 `tokio::spawn`；必须由显式入口方法（如 `start_background_xxx(&self)`）在 `init()` / `main()` 中调用。
2. **runtime 守卫**：用 `tokio::runtime::Handle::try_current()` 包一层，或只在已知 runtime 上下文触发，避免同步测试/CLI 场景 panic。
3. **超时兜底**：长跑任务里每次异步调用都要有 `tokio::time::timeout(...)`。
4. **错误不得吞掉**：panic / timeout / 计算错误必须 `tracing::error!` 至少一条，不得 silent `continue`。
5. **可观测**：daemon 写入的共享状态（`Arc<AtomicX>` / `Arc<RwLock<X>>`）必须至少有一个日志行或 endpoint 能读到最新值，否则无法判断它是死是活。用现成的 `tracing` 就够，**不要为此引入 metrics 依赖**。

---

## 四、避免生成幻觉代码（Ghost Code）

以下模式一律视为幻觉：

1. **有定义无调用**：写了函数 / 类型，但全仓库 `grep` 找不到任何 caller。
2. **有路径无写入**：新增 `cached_xxx: Arc<AtomicU64>` 字段，有 load 点但没有 store 点。
3. **埋点单向**（**仅当这次改动新增了 metric / 计数器**）：只埋上升不埋下降，或反之。本仓库当前没有 metrics 设施，不要为满足本条而引入。
4. **有 TODO 无 issue**：`// TODO: ...` 必须关联 GitHub issue 或 ticket。

发现此类幻觉，要求一次性补齐 caller / store / 下游埋点，不接受"下一轮再做"。

---

## 五、脚本化代码修改（`sed` / Python regex / awk）

用脚本批量改源码是高频犯错场景：

1. **替换后必须 grep 验证**：把 `foo` 换成 `bar` 后，必须 `rg "foo"` 确认剩余零匹配，或剩余匹配在预期内。
2. **`re.sub` 失败不抛异常**：不匹配时返回原串。脚本跑完必须对比 diff 或重跑 grep 验证，不得直接 `git commit`。
3. **多行 regex 特别危险**：空白、引号、拼写的任何偏差都会静默失败。优先用 `StrReplace` 工具而非自写脚本。
4. **内嵌字符串转义**：Python `'...'` 里含 `"don't"` 这类内容时逃逸容易残留，脚本改完必须人肉 review 生成的代码段。

---

## 六、限流 / 队列

xlite 没有 `src/pool/` 准入队列，也**不要**把 xrouter 的限流默认值、AIMD 调参、
`XLITE_MAX_IN_FLIGHT` 或过载 429 带进来（数据面边界见 AGENTS.md 规则 9）。

---

## 七、数据库迁移

新增 `migrations/NNN_*.sql` 后，必须按 skill [`add-sql-migration`](../../../.agents/skills/add-sql-migration/SKILL.md) 强制重建二进制；仅 `cargo build` 而不 `touch` 嵌入点时，新迁移常不会进二进制，极易误判为 SQL 写错。

---

## 八、主干不做业务定制

`src/proxy.rs` 只做鉴权、选通道、字节透传。禁止在主干为**特定产品、租户或客户端 header**
硬编码业务分支（`if header == "x-custom" { ... }`），也不要「顺手加个开关」。这类需求属于
扩展层：先设计扩展点，再落主干。

---

## 九、命令管道的退出码与「看似验证」陷阱

管道退出码默认取**末位命令**：`失败命令 | tail -3` 的退出码是 tail 的 0。两个独立场景复现过
（CI watcher `| tail` 误报全绿；试合并 `| tail` 把 fatal 当成功打印 ✓）。

- **症状**：管道后的 `&&` 链继续执行；脚本分支依据「末位命令」的退出码给出假结论。
- **正确做法**：用 `set -o pipefail`（脚本内）、临时文件承接输出，或先跑命令再读输出；判断「跑完了吗」直接查权威源（`gh pr checks` / `gh run view` 的 conclusion），不信代理进程退出码。
- **验证**：`bash -n` 只验语法；必须实跑并人为制造一次上游失败（如 `false | true`）确认链路中断。注意 `bash -o pipefail -n` 是无效组合——`-n` 不执行脚本，pipefail 不起作用，这本身就是一个「看似验证」。

---

## 十、违规处理

以下情形视为严重违规，审查方有权要求：

1. **报告与代码不符**：`git reset --hard` 回退到上一个诚实状态，重写提交。
2. **编译失败还报完成**：工作权限临时暂停，恢复前需提交书面复盘。
3. **重复同类错误 3 次以上**：该代理后续所有 commit 必须 pair review，两人 sign-off 才能合并。
4. **夹带生产参数私货**：立即 revert commit，重新按单独 commit 格式提交。

---

## 附：本规范的由来

本规范源自一次网关优化的审查过程。该过程中发现以下反复出现的问题：

- 宣称"lock-free 全局令牌桶已实现"，实际代码里还是 `Mutex<TokenBucket>`
- 宣称"AIMD scale_down 埋点已补全"，实际 regex 静默失败，埋点从未落地
- 宣称"saturation daemon 每 5 秒刷新 cache"，实际 `cached_saturation_ratio.store()` 从未被调用
- 宣称"所有 49 个测试通过"，实际 `git stash` 把核心依赖抽走后根本不编译
- 在 remediation commit 里夹带 AIMD 默认值从 `5/100/10` → `100/500/5` 的生产参数调整

每一次都是"流程走得看似到位、实际验证没做"导致的。本规范旨在减少这类幻觉报告与夹带变更。

**注**：其中队列 / 令牌桶 / AIMD / saturation daemon 相关条目来自早期 xrouter 时期的审查，
保留是为了记录规则成因（§六 正是为了避免那套设施倒灌）；**对应设施不在本仓库，不要按这些
条目去实现或引用它们**。
