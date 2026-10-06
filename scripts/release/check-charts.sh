#!/usr/bin/env bash
# Installs the packaged charts on the kind cluster `kind-up.sh cluster` made,
# with the published images they pin (Spec O §24.5 step 1): the operator
# chart cluster-wide into balerix-system, the daemon chart with flow and web
# enabled. Passes when the Daemon and both Plugins are Ready. No Fleet: one
# needs claude credentials (scripts/verify-k8s.sh covers it). The images
# must be pullable without credentials: public on ghcr.
#
# usage: check-charts.sh <archives-dir>     needs helm, kubectl
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 ]] || die "usage: $0 <archives-dir>"
archives=$1
version=$(unit_version charts)
export KUBECONFIG="${KUBECONFIG:-${CARGO_TARGET_DIR:-target}/tmp/kind/kubeconfig}"
ns=charts-check

diagnose() {
  echo "----- charts check failed; the cluster as it stands -----"
  kubectl get daemons.balerix.ai,plugins.balerix.ai,pods,jobs,pvc -A -o wide || true
  kubectl -n "$ns" describe daemons.balerix.ai,plugins.balerix.ai || true
  kubectl -n balerix-system logs deploy/balerix-operator --tail=200 || true
  kubectl get events -A --sort-by=.lastTimestamp | tail -n 60 || true
}
trap 'diagnose >&2' ERR

helm upgrade --install balerix-operator "$archives/balerix-operator-$version.tgz" \
  --namespace balerix-system --create-namespace --wait --timeout 5m
helm upgrade --install balerix "$archives/balerix-daemon-$version.tgz" \
  --namespace "$ns" --create-namespace \
  --set name=default --set plugins.flow.enabled=true --set plugins.web.enabled=true \
  --wait --timeout 5m
# the pool Job installs claude and gh in the cluster first: minutes
kubectl -n "$ns" wait daemons.balerix.ai/default --for=condition=Ready --timeout=20m
kubectl -n "$ns" wait plugins.balerix.ai/flow plugins.balerix.ai/web --for=condition=Ready --timeout=10m
echo "charts: balerix-charts $version installs; the Daemon and the flow and web Plugins are Ready" >&2
