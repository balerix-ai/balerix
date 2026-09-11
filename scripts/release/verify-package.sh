#!/usr/bin/env bash
# Installs one plugin's published release package the way the daemon does
# and starts it (Spec I §7.3): mise must resolve, checksum and attest the
# binary for this architecture, and the binary must run.
#
# usage: verify-package.sh <plugin>           verify
#        verify-package.sh <plugin> --flag    mark the release as a prerelease
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 || $# -eq 2 ]] || die "usage: $0 <plugin> [--flag]"
unit=$1
[[ $unit != core ]] || die "core has no plugin package"
require_unit "$unit"
: "${GITHUB_REPOSITORY:?}"
crate=$(unit_crate "$unit")
version=$(unit_version "$unit")
tag=$(unit_tag "$unit" "$version")
package="$crate-v$version-package.tar.gz"

if [[ ${2:-} == --flag ]]; then
  marker="> **Package verification failed**"
  body=$(gh release view "$tag" --json body --jq .body)
  if ! grep -qF "$marker" <<<"$body"; then
    run="${GITHUB_SERVER_URL:-https://github.com}/$GITHUB_REPOSITORY/actions/runs/${GITHUB_RUN_ID:-}"
    printf '%s on %s ([run](%s)); do not install this release.\n\n%s\n' \
      "$marker" "$(uname -m)" "$run" "$body" >"${RUNNER_TEMP:-/tmp}/notes-$unit.md"
    gh release edit "$tag" --prerelease --notes-file "${RUNNER_TEMP:-/tmp}/notes-$unit.md"
  fi
  exit 0
fi
[[ $# -eq 1 ]] || die "usage: $0 <plugin> [--flag]"

# Outside the repository, so no project mise.toml above it is discovered.
work=$(mktemp -d "${RUNNER_TEMP:-/tmp}/verify-$unit-XXXXXX")
gh release download "$tag" --repo "$GITHUB_REPOSITORY" \
  --pattern "$package" --pattern SHA256SUMS --dir "$work"
(cd "$work" && sha256sum --check --ignore-missing SHA256SUMS)

mkdir "$work/package"
tar -xzf "$work/$package" -C "$work/package"

export MISE_DATA_DIR="$work/data" MISE_CACHE_DIR="$work/cache" MISE_STATE_DIR="$work/state"
export MISE_GLOBAL_CONFIG_FILE="$work/global.toml"
: >"$MISE_GLOBAL_CONFIG_FILE"
cd "$work/package"
mise trust
mise install

set +e
# The plugin binary runs without the job's tokens; mise install above is
# the only step that needs them.
err=$(env -u GH_TOKEN -u GITHUB_TOKEN -u BALERIX_PLUGIN_TOKEN mise run serve 2>&1 >/dev/null)
code=$?
set -e
[[ $code -ne 0 ]] || die "$unit: the packaged plugin started without a daemon environment"
grep -q "^$unit: " <<<"$err" || die "$unit: the packaged plugin did not report '$unit: …': $err"
echo "$unit: $package installs and runs on $(uname -m)" >&2
