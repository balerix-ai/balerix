#!/usr/bin/env bash
# Creates vendor/hyper (hyper 1.11.1 plus the spike's instrumentation) and
# vendor/want (want 0.3.1 plus `Taker::unwant`), which operator/Cargo.toml
# patches in (balerix#129). With --fix, vendor/hyper also gets the candidate
# fix: the dispatcher withdraws a stale `want` when it takes a request.
# usage: setup.sh [--fix]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
fix=0
[[ ${1:-} == --fix ]] && fix=1
# the stock reproducer depends on exactly these versions: fetch them
cargo fetch -q --manifest-path "$here/repro/Cargo.toml"
reg="$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/index.crates.io-*/ | head -1)"
rm -rf "$here/vendor"
mkdir -p "$here/vendor"
cp -r "$reg/hyper-1.11.1" "$here/vendor/hyper"
cp -r "$reg/want-0.3.1" "$here/vendor/want"
patch -s -p1 -d "$here/vendor/hyper" <"$here/patches/hyper-1.11.1-instrumentation.patch"
patch -s -p1 -d "$here/vendor/want" <"$here/patches/want-0.3.1-unwant.patch"
if [[ $fix == 1 ]]; then
  patch -s -p1 -d "$here/vendor/hyper" <"$here/patches/hyper-1.11.1-unwant.patch"
fi
echo "vendor/ ready (fix=$fix)"
