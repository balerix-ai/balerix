#!/usr/bin/env bash
# Copies both charts to <out-dir> as they will be published (Spec O §24.5).
# On a fork (GITHUB_REPOSITORY_OWNER other than balerix-ai) every image
# repository in the values becomes the fork's, so a fork's charts install
# its own images. The operator names the daemon and agent images itself
# (ghcr.io/balerix-ai/… at its own version) when `images` is empty, so a
# fork's operator chart sets both to the fork's at the charts' appVersion.
#
# usage: stage-charts.sh <out-dir>
# Prints the staged chart directories, one per line.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 ]] || die "usage: $0 <out-dir>"
out=$1
owner=${GITHUB_REPOSITORY_OWNER:-balerix-ai}
owner=${owner,,}

rm -rf "$out"
mkdir -p "$out"
for chart in "${CHARTS[@]}"; do cp -R "charts/$chart" "$out/$chart"; done

if [[ $owner != balerix-ai ]]; then
  app=$(charts_field appVersion)
  for chart in "${CHARTS[@]}"; do
    sed -i "s|ghcr\.io/balerix-ai/|ghcr.io/$owner/|g" "$out/$chart/values.yaml"
  done
  values="$out/balerix-operator/values.yaml"
  sed -i \
    -e "s|^  daemon: \"\"\$|  daemon: \"ghcr.io/$owner/balerix:$app\"|" \
    -e "s|^  agent: \"\"\$|  agent: \"ghcr.io/$owner/balerix-agent:$app\"|" \
    "$values"
  grep -qx "  daemon: \"ghcr.io/$owner/balerix:$app\"" "$values" || die "charts: could not set images.daemon in $values"
  grep -qx "  agent: \"ghcr.io/$owner/balerix-agent:$app\"" "$values" || die "charts: could not set images.agent in $values"
fi

for chart in "${CHARTS[@]}"; do echo "$out/$chart"; done
