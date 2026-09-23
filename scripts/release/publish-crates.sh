#!/usr/bin/env bash
# Publishes the crates of the named units (Spec I §5.5, Spec K §5): core's
# balerix-api and balerix-plugin-sdk at the core version, and a library
# unit's crate at its own version. Skips any version crates.io already
# has, so a re-run is safe.
#
# usage: publish-crates.sh [--dry-run] <unit>...
# CARGO_REGISTRY_TOKEN must be set unless --dry-run is given.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

dry_run=false
if [[ ${1:-} == --dry-run ]]; then
  dry_run=true
  shift
fi
(($#)) || die "usage: $0 [--dry-run] <unit>..."

# The sparse index path of a crate name of four or more characters.
published() {
  local name=$1 version=$2
  curl -fsS "https://index.crates.io/${name:0:2}/${name:2:2}/$name" 2>/dev/null |
    jq -e --arg v "$version" 'select(.vers == $v)' >/dev/null
}

# One `cargo publish` per manifest: the core crates share the root
# workspace, a library is its own project.
publish() {
  local manifest=$1
  shift
  if $dry_run; then
    cargo publish --dry-run --locked --manifest-path "$manifest" "$@"
  else
    : "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN must come from crates-io-auth-action}"
    cargo publish --locked --manifest-path "$manifest" "$@"
  fi
}

for unit in "$@"; do
  require_unit "$unit"
  version=$(unit_version "$unit")
  case $(unit_kind "$unit") in
    core)
      crates=()
      for crate in balerix-api balerix-plugin-sdk; do
        if published "$crate" "$version"; then
          echo "$crate $version is already on crates.io; skipping" >&2
        else
          crates+=(-p "$crate")
        fi
      done
      if ((${#crates[@]})); then publish Cargo.toml "${crates[@]}"; fi
      ;;
    library)
      crate=$(unit_crate "$unit")
      if published "$crate" "$version"; then
        echo "$crate $version is already on crates.io; skipping" >&2
      else
        publish "$(unit_manifest "$unit")"
      fi
      ;;
    *) die "$unit publishes no crates" ;;
  esac
done
