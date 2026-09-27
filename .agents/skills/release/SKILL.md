---
name: release
description: 发版（打 tag）：本地门禁全量重跑、版本号与 changelog 对齐、文档生命周期、人工批准后 tag。Use before cutting any vX.Y.Z tag.
---

# Release

xlite 没有 CI 与部署自动化：tag 只是发布标记，不会自动上生产。发版前的 t2 只读校验（`scripts/check_release_sync.sh` 等）在 **Linux 机**上跑，见 Phase 0。

## Phase 0 门禁

按 skill [`pre-push-local-gates`](../pre-push-local-gates/SKILL.md) 走全量，或直接
`scripts/ci_local.sh --tier t2`（= t1 全量 + release sync）。

## Phase 1 发版三查

1. **版本号**：根 `Cargo.toml` `version` 改为目标版本。`admin/package.json` 独立演进。
2. **changelog 转正**：`docs/changelog.md` 的 `## [Unreleased]` 下条目移到 `## [X.Y.Z] - <当日日期>`，其上重建空的 `## [Unreleased]`。
3. **设置页版本**：`GET /api/overview` 的 `version` 来自 `CARGO_PKG_VERSION`，设置页展示它。xlite 没有帮助中心四触点。
4. **文档生命周期**：`scripts/check_docs_lifecycle.sh`。

机械检查：

```bash
scripts/check_release_sync.sh
scripts/check_docs_lifecycle.sh
```

## Phase 2 硬停

**获得用户明确批准前不得打 tag。**

## Phase 3 bump / tag

```bash
git commit -m "chore(release): bump version to vX.Y.Z"
git tag vX.Y.Z
git push origin main vX.Y.Z
```
