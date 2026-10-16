#!/usr/bin/env bash
# Changelog placement & structure gate.
#
# Enforces three mechanical invariants on `docs/changelog.md`:
#   1. Placement: Any newly added entry in this diff MUST appear under `## [Unreleased]`.
#      Bolded entries (`- **Title**: ...`) are keyed by their bold title so moving an existing
#      entry across sections or editing its body does not trigger a false positive; unbolded
#      entries are keyed by the full line.
#   2. Global invariant: `## [Unreleased]` appears exactly once.
#   3. Scoped structure (only for sections touched by the current diff): No duplicate
#      subsection headings (`### Added`, `### Changed`, etc.) within a version section,
#      and no duplicate version headings (`## [x.y.z]`).
#
# Usage:
#   scripts/check_changelog_placement.sh                 # compare against origin/main
#   scripts/check_changelog_placement.sh --base main     # compare against a branch or commit SHA
#   scripts/check_changelog_placement.sh --self-test     # run built-in regression test suite
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"
changelog="docs/changelog.md"
unreleased_literal='## [Unreleased]'
unreleased_re='^## \[Unreleased\]$'

die() { echo "✗ $*" >&2; exit 1; }

# ---------------------------------------------------------------- Structure check
check_structure() {
  local f="$1"; shift
  local targets=("$@") err=0 count

  [[ -f "$f" ]] || { echo "✗ changelog not found: $f"; return 1; }

  count="$(grep -cE "$unreleased_re" "$f" || true)"
  if [[ "$count" != "1" ]]; then
    echo "✗ ${f}: '${unreleased_literal}' appears ${count} times (expected exactly 1)"
    err=1
  fi

  in_scope() {
    [[ "${#targets[@]}" -eq 0 ]] && return 0
    local t
    for t in "${targets[@]}"; do [[ "$t" == "$1" ]] && return 0; done
    return 1
  }

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
    echo "✗ ${f}: duplicate subsection heading in '${section}': ${heading}"
    err=1
  done <<< "$dups"

  local dup_versions
  dup_versions="$(awk '/^## \[/ { h[$0]++ } END { for (k in h) if (h[k] > 1) print k }' "$f")"
  while IFS= read -r section; do
    [[ -z "${section:-}" ]] && continue
    in_scope "$section" || continue
    echo "✗ ${f}: duplicate version heading: ${section}"
    err=1
  done <<< "$dup_versions"

  return "$err"
}

check_unreleased_only() {
  local f="$1" count
  [[ -f "$f" ]] || { echo "✗ changelog not found: $f"; return 1; }
  count="$(grep -cE "$unreleased_re" "$f" || true)"
  if [[ "$count" != "1" ]]; then
    echo "✗ ${f}: '${unreleased_literal}' appears ${count} times (expected exactly 1)"
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------- Placement check
section_at() {
  awk -v target="$2" 'NR <= target && /^## \[/ { section = $0 } NR == target { print section }' "$1"
}

titles_of() { sed -n 's/^- \*\*\([^*]*\)\*\*.*/\1/p' "$1" | LC_ALL=C sort -u; }

check_placement() {
  local base="$1" head="$2" err=0 base_titles entry line_no text title section key

  base_titles="$(mktemp)"
  titles_of "$base" > "$base_titles"

  while IFS= read -r entry; do
    [[ -z "$entry" ]] && continue
    line_no="${entry%%:*}"
    text="${entry#*:}"
    title="$(printf '%s\n' "$text" | sed -n 's/^- \*\*\([^*]*\)\*\*.*/\1/p')"
    if [[ -n "$title" ]]; then
      grep -qxF -- "$title" "$base_titles" && continue
      key="$title"
    else
      grep -qxF -- "$text" "$base" && continue
      key="$text"
    fi
    section="$(section_at "$head" "$line_no")"
    if [[ "$section" != "$unreleased_literal" ]]; then
      echo "✗ ${head}:${line_no} new entry placed under '${section:-no section}' instead of ${unreleased_literal}: ${key}"
      err=1
    fi
  done < <(grep -n '^- ' "$head" || true)

  rm -f "$base_titles"
  return "$err"
}

touched_sections() {
  local base="$1" head="$2" added line_no section
  added="$(LC_ALL=C comm -13 <(LC_ALL=C sort -u "$base") <(LC_ALL=C sort -u "$head") | grep -E '^\S' || true)"
  while IFS= read -r text; do
    [[ -z "$text" ]] && continue
    line_no="$(grep -nF -m1 -- "$text" "$head" | cut -d: -f1 || true)"
    [[ -z "$line_no" ]] && continue
    section="$(section_at "$head" "$line_no")"
    [[ -n "$section" ]] && printf '%s\n' "$section"
  done <<< "$added" | LC_ALL=C sort -u
}

# ---------------------------------------------------------------- Self-test
self_test() {
  local tmp failures=0
  tmp="$(mktemp -d)"
  trap 'rm -rf "${tmp:-}"' EXIT

  cat > "$tmp/base.md" <<'MD'
## [Unreleased]

## [9.9.0] - 2026-01-01

### Changed
- **Existing entry**: Content in an older release.
MD

  sed 's/^## \[Unreleased\]$/## [Unreleased]\n\n### Changed\n- **New entry**: Placed under Unreleased./' \
    "$tmp/base.md" > "$tmp/ok.md"

  cat > "$tmp/misfiled.md" <<'MD'
## [Unreleased]

## [9.9.0] - 2026-01-01

### Changed
- **Existing entry**: Content in an older release.
- **New entry**: Misfiled under a released section.
MD

  cat > "$tmp/dup-section.md" <<'MD'
## [Unreleased]

## [9.9.0] - 2026-01-01

### Changed
- **Existing entry**: Content in an older release.

### Changed
- **Another entry**: Duplicate subsection heading.
MD

  cat > "$tmp/no-unreleased.md" <<'MD'
## [9.9.0] - 2026-01-01

### Changed
- **Entry**: Missing Unreleased section.
MD

  expect() {
    local want="$1" name="$2" fn="$3"; shift 3
    local got=pass
    if ! "$fn" "$@" >/dev/null 2>&1; then got=fail; fi
    if [[ "$got" == "$want" ]]; then
      echo "  ✓ ${name} (${got})"
    else
      echo "  ✗ ${name}: expected ${want}, got ${got}"
      failures=$((failures + 1))
    fi
  }

  echo "check_changelog_placement self-test:"
  expect pass "global invariant: exactly one Unreleased" check_unreleased_only "$tmp/base.md"
  expect fail "global invariant: missing Unreleased" check_unreleased_only "$tmp/no-unreleased.md"
  expect pass "valid structure" check_structure "$tmp/base.md"
  expect fail "missing Unreleased section" check_structure "$tmp/no-unreleased.md"
  expect fail "duplicate subsection heading in same release" check_structure "$tmp/dup-section.md"
  expect pass "new bold entry under Unreleased" check_placement "$tmp/base.md" "$tmp/ok.md"
  expect fail "new bold entry under released section" check_placement "$tmp/base.md" "$tmp/misfiled.md"
  expect pass "unchanged entry not flagged" check_placement "$tmp/base.md" "$tmp/base.md"

  cat > "$tmp/plain-ok.md" <<'MD'
## [Unreleased]

### Changed
- Plain unbolded entry under Unreleased.

## [9.9.0] - 2026-01-01

### Changed
- **Existing entry**: Content in an older release.
MD

  cat > "$tmp/plain-misfiled.md" <<'MD'
## [Unreleased]

## [9.9.0] - 2026-01-01

### Changed
- **Existing entry**: Content in an older release.
- Plain unbolded entry misfiled under released section.
MD

  expect pass "plain unbolded entry under Unreleased" check_placement "$tmp/base.md" "$tmp/plain-ok.md"
  expect fail "plain unbolded entry under released section" check_placement "$tmp/base.md" "$tmp/plain-misfiled.md"

  cat > "$tmp/release-base.md" <<'MD'
## [Unreleased]

### Changed
- **Pending release entry**: Body text.
- Plain unbolded pending release entry.
MD

  cat > "$tmp/release-head.md" <<'MD'
## [Unreleased]

## [9.9.0] - 2026-02-02

### Changed
- **Pending release entry**: Body text.
- Plain unbolded pending release entry.
MD

  expect pass "release promotion (Unreleased -> released section with identical text)" check_placement "$tmp/release-base.md" "$tmp/release-head.md"
  expect pass "out-of-scope duplicate ignored" check_structure "$tmp/ok.md" "## [9.9.0] - 2026-01-01"
  expect fail "in-scope duplicate caught" check_structure "$tmp/dup-section.md" "## [9.9.0] - 2026-01-01"

  if [[ "$(grep -cE "$unreleased_re" "$changelog" || true)" == "1" ]]; then
    echo "  ✓ real ${changelog} has exactly one Unreleased section"
  else
    echo "  ✗ real ${changelog} Unreleased section count is invalid"
    failures=$((failures + 1))
  fi

  if [[ "$failures" -eq 0 ]]; then
    echo "✓ check_changelog_placement: all self-tests passed"
    return 0
  fi
  echo "✗ check_changelog_placement: ${failures} self-test(s) failed"
  return 1
}

resolve_base_file() {
  local ref="$1" out="$2" sha
  : > "$out"

  if sha="$(git rev-parse --verify --quiet "${ref}^{commit}" 2>/dev/null)"; then
    git show "${sha}:${changelog}" > "$out" 2>/dev/null || true
    if [[ -s "$out" ]]; then echo "  (base source: local commit ${sha:0:10})"; return 0; fi
  fi

  if git fetch --no-tags --depth=1 -q origin "$ref" 2>/dev/null; then
    git show "FETCH_HEAD:${changelog}" > "$out" 2>/dev/null || true
    if [[ -s "$out" ]]; then echo "  (base source: git fetch ${ref})"; return 0; fi
  fi

  : > "$out"
  return 1
}

run_with_base() {
  local base_ref="$1"
  command -v git >/dev/null 2>&1 || die "git not found"

  if [[ -z "$base_ref" || "$base_ref" =~ ^0+$ ]]; then
    echo "⚠ no base ref available (new branch or force push); checking global invariant only"
    check_unreleased_only "$changelog"
    return $?
  fi

  local base_file
  base_file="$(mktemp)"
  if ! resolve_base_file "$base_ref" "$base_file"; then
    echo "⚠ could not resolve ${changelog} at base ${base_ref}; checking global invariant only"
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
    check_unreleased_only "$changelog" || status=1
  fi
  check_placement "$base_file" "$changelog" || status=1
  rm -f "$base_file"

  if [[ "$status" -eq 0 ]]; then
    echo "✓ changelog placement & structure check passed (base ${base_ref}; touched sections: ${sections[*]:-none})"
  fi
  return "$status"
}

case "${1:-}" in
  --self-test) self_test ;;
  --base) run_with_base "${2:-${CI_LOCAL_BASE:-origin/main}}" ;;
  --base=*) run_with_base "${1#--base=}" ;;
  "") run_with_base "${CI_LOCAL_BASE:-origin/main}" ;;
  *) die "unknown argument: $1 (see header comment for usage)" ;;
esac
