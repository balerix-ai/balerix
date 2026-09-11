#!/usr/bin/env bash
# Points <major>.<minor> and latest at one unit's released <version> index
# (Spec I §5.8, I-9). A release flagged as a prerelease (verify-package.sh
# failed) keeps its tags where they are.
#
# usage: promote-image.sh <unit>
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 ]] || die "usage: $0 <unit>"
unit=$1
image=$(unit_image "$unit")
version=$(unit_version "$unit")
tag=$(unit_tag "$unit" "$version")

prerelease=$(gh release view "$tag" --json isPrerelease --jq .isPrerelease)
if [[ $prerelease != false ]]; then
  echo "::warning::$tag is flagged as a prerelease; $image keeps its moving tags"
  exit 0
fi

docker buildx imagetools create \
  --tag "$image:${version%.*}" \
  --tag "$image:latest" \
  "$image:$version"
