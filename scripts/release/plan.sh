#!/usr/bin/env bash
# Which release units have a manifest version with no tag yet (Spec I §5.1).
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
  tag=$(unit_tag "$unit" "$(unit_version "$unit")")
  if tag_exists "$tag"; then
    echo "$unit: $tag exists" >&2
    continue
  fi
  echo "$unit: releasing $tag" >&2
  units+=("$unit")
  if [[ $unit == core ]]; then core=true; else plugins+=("$unit"); fi
done

echo "units=$(json "${units[@]}")"
echo "plugins=$(json "${plugins[@]}")"
echo "core=$core"
