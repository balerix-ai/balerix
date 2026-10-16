#!/usr/bin/env bash
# Whether a ci.yml job's unit is affected by a pull request: prints
# run=true or run=false, and appends it to $GITHUB_OUTPUT when that is set.
# Each unit lists the paths it compiles, reads or lints, its path
# dependencies included, so a job skips its steps only when none of them
# changed. ci.yml gates steps, never jobs: a skipped job reports nothing,
# and a required check waiting on it would block the pull request forever.
#
#   ci-affected.sh <unit>          EVENT and BASE from the workflow
#   ci-affected.sh <unit> --stdin  changed paths on stdin (to try a unit)
#
# Units: check, plugin-<name>, agent, operator, charts, e2e-k8s.
set -euo pipefail

# Every unit: this workflow and script, the toolchain, and the clippy
# config every cargo project picks up from the repository root.
shared='^(\.github/workflows/ci\.yml|scripts/ci-affected\.sh|mise\.toml|clippy\.toml)$'
# What every plugin compiles besides itself: the two published core crates
# (they take their dependency versions from the root manifest) and common.
plugin_deps='^(crates/balerix-(api|plugin-sdk)/|Cargo\.toml$|plugins/common/|scripts/plugin\.sh$)'

unit=${1:-}
case "$unit" in
  # The core workspace runs unless every change is in a project it never
  # builds: it lints .github, scripts and docker, reads docs/ and
  # examples/, and builds common, flow and web for the e2e.
  check) skip='^(agent/|operator/|charts/|plugins/(matrix|github)/)' ;;
  plugin-common) only=$plugin_deps ;;
  plugin-flow | plugin-web | plugin-matrix | plugin-github)
    only="$plugin_deps|^plugins/${unit#plugin-}/" ;;
  # agent and operator build the balerix binary from the core workspace.
  agent) only='^(agent/|crates/|Cargo\.(toml|lock)$|scripts/agent\.sh$)' ;;
  operator)
    only='^(operator/|crates/|Cargo\.(toml|lock)$|charts/balerix-operator/templates/crds/|scripts/operator\.sh$)' ;;
  # charts_it compiles the operator and reads every plugin's package manifest.
  charts)
    only='^(charts/|operator/|plugins/[^/]+/package/|crates/|Cargo\.toml$|scripts/operator\.sh$|scripts/kind-up\.sh$)' ;;
  # kind-up builds the flow and web plugin images (and common with them).
  e2e-k8s)
    only='^(charts/|operator/|agent/|crates/|plugins/(common|flow|web)/|docker/|scripts/kind-up\.sh$|scripts/operator\.sh$|scripts/release/(image-context|build|lib)\.sh$|Cargo\.(toml|lock)$)' ;;
  *) echo "usage: $0 {check|plugin-<name>|agent|operator|charts|e2e-k8s} [--stdin]" >&2; exit 2 ;;
esac

say() {
  echo "run=$1"
  if [[ -n ${GITHUB_OUTPUT:-} ]]; then echo "run=$1" >>"$GITHUB_OUTPUT"; fi
  exit 0
}

if [[ ${2:-} == --stdin ]]; then
  changed=$(cat)
else
  # main and the nightly always run everything (and keep the caches warm).
  if [[ ${EVENT:-} != pull_request ]]; then say true; fi
  git fetch -q --depth=1 origin "$BASE"
  changed=$(git diff --name-only "$BASE" HEAD)
fi

if grep -qE "$shared" <<<"$changed"; then say true; fi
if [[ -n ${skip:-} ]]; then
  if grep -vqE "$skip" <<<"$changed"; then say true; fi
elif grep -qE "$only" <<<"$changed"; then
  say true
fi
echo "nothing $unit depends on changed" >&2
say false
