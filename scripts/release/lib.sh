# shellcheck shell=bash
# The release units (Spec I §3): the one place that knows each unit's crate,
# manifest, tag, changelog, image and include paths (`charts` is a unit with no crate). Sourced by every script
# under scripts/release/, never run on its own. Callers `cd` to the
# repository root first.

# shellcheck disable=SC2034 # read by the scripts that source this file
UNITS=(core common flow web matrix github charts)
# shellcheck disable=SC2034
PLUGIN_UNITS=(flow web matrix github)
# Units that ship crates and no binary (Spec K §5).
# shellcheck disable=SC2034
LIBRARY_UNITS=(common)
# Units with a binary, an image and an archive: every unit but the libraries.
# shellcheck disable=SC2034
IMAGE_UNITS=(core flow web matrix github)
# The standalone projects that release with core (Spec O §13, §24.4): each
# has its own manifest, lockfile and target directory, and core's version.
# shellcheck disable=SC2034
CORE_PROJECTS=(operator agent)

# The arguments as a JSON array of strings: unit names and other bare
# identifiers, nothing that needs escaping.
json_list() {
  local quoted=()
  if (($#)); then quoted=("${@/#/\"}") && quoted=("${quoted[@]/%/\"}"); fi
  local IFS=,
  echo "[${quoted[*]}]"
}

die() {
  echo "$*" >&2
  exit 1
}

require_unit() {
  case ${1:-} in
    core | common | flow | web | matrix | github | charts) ;;
    *) die "unknown release unit: '${1:-}' (expected one of: ${UNITS[*]})" ;;
  esac
}

# core, library, plugin or charts.
unit_kind() {
  require_unit "$1"
  case $1 in
    core) echo core ;;
    common) echo library ;;
    charts) echo charts ;;
    *) echo plugin ;;
  esac
}

unit_crate() {
  require_unit "$1"
  case $1 in
    core) echo balerix ;;
    # no crate: the release's name, for its tag and its title
    charts) echo balerix-charts ;;
    *) echo "balerix-plugin-$1" ;;
  esac
}

unit_tag_prefix() {
  require_unit "$1"
  echo "$(unit_crate "$1")-v"
}

unit_tag() {
  require_unit "$1"
  echo "$(unit_tag_prefix "$1")$2"
}

unit_manifest() {
  require_unit "$1"
  case $1 in
    core) echo Cargo.toml ;;
    charts) echo charts/balerix-operator/Chart.yaml ;;
    *) echo "plugins/$1/Cargo.toml" ;;
  esac
}

unit_changelog() {
  require_unit "$1"
  case $1 in
    core) echo CHANGELOG.md ;;
    charts) echo charts/CHANGELOG.md ;;
    *) echo "plugins/$1/CHANGELOG.md" ;;
  esac
}

# The images a unit ships, by repository name, in build order (Spec O
# §24.4): core's balerix-agent is built on the balerix just built.
unit_images() {
  require_unit "$1"
  case $(unit_kind "$1") in
    core) printf '%s\n' balerix balerix-agent balerix-operator ;;
    plugin) unit_crate "$1" ;;
    *) die "$1 has no image" ;;
  esac
}

# The release unit an image belongs to.
image_unit() {
  local unit
  case $1 in
    balerix | balerix-agent | balerix-operator) echo core ;;
    balerix-plugin-*)
      unit=${1#balerix-plugin-}
      require_unit "$unit"
      [[ $(unit_kind "$unit") == plugin ]] || die "$unit is a library and has no image"
      echo "$unit"
      ;;
    *) die "unknown image: '$1'" ;;
  esac
}

# ghcr repositories must be lowercase; a fork's owner may not be.
image_ref() {
  image_unit "$1" >/dev/null
  local owner=${GITHUB_REPOSITORY_OWNER:-balerix-ai}
  echo "ghcr.io/${owner,,}/$1"
}

# The image's description: its crate's, less the trailing spec reference.
image_description() {
  case $1 in
    balerix-agent | balerix-operator)
      cargo metadata --manifest-path "${1#balerix-}/Cargo.toml" --no-deps --format-version 1 |
        jq -r --arg c "$1" '.packages[] | select(.name == $c) | .description // ""' |
        sed -E 's/ \([^()]*\)$//'
      ;;
    *) unit_description "$(image_unit "$1")" ;;
  esac
}

# The paths whose commits count toward the unit, one per line. The SDK and
# the api are compiled into every plugin binary and into common, so they
# count for all of them; common is compiled into every plugin whose
# manifest names it, so it counts for those.
unit_paths() {
  require_unit "$1"
  if [[ $1 == core ]]; then
    printf '%s\n' 'crates/**' Cargo.toml Cargo.lock mise.toml \
      'operator/**' 'agent/**' 'docker/operator/**' 'docker/agent/**'
  elif [[ $1 == charts ]]; then
    printf '%s\n' 'charts/**'
  else
    printf '%s\n' "plugins/$1/**" 'crates/balerix-api/**' 'crates/balerix-plugin-sdk/**'
    if [[ $(unit_kind "$1") == plugin ]] && grep -q '^balerix-plugin-common ' "plugins/$1/Cargo.toml"; then
      printf '%s\n' 'plugins/common/**'
    fi
  fi
}

# The version a dependency names in a manifest: the `version = "…"` inside
# `<crate> = { … }`. Empty when the dependency carries no version.
dep_version() {
  local manifest=$1 crate=$2
  sed -n "s/^$crate = {.*version = \"\([^\"]*\)\".*/\1/p" "$manifest" | head -n 1
}

# The unit's crate as `cargo metadata` reports it.
unit_package() {
  local crate
  crate=$(unit_crate "$1")
  cargo metadata --manifest-path "$(unit_manifest "$1")" --no-deps --format-version 1 |
    jq --arg c "$crate" '.packages[] | select(.name == $c)'
}

unit_version() {
  if [[ $1 == charts ]]; then charts_field version; else unit_package "$1" | jq -r .version; fi
}

# A core project's version (balerix-<project> in <project>/Cargo.toml).
project_version() {
  cargo metadata --manifest-path "$1/Cargo.toml" --no-deps --format-version 1 |
    jq -r --arg c "balerix-$1" '.packages[] | select(.name == $c) | .version'
}

# The core crates <project> builds on by path, as its lockfile lists them:
# the balerix-* packages with no `source` line, less the project itself.
project_path_crates() {
  awk -v self="balerix-$1" '
    function flush() { if (name != "" && !src && name != self) print name; name = ""; src = 0 }
    /^\[\[package\]\]/ { flush() }
    /^name = "balerix/ { name = $3; gsub(/"/, "", name) }
    /^source = / { src = 1 }
    END { flush() }
  ' "$1/Cargo.lock"
}

# The crate's description, less a trailing spec reference ("… (Spec G)")
# that means nothing outside this repository.
unit_description() {
  unit_package "$1" | jq -r '.description // ""' | sed -E 's/ \([^()]*\)$//'
}

tag_exists() { git rev-parse -q --verify "refs/tags/$1" >/dev/null; }

# Whether the unit's changelog has a `## <version> - ` section: what a merged
# release PR leaves behind.
has_section() {
  require_unit "$1"
  local changelog
  changelog=$(unit_changelog "$1")
  [[ -f $changelog ]] && grep -q "^## ${2//./\\.} - " "$changelog"
}

# The unit's newest release tag, or nothing.
last_tag() {
  require_unit "$1"
  git tag --list "$(unit_tag_prefix "$1")*" --sort=-v:refname | head -n 1
}

# git-cliff scoped to the unit: its tag pattern and its include paths.
cliff() {
  require_unit "$1"
  local unit=$1 path
  shift
  local args=(--config cliff.toml --tag-pattern "^$(unit_tag_prefix "$unit")")
  while IFS= read -r path; do args+=(--include-path "$path"); done < <(unit_paths "$unit")
  git-cliff "${args[@]}" "$@"
}

# The two charts (Spec O §14.1): one version and one appVersion between them.
CHARTS=(balerix-operator balerix-daemon)
DAEMON_VALUES=charts/balerix-daemon/values.yaml

# A top-level `<field>: <value>` of a YAML file (`-` is stdin), quotes dropped.
yaml_field() {
  sed -n "s/^$2: \"\{0,1\}\([^\"]*\)\"\{0,1\}\$/\1/p" "$1"
}

chart_field() { yaml_field "charts/$1/Chart.yaml" "$2"; }

# <field> of both charts; dies when they differ (they move together).
charts_field() {
  local field=$1 chart value first=
  for chart in "${CHARTS[@]}"; do
    value=$(chart_field "$chart" "$field")
    [[ -n $value ]] || die "charts/$chart/Chart.yaml has no $field"
    [[ -z $first || $value == "$first" ]] ||
      die "charts: $field is $first in ${CHARTS[0]} but $value in $chart; they move together"
    first=$value
  done
  echo "$first"
}

# Writes <field> into both Chart.yaml files; appVersion stays a quoted string.
set_charts_field() {
  local field=$1 value=$2 chart
  [[ $field != appVersion ]] || value="\"$value\""
  for chart in "${CHARTS[@]}"; do
    sed -i "s/^$field: .*/$field: $value/" "charts/$chart/Chart.yaml"
  done
}

# plugins.<plugin>.image.tag in the daemon chart's values: the plugin version
# the chart installs (Spec O §24.5). Only the key under `plugins:` counts:
# `credentials:` has a `github:` key at the same depth.
chart_pin() {
  awk -v want="  $1:" '
    /^[^ #]/ { top = $1; inside = 0 }
    top == "plugins:" && /^  [^ #]/ { inside = ($0 == want) }
    inside && /^      tag: / { sub(/^      tag: "?/, ""); sub(/"$/, ""); print; exit }
  ' "${2:-$DAEMON_VALUES}"
}

# Every version the charts name, one `<unit> <version> <field>` line each:
# core's (appVersion) and each plugin's (its image tag).
charts_pins() {
  local plugin pin
  echo "core $(charts_field appVersion) appVersion"
  for plugin in "${PLUGIN_UNITS[@]}"; do
    pin=$(chart_pin "$plugin")
    [[ -n $pin ]] || die "charts: $DAEMON_VALUES has no plugins.$plugin.image.tag"
    echo "$plugin $pin plugins.$plugin.image.tag"
  done
}

set_chart_pin() {
  local tmp="$DAEMON_VALUES.new"
  awk -v want="  $1:" -v v="$2" '
    /^[^ #]/ { top = $1; inside = 0 }
    top == "plugins:" && /^  [^ #]/ { inside = ($0 == want) }
    inside && /^      tag: / { $0 = "      tag: \"" v "\""; inside = 0 }
    { print }
  ' "$DAEMON_VALUES" >"$tmp"
  mv "$tmp" "$DAEMON_VALUES"
  [[ $(chart_pin "$1") == "$2" ]] || die "charts: could not set plugins.$1.image.tag in $DAEMON_VALUES"
}

# major, minor or patch: the largest component that differs between two
# versions; nothing when they are equal.
bump_level() {
  local -a a b
  IFS=. read -ra a <<<"$1"
  IFS=. read -ra b <<<"$2"
  if [[ ${a[0]:-} != "${b[0]:-}" ]]; then
    echo major
  elif [[ ${a[1]:-} != "${b[1]:-}" ]]; then
    echo minor
  elif [[ ${a[2]:-} != "${b[2]:-}" ]]; then
    echo patch
  fi
}

bump_version() {
  local -a v
  IFS=. read -ra v <<<"$1"
  case $2 in
    major) echo "$((v[0] + 1)).0.0" ;;
    minor) echo "${v[0]}.$((v[1] + 1)).0" ;;
    patch) echo "${v[0]}.${v[1]}.$((v[2] + 1))" ;;
    *) die "not a bump level: '$2'" ;;
  esac
}

level_rank() {
  case ${1:-} in
    major) echo 3 ;;
    minor) echo 2 ;;
    patch) echo 1 ;;
    *) echo 0 ;;
  esac
}

# Flags <tag>'s GitHub Release as a prerelease, putting <marker>, this
# runner's architecture, a link to this run and <advice> at the top of its
# notes; once (Spec I §7.3).
flag_prerelease() {
  local tag=$1 marker=$2 advice=$3 body run file
  body=$(gh release view "$tag" --json body --jq .body)
  if grep -qF "$marker" <<<"$body"; then return 0; fi
  run="${GITHUB_SERVER_URL:-https://github.com}/$GITHUB_REPOSITORY/actions/runs/${GITHUB_RUN_ID:-}"
  file="${RUNNER_TEMP:-/tmp}/notes-$tag.md"
  printf '%s on %s ([run](%s)); %s\n\n%s\n' "$marker" "$(uname -m)" "$run" "$advice" "$body" >"$file"
  gh release edit "$tag" --prerelease --notes-file "$file"
}
