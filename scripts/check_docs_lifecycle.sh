#!/usr/bin/env bash
# Documentation link & version sync check.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

findings=0

# 1. Relative markdown link check across docs/ and README.md / deploy/
broken="$( set +e
{ find docs deploy -name '*.md' -print0 2>/dev/null; printf 'README.md\0'; } | while IFS= read -r -d '' md; do
  [[ -f "$md" ]] || continue
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

# 2. Mangled markdown links check
mangled="$(grep -rnE '\[[^]]*\((\.\./)?(architecture|dev|design|manual)/' docs --include='*.md' 2>/dev/null || true)"
if [[ -n "$mangled" ]]; then
  printf '%s\n' "$mangled"
  findings=1
fi

# 3. deploy/ image tags and /health version expectations must match Cargo.toml
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
pinned="$(grep -rnE 'xlite:[0-9]+\.[0-9]+\.[0-9]+|"version": *"[0-9]+\.[0-9]+\.[0-9]+"' deploy 2>/dev/null \
          | grep -vE "xlite:${version}([^0-9.]|$)|\"version\": *\"${version}\"" || true)"
if [[ -n "$pinned" ]]; then
  printf '%s\n' "$pinned"
  findings=1
fi

if [[ "$findings" -eq 0 ]]; then
  echo "✓ 文档生命周期与链接检查通过：断链/残缺零发现；deploy/ 版本引用与 Cargo.toml (${version}) 一致"
fi
exit "$findings"
