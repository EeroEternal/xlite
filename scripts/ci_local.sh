#!/usr/bin/env bash
# Local gates for xlite. Equivalent to pre-push.
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

FULL=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --full) FULL=1; shift ;;
    --help|-h) sed -n '2,3p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

say() { printf '\n\033[1;36m== %s ==\033[0m\n' "$*"; }
ok() { printf '\033[32m✓ %s\033[0m\n' "$*"; }

say "changelog placement"
bash scripts/check_changelog_placement.sh

say "rust fmt"
cargo fmt -p xlite -p xlite-h2-pool --check
ok "fmt"

say "rust clippy"
cargo clippy -p xlite --lib --tests -- -D warnings
cargo clippy -p xlite-h2-pool --lib --tests -- -D warnings
ok "clippy"

say "rust tests"
cargo test --lib
cargo test -p xlite-h2-pool --lib
ok "cargo test --lib"

if [[ "$FULL" == "1" ]]; then
  cargo test -p xlite-h2-pool
fi

say "admin"
(
  cd admin
  npx tsc -b --noEmit
  npm run lint
  npx vitest run
  npm run build
)
ok "admin"

ok "ci_local done"
