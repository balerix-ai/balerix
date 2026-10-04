#!/usr/bin/env bash
# A kind cluster for e2e-k8s (Spec O §15, §19.3, §21.4): every node (the
# control plane and two workers) mounts one host directory, the local-path
# provisioner told to provision ReadWriteMany claims from it, the five
# definitions applied, and the daemon and agent images built from this
# tree and loaded. The control plane mounts it too: local-path's helper
# pod, which creates each volume's directory, tolerates the control plane
# and may run there. Nothing here is
# a production class: the shared directory is one host path, not a network
# filesystem.
#
# usage: kind-up.sh [up|down]
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"

name=balerix-e2e
root="${CARGO_TARGET_DIR:-$repo/target}/tmp/kind"
shared="$root/shared"
export KUBECONFIG="$root/kubeconfig"
# kind 0.33.0's node image for the spike's Kubernetes line (§19)
node_image="kindest/node:v1.34.11@sha256:44e222ee2132dab25ff87301682f89eb82c7880ea3a1bf543bfe9708fd08d67d"

for tool in kind kubectl docker cargo; do
  command -v "$tool" >/dev/null || { echo "kind-up: $tool is not on PATH" >&2; exit 2; }
done

case "${1:-up}" in
  down)
    kind delete cluster --name "$name" 2>/dev/null || true
    rm -rf "$root"
    exit 0
    ;;
  up) ;;
  *) echo "usage: $0 [up|down]" >&2; exit 2 ;;
esac

mkdir -p "$root" "$shared"
chmod 0777 "$shared"   # uid 10001 in the pods writes it (§19.3)

if ! kind get clusters 2>/dev/null | grep -qx "$name"; then
  cat >"$root/kind.yaml" <<EOF2
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
- role: control-plane
  extraMounts: [{ hostPath: "$shared", containerPath: /var/local-path-shared }]
- role: worker
  extraMounts: [{ hostPath: "$shared", containerPath: /var/local-path-shared }]
- role: worker
  extraMounts: [{ hostPath: "$shared", containerPath: /var/local-path-shared }]
EOF2
  kind create cluster --name "$name" --image "$node_image" --config "$root/kind.yaml" --wait 180s
fi

# the shared class: local-path over the one directory every node mounts
kubectl -n local-path-storage patch configmap local-path-config --type merge \
  -p '{"data":{"config.json":"{\"nodePathMap\":[],\"sharedFileSystemPath\":\"/var/local-path-shared\"}"}}'
kubectl -n local-path-storage rollout restart deployment local-path-provisioner
kubectl -n local-path-storage rollout status deployment local-path-provisioner --timeout=120s
kubectl apply -f operator/crds/

# the two images, from this tree: native release builds, no musl, no scan
cargo build --release -q -p balerix
CARGO_TARGET_DIR="$repo/agent/target" cargo build --release -q --manifest-path agent/Cargo.toml
dist="$root/dist"
rm -rf "$dist" && mkdir -p "$dist"
cp "${CARGO_TARGET_DIR:-$repo/target}/release/balerix" "$dist/balerix"
context=$(scripts/release/image-context.sh core "$dist" "$root/context-core" | sed -n 's/^context=//p')
secret=()
[[ -n ${GITHUB_TOKEN:-} ]] && secret=(--secret "id=github_token,env=GITHUB_TOKEN")
# bash 3.2 (macOS) calls an empty "${secret[@]}" unbound under set -u
docker build ${secret[@]+"${secret[@]}"} -t balerix:e2e -f docker/balerix/Dockerfile "$context"
rm -rf "$root/context-agent" && mkdir -p "$root/context-agent"
cp "$repo/agent/target/release/balerix-agent" "$root/context-agent/balerix-agent"
docker build --build-arg BASE=balerix:e2e -t balerix-agent:e2e -f docker/agent/Dockerfile "$root/context-agent"
kind load docker-image --name "$name" balerix:e2e balerix-agent:e2e

echo "kind-up: cluster $name ready; KUBECONFIG=$KUBECONFIG"
