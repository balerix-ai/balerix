#!/usr/bin/env bash
# Dependency advisories and policy for the units about to be released
# (Spec I §5.1): the nightly `mise run audit`, as a gate, per unit.
#
# usage: audit.sh <unit>...
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

(($#)) || die "usage: $0 <unit>..."
for unit in "$@"; do
  require_unit "$unit"
  echo "== $unit" >&2
  if [[ $unit == core ]]; then
    cargo audit
    cargo deny check advisories bans sources licenses
  else
    cargo audit --file "plugins/$unit/Cargo.lock"
    cargo deny --manifest-path "plugins/$unit/Cargo.toml" check advisories bans sources licenses
  fi
done
