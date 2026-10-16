#!/usr/bin/env bash
# Cargo.toml version must match the latest released changelog heading.
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
if [[ -z "$version" ]]; then
  echo "✗ Failed to parse version from Cargo.toml"
  exit 1
fi

changelog_ver="$(grep -E '^## \[[0-9]+\.[0-9]+\.[0-9]+\]' docs/changelog.md | head -1 | sed -E 's/^## \[([0-9.]+)\].*/\1/')"
if [[ "$changelog_ver" != "$version" ]]; then
  echo "✗ Latest released version in docs/changelog.md (${changelog_ver:-?}) != Cargo.toml (${version})"
  exit 1
fi

echo "✓ Release version sync: Cargo.toml and docs/changelog.md both match ${version}"
