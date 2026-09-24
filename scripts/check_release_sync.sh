#!/usr/bin/env bash
# Cargo.toml version must match the latest released changelog heading.
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
if [[ -z "$version" ]]; then
  echo "✗ 未能从 Cargo.toml 解析到版本号"
  exit 1
fi

changelog_ver="$(grep -E '^## \[[0-9]+\.[0-9]+\.[0-9]+\]' docs/changelog.md | head -1 | sed -E 's/^## \[([0-9.]+)\].*/\1/')"
if [[ "$changelog_ver" != "$version" ]]; then
  echo "✗ docs/changelog.md 最新已发布版本 ${changelog_ver:-?} ≠ Cargo.toml ${version}"
  exit 1
fi

echo "✓ 发版一致性：Cargo.toml 与 changelog 均为 ${version}"
