---
name: pre-push-local-gates
description: Push 前必须跑本地门禁 scripts/ci_local.sh（t0，按改动路径裁剪），禁止把 CI 当本地沙盒。Use before every git push.
---

# Pre-push local gates（推送前本地门禁）

## Symptom / misjudgment
把 CI 当本地沙盒:推送后才发现 eslint 未用导入、rustfmt 残差、ENOSPC、测试失败,
形成「推 → 挂 → 本地修 → 再推」的慢循环——污染提交历史、烧掉 runner 配额、
还可能在失败窗口内阻塞他人合入。

## Rule
`git push` 之前必须跑：

```bash
scripts/ci_local.sh          # t0：只跑被改动路径命中的面
```

t0 是**推送门禁，不是合入门禁**：它按改动路径裁剪（只改 `docs/**` 就只跑 meta，秒级完成），
合入 main 的严格度由**本机 t1 全量**承担，发版由**本机 t2** 承担（仓库没有 GitHub CI）。分层只有一处实现，
见 skill [`ci-local-runner`](../ci-local-runner/SKILL.md)。

t0 各面等价于手动逐条跑（想强制所有面用 `scripts/ci_local.sh --all`）：

| 面 | 命令 |
| --- | --- |
| rust | `cargo fmt -p xlite -p xlite-h2-pool --check`；`cargo clippy -p xlite --lib --tests -- -D warnings`；`cargo clippy -p xlite-h2-pool --lib --tests -- -D warnings`；`cargo test --lib`；`cargo test -p xlite-h2-pool --lib` |
| admin | `npx tsc -b --noEmit`（**必须带 `-b`**：项目引用 tsconfig 下裸 `tsc --noEmit` 编译空程序、假绿）；`npm run lint`；`npx vitest run` |
| meta | `bash scripts/check_changelog_placement.sh`；`bash scripts/check_docs_lifecycle.sh`；改动过的 `*.sh` 跑 `bash -n` |

打 tag / 发版前，转入 skill [`release`](../release/SKILL.md) 执行完整流程（三查 + 人工批准硬停）。

## Scope
- 大重构允许中间 commit 不全跑,但 **push 前最后一次提交必须让 t0 全绿**。
- 本地门禁挂了不算完:修复后先复现该失败的等价命令并确认通过,再推。
- 只改 `docs/**` / `.agents/**` 这类纯元数据改动时 t0 会跳过 rust/admin —— 这是**有意的**
  路径裁剪，不是漏跑；合入 main 前再跑一次 t1 全量。
