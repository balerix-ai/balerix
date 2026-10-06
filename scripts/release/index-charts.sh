#!/usr/bin/env bash
# Adds both charts' release to <owner>/helm-charts' index.yaml and pushes it
# (Spec O §14.3 step 5, §24.5). It runs after the GitHub Release, whose
# asset URLs the index names. An index that already lists both archives is
# left alone, so a re-run makes no second commit. Nobody edits it by hand.
#
# usage: index-charts.sh <archives-dir> <helm-charts-checkout>
#   GITHUB_REPOSITORY names this repository; GH_TOKEN can push to helm-charts.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 2 ]] || die "usage: $0 <archives-dir> <helm-charts-checkout>"
archives=$1
pages=$2
: "${GITHUB_REPOSITORY:?}"
version=$(unit_version charts)
tag=$(unit_tag charts "$version")
base="https://github.com/$GITHUB_REPOSITORY/releases/download/$tag"
index="$pages/index.yaml"

listed=0
for chart in "${CHARTS[@]}"; do
  if [[ -f $index ]] && grep -qF "$base/$chart-$version.tgz" "$index"; then listed=$((listed + 1)); fi
done
if ((listed == ${#CHARTS[@]})); then
  echo "charts: $index already lists $tag" >&2
  exit 0
fi

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
for chart in "${CHARTS[@]}"; do cp "$archives/$chart-$version.tgz" "$stage/"; done
merge=()
[[ ! -f $index ]] || merge=(--merge "$index")
helm repo index "$stage" --url "$base" "${merge[@]}"
cp "$stage/index.yaml" "$index"

git -C "$pages" add index.yaml
git -C "$pages" -c user.name="github-actions[bot]" \
  -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
  commit -q -m "chore: index $tag"
: "${GH_TOKEN:?GH_TOKEN must be the release App token}"
git -C "$pages" push -q "https://x-access-token:${GH_TOKEN}@github.com/${GITHUB_REPOSITORY%%/*}/helm-charts.git" HEAD:main
echo "charts: indexed $tag in ${GITHUB_REPOSITORY%%/*}/helm-charts" >&2
