#!/usr/bin/env bash
# Publishes balerix-api and balerix-plugin-sdk at the core version (Spec I
# §5.5), skipping any that crates.io already has, so a re-run is safe.
#
# usage: publish-crates.sh [--dry-run]
# CARGO_REGISTRY_TOKEN must be set unless --dry-run is given.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

dry_run=false
case ${1:-} in
  "") ;;
  --dry-run) dry_run=true ;;
  *) die "usage: $0 [--dry-run]" ;;
esac

version=$(unit_version core)

# The sparse index path of a crate name of four or more characters.
published() {
  local name=$1
  curl -fsS "https://index.crates.io/${name:0:2}/${name:2:2}/$name" 2>/dev/null |
    jq -e --arg v "$version" 'select(.vers == $v)' >/dev/null
}

crates=()
for crate in balerix-api balerix-plugin-sdk; do
  if published "$crate"; then
    echo "$crate $version is already on crates.io; skipping" >&2
  else
    crates+=(-p "$crate")
  fi
done
((${#crates[@]})) || exit 0

if $dry_run; then
  cargo publish --dry-run --locked "${crates[@]}"
else
  : "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN must come from crates-io-auth-action}"
  cargo publish --locked "${crates[@]}"
fi
