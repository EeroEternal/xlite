#!/usr/bin/env bash
# Issue #96 acceptance: `xlite-transport` with `--no-default-features` must not
# pull a second control plane (no sqlx / axum / redis / serde_yaml, and no
# dependency on the `xlite` binary crate at all). This is a product rule
# ("no second control plane"), so it is a gate, not a preference.
set -euo pipefail
cd "$(dirname "$0")/.."

tree="$(cargo tree -p xlite-transport --no-default-features -e normal --prefix none | sort -u)"

forbidden='^(sqlx|axum|redis|serde_yaml|xlite) '
if echo "$tree" | grep -qE "$forbidden"; then
  echo 'xlite-transport --no-default-features pulls a forbidden dependency:' >&2
  echo "$tree" | grep -E "$forbidden" >&2
  exit 1
fi

echo 'xlite-transport --no-default-features: no sqlx/axum/redis/serde_yaml/xlite'
