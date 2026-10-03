#!/usr/bin/env bash
# Build, format, lint and test the standalone agent project (Spec O §12),
# the way scripts/plugin.sh does a plugin: its own cargo workspace, its own
# lockfile, its own target directory. The sidecar's two-process tests need
# the `balerix` binary (launch.sh runs `balerix agent-supervise` and
# `balerix hook-relay`; `balerix serve --mode kubernetes` is the Daemon
# under test), so `check` builds it from the core workspace first and
# hands its path over as BALERIX_BIN; without it those tests skip (fail
# under BALERIX_REQUIRE_TOOLS=1).
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"

usage() { echo "usage: $0 {build|fmt|check}" >&2; exit 2; }
[[ $# -eq 1 ]] || usage
dir="$repo/agent"
target="$dir/target"

case "$1" in
  build)
    CARGO_TARGET_DIR="$target" cargo build -q --manifest-path "$dir/Cargo.toml"
    ;;
  fmt)
    CARGO_TARGET_DIR="$target" cargo fmt --manifest-path "$dir/Cargo.toml" --all
    ;;
  check)
    (cd "$repo" && cargo build -q -p balerix)
    export BALERIX_BIN="${CARGO_TARGET_DIR:-$repo/target}/debug/balerix"
    CARGO_TARGET_DIR="$target" cargo fmt --manifest-path "$dir/Cargo.toml" --all --check
    CARGO_TARGET_DIR="$target" cargo clippy --manifest-path "$dir/Cargo.toml" --all-targets -- -D warnings
    CARGO_TARGET_DIR="$target" cargo nextest run \
      --config-file "$repo/.config/nextest.toml" \
      --manifest-path "$dir/Cargo.toml"
    ;;
  *)
    usage
    ;;
esac
