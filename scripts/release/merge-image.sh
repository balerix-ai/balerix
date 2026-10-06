#!/usr/bin/env bash
# Combines one image's two per-architecture digests into a multi-arch
# index tagged <version> (Spec I §6.1). Moving tags come later
# (promote-image.sh).
#
# usage: merge-image.sh <image> <digests-dir>
#   <digests-dir> holds one empty file per pushed digest of that image,
#   named by its hex.
# Prints GitHub step outputs: image=, digest=.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 2 ]] || die "usage: $0 <image> <digests-dir>"
name=$1
digests=$2
image=$(image_ref "$name")
version=$(unit_version "$(image_unit "$name")")

refs=()
for file in "$digests"/*; do
  refs+=("$image@sha256:$(basename "$file")")
done
((${#refs[@]} == 2)) || die "$name: expected 2 per-architecture digests, found ${#refs[@]}"

docker buildx imagetools create --tag "$image:$version" "${refs[@]}" >&2
digest=$(docker buildx imagetools inspect "$image:$version" --format '{{json .Manifest}}' | jq -r .digest)
[[ $digest == sha256:* ]] || die "$name: could not read the index digest of $image:$version"

echo "image=$image"
echo "digest=$digest"
