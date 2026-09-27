#!/usr/bin/env bash
# Changelog 归属与结构门禁。
#
# 背景：一个发布窗口内连续出现 5 次同类错误——新条目被插进了**已发布版本段**，因为插入
# 锚点（`### Changed` 或某条旧 bullet）在旧版本段里同样存在；最严重的一次把
# `## [3.26.0]` 段搞出三个重复的 `### Changed` 标题。人工检查不可靠，故设为机械门禁。
#
# 三项检查：
#   1. 归属：本次变更**新增**的条目必须落在 `## [Unreleased]` 段内。加粗条目（`- **标题**：…`）
#      按标题判定，条目被移动段落或只改正文不误报；**无加粗标题的条目按整行判定**（它们没有
#      标题可退让，且曾让这条检查整个失效：2026-09-16 有一条无加粗标题的新条目被插进已发布的
#      `## [4.0.0]` 段而门禁全绿）；
#   2. 全局：`## [Unreleased]` 恰好出现一次；
#   3. 结构（**仅针对本次改动触及的版本段**）：段内小节标题（`### Added` / `### Changed`
#      / `### Fixed` …）与版本标题不得重复。为什么分范围：仓库历史里 0.12.x 等老版本段
#      本身就有重复标题（6 处，属历史遗留），全文件严格校验会让门禁一上来就红、从而被
#      绕过；而门禁要防的是**本次改动引入**的错位与重复。
#
# 用法：
#   scripts/check_changelog_placement.sh                 # 与 origin/main 比对
#   scripts/check_changelog_placement.sh --base main     # 分支名或 commit SHA 均可
#   scripts/check_changelog_placement.sh --self-test     # 夹具自证门禁会拦
#
# 基线来源：`--base` / `CI_LOCAL_BASE`，默认 `origin/main`（本地 git 直接解析；
# 解析不了就 fetch 一次）。仓库没有 GitHub CI，也不再有 GitHub API 回退。
# 基线解析顺序：本地可解析为 commit → GitHub contents API（任意 SHA 可用）→ fetch ref → 跳过。
# 取不到基线时（新分支 / 浅克隆 / 全部策略失败）**明确打印跳过**并只
# 校验全局不变式，而不是假装通过——门禁的价值在编写阶段，不该因基线取不到而把门禁卡红。
#
# 归属判定用「加粗标题」而非整行文本：条目被移动段落（标题在 base 里已存在）或只改正文
# （标题不变）都不算新增，避免误报。
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"
changelog="docs/changelog.md"
unreleased_literal='## [Unreleased]'
unreleased_re='^## \[Unreleased\]$'

die() { echo "✗ $*" >&2; exit 1; }

# ---------------------------------------------------------------- 结构检查
# check_structure <文件> [<须检查的版本段标题>...]
# 不传版本段则检查全部（自测用）；真实仓库只传本次改动触及的段，避免历史遗留重复误报。
#
# 注意：不要把「版本段清单」当作多行字符串塞进 awk 的 -v（BSD awk 会报
# `newline in string` 并静默失效，实测导致重复小节标题漏检）。awk 只负责产出全部重复项，
# 范围过滤交给 shell。
check_structure() {
  local f="$1"; shift
  local targets=("$@") err=0 count

  [[ -f "$f" ]] || { echo "✗ 找不到 changelog：$f"; return 1; }

  count="$(grep -cE "$unreleased_re" "$f" || true)"
  if [[ "$count" != "1" ]]; then
    echo "✗ ${f}: '${unreleased_literal}' 出现 ${count} 次（应恰好 1 次）"
    err=1
  fi

  in_scope() { # <section>：无 targets 则全为范围内
    [[ "${#targets[@]}" -eq 0 ]] && return 0
    local t
    for t in "${targets[@]}"; do [[ "$t" == "$1" ]] && return 0; done
    return 1
  }

  # 段内重复小节标题
  local dups
  dups="$(awk '
    /^## \[/ { section = $0; next }
    /^### / {
      key = section SUBSEP $0
      if (++count[key] == 2) printf "%s\t%s\n", section, $0
    }' "$f")"
  while IFS=$'\t' read -r section heading; do
    [[ -z "${section:-}" ]] && continue
    in_scope "$section" || continue
    echo "✗ ${f}: 「${section}」段内重复的小节标题：${heading}"
    err=1
  done <<< "$dups"

  # 版本标题重复
  local dup_versions
  dup_versions="$(awk '/^## \[/ { h[$0]++ } END { for (k in h) if (h[k] > 1) print k }' "$f")"
  while IFS= read -r section; do
    [[ -z "${section:-}" ]] && continue
    in_scope "$section" || continue
    echo "✗ ${f}: 版本标题重复：${section}"
    err=1
  done <<< "$dup_versions"

  return "$err"
}

# 全局不变式：恰好一个 Unreleased 段（不判断任何历史版本段的结构）
check_unreleased_only() {
  local f="$1" count
  [[ -f "$f" ]] || { echo "✗ 找不到 changelog：$f"; return 1; }
  count="$(grep -cE "$unreleased_re" "$f" || true)"
  if [[ "$count" != "1" ]]; then
    echo "✗ ${f}: '${unreleased_literal}' 出现 ${count} 次（应恰好 1 次）"
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------- 归属检查
section_at() { # <文件> <行号>
  awk -v target="$2" 'NR <= target && /^## \[/ { section = $0 } NR == target { print section }' "$1"
}

# LC_ALL=C：非 C locale 下 sort -u 会按本地化归并把不同标题判为相等而丢失（实测
# 551 条只剩 507 条，python 基准是 544 条），会造成大量误报。
titles_of() { sed -n 's/^- \*\*\([^*]*\)\*\*.*/\1/p' "$1" | LC_ALL=C sort -u; }

check_placement() { # <base 文件> <head 文件>
  local base="$1" head="$2" err=0 base_titles entry line_no text title section key

  base_titles="$(mktemp)"
  titles_of "$base" > "$base_titles"

  while IFS= read -r entry; do
    [[ -z "$entry" ]] && continue
    line_no="${entry%%:*}"
    text="${entry#*:}"
    title="$(printf '%s\n' "$text" | sed -n 's/^- \*\*\([^*]*\)\*\*.*/\1/p')"
    if [[ -n "$title" ]]; then
      # 加粗标题：标题在基线里已存在就不算新增（条目被移动段落 / 只改正文，避免误报）
      grep -qxF -- "$title" "$base_titles" && continue
      key="$title"
    else
      # 没有加粗标题：整行当键。这类条目曾让归属检查**整个失效**：2026-09-16 发现
      # 8c3308a8 把一条无加粗标题的新条目插进了已发布的 [4.0.0] 段，而门禁全绿。
      # 没有标题可退让，因此这里比加粗路径严格——已发布段里的条目文本不应当改动。
      grep -qxF -- "$text" "$base" && continue
      key="$text"
    fi
    section="$(section_at "$head" "$line_no")"
    if [[ "$section" != "$unreleased_literal" ]]; then
      echo "✗ ${head}:${line_no} 新增条目落在「${section:-无版本段}」而不是 ${unreleased_literal}：${key}"
      err=1
    fi
  done < <(grep -n '^- ' "$head" || true)

  rm -f "$base_titles"
  return "$err"
}

# 本次改动触及的版本段 = 所有「新增行」所在段
touched_sections() { # <base 文件> <head 文件>
  local base="$1" head="$2" added line_no section
  # comm 也必须是 LC_ALL=C：它要求输入按**它自己的**排序规则有序，否则会把字节相同的行
  # 误判为「仅在 head 中」，从而把无关的历史版本段纳入结构校验（若命中本身有重复标题的段
  # 就会假失败）。实测：只给 sort 加 C 时，5 条历史条目被误判为新增。
  added="$(LC_ALL=C comm -13 <(LC_ALL=C sort -u "$base") <(LC_ALL=C sort -u "$head") | grep -E '^\S' || true)"
  while IFS= read -r text; do
    [[ -z "$text" ]] && continue
    line_no="$(grep -nF -m1 -- "$text" "$head" | cut -d: -f1 || true)"
    [[ -z "$line_no" ]] && continue
    section="$(section_at "$head" "$line_no")"
    [[ -n "$section" ]] && printf '%s\n' "$section"
  done <<< "$added" | LC_ALL=C sort -u
}

# ---------------------------------------------------------------- 自测
self_test() {
  local tmp failures=0
  tmp="$(mktemp -d)"
  trap 'rm -rf "${tmp:-}"' EXIT

  cat > "$tmp/base.md" <<'MD'
## [Unreleased]

## [9.9.0] - 2026-01-01

### Changed
- **既有条目**：老版本里的内容。
MD

  sed 's/^## \[Unreleased\]$/## [Unreleased]\n\n### Changed\n- **新条目**：落在未发布段里。/' \
    "$tmp/base.md" > "$tmp/ok.md"

  cat > "$tmp/misfiled.md" <<'MD'
## [Unreleased]

## [9.9.0] - 2026-01-01

### Changed
- **既有条目**：老版本里的内容。
- **新条目**：被错误地插进了已发布段。
MD

  cat > "$tmp/dup-section.md" <<'MD'
## [Unreleased]

## [9.9.0] - 2026-01-01

### Changed
- **既有条目**：老版本里的内容。

### Changed
- **另一条**：重复的小节标题。
MD

  cat > "$tmp/no-unreleased.md" <<'MD'
## [9.9.0] - 2026-01-01

### Changed
- **条目**：没有 Unreleased 段。
MD

  expect() { # expect <pass|fail> <名称> <函数> <参数...>
    local want="$1" name="$2" fn="$3"; shift 3
    local got=pass
    if ! "$fn" "$@" >/dev/null 2>&1; then got=fail; fi
    if [[ "$got" == "$want" ]]; then
      echo "  ✓ ${name}（${got}）"
    else
      echo "  ✗ ${name}：期望 ${want}，实际 ${got}"
      failures=$((failures + 1))
    fi
  }

  echo "check_changelog_placement 自测："
  expect pass "全局不变式：恰一个 Unreleased" check_unreleased_only "$tmp/base.md"
  expect fail "全局不变式：缺少 Unreleased" check_unreleased_only "$tmp/no-unreleased.md"
  expect pass "结构完好（含 Unreleased 唯一）" check_structure "$tmp/base.md"
  expect fail "缺少 Unreleased 段" check_structure "$tmp/no-unreleased.md"
  expect fail "同一版本段内重复小节标题" check_structure "$tmp/dup-section.md"
  expect pass "新增条目落在 Unreleased" check_placement "$tmp/base.md" "$tmp/ok.md"
  expect fail "新增条目落进已发布段" check_placement "$tmp/base.md" "$tmp/misfiled.md"
  expect pass "条目被移动段落（不算新增，不误报）" check_placement "$tmp/base.md" "$tmp/base.md"

  # 无加粗标题的条目：没有标题可退让，按整行判定（旧漏洞的回归夹具）
  cat > "$tmp/plain-ok.md" <<'MD'
## [Unreleased]

### Changed
- 无加粗标题的新条目，落在未发布段。

## [9.9.0] - 2026-01-01

### Changed
- **既有条目**：老版本里的内容。
MD

  cat > "$tmp/plain-misfiled.md" <<'MD'
## [Unreleased]

## [9.9.0] - 2026-01-01

### Changed
- **既有条目**：老版本里的内容。
- 无加粗标题的新条目，被错误地插进了已发布段。
MD

  expect pass "无加粗标题的条目落在 Unreleased" check_placement "$tmp/base.md" "$tmp/plain-ok.md"
  expect fail "无加粗标题的条目落进已发布段（2026-09-16 漏洞）" check_placement "$tmp/base.md" "$tmp/plain-misfiled.md"

  # 发版转正：Unreleased 段整体改名为已发布版本段、条目文本不变 → 不得误报，
  # 否则发版 commit 会莫名其妙卡门禁（加粗与无加粗两条路径都要覆盖）。
  cat > "$tmp/release-base.md" <<'MD'
## [Unreleased]

### Changed
- **待发布条目**：正文。
- 无加粗标题的待发布条目。
MD

  cat > "$tmp/release-head.md" <<'MD'
## [Unreleased]

## [9.9.0] - 2026-02-02

### Changed
- **待发布条目**：正文。
- 无加粗标题的待发布条目。
MD

  expect pass "发版转正（Unreleased → 已发布段，文本不变）" check_placement "$tmp/release-base.md" "$tmp/release-head.md"

  # 触及段落范围：重复标题在“无关段落”时才出现，不应拦住本次改动
  expect pass "范围外的历史重复不误报（只查 9.9.0）" check_structure "$tmp/ok.md" "## [9.9.0] - 2026-01-01"
  expect fail "范围内的重复会拦住" check_structure "$tmp/dup-section.md" "## [9.9.0] - 2026-01-01"

  # touched_sections：历史 CJK 条目不得被误判为「新增」（LC_ALL=C comm 回归）
  cat > "$tmp/touch-base.md" <<'MD'
## [Unreleased]

## [9.9.0] - 2026-01-01

### Changed
- **既有中文条目**：老版本里的内容，含冒号与标点。
- **另一条中文条目**：也不该被当成新增。
MD

  cat > "$tmp/touch-head.md" <<'MD'
## [Unreleased]

### Changed
- **新中文条目**：应被判定为落在 Unreleased。

## [9.9.0] - 2026-01-01

### Changed
- **既有中文条目**：老版本里的内容，含冒号与标点。
- **另一条中文条目**：也不该被当成新增。
MD

  local touched
  touched="$(touched_sections "$tmp/touch-base.md" "$tmp/touch-head.md" | tr '\n' ' ' | sed 's/ *$//')"
  if [[ "$touched" == "## [Unreleased]" ]]; then
    echo "  ✓ 触及段落只含 Unreleased（历史 CJK 条目未被误判）"
  else
    echo "  ✗ 触及段落异常：[$touched]"
    failures=$((failures + 1))
  fi

  # 真实仓库：Unreleased 必须唯一（结构性历史遗留不在判罚范围）
  if [[ "$(grep -cE "$unreleased_re" "$changelog" || true)" == "1" ]]; then
    echo "  ✓ 真实 ${changelog} 恰有一个 Unreleased 段"
  else
    echo "  ✗ 真实 ${changelog} 的 Unreleased 段个数异常"
    failures=$((failures + 1))
  fi

  if [[ "$failures" -eq 0 ]]; then
    echo "✓ check_changelog_placement：全部用例通过"
    return 0
  fi
  echo "✗ check_changelog_placement：${failures} 个用例失败"
  return 1
}

# ---------------------------------------------------------------- 入口
# 解析基线内容。策略依次为：本地可作为 commit 解析 → fetch 该 ref 后用 FETCH_HEAD。
# 任何一步拿不到**非空**内容即返回非零，由调用方明确跳过——绝不能把空基线当成
# 「所有条目都是新增」。（GitHub contents API 回退已随无 CI 化删除：无人再设
# `GITHUB_REPOSITORY`，本地与 fetch 两条路覆盖纯本地工作流。）
resolve_base_file() { # <base_ref> <输出文件>
  local ref="$1" out="$2" sha
  : > "$out"

  if sha="$(git rev-parse --verify --quiet "${ref}^{commit}" 2>/dev/null)"; then
    git show "${sha}:${changelog}" > "$out" 2>/dev/null || true
    if [[ -s "$out" ]]; then echo "  （基线来源：本地 commit ${sha:0:10}）"; return 0; fi
  fi

  if git fetch --no-tags --depth=1 -q origin "$ref" 2>/dev/null; then
    git show "FETCH_HEAD:${changelog}" > "$out" 2>/dev/null || true
    if [[ -s "$out" ]]; then echo "  （基线来源：git fetch ${ref}）"; return 0; fi
  fi

  : > "$out"
  return 1
}

run_with_base() {
  local base_ref="$1"
  command -v git >/dev/null 2>&1 || die "未找到 git"

  # 无基线（新分支 / force push 的 all-zero before）
  if [[ -z "$base_ref" || "$base_ref" =~ ^0+$ ]]; then
    echo "⚠ 无可用基线（新分支或 force push），跳过归属检查，仅校验全局不变式"
    check_unreleased_only "$changelog"
    return $?
  fi

  local base_file
  base_file="$(mktemp)"
  if ! resolve_base_file "$base_ref" "$base_file"; then
    echo "⚠ 无法取得基线 ${base_ref} 的 ${changelog}（本地不可解析 / fetch 失败），跳过归属检查"
    check_unreleased_only "$changelog"
    local rc=$?
    rm -f "$base_file"
    return "$rc"
  fi

  local status=0 sections=()
  while IFS= read -r s; do [[ -n "$s" ]] && sections+=("$s"); done < <(touched_sections "$base_file" "$changelog")
  if [[ "${#sections[@]}" -gt 0 ]]; then
    check_structure "$changelog" "${sections[@]}" || status=1
  else
    # 本次没有触及任何版本段：只验证全局不变式。绝不能退化成「全文件校验」——
    # 仓库历史里本来就有重复标题（0.12.x 等 6 处），那样门禁会天天红而被绕过。
    check_unreleased_only "$changelog" || status=1
  fi
  check_placement "$base_file" "$changelog" || status=1
  rm -f "$base_file"

  if [[ "$status" -eq 0 ]]; then
    echo "✓ changelog 归属与结构检查通过（基线 ${base_ref}；触及段落：${sections[*]:-无}）"
  fi
  return "$status"
}

case "${1:-}" in
  --self-test) self_test ;;
  --base) run_with_base "${2:-origin/main}" ;;
  --base=*) run_with_base "${1#--base=}" ;;
  "") run_with_base origin/main ;;
  *) die "未知参数：$1（脚本头注释有用法）" ;;
esac
