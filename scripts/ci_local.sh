#!/usr/bin/env bash
# Local gates for xlite, tiered by risk event (see .agents/skills/ci-local-runner).
#
#   t0 (default)  push, any platform: path-scoped — only the faces your change touches
#   t1            PR -> main (Linux CI): every face + admin build + h2-pool integration
#   t2            release / tag: t1 + release-sync checks
#
# Usage: scripts/ci_local.sh [--tier t0|t1|t2] [--all] [--base <ref>] [--full]
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

TIER=t0
BASE="${CI_LOCAL_BASE:-origin/main}"
FORCE_ALL=0
ONLY=""

usage() {
  cat <<'EOF'
Usage: scripts/ci_local.sh [--tier t0|t1|t2] [--all] [--base <ref>] [--full]

  --tier t0   push, any platform: path-scoped, only the faces you changed (default)
  --tier t1   PR -> main (Linux CI): every face + admin build + h2-pool integration tests
  --tier t2   release / tag: t1 + release-sync checks
  --full      alias for --tier t1
  --all       ignore path scoping; run every face
  --only F    force faces (comma-separated: rust,admin,meta); overrides path scoping.
              CI uses this to run one face per job from this same script.
  --base REF  base ref for path scoping (default: origin/main; env CI_LOCAL_BASE)

Skipping is only allowed when the changed paths are known: if the base ref cannot
be diffed, or nothing is visible against it, every face runs.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tier)    TIER="${2:-}"; shift 2 ;;
    --tier=*)  TIER="${1#*=}"; shift ;;
    --full)    TIER=t1; shift ;;
    --all)     FORCE_ALL=1; shift ;;
    --only)    ONLY="${2:-}"; shift 2 ;;
    --only=*)  ONLY="${1#*=}"; shift ;;
    --base)    BASE="${2:-}"; shift 2 ;;
    --base=*)  BASE="${1#*=}"; shift ;;
    --help|-h) usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$TIER" in
  t0|t1|t2) ;;
  *) echo "unknown tier: $TIER (want t0|t1|t2)" >&2; exit 2 ;;
esac

say()  { printf '\n\033[1;36m== %s ==\033[0m\n' "$*"; }
ok()   { printf '\033[32m✓ %s\033[0m\n' "$*"; }
skip() { printf '\033[2m- skipped: %s\033[0m\n' "$*"; }
warn() { printf '\033[33m! %s\033[0m\n' "$*"; }

# ---- change scope -----------------------------------------------------------
# Absence of evidence is not evidence of absence: a face is only skipped when we
# can actually name the paths that changed.
paths=""
scope_known=0
if [[ "$FORCE_ALL" == "0" ]]; then
  merge="$(git merge-base "$BASE" HEAD 2>/dev/null || true)"
  if [[ -n "$merge" ]]; then
    scope_known=1
    paths="$(
      {
        git diff --name-only "$merge" HEAD
        git diff --name-only HEAD
        git diff --name-only --cached
        git ls-files --others --exclude-standard
      } 2>/dev/null | sort -u
    )"
  fi
fi

want_rust=0 want_admin=0 want_meta=0
if [[ "$FORCE_ALL" == "1" || "$scope_known" == "0" || -z "$paths" ]]; then
  want_rust=1 want_admin=1 want_meta=1
  if [[ "$FORCE_ALL" == "1" ]]; then
    printf 'scope: every face (--all)\n'
  elif [[ -z "$ONLY" ]]; then
    printf 'scope: every face (cannot diff vs %s, or nothing changed against it)\n' "$BASE"
  fi
else
  changed_n=0
  while IFS= read -r p; do
    [[ -z "$p" ]] && continue
    changed_n=$((changed_n + 1))
    case "$p" in
      src/*|crates/*|tests/*|migrations/*|benches/*|build.rs|Cargo.toml|Cargo.lock|rust-toolchain.toml) want_rust=1 ;;
    esac
    case "$p" in
      admin/*) want_admin=1 ;;
    esac
    case "$p" in
      docs/*|*.md|.agents/*|.github/*|scripts/*|.mise.toml) want_meta=1 ;;
    esac
  done <<< "$paths"
  if [[ -z "$ONLY" ]]; then
    printf 'scope: %s changed path(s) vs %s\n' "$changed_n" "$BASE"
    if [[ "$want_rust" == "0" && "$want_admin" == "0" && "$want_meta" == "0" ]]; then
      warn "no face matched the changed paths — running meta. Teach the routing table in this script about those paths."
      want_meta=1
    fi
  fi
fi

# --only overrides path scoping: CI runs one face per job through this same script,
# so the CI commands and the local commands cannot drift apart.
if [[ -n "$ONLY" ]]; then
  want_rust=0 want_admin=0 want_meta=0
  IFS=',' read -r -a faces <<< "$ONLY"
  for f in "${faces[@]}"; do
    case "$f" in
      rust)  want_rust=1 ;;
      admin) want_admin=1 ;;
      meta)  want_meta=1 ;;
      *) echo "unknown face: $f (want rust|admin|meta)" >&2; exit 2 ;;
    esac
  done
  printf 'scope: forced faces (--only %s)\n' "$ONLY"
fi

# ---- meta ------------------------------------------------------------------
if [[ "$want_meta" == "1" ]]; then
  say "meta (changelog / docs lifecycle / shell syntax)"
  bash scripts/check_changelog_placement.sh
  bash scripts/check_docs_lifecycle.sh
  if [[ "$TIER" == "t2" ]]; then
    bash scripts/check_release_sync.sh
  fi
  # Shell syntax of the scripts that actually changed. bash 3.2 on macOS: no declare -A.
  while IFS= read -r p; do
    case "$p" in
      *.sh)
        if [[ -f "$p" ]]; then
          bash -n "$p"
          ok "bash -n $p"
        fi
        ;;
    esac
  done <<< "$paths"
  # Parse changed workflow YAML when a parser exists; a missing parser must not fail the gate.
  if python3 -c 'import yaml' 2>/dev/null; then
    while IFS= read -r p; do
      case "$p" in
        *.yml|*.yaml)
          if [[ -f "$p" ]]; then
            python3 -c 'import sys, yaml; yaml.safe_load(open(sys.argv[1]))' "$p"
            ok "yaml $p"
          fi
          ;;
      esac
    done <<< "$paths"
  fi
  ok "meta"
else
  skip "meta: no docs/** | *.md | .agents/** | .github/** | scripts/** change"
fi

# ---- rust ------------------------------------------------------------------
if [[ "$want_rust" == "1" ]]; then
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
  if [[ "$TIER" != "t0" ]]; then
    cargo test -p xlite-h2-pool
  fi
  ok "cargo test"
else
  skip "rust: no src/** | crates/** | tests/** | migrations/** | Cargo.* change"
fi

# ---- admin -----------------------------------------------------------------
if [[ "$want_admin" == "1" ]]; then
  say "admin"
  (
    cd admin
    npx tsc -b --noEmit
    npm run lint
    npx vitest run
    if [[ "$TIER" != "t0" ]]; then
      npm run build
    fi
  )
  ok "admin"
else
  skip "admin: no admin/** change"
fi

if [[ "$TIER" == "t0" ]]; then
  printf '\ntier t0 is a push gate, not a merge gate: CI runs tier t1 on every PR to main.\n'
fi
ok "ci_local done (tier $TIER)"
