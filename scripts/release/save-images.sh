#!/usr/bin/env bash
# A dry run pushes no images, so the charts job cannot pull the tags of the
# pins this run moves (#150). Saves one unit's smoke-tested images (the
# `<image_ref>:smoke` tags build-image loads) as one gzipped archive, which
# the images job uploads and kind-load-pins.sh loads into kind (#167). One
# archive per unit, so core's balerix-agent carries balerix's layers once.
#
# usage: save-images.sh <unit> <out-dir>     needs docker
# Writes <out-dir>/<unit>.tar.gz; prints its path.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 2 ]] || die "usage: $0 <unit> <out-dir>"
unit=$1
out=$2
refs=()
while read -r image; do
  refs+=("$(image_ref "$image"):smoke")
done < <(unit_images "$unit")
mkdir -p "$out"
archive="$out/$unit.tar.gz"
docker save "${refs[@]}" | gzip -6 >"$archive"
echo "$unit: saved ${refs[*]} ($(du -h "$archive" | cut -f1))" >&2
echo "$archive"
