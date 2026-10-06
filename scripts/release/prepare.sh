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
#   status=none          no releasable commit since the last tag, or a
#                        library or charts unit whose dependencies have not
#                        been released yet
#                        (the reason is on stderr)
#   status=in-progress   the manifest version is already awaiting its tag
#
# Dies (no status line) if an older tag exists and the manifest version has
# neither a tag nor a changelog section: it was changed by hand outside a
# release PR. Force that version to release it, or revert the edit.
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
count=-1
pins=

emit() { printf '%s=%s\n' "$@"; }

# A library publishes to crates.io, where its `{ version, path }`
# dependencies must already exist at the code it compiles against: until
# the core release that published that SDK version is tagged, and while
# either core crate has changed since that tag, there is nothing to
# propose (Spec K §5). A status, not a failure: release-pr runs for every
# unit on the push that merges a core release PR, before release.yml has
# tagged it.
library_blocked() {
  local sdk
  sdk=$(dep_version "$(unit_manifest "$unit")" balerix-plugin-sdk)
  [[ -n $sdk ]] || die "$unit: $(unit_manifest "$unit") names balerix-plugin-sdk without a version"
  if ! tag_exists "$(unit_tag core "$sdk")"; then
    echo "$unit: its manifest names balerix-plugin-sdk $sdk, which has no tag balerix-v$sdk yet;" \
      "release core $sdk first, then $unit" >&2
  elif ! git diff --quiet "$(unit_tag core "$sdk")" HEAD -- crates/balerix-api crates/balerix-plugin-sdk; then
    echo "$unit: crates/balerix-api or crates/balerix-plugin-sdk changed since balerix-v$sdk;" \
      "release core first, then $unit" >&2
  else
    return 1
  fi
}

# The charts name core's version (appVersion) and each plugin's (its image
# tag): until every one is tagged, a release would install images that do
# not exist (Spec O §24.5). A status, not a failure, as for a library.
charts_blocked() {
  local app plugin pin missing=()
  app=$(charts_field appVersion)
  tag_exists "$(unit_tag core "$app")" || missing+=("$(unit_tag core "$app") (appVersion)")
  for plugin in "${PLUGIN_UNITS[@]}"; do
    pin=$(chart_pin "$plugin")
    [[ -n $pin ]] || die "charts: $DAEMON_VALUES has no plugins.$plugin.image.tag"
    tag_exists "$(unit_tag "$plugin" "$pin")" ||
      missing+=("$(unit_tag "$plugin" "$pin") (plugins.$plugin.image.tag)")
  done
  ((${#missing[@]})) || return 1
  echo "charts: they name versions with no release tag yet: ${missing[*]}; release those first, then charts" >&2
}

# The pins that moved since <tag>, one `<image> <old> <new>` line each: a
# core or plugin release moves them in a chore(release) commit, which
# git-cliff does not count, so they count here (Spec O §24.5).
charts_pin_changes() {
  local tag=$1 plugin old new
  old=$(git show "$tag:charts/balerix-operator/Chart.yaml" | yaml_field - appVersion)
  new=$(charts_field appVersion)
  [[ $old == "$new" ]] || echo "balerix $old $new"
  for plugin in "${PLUGIN_UNITS[@]}"; do
    old=$(chart_pin "$plugin" <(git show "$tag:$DAEMON_VALUES"))
    new=$(chart_pin "$plugin")
    [[ $old == "$new" ]] || echo "$(unit_crate "$plugin") $old $new"
  done
}

# The charts' next version: the largest of the bump their own commits ask
# and each moved pin's. A core minor is a chart minor.
charts_next() {
  local lastversion=$1 count=$2 pins=$3 level='' candidate image old new
  if ((count)); then
    candidate=$(cliff charts --bumped-version)
    level=$(bump_level "$lastversion" "${candidate#"$prefix"}")
  fi
  while read -r image old new; do
    [[ -n $image ]] || continue
    candidate=$(bump_level "$old" "$new")
    if (($(level_rank "$candidate") > $(level_rank "$level"))); then level=$candidate; fi
  done <<<"$pins"
  bump_version "$lastversion" "$level"
}

# `last` is always an existing tag or empty.
if [[ $unit == charts && -n $last ]]; then pins=$(charts_pin_changes "$last"); fi

if ! tag_exists "$(unit_tag "$unit" "$current")" && has_section "$unit" "$current"; then
  # A release PR was merged and release.yml has not tagged it yet, or failed
  # (Spec I §8.2): prepare.sh always writes the section, so this also covers
  # a merged initial release PR. Proposing anything now would release twice,
  # so this wins even over a forced version.
  echo "$unit: $current is awaiting its tag; release in progress" >&2
  emit status in-progress version "$current"
  exit 0
elif [[ $(unit_kind "$unit") == library ]] && library_blocked; then
  emit status none
  exit 0
elif [[ $unit == charts ]] && charts_blocked; then
  emit status none
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
elif ! tag_exists "$(unit_tag "$unit" "$current")"; then
  # $last exists, so a release has shipped before, but $current has neither
  # a tag nor a changelog section: nobody proposed it through prepare.sh.
  # $current need not be above $last (it could be a revert, or not even a
  # release version), in which case forcing it is refused; say so.
  die "$unit: $current has no tag and no $changelog section, though $last exists;" \
    "it was changed outside a release PR. Revert the edit, or, if it is a" \
    "release version above ${last#"$prefix"}, dispatch release-pr with" \
    "version set to it."
else
  count=$(cliff "$unit" --unreleased --context | jq '[.[].commits[]] | length')
  if [[ $count -eq 0 && -z $pins ]]; then
    echo "$unit: nothing to release since $last" >&2
    emit status none
    exit 0
  fi
  if [[ $unit == charts ]]; then
    next=$(charts_next "${last#"$prefix"}" "$count" "$pins")
  else
    next=$(cliff "$unit" --bumped-version)
    next=${next#"$prefix"}
  fi
fi

tag=$(unit_tag "$unit" "$next")

if [[ $next != "$current" ]]; then
  case $(unit_kind "$unit") in
    core) cargo set-version --workspace "$next" >&2 ;;
    charts) set_charts_field version "$next" ;;
    *) cargo set-version --manifest-path "plugins/$unit/Cargo.toml" "$next" >&2 ;;
  esac
fi
if [[ $unit == core ]]; then
  # A library names the two core crates by version for crates.io; the
  # version moves with core (Spec K §5). This must run before the plugin
  # loop below: a plugin whose manifest names common reaches balerix-api
  # and balerix-plugin-sdk through common's own path dependency, and cargo
  # refuses to update that plugin's lockfile while common's manifest still
  # names the old version.
  for library in "${LIBRARY_UNITS[@]}"; do
    manifest=$(unit_manifest "$library")
    for crate in balerix-api balerix-plugin-sdk; do
      sed -i "s/^\($crate = {.*version = \"\)[^\"]*\(\".*\)$/\1$next\2/" "$manifest"
    done
    cargo update --manifest-path "$manifest" -p balerix-api -p balerix-plugin-sdk >&2
  done
  # Plugins lock the SDK and api versions through their path dependency.
  # Refresh even when $next == $current: a forced version equal to a
  # hand-bumped manifest (§8.2) skips cargo set-version above, but the
  # plugin lockfiles were never updated for that hand edit either.
  for plugin in "${PLUGIN_UNITS[@]}"; do
    cargo update --manifest-path "plugins/$plugin/Cargo.toml" -p balerix-api -p balerix-plugin-sdk >&2
  done
  # The operator and the agent release with core (Spec O §24.4): core's
  # version, and lockfiles that lock the core crates they build on by path.
  # Refresh even when the version is already right, for the reason above.
  for project in "${CORE_PROJECTS[@]}"; do
    if [[ $(project_version "$project") != "$next" ]]; then
      cargo set-version --manifest-path "$project/Cargo.toml" "$next" >&2
    fi
    mapfile -t locals < <(project_path_crates "$project")
    cargo update --manifest-path "$project/Cargo.toml" "${locals[@]/#/--package=}" >&2
  done
  # The charts run this core version (Spec O §24.5).
  set_charts_field appVersion "$next"
fi
if [[ $(unit_kind "$unit") == library ]]; then
  # Plugins built on the library lock it through their path dependency, and
  # build.sh builds them --locked. Refresh even when $next == $current, for
  # the same reason as the core loop above.
  for plugin in "${PLUGIN_UNITS[@]}"; do
    if grep -q "^$(unit_crate "$unit") " "plugins/$plugin/Cargo.toml"; then
      cargo update --manifest-path "plugins/$plugin/Cargo.toml" -p "$(unit_crate "$unit")" >&2
    fi
  done
fi
if [[ $(unit_kind "$unit") == plugin ]]; then
  sed -i "s/^version: .*/version: $next/" "plugins/$unit/package/balerix-plugin.yaml"
  # The daemon chart installs this plugin version (Spec O §24.5).
  set_chart_pin "$unit" "$next"
fi

mkdir -p "$notes_dir"
cliff "$unit" --unreleased --tag "$tag" --strip header >"$notes"
if [[ $unit == charts ]]; then
  # A pin-only release has no commit; the template's "Initial release." is
  # wrong for it: the moved images are the change.
  [[ $count -ne 0 ]] || sed -i '/^- Initial release\.$/d' "$notes"
  if [[ -n $pins ]]; then
    {
      printf '\n### Images\n\n'
      while read -r image old new; do
        # shellcheck disable=SC2016 # literal backticks: markdown code
        printf -- '- `%s` %s → %s\n' "$image" "$old" "$new"
      done <<<"$pins"
    } >>"$notes"
  fi
fi
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
