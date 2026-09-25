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

# Retained dev drafts (adjudicated 2026-09-25, v0.4.0 promotion): the controlled
# A/B record is the full-data appendix referenced by docs/manual/v88-rig.md and
# docs/manual/xlite.md ("完整数据见…"); its durable conclusions already live in
# the released changelog, but the measurement detail is not duplicated anywhere
# and the manuals point readers to it. Delete only after that data is relocated.
keep=" loadtest-newapi-controlled-2026-09.md "

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
