#!/usr/bin/env bash
# The charts' release-time check (Spec O §24.5): every version the charts
# name (appVersion, each plugin's image tag) is tagged already, or is the
# version a unit in this same run releases. The charts job waits for
# merge-images, so such a unit's image tags exist by the time it installs
# them. prepare.sh refuses to propose charts that name an untagged version,
# but a mixed run (a core or plugin release PR merged with the charts PR, or
# queued behind it) can still move a pin before its tag exists.
#
# usage: check-pins.sh [--dry-run] <unit>...     the units this run releases
#
# With --dry-run, prints the pins this run moves on stdout, one
# `<tag> (<field>)` line each: a dry run pushes no images, so those tags do
# not exist and the charts cannot be installed with them (#150).
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

dry_run=
if [[ ${1:-} == --dry-run ]]; then
  dry_run=1
  shift
fi
running=" $* "
pins=$(charts_pins)
missing=()
while read -r unit version field; do
  tag=$(unit_tag "$unit" "$version")
  if tag_exists "$tag"; then
    echo "charts: $field $version is released ($tag)" >&2
  elif [[ $running == *" $unit "* && $(unit_version "$unit") == "$version" ]]; then
    echo "charts: $field $version is released by this run" >&2
    [[ -z $dry_run ]] || echo "$tag ($field)"
  else
    missing+=("$tag ($field)")
  fi
done <<<"$pins"
((${#missing[@]} == 0)) ||
  die "charts: they name versions neither tagged nor released by this run: ${missing[*]}; their images do not exist"
