#!/usr/bin/env bash
# Packages both charts as stage-charts.sh stages them (Spec O §24.5 step 2):
# <out-dir>/<chart>-<version>.tgz each. The shared github-release job writes
# SHA256SUMS over them and attests them.
#
# usage: package-charts.sh <out-dir>     needs helm on PATH
# Prints the archive paths, one per line.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 ]] || die "usage: $0 <out-dir>"
out=$1
version=$(unit_version charts)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mapfile -t dirs < <(scripts/release/stage-charts.sh "$stage/charts")
mkdir -p "$out"
for dir in "${dirs[@]}"; do
  helm lint --strict "$dir" >&2
  helm package "$dir" --destination "$out" >&2
  archive="$out/$(basename "$dir")-$version.tgz"
  [[ -f $archive ]] || die "charts: helm package wrote no $archive"
  echo "$archive"
done
