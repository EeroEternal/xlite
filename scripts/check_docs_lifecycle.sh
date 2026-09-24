#!/usr/bin/env bash
# 文档生命周期门禁：审查 docs/dev/ 草案是否该清理或归位（docs/rules/documentation.md
# 规定「已落地结论应写入 changelog 或稳定入口，再删除过时草案」，本脚本提供机械判据）。
#
# 输出两类候审项（输出即非零退出，逐个人工裁决后清除）：
#   landed:  文件名关键词命中 changelog 已发布段落（Unreleased 之外）→ 疑似已落地，
#            确认持久结论已在 changelog/架构文档后删除草案
#   promote: 被 AGENTS.md 或 .agents/skills（常设面）引用 → 疑似事实规范，
#            应归位到稳定入口（如 docs/design/）并更新引用，不应住在提案目录
#   注意：docs/README.md 与 docs/architecture.md 对 dev/ 的引用是合法的「详见设计稿」链接，
#   不计入 promote；但删除某文档时需同步修剪这些链接。
#
# 模式：默认全量；`--links-only` 只跑 ③④（断链/残缺）两类确定性检查，跳过
#   landed/promote/artifact 候审——供 Stop hook（agent_stop_gate.sh）使用，
#   判断题不阻断收工，只阻断客观腐化。
set -euo pipefail

links_only=0
[[ "${1:-}" == "--links-only" ]] && links_only=1

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

# changelog 去掉顶部 Unreleased 段，只留已发布部分
released="$(awk '/^## \[Unreleased\]/{skip=1; next} /^## \[[0-9]/{skip=0} !skip' docs/changelog.md)"

# 常设面：元代码（AGENTS.md + skills）。dev/ 中的活跃提案（未提交/本周修改）跳过不报。
standing="$(cat AGENTS.md .agents/skills/*/SKILL.md 2>/dev/null || true)"

# 已裁决保留白名单（2026-08-28 逐项裁决；落地/归位条件变化时重新裁决并更新）：
#   adaptive-routing            进行中（smart-routing 一期骨架落地中）
#   api-key-create-standard.html  intelligence-product（进行中）的线框产物
#   v4-issue-report.html          2026-09-18 v4.0.0 黑盒测试报告原文（BUG-012~015）
#   customer-governance-scope   范围决策记录，非交付提案
#   future-optimization-plan    未来路线图（v3.15.2 刚发布）
#   ha-shadow-cutover           shadow→execute 切换未执行，清单仍需可用
#   key-multi-route             阶段 B 核心已落地，后续阶段未完
#   mcp-oauth21                 设计稿（未落地）
#   plugin-migration-plan       持续执行的插件化治理计划
#   queue-design                含暂停中的 Redis 分布式阶段（活设计）
#   routing-pipeline            提案（未实施）
#   catalog-pricing             价目 A/B/C 已落地，实例合同价（D）未做，草案仍作后续入口
#   ha-sentinel                 已落地设计锁定，部署/手册仍引用为 Sentinel 路径权威
#   agent-security-enhancement-plan / mcp-gateway-aggregation /
#   mcp-runner-security-architecture  2026-09-11 裁决：仍为设计提案（src/ 与 crates/ 无对应实现），
#                               仅因 v3.25.0 发版把 Unreleased 条目转正而命中 released，保留待实施
#   loadtest-ttft-queue-analysis-2026-09  2026-09-21 裁决：对标压测分析 + P0-P3 roadmap；
#                                P0/P1 已落地（v4.3.0），P2/P3 推迟与解锁条件仍在第九节，
#                                next-round 真机数据仍需引用它，保留作 roadmap
#   loadtest-linkbound-2026-09   2026-09-23 裁决：双机 1 GbE 台架在 40k 长流上被链路饱和的定案
#                                （~890 rps = 链路带宽，非网关；同机 loopback 直连地板 2553 rps）；
#                                换 10G/双前重测前仍需它作为为什么不能做网关吞吐结论的依据
#   h2-pinned-poll-2026-09       2026-09-23 裁决：由 loadtest-linkbound-2026-09 §十三 指向的最小结构实验
#                                （一份池 + 连接单任务 poll）；已落地（默认关），保留作入口
#   per-runtime-epoll-shard-2026-09  2026-09-23 裁决：残留 18% = 内核 epoll 等待队列锁（调用栈已对）；
#                                下一刀 = epoll 实例按 runtime 分片（不含每核池）；未实施，保留作下次入口
#   （已归位 docs/architecture/：smart-routing / routing-strategies / org-user-model /
#    unigateway-protocol / plugin-system；已删除：force-model-match / ha-progress /
#    multi-org-plan / anthropic-adaptation）
keep=" adaptive-routing.md agent-security-enhancement-plan.md api-key-create-standard.html catalog-pricing.md customer-governance-scope.md future-optimization-plan.md h2-pinned-poll-2026-09.md ha-sentinel.md ha-shadow-cutover.md key-multi-route.md loadtest-linkbound-2026-09.md loadtest-ttft-queue-analysis-2026-09.md mcp-gateway-aggregation.md mcp-oauth21.md mcp-runner-security-architecture.md per-runtime-epoll-shard-2026-09.md plugin-migration-plan.md queue-design.md routing-pipeline.md v4-issue-report.html "

findings=0
for f in docs/dev/*; do
  [[ "$links_only" -eq 0 ]] || break
  [[ -e "$f" ]] || continue
  base="$(basename "$f")"
  stem="${base%.*}"

  if [[ "$keep" == *" $base "* ]]; then
    continue
  fi

  if [[ "$base" != *.md ]]; then
    echo "artifact: ${f}（dev 下只应有 Markdown 草案，产物应放 gitignore 的目录）"
    findings=1
    continue
  fi

  if grep -qF "$base" <<<"$standing"; then
    echo "promote: $f 被常设面（AGENTS.md/skills）引用，应归位到稳定入口并更新引用"
    findings=1
  fi

  # 文件名连字符视作可分隔（adaptive-routing 命中 "adaptive routing" / "adaptive-routing"）
  pat="$(printf '%s' "$stem" | sed 's/-/[ -]/g')"
  if grep -qiE "$pat" <<<"$released"; then
    echo "landed:  $f 疑似已落地（changelog 已发布段落命中），确认后删除"
    findings=1
  fi
done

# ③ 相对断链检查：docs 全树 + AGENTS.md + skills 的 Markdown 相对链接目标必须存在
# （锚点只查文件存在性；外链 http/mailto 跳过）
broken="$( set +e
{ find docs -name '*.md' -print0; printf 'AGENTS.md\0'; find .agents/skills -name '*.md' -print0; } | while IFS= read -r -d '' md; do
  dir="$(dirname "$md")"
  grep -oE '\]\([^)#]+(#[^)]*)?\)' "$md" 2>/dev/null | sed -E 's/^\]\(//; s/(#[^)]*)?\)$//' | while IFS= read -r tgt; do
    case "$tgt" in http*|mailto:*|/*) continue ;; esac
    [[ -e "$dir/$tgt" ]] || echo "broken:  $md → $tgt"
  done
done
true )"
if [[ -n "$broken" ]]; then
  printf '%s\n' "$broken"
  findings=1
fi

# ④ 残缺链接：批量 sed 吃掉闭合 ] 的典型伤（[文本(../path) 形式，无法被 ③ 的链接解析看到）
mangled="$(grep -rnE '\[[^]]*\((\.\./)?(architecture|dev|design|manual)/' docs --include='*.md' 2>/dev/null || true)"
if [[ -n "$mangled" ]]; then
  printf '%s\n' "$mangled"
  findings=1
fi

if [[ "$findings" -eq 0 ]]; then
  if [[ "$links_only" -eq 1 ]]; then
    echo "✓ 文档链接检查通过（--links-only：断链/残缺零发现）"
  else
    echo "✓ 文档生命周期检查通过：dev/ 无落地未清理、无常设引用未归位的草案"
  fi
fi
exit "$findings"
