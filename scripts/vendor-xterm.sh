#!/usr/bin/env bash
# Fetches the pinned xterm.js build and installs it into
# plugins/web/assets/ only after every file has verified
# against its VENDOR.md row (plugins spec §18.5). cargo audit and deny.toml
# do not cover JavaScript; the recorded digests are the supply-chain
# control. To bump: change the versions below, run this (it prints the new
# digests and exits 1 without touching the tree), copy them into
# VENDOR.md, run it again. `--check` verifies the committed files against
# VENDOR.md without fetching anything.
set -euo pipefail
cd "$(dirname "$0")/.."

XTERM=6.0.0
FIT=0.11.0
out=plugins/web/assets
files=(xterm.js xterm.css addon-fit.js)

# The digest VENDOR.md records for one file: the last backticked field of
# the table row that starts with that file, so a digest recorded against
# another file cannot pass for this one.
recorded() {
  grep -E "^\| \`$1\` \|" "$out/VENDOR.md" | sed -E 's/.*`([0-9a-f]{64})` \|$/\1/'
}

# Verifies every file in $1 against VENDOR.md; prints the digests either way.
verify() {
  local dir=$1 ok=1 sum file
  echo "files:"
  (cd "$dir" && sha256sum "${files[@]}")
  for file in "${files[@]}"; do
    sum=$(sha256sum "$dir/$file" | cut -d' ' -f1)
    if [ "$(recorded "$file")" != "$sum" ]; then
      echo "VENDOR.md does not record $file $sum" >&2
      ok=0
    fi
  done
  [ "$ok" = 1 ]
}

if [ "${1:-}" = "--check" ]; then
  verify "$out"
  echo "assets match VENDOR.md"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -sSfL -o "$tmp/xterm.tgz" "https://registry.npmjs.org/@xterm/xterm/-/xterm-$XTERM.tgz"
curl -sSfL -o "$tmp/fit.tgz" "https://registry.npmjs.org/@xterm/addon-fit/-/addon-fit-$FIT.tgz"
mkdir -p "$tmp/xterm" "$tmp/fit" "$tmp/stage" "$out"
tar -xzf "$tmp/xterm.tgz" -C "$tmp/xterm" package/lib/xterm.js package/css/xterm.css package/LICENSE
tar -xzf "$tmp/fit.tgz" -C "$tmp/fit" package/lib/addon-fit.js
cp "$tmp/xterm/package/lib/xterm.js" "$tmp/stage/xterm.js"
cp "$tmp/xterm/package/css/xterm.css" "$tmp/stage/xterm.css"
cp "$tmp/fit/package/lib/addon-fit.js" "$tmp/stage/addon-fit.js"

echo "tarballs:"
(cd "$tmp" && sha256sum xterm.tgz fit.tgz)
# Verified in the staging directory: a fetch that does not match leaves the
# tree exactly as it was.
verify "$tmp/stage"
cp "$tmp/stage/"* "$out/"
cp "$tmp/xterm/package/LICENSE" "$out/LICENSE.xterm"
echo "assets match VENDOR.md and were installed"
