#!/usr/bin/env bash
# Which release units have a merged release PR awaiting its tag (Spec I §5.1):
# a manifest version with no tag and a changelog section for that version.
# Prints GitHub step outputs: units= and plugins= (JSON arrays), core=true|false.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

json() {
  if (($#)); then printf '%s\n' "$@" | jq -R . | jq -cs .; else echo '[]'; fi
}

units=()
plugins=()
core=false
for unit in "${UNITS[@]}"; do
  version=$(unit_version "$unit")
  tag=$(unit_tag "$unit" "$version")
  if tag_exists "$tag"; then
    echo "$unit: $tag exists" >&2
    continue
  fi
  # No section: no release PR for this version was merged (before a unit's
  # first release PR, say), so nobody proposed releasing it.
  if ! has_section "$unit" "$version"; then
    echo "$unit: $version not proposed yet" >&2
    continue
  fi
  echo "$unit: releasing $tag" >&2
  units+=("$unit")
  if [[ $unit == core ]]; then core=true; else plugins+=("$unit"); fi
done

echo "units=$(json "${units[@]}")"
echo "plugins=$(json "${plugins[@]}")"
echo "core=$core"
