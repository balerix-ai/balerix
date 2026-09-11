#!/usr/bin/env bash
# Combines one unit's two per-architecture image digests into a multi-arch
# index tagged <version> (Spec I §6.1). Moving tags come later
# (promote-image.sh).
#
# usage: merge-image.sh <unit> <digests-dir>
#   <digests-dir> holds one empty file per pushed digest, named by its hex.
# Prints GitHub step outputs: image=, digest=.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 2 ]] || die "usage: $0 <unit> <digests-dir>"
unit=$1
digests=$2
image=$(unit_image "$unit")
version=$(unit_version "$unit")

refs=()
for file in "$digests"/*; do
  refs+=("$image@sha256:$(basename "$file")")
done
((${#refs[@]} == 2)) || die "$unit: expected 2 per-architecture digests, found ${#refs[@]}"

docker buildx imagetools create --tag "$image:$version" "${refs[@]}" >&2
digest=$(docker buildx imagetools inspect "$image:$version" --format '{{json .Manifest}}' | jq -r .digest)
[[ $digest == sha256:* ]] || die "$unit: could not read the index digest of $image:$version"

echo "image=$image"
echo "digest=$digest"
