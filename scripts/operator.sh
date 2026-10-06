#!/usr/bin/env bash
# Build, format, lint and test the standalone operator project (Spec O
# §12), the way scripts/agent.sh does the agent: its own cargo workspace,
# lockfile and target directory. The Daemon client's test runs a real
# `balerix serve --mode kubernetes`, so `check` builds `balerix` from the
# core workspace first and hands its path over as BALERIX_BIN; without it
# that test skips (fails under BALERIX_REQUIRE_TOOLS=1).
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"

usage() { echo "usage: $0 {build|fmt|check|crds|charts|e2e}" >&2; exit 2; }
[[ $# -eq 1 ]] || usage
dir="$repo/operator"
target="$dir/target"

case "$1" in
  build)
    CARGO_TARGET_DIR="$target" cargo build -q --manifest-path "$dir/Cargo.toml"
    ;;
  fmt)
    CARGO_TARGET_DIR="$target" cargo fmt --manifest-path "$dir/Cargo.toml" --all
    ;;
  crds)
    CARGO_TARGET_DIR="$target" cargo run -q --manifest-path "$dir/Cargo.toml" -- \
        crds --chart-dir "$repo/charts/balerix-operator/templates/crds"
    ;;
  check)
    (cd "$repo" && cargo build -q -p balerix)
    export BALERIX_BIN="${CARGO_TARGET_DIR:-$repo/target}/debug/balerix"
    CARGO_TARGET_DIR="$target" cargo fmt --manifest-path "$dir/Cargo.toml" --all --check
    CARGO_TARGET_DIR="$target" cargo clippy --manifest-path "$dir/Cargo.toml" --all-targets -- -D warnings
    # the envtest binaries the task pins; the harness skips (fails under
    # BALERIX_REQUIRE_TOOLS=1) without them
    if command -v kube-apiserver >/dev/null; then
      ENVTEST_DIR="$(dirname "$(command -v kube-apiserver)")"
      export ENVTEST_DIR
    fi
    # Every test but tests/e2e_k8s.rs, the journey, which needs the kind
    # cluster and fails without it under BALERIX_REQUIRE_TOOLS=1 (the `e2e`
    # mode runs it), and tests/charts_it.rs, which needs helm (the `charts`
    # mode runs it). A filter, not a list of targets, so a new tests/*.rs
    # runs here without an edit; nextest rejects a binary() filter that
    # matches no binary, so the journey's file must keep its name.
    CARGO_TARGET_DIR="$target" cargo nextest run \
      --config-file "$dir/.config/nextest.toml" \
      --manifest-path "$dir/Cargo.toml" \
      -E 'not binary(e2e_k8s) and not binary(charts_it)'
    ;;
  charts)
    # Spec O §24.3: lint both charts, then tests/charts_it.rs, which
    # renders them and applies them against the envtest API server
    for c in "$repo"/charts/*/; do helm lint --strict "$c"; done
    if command -v kube-apiserver >/dev/null; then
      ENVTEST_DIR="$(dirname "$(command -v kube-apiserver)")"
      export ENVTEST_DIR
    fi
    CARGO_TARGET_DIR="$target" cargo nextest run \
      --config-file "$dir/.config/nextest.toml" \
      --manifest-path "$dir/Cargo.toml" --test charts_it
    ;;
  e2e)
    root="${CARGO_TARGET_DIR:-$repo/target}/tmp/kind"
    export KUBECONFIG="$root/kubeconfig"
    export BALERIX_K8S_IMAGES="${BALERIX_K8S_IMAGES:-balerix:e2e,balerix-agent:e2e}"
    export BALERIX_K8S_PLUGIN_IMAGES="${BALERIX_K8S_PLUGIN_IMAGES:-balerix-plugin-flow:e2e,balerix-plugin-web:e2e,balerix-fake-plugin:e2e}"
    # the operator in the cluster, from the chart, under its own RBAC
    # (Spec O §24.3): one cluster-wide release both journeys share
    helm upgrade --install balerix-operator "$repo/charts/balerix-operator" \
      --namespace balerix-system --create-namespace \
      --set image.repository=balerix-operator --set image.tag=e2e \
      --set images.daemon=balerix:e2e --set images.agent=balerix-agent:e2e \
      --wait --timeout 5m
    export BALERIX_K8S_CHARTS="$repo/charts"
    # the e2e-k8s profile: the journey waits minutes per step, past the
    # default profile's three-minute termination
    CARGO_TARGET_DIR="$target" cargo nextest run \
      --config-file "$dir/.config/nextest.toml" --profile e2e-k8s \
      --manifest-path "$dir/Cargo.toml" --test e2e_k8s --no-capture
    ;;
  *)
    usage
    ;;
esac
