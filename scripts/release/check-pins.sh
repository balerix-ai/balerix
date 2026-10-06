#!/usr/bin/env bash
# The charts' release-time check (Spec O §24.5): every version the charts
# name (appVersion, each plugin's image tag) is tagged already, or is the
# version a unit in this same run releases. The charts job waits for
# merge-images, so such a unit's image tags exist by the time it installs
# them. prepare.sh refuses to propose charts that name an untagged version,
# but a mixed run (a core or plugin release PR merged with the charts PR, or
# queued behind it) can still move a pin before its tag exists.
#
# usage: check-pins.sh <unit>...     the units this run releases
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

running=" $* "
pins=$(charts_pins)
missing=()
while read -r unit version field; do
  tag=$(unit_tag "$unit" "$version")
  if tag_exists "$tag"; then
    echo "charts: $field $version is released ($tag)" >&2
  elif [[ $running == *" $unit "* && $(unit_version "$unit") == "$version" ]]; then
    echo "charts: $field $version is released by this run" >&2
  else
    missing+=("$tag ($field)")
  fi
done <<<"$pins"
((${#missing[@]} == 0)) ||
  die "charts: they name versions neither tagged nor released by this run: ${missing[*]}; their images do not exist"
