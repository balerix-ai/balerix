#!/usr/bin/env bash
# Build, format, lint and test one standalone plugin project (Spec H).
#
# Each plugin is its own cargo workspace with its own lockfile, so every
# invocation runs from that plugin's directory. Its target directory is
# pinned here rather than inherited, because an ambient CARGO_TARGET_DIR
# would otherwise move the binary out from under package-plugins.sh.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"

usage() { echo "usage: $0 {target-dir|build|fmt|check} <name>" >&2; exit 2; }
[[ $# -eq 2 ]] || usage
cmd=$1
name=$2
# A plugin name is a directory under plugins/, not a path: without this,
# `../..` passes the -d test below and cargo runs somewhere unintended.
[[ $name =~ ^[a-z][a-z0-9-]*$ ]] || { echo "not a plugin name: $name" >&2; exit 1; }
dir="$repo/plugins/$name"
[[ -d $dir ]] || { echo "no such plugin: $name" >&2; exit 1; }
target="$dir/target"

case "$cmd" in
  target-dir)
    echo "$target"
    ;;
  build)
    CARGO_TARGET_DIR="$target" cargo build -q --manifest-path "$dir/Cargo.toml"
    ;;
  fmt)
    CARGO_TARGET_DIR="$target" cargo fmt --manifest-path "$dir/Cargo.toml" --all
    ;;
  check)
    CARGO_TARGET_DIR="$target" cargo fmt --manifest-path "$dir/Cargo.toml" --all --check
    CARGO_TARGET_DIR="$target" cargo clippy --manifest-path "$dir/Cargo.toml" --all-targets -- -D warnings
    # nextest resolves .config/nextest.toml from the workspace root, and each
    # plugin is now its own root: without --config-file the plugin tiers would
    # run under stock defaults, with no slow-timeout to kill a hung test.
    CARGO_TARGET_DIR="$target" cargo nextest run \
      --config-file "$repo/.config/nextest.toml" \
      --manifest-path "$dir/Cargo.toml"
    ;;
  *)
    usage
    ;;
esac
