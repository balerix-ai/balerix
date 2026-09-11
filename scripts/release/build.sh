#!/usr/bin/env bash
# Builds one release unit's static binary for one musl target, smoke-tests it
# on this machine and archives it (Spec I §5.2). Run it on a host of the
# target's architecture: the smoke test executes the binary.
#
# usage: build.sh <unit> <target> <out-dir>
# Writes <out-dir>/<crate> (the bare binary, for images) and
# <out-dir>/<crate>-v<version>-<target>.tar.gz; prints the archive path.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 3 ]] || die "usage: $0 <unit> <target> <out-dir>"
unit=$1
target=$2
out=$3
require_unit "$unit"
case $target in
  x86_64-unknown-linux-musl | aarch64-unknown-linux-musl) ;;
  *) die "unsupported target: $target" ;;
esac
crate=$(unit_crate "$unit")
version=$(unit_version "$unit")

# matrix's aws-lc-sys and bundled SQLite compile C. On an arm64 host cc-rs
# looks for aarch64-linux-musl-gcc, which musl-tools does not ship; musl-gcc
# is the native wrapper on both architectures. Pure-Rust units never call it.
export CC_x86_64_unknown_linux_musl=${CC_x86_64_unknown_linux_musl:-musl-gcc}
export CC_aarch64_unknown_linux_musl=${CC_aarch64_unknown_linux_musl:-musl-gcc}

if [[ $unit == core ]]; then
  cargo build --release --locked --target "$target" -p balerix
  bin="${CARGO_TARGET_DIR:-target}/$target/release/$crate"
else
  target_dir=$(scripts/plugin.sh target-dir "$unit")
  CARGO_TARGET_DIR="$target_dir" cargo build --release --locked --target "$target" \
    --manifest-path "plugins/$unit/Cargo.toml"
  bin="$target_dir/$target/release/$crate"
fi

# Protocol §7: static builds, so no host libc can mismatch.
linkage=$(ldd "$bin" 2>&1 || true)
grep -Eq 'not a dynamic executable|statically linked' <<<"$linkage" ||
  die "$bin is not statically linked: $linkage"

if [[ $unit == core ]]; then
  got=$("$bin" --version)
  [[ $got == "balerix $version" ]] || die "balerix --version printed '$got', expected 'balerix $version'"
else
  # Without the daemon's environment a plugin fails in Env::from_process and
  # exits 1 with "<name>: …", which proves it starts on this architecture.
  set +e
  err=$(env -i "$bin" 2>&1 >/dev/null)
  code=$?
  set -e
  [[ $code -eq 1 ]] || die "$crate exited $code without a daemon environment, expected 1: $err"
  grep -q "^$unit: " <<<"$err" || die "$crate did not report '$unit: …': $err"
fi

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
files=("$crate" LICENSE)
cp "$bin" "$stage/$crate"
cp LICENSE "$stage/LICENSE"
changelog=$(unit_changelog "$unit")
if [[ -f $changelog ]]; then
  cp "$changelog" "$stage/CHANGELOG.md"
  files+=(CHANGELOG.md)
fi

mkdir -p "$out"
cp "$bin" "$out/$crate"
archive="$out/$crate-v$version-$target.tar.gz"
tar -czf "$archive" -C "$stage" "${files[@]}"
echo "$archive"
