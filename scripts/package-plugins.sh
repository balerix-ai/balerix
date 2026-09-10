#!/usr/bin/env bash
# Assembles each named plugin as a directory source under
# $CARGO_TARGET_DIR/plugins/ (target/plugins/ by default; plugins spec
# §17.6): the project's package/ files plus the freshly built binary in
# bin/. The e2e locates the built binary and this output directory the
# same way, relative to $CARGO_TARGET_DIR, so both must agree.
#
# Plugins are standalone projects (Spec H), so the binary is built inside
# plugins/<name>/ and copied here; only the output location is shared.
# With no arguments, every in-tree plugin.
set -euo pipefail
cd "$(dirname "$0")/.."

names=("$@")
(( ${#names[@]} )) || names=(flow web matrix)

out_root="${CARGO_TARGET_DIR:-target}"

for name in "${names[@]}"; do
  crate="balerix-plugin-$name"
  scripts/plugin.sh build "$name"
  built="$(scripts/plugin.sh target-dir "$name")/debug/$crate"
  out="$out_root/plugins/$name"
  rm -rf "$out"
  mkdir -p "$out/bin"
  cp "$built" "$out/bin/$crate"
  cp "plugins/$name/package/mise.toml" "plugins/$name/package/balerix-plugin.yaml" "$out/"
  echo "packaged $name -> $out"
done
