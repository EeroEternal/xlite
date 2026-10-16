#!/usr/bin/env bash
# Materializes local offline build scaffolding (`.cargo/` + `vendor/`).
#
# Both directories are gitignored so fresh clones build online by default, while
# `Dockerfile` and `cargo build --offline` automatically switch to offline vendored
# mode whenever `.cargo/config.toml` and `vendor/` exist.
#
# Requires network access (crates.io) once to vendor dependencies. Afterward, both
# commands below run completely offline:
#   cargo build --release --bin xlite --offline
#   docker build -t xlite:0.5.6 .
#
# Usage:
#   scripts/prepare_offline_vendor.sh            # generate / refresh vendor scaffolding
#   scripts/prepare_offline_vendor.sh --check    # verify offline readiness (no network; exits 2 on failure)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

CHECK=0
case "${1:-}" in
  --check) CHECK=1 ;;
  "" ) ;;
  -h|--help)
    sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0 ;;
  *) echo "unknown arg: $1 (want --check)" >&2; exit 2 ;;
esac

say() { printf '\n\033[1;36m== %s ==\033[0m\n' "$*"; }
ok()  { printf '\033[32m✓ %s\033[0m\n' "$*"; }
bad() { printf '\033[31m✗ %s\033[0m\n' "$*" >&2; }
warn(){ printf '\033[33m! %s\033[0m\n' "$*"; }

verify_files() {
  local rc=0
  if [[ ! -f .cargo/config.toml ]]; then
    bad "missing .cargo/config.toml — Dockerfile will fall back to online crates.io build"
    rc=1
  elif ! grep -q 'replace-with *= *"vendored-sources"' .cargo/config.toml; then
    bad ".cargo/config.toml exists but lacks replace-with = \"vendored-sources\""
    rc=1
  elif ! grep -q 'directory *= *"vendor"' .cargo/config.toml; then
    bad ".cargo/config.toml [source.vendored-sources] lacks directory = \"vendor\""
    rc=1
  fi
  if [[ ! -d vendor ]]; then
    bad "missing vendor/ directory — --offline and Dockerfile offline branch are unavailable"
    rc=1
  elif [[ -z "$(ls -A vendor)" ]]; then
    bad "vendor/ directory is empty"
    rc=1
  fi
  return "$rc"
}

verify_resolves() {
  if ! command -v cargo >/dev/null 2>&1; then
    warn "cargo not found on PATH — skipping `cargo metadata --offline --locked` resolution check"
    return 2
  fi
  if cargo metadata --offline --locked --format-version 1 >/dev/null 2>&1; then
    return 0
  fi
  bad "cargo metadata --offline --locked failed — vendor/ is out of sync with Cargo.lock"
  echo "    inspect with: cargo metadata --offline --locked" >&2
  return 1
}

if (( CHECK )); then
  say "Verifying offline vendor scaffolding"
  rc=0
  verify_files || rc=1
  set +e; verify_resolves; r=$?; set -e
  (( r == 1 )) && rc=1
  if (( rc != 0 )); then
    echo >&2
    bad "Offline check failed. Run scripts/prepare_offline_vendor.sh on a machine with network access first."
    exit 2
  fi
  ok "Offline scaffolding ready: .cargo/config.toml + vendor/ ($(du -sh vendor | cut -f1))"
  exit 0
fi

command -v cargo >/dev/null 2>&1 || {
  bad "cargo not found. Install the Rust toolchain via rustup/mise first."
  exit 1
}

say "Generating offline vendor scaffolding from Cargo.lock"
mkdir -p .cargo

if [[ -f .cargo/config.toml ]] && ! grep -q 'vendored-sources' .cargo/config.toml; then
  cp .cargo/config.toml .cargo/config.toml.bak
  warn "Existing non-vendored .cargo/config.toml backed up to .cargo/config.toml.bak"
fi

tmp="$repo_root/.cargo/config.toml.tmp"
trap 'rm -f "$tmp"' EXIT
cargo vendor --locked >"$tmp"
mv "$tmp" .cargo/config.toml
ok "vendor/ and .cargo/config.toml generated"

say "Self-check: resolving Cargo.lock offline against vendor/"
set +e; verify_resolves; r=$?; set -e
if (( r == 1 )); then
  bad "Newly generated vendor scaffolding failed offline resolution check."
  exit 1
fi
ok "Offline resolution verified"
echo
ok "Done. Both commands below now run without network access:"
echo "    cargo build --release --bin xlite --offline"
echo "    docker build -t xlite:0.5.6 ."
echo "  Note: .cargo/ and vendor/ remain gitignored ($(du -sh vendor | cut -f1)); do not commit them."
