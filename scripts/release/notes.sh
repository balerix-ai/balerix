#!/usr/bin/env bash
# Prints one released version's section from the unit's CHANGELOG.md, without
# its heading (Spec I §5.6: the GitHub Release notes).
#
# usage: notes.sh <unit> <version>
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 2 ]] || die "usage: $0 <unit> <version>"
unit=$1
version=$2
changelog=$(unit_changelog "$unit")
[[ -f $changelog ]] || die "$unit: no $changelog"

section=$(awk -v heading="## $version - " '
  index($0, heading) == 1 { found = 1; next }
  found && /^## / { exit }
  found { print }
' "$changelog")
[[ -n ${section//[[:space:]]/} ]] || die "$unit: $changelog has no section for $version"
# Trim the blank lines around the section.
printf '%s\n' "$section" | sed -e '/./,$!d' | tac | sed -e '/./,$!d' | tac
