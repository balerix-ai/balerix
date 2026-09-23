#!/usr/bin/env bash
# Which release units have a merged release PR awaiting its tag (Spec I §5.1):
# a manifest version with no tag and a changelog section for that version.
# Prints GitHub step outputs: units=, plugins=, binaries= and crates=
# (JSON arrays), core=true|false.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

units=()
plugins=()
binaries=()
crates=()
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
  case $(unit_kind "$unit") in
    core) core=true; binaries+=("$unit"); crates+=("$unit") ;;
    plugin) plugins+=("$unit"); binaries+=("$unit") ;;
    library) crates+=("$unit") ;;
  esac
done

echo "units=$(json_list "${units[@]}")"
echo "plugins=$(json_list "${plugins[@]}")"
echo "binaries=$(json_list "${binaries[@]}")"
echo "crates=$(json_list "${crates[@]}")"
echo "core=$core"
