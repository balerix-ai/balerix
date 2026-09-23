#!/usr/bin/env bash
# Which image-bearing release units a change can break the image of, for
# images.yml's pull-request tier (Spec I §6.5): a unit whose include paths
# (lib.sh `unit_paths`) hold a changed file, and every unit when the change
# touches what all the images are built from — the Dockerfiles, the tool
# pins, the scan configuration, these scripts, the actions and the workflow
# itself. Without a range (the nightly, a dispatch) every unit.
#
# usage: affected-units.sh [<base> <head>]
# Prints a GitHub step output: units= (JSON array, in IMAGE_UNITS order).
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 0 || $# -eq 2 ]] || die "usage: $0 [<base> <head>]"

if (($# == 0)); then
  echo "units=$(json_list "${IMAGE_UNITS[@]}")"
  exit 0
fi

# What every unit's image is built from: images.yml's own trigger list.
shared_paths=(
  'docker/**'
  mise.toml
  .hadolint.yaml
  .trivyignore.yaml
  'scripts/release/**'
  '.github/actions/**'
  .github/workflows/images.yml
)

# Whether <file> matches <pattern>: a literal path, or `<dir>/**` for the
# tree under it (the two shapes `unit_paths` uses).
matches() {
  local file=$1 pattern=$2
  if [[ $pattern == */'**' ]]; then
    [[ $file == "${pattern%'**'}"* ]]
  else
    [[ $file == "$pattern" ]]
  fi
}

# The first changed file <pattern> matches, if any.
first_match() {
  local pattern=$1 file
  for file in "${changed[@]}"; do
    if matches "$file" "$pattern"; then
      echo "$file"
      return 0
    fi
  done
  return 1
}

mapfile -t changed < <(git diff --name-only "$1" "$2")

affected=()
for unit in "${IMAGE_UNITS[@]}"; do
  while IFS= read -r pattern; do
    if hit=$(first_match "$pattern"); then
      echo "$unit: $hit" >&2
      affected+=("$unit")
      break
    fi
  done < <(unit_paths "$unit" && printf '%s\n' "${shared_paths[@]}")
done

echo "units=$(json_list "${affected[@]}")"
