#!/usr/bin/env bash
# The by-hand check of Spec O §24.6 with the real `claude`: both charts on
# the cluster KUBECONFIG names, from this tree (--from tree, the default)
# or from the published index (--from index); a Fleet of
# examples/payments.yaml's shape; and the e2e journey's checks walked with a
# real Claude. Prints a report to paste back. `down` removes what it made.
# Not part of any CI tier.
#
# The Fleet is examples/payments.yaml with three changes, kept in step with
# that file by hand: the repo is a git server pod in the namespace (no
# GitHub access needed), git push is off, and the runner is a pod.
#
# Needs: KUBECONFIG; a ReadWriteMany storage class, BALERIX_VERIFY_SHARED_CLASS
# (default: the cluster's default class); your claude login,
# ~/.claude/.credentials.json or BALERIX_VERIFY_CLAUDE_CREDENTIALS. For
# --from index: BALERIX_VERIFY_OWNER (default balerix-ai; the index at
# https://<owner>.github.io/helm-charts) and optionally
# BALERIX_VERIFY_VERSION (default the newest). The cluster pulls the
# published images: they must be public. The operator release watches only
# this script's namespace; the definitions it installs are cluster-wide, so
# use a cluster with no other balerix operator. Nothing secret is printed.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="$REPO/target/tmp/verify-k8s"
SYSTEM=balerix-verify-system
NS=balerix-verify
FROM=tree
MODE=up
CREDS="${BALERIX_VERIFY_CLAUDE_CREDENTIALS:-$HOME/.claude/.credentials.json}"
OWNER="${BALERIX_VERIFY_OWNER:-balerix-ai}"
CLASS="${BALERIX_VERIFY_SHARED_CLASS:-}"
PROMPT="Reply with the single word: ready."
FLOW_TEXT="Run the tests and fix any failures."
FAILED=()

while (($#)); do
  case $1 in
    --from) FROM=${2:-}; shift 2 ;;
    down) MODE=down; shift ;;
    *) echo "usage: $0 [--from tree|index] | down" >&2; exit 2 ;;
  esac
done
[[ $FROM == tree || $FROM == index ]] || { echo "--from is tree or index" >&2; exit 2; }

say() { printf '%s\n' "$*"; }
hr() { say "----- $* -----"; }
ok() { say "PASS $*"; }
bad() { say "FAIL $*"; FAILED+=("$*"); }
k() { kubectl "$@"; }
kn() { kubectl -n "$NS" "$@"; }
# wait_until <seconds> <command…>: polls every 5 s
wait_until() {
  local deadline=$((SECONDS + $1))
  shift
  until "$@" >/dev/null 2>&1; do
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
}
in_agent() { kn exec "$1" -c agent -- sh -c "$2"; }

if [[ $MODE == down ]]; then
  hr "down"
  kn delete fleets.balerix.ai --all --wait --timeout=10m 2>/dev/null || true
  helm uninstall balerix -n "$NS" 2>/dev/null || true
  k delete namespace "$NS" --wait --timeout=10m 2>/dev/null || true
  helm uninstall balerix-operator -n "$SYSTEM" 2>/dev/null || true
  k delete namespace "$SYSTEM" --wait --timeout=5m 2>/dev/null || true
  say "the CustomResourceDefinitions stay (helm.sh/resource-policy: keep);"
  say "delete them with: kubectl delete crd agents.balerix.ai crews.balerix.ai daemons.balerix.ai fleets.balerix.ai plugins.balerix.ai"
  rm -rf "$ROOT"
  exit 0
fi

hr "preflight"
for t in kubectl helm jq; do command -v "$t" >/dev/null || { say "missing tool on PATH: $t"; exit 2; }; done
k version 2>/dev/null | sed 's/^/  /' || { say "kubectl cannot reach the cluster in KUBECONFIG=${KUBECONFIG:-~/.kube/config}"; exit 2; }
[[ $(k auth can-i create customresourcedefinitions) == yes ]] || { say "this account cannot create CustomResourceDefinitions"; exit 2; }
[[ -f $CREDS ]] || { say "no claude credentials at $CREDS (log in with claude, or set BALERIX_VERIFY_CLAUDE_CREDENTIALS)"; exit 2; }
if [[ -n $CLASS ]]; then
  k get storageclass "$CLASS" >/dev/null || { say "no storage class $CLASS"; exit 2; }
else
  say "shared class: the cluster's default ($(k get storageclass -o json | jq -r '[.items[] | select(.metadata.annotations["storageclass.kubernetes.io/is-default-class"] == "true") | .metadata.name] | join(",")'));"
  say "  it must serve ReadWriteMany; set BALERIX_VERIFY_SHARED_CLASS otherwise"
fi
rm -rf "$ROOT" && mkdir -p "$ROOT"

hr "A. charts (--from $FROM)"
version_args=()
if [[ $FROM == tree ]]; then
  op_chart="$REPO/charts/balerix-operator"
  daemon_chart="$REPO/charts/balerix-daemon"
else
  export HELM_CONFIG_HOME="$ROOT/helm/config" HELM_CACHE_HOME="$ROOT/helm/cache" HELM_DATA_HOME="$ROOT/helm/data"
  helm repo add balerix-verify "https://${OWNER,,}.github.io/helm-charts" >/dev/null || { say "cannot add the index for $OWNER"; exit 2; }
  helm repo update balerix-verify >/dev/null
  op_chart=balerix-verify/balerix-operator
  daemon_chart=balerix-verify/balerix-daemon
  [[ -z ${BALERIX_VERIFY_VERSION:-} ]] || version_args=(--version "$BALERIX_VERIFY_VERSION")
fi
helm upgrade --install balerix-operator "$op_chart" "${version_args[@]}" \
  --namespace "$SYSTEM" --create-namespace --set "watchNamespaces={$NS}" --wait --timeout 5m ||
  { say "the operator chart did not install"; exit 1; }
helm list -n "$SYSTEM" | sed 's/^/  /'
operator_json=$(k -n "$SYSTEM" get deploy balerix-operator -o json)
say "  operator image: $(jq -r '.spec.template.spec.containers[0].image' <<<"$operator_json")"
app=$(helm list -n "$SYSTEM" -o json | jq -r '.[] | select(.name == "balerix-operator") | .app_version')
agent_image=$(jq -r '.spec.template.spec.containers[0].args as $a | ($a | index("--agent-image")) as $i | if $i then $a[$i + 1] else empty end' <<<"$operator_json")
agent_image=${agent_image:-ghcr.io/balerix-ai/balerix-agent:$app}
say "  agent image: $agent_image"

k create namespace "$NS" --dry-run=client -o yaml | k apply -f - >/dev/null
kn create secret generic claude-credentials --from-file=credentials.json="$CREDS" \
  --dry-run=client -o yaml | kn apply -f - >/dev/null

# the repository: a git server pod seeded with one commit, as the e2e journey's
kn apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: { name: git, labels: { app: git } }
spec:
  containers:
    - name: git
      image: $agent_image
      command: [bash, -c, "set -e; git init -q --bare /srv/repo.git; exec git daemon --base-path=/srv --export-all --enable=receive-pack --reuseaddr --listen=0.0.0.0 /srv"]
      ports: [{ containerPort: 9418 }]
      volumeMounts: [{ name: srv, mountPath: /srv }, { name: tmp, mountPath: /tmp }]
  volumes: [{ name: srv, emptyDir: {} }, { name: tmp, emptyDir: {} }]
---
apiVersion: v1
kind: Service
metadata: { name: git }
spec: { selector: { app: git }, ports: [{ port: 9418, targetPort: 9418 }] }
EOF
kn wait pod/git --for=condition=Ready --timeout=5m >/dev/null || { say "the git pod never became Ready"; exit 1; }
kn exec git -- bash -c 'set -e; cd /tmp && rm -rf w && git clone -q /srv/repo.git w && cd w && echo hi > README && git add . && git -c user.name=t -c user.email=t@t commit -qm init && git push -q origin HEAD:main' ||
  { say "could not seed the repository"; exit 1; }

jq -n --arg class "$CLASS" '{
  name: "default",
  credentials: { claude: { secretName: "claude-credentials" } },
  storage: { shared: { storageClassName: $class } },
  plugins: { flow: { enabled: true }, web: { enabled: true } }
}' >"$ROOT/daemon-values.json"
helm upgrade --install balerix "$daemon_chart" "${version_args[@]}" --namespace "$NS" \
  -f "$ROOT/daemon-values.json" --wait --timeout 5m || { say "the daemon chart did not install"; exit 1; }
helm list -n "$NS" | sed 's/^/  /'

hr "B. Daemon and Plugins"
if kn wait daemons.balerix.ai/default --for=condition=Ready --timeout=20m >/dev/null; then ok "Daemon Ready"; else bad "Daemon Ready"; fi
if kn wait plugins.balerix.ai/flow plugins.balerix.ai/web --for=condition=Ready --timeout=10m >/dev/null; then ok "flow and web Ready"; else bad "flow and web Ready"; fi

hr "C. the Fleet, real claude in two pods"
kn apply -f - >/dev/null <<EOF
apiVersion: balerix.ai/v1alpha1
kind: Fleet
metadata: { name: payments }
spec:
  daemon: default
  retain: None
  defaults:
    claude:
      settings: { model: sonnet, permissions: { allow: ["Bash(git *)"] } }
      args: ["--verbose"]
      resume: true
    sandbox:
      network: { block: false }
    tools: { node: "22.11.0" }
    env: { RUST_LOG: info }
    runner: { type: pod }
    plugins:
      web: {}
  crews:
    backend:
      repo: git://git.$NS.svc:9418/repo.git
      ref: main
      git: { push: false, auth: none }
      defaults:
        tools: { python: "3.12.8" }
      agents:
        alice:
          plugins:
            flow:
              initial: working
              states:
                working:
                  on:
                    - event: PreToolUse
                      match: { /tool_input/command: "rm -rf.*" }
                      respond: { decision: block, reason: "no recursive deletes" }
                    - event: Stop
                      goto: review
                      send: { text: "$FLOW_TEXT" }
                review:
                  on:
                    - event: Stop
                      goto: done
                done: {}
        bob:
          claude: { settings: { model: opus } }
EOF
if kn wait fleets.balerix.ai/payments --for=condition=Ready --timeout=30m >/dev/null; then
  ok "Fleet Ready: both agents' SessionStart arrived from the pods"
else
  bad "Fleet Ready"
  kn get fleets.balerix.ai,crews.balerix.ai,agents.balerix.ai,pods,jobs -o wide
fi
kn get agents.balerix.ai -o custom-columns=NAME:.metadata.name,PHASE:.status.phase,READY:'.status.conditions[?(@.type=="Ready")].status' | sed 's/^/  /'

hr "D. a flow rule on alice's Stop"
alice=payments-backend-alice
# shellcheck disable=SC2016
window=$(in_agent "$alice" "tmux -u -S /balerix/run/tmux.sock list-windows -a -F '#{window_id} #{window_name}'" 2>/dev/null | awk '$2 == "alice" { print $1; exit }')
transcript_has() { in_agent "$alice" "grep -rlF '$1' /balerix/agent --include='*.jsonl' 2>/dev/null | head -n 1" | grep -q .; }
if [[ -z $window ]]; then
  bad "alice's tmux window not found"
else
  # shellcheck disable=SC2016
  in_agent "$alice" "tmux -u -S /balerix/run/tmux.sock send-keys -t '$window' -l '$PROMPT'"
  # closed loop (#99): Claude's TUI can swallow an Enter while it starts
  submitted=false
  for _ in $(seq 12); do
    # shellcheck disable=SC2016
    in_agent "$alice" "tmux -u -S /balerix/run/tmux.sock send-keys -t '$window' Enter"
    if wait_until 10 transcript_has "$PROMPT"; then submitted=true; break; fi
  done
  if ! $submitted; then
    bad "the prompt never reached alice's transcript"
  elif wait_until 300 transcript_has "$FLOW_TEXT"; then
    ok "flow sent '$FLOW_TEXT' after alice's first Stop"
  else
    bad "flow's text never reached alice's transcript"
  fi
fi

hr "E. evict alice: same claim, Ready again, home and session intact"
in_agent "$alice" "touch /balerix/agent/verify-k8s-marker" || true
uid=$(kn get pod "$alice" -o jsonpath='{.metadata.uid}')
kn delete pod "$alice" --wait=false >/dev/null
new_ready() {
  [[ $(kn get pod "$alice" -o jsonpath='{.metadata.uid}') != "$uid" ]] &&
    kn wait pod "$alice" --for=condition=Ready --timeout=1s &&
    kn wait agents.balerix.ai/"$alice" --for=condition=Ready --timeout=1s
}
if wait_until 900 new_ready; then
  ok "alice's new pod Ready"
  if in_agent "$alice" "test -f /balerix/agent/verify-k8s-marker"; then ok "the claim kept the marker"; else bad "marker gone"; fi
  if transcript_has "$FLOW_TEXT"; then ok "the session transcript survived"; else bad "transcript gone"; fi
else
  bad "alice's new pod never Ready"
fi

hr "F. drop bob: harvested into the crew cache"
: >"$ROOT/harvest"
(
  for _ in $(seq 400); do
    m=$(kn get pods -l job-name=payments-backend-bob-harvest -o jsonpath='{.items[*].status.containerStatuses[*].state.terminated.message}' 2>/dev/null)
    if [[ -n $m ]]; then printf '%s\n' "$m" >"$ROOT/harvest"; exit 0; fi
    sleep 2
  done
) &
watcher=$!
kn patch fleets.balerix.ai payments --type merge -p '{"spec":{"crews":{"backend":{"agents":{"bob":null}}}}}' >/dev/null
if wait_until 900 sh -c "! kubectl -n $NS get agents.balerix.ai payments-backend-bob"; then ok "bob's Agent gone"; else bad "bob's Agent still there"; fi
kill "$watcher" 2>/dev/null; wait "$watcher" 2>/dev/null
branch=$(sed -n 's/^harvested //p' "$ROOT/harvest" | head -n 1)
if [[ -z $branch ]]; then
  bad "no 'harvested <branch>' message seen from the harvest Job ($(cat "$ROOT/harvest"))"
else
  kn run probe --image="$agent_image" --restart=Never --overrides='{"spec":{"containers":[{"name":"probe","image":"'"$agent_image"'","command":["sleep","infinity"],"volumeMounts":[{"name":"shared","mountPath":"/balerix/volume"}]}],"volumes":[{"name":"shared","persistentVolumeClaim":{"claimName":"balerix-default-shared"}}]}}' >/dev/null
  kn wait pod/probe --for=condition=Ready --timeout=5m >/dev/null
  if kn exec probe -- git --git-dir=/balerix/volume/fleets/payments/crews/backend/repo/.git branch --list "$branch" | grep -q .; then
    ok "branch $branch in the crew cache"
  else
    bad "branch $branch not in the crew cache"
  fi
  kn delete pod probe --wait=false >/dev/null
fi

hr "G. delete the Fleet"
if kn delete fleets.balerix.ai payments --wait --timeout=10m >/dev/null; then ok "Fleet deleted"; else bad "Fleet delete timed out"; fi

hr "report"
if ((${#FAILED[@]})); then
  say "${#FAILED[@]} check(s) failed:"
  printf '  %s\n' "${FAILED[@]}"
  say "state kept: namespace $NS; \`mise run verify-k8s -- down\` removes it"
  exit 1
fi
say "verify-k8s: every check passed (--from $FROM)"
say "\`mise run verify-k8s -- down\` removes the namespaces and both releases"
