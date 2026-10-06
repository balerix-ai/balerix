# shellcheck shell=bash
# The release units (Spec I §3): the one place that knows each unit's crate,
# manifest, tag, changelog, image and include paths. Sourced by every script
# under scripts/release/, never run on its own. Callers `cd` to the
# repository root first.

# shellcheck disable=SC2034 # read by the scripts that source this file
UNITS=(core common flow web matrix github)
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
    core | common | flow | web | matrix | github) ;;
    *) die "unknown release unit: '${1:-}' (expected one of: ${UNITS[*]})" ;;
  esac
}

# core, library or plugin.
unit_kind() {
  require_unit "$1"
  case $1 in
    core) echo core ;;
    common) echo library ;;
    *) echo plugin ;;
  esac
}

unit_crate() {
  require_unit "$1"
  if [[ $1 == core ]]; then echo balerix; else echo "balerix-plugin-$1"; fi
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
  if [[ $1 == core ]]; then echo Cargo.toml; else echo "plugins/$1/Cargo.toml"; fi
}

unit_changelog() {
  require_unit "$1"
  if [[ $1 == core ]]; then echo CHANGELOG.md; else echo "plugins/$1/CHANGELOG.md"; fi
}

# ghcr repositories must be lowercase; a fork's owner may not be.
unit_image() {
  require_unit "$1"
  [[ $(unit_kind "$1") != library ]] || die "$1 is a library and has no image"
  local owner=${GITHUB_REPOSITORY_OWNER:-balerix-ai}
  echo "ghcr.io/${owner,,}/$(unit_crate "$1")"
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

unit_version() { unit_package "$1" | jq -r .version; }

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
