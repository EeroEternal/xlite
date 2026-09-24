---
name: pre-push-local-gates
description: Push 前必须在本地跑满与 CI 等效的门禁(Rust fmt/clippy/tests、admin tsc/lint/vitest),禁止把 CI 当本地沙盒。Use before every git push touching src/, tests/, or admin/.
---

# Pre-push local gates（推送前本地门禁）

## Symptom / misjudgment
把 CI 当本地沙盒:推送后才发现 eslint 未用导入、rustfmt 残差、ENOSPC、测试失败,
形成「推 → 挂 → 本地修 → 再推」的慢循环——污染提交历史、烧掉 runner 配额、
还可能在失败窗口内阻塞他人合入。

## Rule
`git push` 之前,以下命令必须**全部本地通过**(与本地 CI runner `scripts/ci_local.sh` 等效;
想一键全跑用 `scripts/ci_local.sh --full`,见 skill [`ci-local-runner`](../ci-local-runner/SKILL.md)):

```bash
# Rust
cargo fmt -p xlite -p xlite-h2-pool --check
cargo clippy -p xlite --lib --tests -- -D warnings
cargo test --lib

# Admin(admin/ 目录)
npx tsc -b --noEmit   # 必须带 -b：项目引用 tsconfig 下裸 tsc --noEmit 编译空程序、假绿
npm run lint
npx vitest run

# 改了 docs/changelog.md 的时候
bash scripts/check_changelog_placement.sh

# 一键
scripts/ci_local.sh
```

UI 页面改动另跑对应 e2e spec;`workflow_dispatch` 部署 / 打 tag 前,转入 skill
[`release`](../release/SKILL.md) 执行完整发版流程(三查 + 人工批准硬停)。

## Scope
- 大重构允许中间 commit 不全跑,但 **push 前最后一次提交必须全绿**。
- 视觉回归套件(`e2e/visual/`,Playwright 截图基线)需要完整栈(网关 + 数据库 + admin 构建产物)。CI 已迁本机 Linux,可在本地直接生成/比对基线:`scripts/ci_local.sh --visual-update`(重新生成并提交)或 `scripts/ci_local.sh --full`(比对);改了用户可见外观时按 skill [`admin-ui-change`](../admin-ui-change/SKILL.md) 的截图回归流程。
- 本地跑集成测试被中断(Ctrl-C / 超时杀进程)时 `TestContext` 的 `Drop` 不会执行,会在本机 Postgres 留下 `xrouter_<suite>_<32位hex>` 孤儿库;用 [`scripts/cleanup_test_dbs.sh`](../../../scripts/cleanup_test_dbs.sh) 清理(默认 dry-run,`--apply` 执行,自带“活动连接跳过”与“新建库年龄保护”两道护栏)。
- 本地跑不动整个集成矩阵时,至少覆盖:① 与改动同模块的套件;② 随机抽 1 个
  跨模块套件作哨兵。
- 本地门禁挂了不算完:修复后先复现该失败的等价命令并确认通过,再推。
