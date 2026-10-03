#!/usr/bin/env bash
# Build, format, lint and test the standalone operator project (Spec O
# §12), the way scripts/agent.sh does the agent: its own cargo workspace,
# lockfile and target directory. The Daemon client's test runs a real
# `balerix serve --mode kubernetes`, so `check` builds `balerix` from the
# core workspace first and hands its path over as BALERIX_BIN; without it
# that test skips (fails under BALERIX_REQUIRE_TOOLS=1).
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"

usage() { echo "usage: $0 {build|fmt|check|crds}" >&2; exit 2; }
[[ $# -eq 1 ]] || usage
dir="$repo/operator"
target="$dir/target"

case "$1" in
  build)
    CARGO_TARGET_DIR="$target" cargo build -q --manifest-path "$dir/Cargo.toml"
    ;;
  fmt)
    CARGO_TARGET_DIR="$target" cargo fmt --manifest-path "$dir/Cargo.toml" --all
    ;;
  crds)
    CARGO_TARGET_DIR="$target" cargo run -q --manifest-path "$dir/Cargo.toml" -- crds --out "$dir/crds"
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
