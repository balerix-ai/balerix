#!/usr/bin/env bash
# Pushes both chart archives to oci://ghcr.io/<owner>/charts and cosign-signs
# each by digest (Spec O §24.5 step 3). An archive the registry already holds
# pushes again as a no-op there; it is signed again (a second signature on a
# re-run is harmless). helm and cosign must be logged in to ghcr.io.
#
# usage: push-charts.sh <archives-dir>
# Prints <chart>=<repository>@<digest> lines.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 ]] || die "usage: $0 <archives-dir>"
archives=$1
version=$(unit_version charts)
owner=${GITHUB_REPOSITORY_OWNER:-balerix-ai}
registry="ghcr.io/${owner,,}/charts"

for chart in "${CHARTS[@]}"; do
  archive="$archives/$chart-$version.tgz"
  [[ -f $archive ]] || die "charts: no $archive"
  out=$(helm push "$archive" "oci://$registry" 2>&1) || die "helm push $archive: $out"
  printf '%s\n' "$out" >&2
  digest=$(sed -n 's/^Digest: \(sha256:[0-9a-f]\{64\}\)$/\1/p' <<<"$out")
  [[ -n $digest ]] || die "$chart: helm push printed no digest: $out"
  cosign sign --yes "$registry/$chart@$digest"
  echo "$chart=$registry/$chart@$digest"
done
