#!/usr/bin/env bash
# Pulls both charts through the published index, the way a user adds the
# repository, and checks them against the release's SHA256SUMS (Spec O
# §14.3 step 6). GitHub Pages serves an index commit a minute or more after
# the push, so it retries for ten minutes before failing.
#
# usage: verify-charts.sh           verify
#        verify-charts.sh --flag    mark the charts release as a prerelease
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -le 1 ]] || die "usage: $0 [--flag]"
: "${GITHUB_REPOSITORY:?}"
version=$(unit_version charts)
tag=$(unit_tag charts "$version")
if [[ ${1:-} == --flag ]]; then
  flag_prerelease "$tag" "> **Chart verification failed**" "do not install these charts."
  exit 0
fi
[[ $# -eq 0 ]] || die "usage: $0 [--flag]"

owner=${GITHUB_REPOSITORY%%/*}
url="https://${owner,,}.github.io/helm-charts"
work=$(mktemp -d "${RUNNER_TEMP:-/tmp}/verify-charts-XXXXXX")
export HELM_CONFIG_HOME="$work/config" HELM_CACHE_HOME="$work/cache" HELM_DATA_HOME="$work/data"
gh release download "$tag" --repo "$GITHUB_REPOSITORY" --pattern SHA256SUMS --dir "$work"
for chart in "${CHARTS[@]}"; do
  grep -q " $chart-$version\.tgz\$" "$work/SHA256SUMS" || die "charts: $tag's SHA256SUMS lists no $chart-$version.tgz"
done

pull_all() {
  helm repo add --force-update balerix "$url" >/dev/null 2>&1 || return 1
  helm repo update balerix >/dev/null 2>&1 || return 1
  rm -rf "$work/pulled"
  mkdir "$work/pulled"
  local chart
  for chart in "${CHARTS[@]}"; do
    helm pull "balerix/$chart" --version "$version" --destination "$work/pulled" >/dev/null 2>&1 || return 1
  done
}
for attempt in $(seq 20); do
  if pull_all; then break; fi
  ((attempt < 20)) || die "charts: $url never offered $tag's charts"
  echo "charts: $url does not offer $version yet; retrying in 30 s" >&2
  sleep 30
done
(cd "$work/pulled" && sha256sum --check --ignore-missing ../SHA256SUMS)
echo "charts: $url serves $tag's archives as released" >&2
