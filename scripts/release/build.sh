#!/usr/bin/env bash
# Builds one release unit's static binary for one musl target, smoke-tests it
# on this machine and archives it (Spec I §5.2). Run it on a host of the
# target's architecture: the smoke test executes the binary.
#
# usage: build.sh <unit> <target> <out-dir>
# Writes <out-dir>/<binary> for each binary the unit ships (core: balerix,
# balerix-agent, balerix-operator; a plugin: its one), the bare binaries for
# images, and <out-dir>/<crate>-v<version>-<target>.tar.gz; prints the archive path.
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
  bins=("${CARGO_TARGET_DIR:-target}/$target/release/balerix")
  # The operator and the agent release with core (Spec O §24.4), each from
  # its own project and target directory, as scripts/operator.sh builds them.
  for project in "${CORE_PROJECTS[@]}"; do
    CARGO_TARGET_DIR="$project/target" cargo build --release --locked --target "$target" \
      --manifest-path "$project/Cargo.toml"
    bins+=("$project/target/$target/release/balerix-$project")
  done
else
  target_dir=$(scripts/plugin.sh target-dir "$unit")
  CARGO_TARGET_DIR="$target_dir" cargo build --release --locked --target "$target" \
    --manifest-path "plugins/$unit/Cargo.toml"
  bins=("$target_dir/$target/release/$crate")
fi

for bin in "${bins[@]}"; do
  # Protocol §7: static builds, so no host libc can mismatch.
  linkage=$(ldd "$bin" 2>&1 || true)
  grep -Eq 'not a dynamic executable|statically linked' <<<"$linkage" ||
    die "$bin is not statically linked: $linkage"
done

if [[ $unit == core ]]; then
  for bin in "${bins[@]}"; do
    name=$(basename "$bin")
    got=$("$bin" --version)
    [[ $got == "$name $version" ]] || die "$name --version printed '$got', expected '$name $version'"
  done
else
  # Without the daemon's environment a plugin fails in Env::from_process and
  # exits 1 with "<name>: …", which proves it starts on this architecture.
  set +e
  err=$(env -i "${bins[0]}" 2>&1 >/dev/null)
  code=$?
  set -e
  [[ $code -eq 1 ]] || die "$crate exited $code without a daemon environment, expected 1: $err"
  grep -q "^$unit: " <<<"$err" || die "$crate did not report '$unit: …': $err"
fi

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
files=()
mkdir -p "$out"
for bin in "${bins[@]}"; do
  name=$(basename "$bin")
  cp "$bin" "$stage/$name"
  cp "$bin" "$out/$name"
  files+=("$name")
done
cp LICENSE "$stage/LICENSE"
files+=(LICENSE)
changelog=$(unit_changelog "$unit")
if [[ -f $changelog ]]; then
  cp "$changelog" "$stage/CHANGELOG.md"
  files+=(CHANGELOG.md)
fi

archive="$out/$crate-v$version-$target.tar.gz"
tar -czf "$archive" -C "$stage" "${files[@]}"
echo "$archive"
