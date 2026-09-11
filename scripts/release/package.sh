#!/usr/bin/env bash
# Assembles one plugin's release package (Spec I §7.2): its manifest plus a
# mise.toml that pins both architectures' archives by checksum, tarred by
# the daemon's own `balerix plugin package`.
#
# usage: package.sh <plugin> <dist-dir> <out-dir>
#   <dist-dir> holds <crate>-v<version>-<target>.tar.gz for both targets.
# Prints package=<path> and sha256=<digest> on stdout.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 3 ]] || die "usage: $0 <plugin> <dist-dir> <out-dir>"
unit=$1
dist=$2
out=$3
[[ $unit != core ]] || die "core has no plugin package"
require_unit "$unit"
crate=$(unit_crate "$unit")
version=$(unit_version "$unit")
repo=${GITHUB_REPOSITORY:-balerix-ai/balerix}

x64="$dist/$crate-v$version-x86_64-unknown-linux-musl.tar.gz"
arm64="$dist/$crate-v$version-aarch64-unknown-linux-musl.tar.gz"
for archive in "$x64" "$arm64"; do
  [[ -f $archive ]] || die "$unit: missing $archive"
done

manifest="plugins/$unit/package/balerix-plugin.yaml"
manifest_version=$(sed -n 's/^version: //p' "$manifest")
[[ $manifest_version == "$version" ]] ||
  die "$unit: $manifest says $manifest_version, Cargo.toml says $version"

pkg=$(mktemp -d)
trap 'rm -rf "$pkg"' EXIT
cp "$manifest" "$pkg/balerix-plugin.yaml"
sed -e "s|@REPO@|$repo|g" \
  -e "s|@CRATE@|$crate|g" \
  -e "s|@VERSION@|$version|g" \
  -e "s|@SHA256_X64@|$(sha256sum "$x64" | cut -d' ' -f1)|g" \
  -e "s|@SHA256_ARM64@|$(sha256sum "$arm64" | cut -d' ' -f1)|g" \
  scripts/release/plugin-mise.toml.tmpl >"$pkg/mise.toml"
! grep -q '@[A-Z0-9_]*@' "$pkg/mise.toml" || die "$unit: unrendered placeholder in mise.toml"

mkdir -p "$out"
asset="$(cd "$out" && pwd)/$crate-v$version-package.tar.gz"
if [[ -n ${BALERIX_BIN:-} ]]; then
  "$BALERIX_BIN" plugin package "$pkg" --out "$asset" >&2
else
  cargo run -q --locked -p balerix -- plugin package "$pkg" --out "$asset" >&2
fi

echo "package=$asset"
echo "sha256=$(sha256sum "$asset" | cut -d' ' -f1)"
