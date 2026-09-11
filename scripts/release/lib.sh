# shellcheck shell=bash
# The release units (Spec I §3): the one place that knows each unit's crate,
# manifest, tag, changelog, image and include paths. Sourced by every script
# under scripts/release/, never run on its own. Callers `cd` to the
# repository root first.

# shellcheck disable=SC2034 # read by the scripts that source this file
UNITS=(core flow web matrix)
# shellcheck disable=SC2034
PLUGIN_UNITS=(flow web matrix)

die() {
  echo "$*" >&2
  exit 1
}

require_unit() {
  case ${1:-} in
    core | flow | web | matrix) ;;
    *) die "unknown release unit: '${1:-}' (expected one of: ${UNITS[*]})" ;;
  esac
}

unit_crate() {
  require_unit "$1"
  if [[ $1 == core ]]; then echo balerix; else echo "balerix-plugin-$1"; fi
}

unit_tag_prefix() { echo "$(unit_crate "$1")-v"; }

unit_tag() { echo "$(unit_tag_prefix "$1")$2"; }

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
  local owner=${GITHUB_REPOSITORY_OWNER:-balerix-ai}
  echo "ghcr.io/${owner,,}/$(unit_crate "$1")"
}

# The paths whose commits count toward the unit, one per line. The SDK and
# the api are compiled into every plugin binary, so they count for plugins.
unit_paths() {
  require_unit "$1"
  if [[ $1 == core ]]; then
    printf '%s\n' 'crates/**' Cargo.toml Cargo.lock mise.toml
  else
    printf '%s\n' "plugins/$1/**" 'crates/balerix-api/**' 'crates/balerix-plugin-sdk/**'
  fi
}

unit_version() {
  local crate
  crate=$(unit_crate "$1")
  cargo metadata --manifest-path "$(unit_manifest "$1")" --no-deps --format-version 1 |
    jq -r --arg c "$crate" '.packages[] | select(.name == $c) | .version'
}

tag_exists() { git rev-parse -q --verify "refs/tags/$1" >/dev/null; }

# The unit's newest release tag, or nothing.
last_tag() { git tag --list "$(unit_tag_prefix "$1")*" --sort=-v:refname | head -n 1; }

# git-cliff scoped to the unit: its tag pattern and its include paths.
cliff() {
  local unit=$1 path
  shift
  local args=(--config cliff.toml --tag-pattern "^$(unit_tag_prefix "$unit")")
  while IFS= read -r path; do args+=(--include-path "$path"); done < <(unit_paths "$unit")
  git-cliff "${args[@]}" "$@"
}
