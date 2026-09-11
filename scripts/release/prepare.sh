#!/usr/bin/env bash
# Prepares one release unit's next release in the working tree (Spec I §4.1):
# works out the version, writes it into the manifests and lockfiles, and
# prepends the changelog section. Nothing is committed.
#
# usage: prepare.sh <unit> [version]    a version forces that exact release,
#                                       but a release already in progress
#                                       (Spec I §8.2) still wins over it
#
# Prints key=value lines on stdout; progress goes to stderr.
#   status=release       files changed; version=, tag= and notes= follow
#   status=none          no releasable commit since the last tag
#   status=in-progress   the manifest version is already awaiting its tag
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 || $# -eq 2 ]] || die "usage: $0 <unit> [version]"
unit=$1
forced=${2:-}
require_unit "$unit"

prefix=$(unit_tag_prefix "$unit")
changelog=$(unit_changelog "$unit")
current=$(unit_version "$unit")
last=$(last_tag "$unit")
notes_dir=${RELEASE_NOTES_DIR:-target/release-notes}
notes="$notes_dir/$unit.md"

emit() { printf '%s=%s\n' "$@"; }

if ! tag_exists "$(unit_tag "$unit" "$current")" && { [[ -n $last ]] || has_section "$unit" "$current"; }; then
  # A release PR was merged and release.yml has not tagged it yet, or failed
  # (Spec I §8.2). Proposing anything now would release twice, so this wins
  # even over a forced version.
  echo "$unit: $current is awaiting its tag; release in progress" >&2
  emit status in-progress version "$current"
  exit 0
elif [[ -n $forced ]]; then
  [[ $forced =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "$unit: not a release version: $forced"
  ! tag_exists "$(unit_tag "$unit" "$forced")" || die "$unit: $forced is already released"
  if [[ -n $last ]]; then
    lastversion=${last#"$prefix"}
    highest=$(printf '%s\n%s\n' "$lastversion" "$forced" | sort -V | tail -n 1)
    [[ $highest == "$forced" && $forced != "$lastversion" ]] ||
      die "$unit: $forced is not above the last release $lastversion"
  fi
  next=$forced
elif [[ -z $last ]]; then
  next=$current
  echo "$unit: no release tag yet; initial release of $next" >&2
else
  count=$(cliff "$unit" --unreleased --context | jq '[.[].commits[]] | length')
  if [[ $count -eq 0 ]]; then
    echo "$unit: nothing to release since $last" >&2
    emit status none
    exit 0
  fi
  next=$(cliff "$unit" --bumped-version)
  next=${next#"$prefix"}
fi

tag=$(unit_tag "$unit" "$next")

if [[ $next != "$current" ]]; then
  if [[ $unit == core ]]; then
    cargo set-version --workspace "$next" >&2
    # Plugins lock the SDK and api versions through their path dependency.
    for plugin in "${PLUGIN_UNITS[@]}"; do
      cargo update --manifest-path "plugins/$plugin/Cargo.toml" -p balerix-api -p balerix-plugin-sdk >&2
    done
  else
    cargo set-version --manifest-path "plugins/$unit/Cargo.toml" "$next" >&2
  fi
fi
if [[ $unit != core ]]; then
  sed -i "s/^version: .*/version: $next/" "plugins/$unit/package/balerix-plugin.yaml"
fi

mkdir -p "$notes_dir"
cliff "$unit" --unreleased --tag "$tag" --strip header >"$notes"
{
  printf '# Changelog\n\n%s\n' "$(<"$notes")"
  if [[ -f $changelog ]]; then
    printf '\n'
    sed '1{/^# Changelog$/d}' "$changelog" | sed '1{/^$/d}'
  fi
} >"$changelog.new"
mv "$changelog.new" "$changelog"

echo "$unit: prepared $tag" >&2
emit status release version "$next" tag "$tag" notes "$notes"
