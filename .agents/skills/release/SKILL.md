---
name: release
description: 发版（打 tag）完整 promoter 流程：本地门禁全量重跑、发版三查（版本号/changelog/帮助中心四触点/文档生命周期）、人工批准硬停、tag 部署与验证。Use before cutting any vX.Y.Z tag or running a production deploy.
---

# Release（发版 promoter 流程）

## Symptom / misjudgment
`v*` tag push 直通生产部署（`deploy-xgateway-cn.yml` 由 tag 触发），而发版知识散落在
AGENTS.md、pre-push-local-gates 与个人记忆中。真实后果：v3.14.0–v3.15.2 连续四个版本
漏更新帮助中心「最新版本」文案（当时的 `help.recentUpdatesTitle` 停在 v3.13.0），用户
看到的版本说明落后实际版本四档。**工作未完成，直到被正确地传达出去。**
2026-09-12 补充：`help.recentUpdatesTitle` / `recentUpdatesSummary` 经核查**从未被任何
组件渲染**，已删除；帮助中心卡片上用户可见的版本文案是
`help.docsSections.changelog.summary`，由 `scripts/check_release_sync.sh` 机械校验。
2026-09-16 补充：CI 已不在 GitHub 上跑（`799f175c`），**推送到 `main` 不会触发任何门禁**。
v4.0.0 的实证：版本号 bump 到 4.0.0 使授权测试夹具的
`max_supported_major_version: Some(3)` 全部失效（`intelligence_data_plane_e2e_tests` 7 条失败，
`intelligence_license_integration_tests` 因分片在首个失败目标即停而根本没跑），而 bump 之后的
提交从未跑过本地门禁，`main` 在 v4.0.0 发版后一直红着，直到同日修复。

## Procedure

### Phase 0 门禁（tag 前全量重跑）
按 skill [`pre-push-local-gates`](../pre-push-local-gates/SKILL.md) 全量执行（fmt /
clippy / `cargo test --lib` + 改动套件 / admin tsc + lint + vitest）。tag 即生产部署，
不允许只跑增量。

**tag 前后的 `main` 直推同样适用本阶段**：GitHub 侧已无 CI 兜底（`799f175c` 删掉
`.github/workflows/ci.yml`，只剩两个部署 workflow），所以**推送到 `main` 不触发任何门禁**。
发版窗口内落在 tag 之后的修复 / 文档 / `crates/` 提交，直推 `main` 前必须对这些提交重跑
等价门禁——「本地门禁只在打 tag 时跑」正是 v4.0.0 漏掉 fixture 回归的原因（见 Symptom）。
用 `scripts/ci_local.sh` 即可（变更感知；`rust=true` 时已含 `rust-aux-crates`）。

### Phase 1 发版三查（逐项勾选）
1. **版本号一致**：`Cargo.toml` 与 `crates/xr/Cargo.toml` 同步改为目标版本，`cargo
   check` 刷新 `Cargo.lock`（admin/package.json 版本独立演进，不随发版）。
2. **changelog 转正**：`docs/changelog.md` 的 `## [Unreleased]` 改为
   `## [X.Y.Z] - <当日日期>`，其上重建空的 `## [Unreleased]`。
3. **帮助中心触点**（`admin/src/lib/i18n/locales/zh.ts` 与 `en.ts` 双语同步）：
   - `help.docsSections.changelog.summary`：以新版本开头的一行摘要——这就是帮助中心卡片上
     用户可见的「最新版本」文案，其版本前缀由 `scripts/check_release_sync.sh` 机械校验；
   - `help.docsSections.changelog.content`：头部插入新版块（`### vX.Y.Z (日期)`）；
   - 本版次用户可见新功能 ↔ `docsSections` 对应章节或 `docs/manual/` 的对应关系，缺则补；
   - 卡片上的「最新版本」徽章文案 `help.recentUpdatesBadge` 不含版本号，无需随发版修改。
   - 发版若改动上述文案，`/help` 的截图基线会随之失效；该用例已对版本摘要段落做遮罩，
     正常情况下**无需**重生成基线（见 skill [`admin-ui-change`](../admin-ui-change/SKILL.md) 第 6 步）。
4. **文档生命周期**：跑 `scripts/check_docs_lifecycle.sh`，逐项裁决其输出（删落地草案 /
   归位常设引用），已裁决保留项更新脚本内白名单。

机械检查（应零差异通过）：

```bash
scripts/check_release_sync.sh
scripts/check_docs_lifecycle.sh
```

第 3 项改动了 `zh.ts` / `en.ts` 帮助文案后，admin 类型校验必须带 `-b`：
`cd admin && npx tsc -b --noEmit`（等价 `npm run build`）。**裸 `npx tsc --noEmit` 是假绿**：
admin 的 `tsconfig.json` 只有 project references、无直接 files，非 build 模式编译空程序。
2026-09-20 v4.2.2 把 `routing's` 撇号写进单引号 `changelog.content` 字符串，裸 `tsc --noEmit` 漏掉，
生产部署才在远程 `tsc -b` 崩掉（TS1005 级联）。

### Phase 2 硬停：人工批准
向用户逐项汇报三查结果（含两个机械检查的输出），**获得明确批准前不得打 tag**
（AGENTS.md 站立约束 5：未经用户明确批准严禁发版打 Tag）。

### Phase 3 bump / tag / 部署
bump commit 只含三查产物，不得夹带功能代码；tag 打在 bump commit 上：

```bash
git commit -m "chore(release): bump version to vX.Y.Z"
git tag vX.Y.Z
git push origin main vX.Y.Z     # tag 触发 deploy-xgateway-cn.yml
gh run list --workflow deploy-xgateway-cn.yml --limit 1   # 确认部署绿
```

部署后人工核对线上 Admin 帮助中心横幅与版本动态页。

### Phase 4 事后
无——homebrew 分发线已于 2026-08-29 退役（tap 停止更新，无分发产物流水线）。
若未来重启 brew 分发，需先建 tag 触发的产物构建工作流，再恢复本节。

## Scope
- 仅在用户明确要求发版时进入本 skill；`workflow_dispatch` 手动部署同样执行 Phase 0–2。
- Phase 0 的门禁不止用于 tag：tag 之后向 `main` 直推的提交同样要跑（上面已说明）。
- 本 skill 不覆盖版本号选择；语义化版本由用户或 changelog 条目规模决定。
