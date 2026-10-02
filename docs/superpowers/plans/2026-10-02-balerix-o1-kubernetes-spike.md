# Spec O, part 1: the Kubernetes spike — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Answer the three questions Spec O §17.1 leaves open, with evidence, and record the answers in the spec.

**Architecture:** Throwaway shell probes under `spikes/k8s/` on a branch that is never merged. Each probe applies a pod, runs commands with `kubectl exec`, and appends one row per check to `RESULTS.md`. A workflow on that branch runs the probes on `kind` in GitHub Actions, because this development container has no Docker. The same scripts run against a managed cluster through a kubectl context. Only the spec amendment merges.

**Tech Stack:** bash, kubectl 1.37.1, kind 0.33.0, tmux 3.7c and nono 0.79.0 (the repository's pins, copied into pods with `kubectl cp`), csi-driver-nfs v4.13.4, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md` (§6.1, §6.4, §8.1, §17.1)

## Global Constraints

- A spike's output is an answer. A FAIL row is a valid result, not an error to fix; never weaken a pod's security settings to turn a FAIL into a PASS.
- Every probe pod runs under these settings, copied from spec §6.1: uid 10001, `runAsNonRoot`, read-only root filesystem, all capabilities dropped, `allowPrivilegeEscalation: false`, `seccompProfile: RuntimeDefault`, `automountServiceAccountToken: false`.
- The probe namespace enforces the `restricted` Pod Security Standard, so the API server refuses a pod that does not meet it.
- The sidecar is a native sidecar: an init container with `restartPolicy: Always`. Minimum Kubernetes is 1.29; the oldest node image kind 0.33.0 ships is 1.34.11, which is the oldest version probed.
- Tool versions are exact: kind `0.33.0`, kubectl `1.37.1`, tmux and nono from the repository's `mise.toml`.
- Nothing under `spikes/` or `.github/workflows/spike-k8s.yml` reaches `main`. The branch `spike/k8s` is deleted when the spec amendment is committed.
- Base image, by digest: `debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132` (the one `docker/balerix/Dockerfile` pins).
- nono is invoked as balerix invokes it: `nono -s --log-file <file> run --profile <profile> -- <command>`, validated with `nono -s profile validate <profile>`, with its own `HOME` outside any granted directory.

## Review Focus

Conditions the spec implies and that are most likely to surprise someone later. Each has a check in the task that owns it.

1. **A node that denies Landlock.** The spec says the agent reports `SandboxUnavailable` and runs nothing. If nono instead runs the command unsandboxed, O-4 is false. Check P2.7 (Task 3).
2. **Pids across containers.** `TmuxRunner` reads `#{pane_pid}`; in a sidecar that pid belongs to another PID namespace. Check P1.6 (Task 2) records whether the sidecar can see it.
3. **A ReadWriteMany class that only works on one node.** kind's default class may bind a ReadWriteMany claim and still give two nodes two different directories. Check P3.3 pins the two pods to different nodes (Task 4).
4. **uid 10001 on shared storage.** `fsGroup` does not apply to NFS, so a non-root Job may be unable to write the cache. Check P3.2 writes as uid 10001 (Task 4).
5. **A read-only sub-path mount.** The spec relies on the mount flag, not on file modes, to keep agents out of the cache. Check P3.4 attempts a write through it (Task 4).

---

## File Structure

All on branch `spike/k8s`, cut from `docs/spec-o-kubernetes`:

| File | Responsibility |
|---|---|
| `spikes/k8s/lib.sh` | `check`, `info`, `inject`; the kubectl wrapper; the results table |
| `spikes/k8s/run.sh` | creates the namespace, runs the chosen probes, prints `RESULTS.md` |
| `spikes/k8s/kind.yaml` | a three-node cluster with the seccomp directory mounted |
| `spikes/k8s/seccomp/no-landlock.json` | a seccomp profile that answers ENOSYS to the Landlock syscalls |
| `spikes/k8s/p1-tmux.sh`, `tmux-pod.yaml` | question 1 |
| `spikes/k8s/p2-landlock.sh`, `landlock-pod.yaml` | question 2 |
| `spikes/k8s/p3-rwx.sh`, `rwx-claim.yaml`, `rwx-writer.yaml`, `rwx-reader.yaml`, `nfs-class.yaml` | question 3 |
| `.github/workflows/spike-k8s.yml` | runs everything on `kind`, on push to `spike/k8s` |

On branch `docs/spec-o-kubernetes`: the spec gains §19 (Task 6).

---

### Task 1: Harness and a cluster in CI

**Files:**
- Create: `spikes/k8s/lib.sh`, `spikes/k8s/run.sh`, `spikes/k8s/kind.yaml`, `spikes/k8s/seccomp/no-landlock.json`, `.github/workflows/spike-k8s.yml`

**Interfaces:**
- Produces, for Tasks 2–5:
  - `K` — bash array, `kubectl [--context $KCONTEXT] -n $NS`
  - `check <id> <description> <command…>` — runs the command; appends `| id | PASS/FAIL | description [— output tail] |` to `$RESULTS`
  - `info <id> <text>` — appends an `INFO` row
  - `inject <pod> <container>` — copies the pinned `nono` and `tmux` into the pod's `/spike/bin`
  - environment: `NS` (default `balerix-spike`), `KCONTEXT` (optional), `SPIKE_KIND` (`1` on kind), `RESULTS`
  - `run.sh <probe…>` where a probe is `p1`, `p2` or `p3`

- [ ] **Step 1: Create the branch**

```bash
git checkout docs/spec-o-kubernetes
git checkout -b spike/k8s
mkdir -p spikes/k8s/seccomp
```

- [ ] **Step 2: Write `spikes/k8s/lib.sh`**

```bash
#!/usr/bin/env bash
# Shared by the probes. Throwaway (Spec O §17.1): never merged.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
RESULTS=${RESULTS:-$HERE/RESULTS.md}
NS=${NS:-balerix-spike}
SPIKE_KIND=${SPIKE_KIND:-0}
K=(kubectl ${KCONTEXT:+--context "$KCONTEXT"} -n "$NS")

row() { printf '| %s | %s | %s |\n' "$1" "$2" "$(tr '\n|' ' /' <<<"$3")" >>"$RESULTS"; }

# check <id> <description> <command...>: a FAIL is recorded, never fatal.
check() {
  local id=$1 desc=$2 out
  shift 2
  if out=$("$@" 2>&1); then
    row "$id" PASS "$desc"
  else
    row "$id" FAIL "$desc — $(tail -c 400 <<<"$out")"
  fi
}

info() { row "$1" INFO "$2"; }

# inject <pod> <container>: the repository's pinned nono and tmux.
inject() {
  local tool
  for tool in nono tmux; do
    "${K[@]}" cp "$(mise which "$tool")" "$1:/spike/bin/$tool" -c "$2" || return 1
  done
}
```

- [ ] **Step 3: Write `spikes/k8s/run.sh`**

```bash
#!/usr/bin/env bash
# usage: run.sh <p1|p2|p3>...   (KCONTEXT, NS, SPIKE_KIND from the environment)
set -uo pipefail
cd "$(dirname "$0")"
# shellcheck source=spikes/k8s/lib.sh
source lib.sh

[[ $# -ge 1 ]] || { echo "usage: $0 <p1|p2|p3>..." >&2; exit 2; }
kubectl ${KCONTEXT:+--context "$KCONTEXT"} version -o json >/dev/null || { echo "no cluster" >&2; exit 2; }

kubectl ${KCONTEXT:+--context "$KCONTEXT"} create namespace "$NS" --dry-run=client -o yaml \
  | kubectl ${KCONTEXT:+--context "$KCONTEXT"} apply -f -
kubectl ${KCONTEXT:+--context "$KCONTEXT"} label namespace "$NS" --overwrite \
  pod-security.kubernetes.io/enforce=restricted pod-security.kubernetes.io/enforce-version=latest

{
  echo "## Spike results: $(kubectl ${KCONTEXT:+--context "$KCONTEXT"} version -o json | jq -r .serverVersion.gitVersion)"
  echo
  echo "| Check | Result | What |"
  echo "|---|---|---|"
} >"$RESULTS"

for probe in "$@"; do
  case $probe in
    p1) bash p1-tmux.sh ;;
    p2) bash p2-landlock.sh ;;
    p3) bash p3-rwx.sh ;;
    *) echo "unknown probe: $probe" >&2; exit 2 ;;
  esac
done
cat "$RESULTS"
```

- [ ] **Step 4: Write `spikes/k8s/kind.yaml`**

Three nodes, because question 3 needs two workers. The seccomp directory is mounted where the kubelet looks for `Localhost` profiles. `@SECCOMP@` is replaced with an absolute path by the workflow.

```yaml
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
name: spike
nodes:
  - role: control-plane
  - role: worker
    extraMounts:
      - { hostPath: "@SECCOMP@", containerPath: /var/lib/kubelet/seccomp/profiles }
  - role: worker
    extraMounts:
      - { hostPath: "@SECCOMP@", containerPath: /var/lib/kubelet/seccomp/profiles }
```

- [ ] **Step 5: Write `spikes/k8s/seccomp/no-landlock.json`**

Allows everything except the three Landlock syscalls, which answer ENOSYS (38), as a kernel without Landlock does.

```json
{
  "defaultAction": "SCMP_ACT_ALLOW",
  "syscalls": [
    {
      "names": ["landlock_create_ruleset", "landlock_add_rule", "landlock_restrict_self"],
      "action": "SCMP_ACT_ERRNO",
      "errnoRet": 38
    }
  ]
}
```

- [ ] **Step 6: Write `.github/workflows/spike-k8s.yml`**

A workflow that exists only on a branch cannot be dispatched by hand, so it runs on push. Action pins are the ones `ci.yml` uses.

```yaml
name: spike-k8s
# Throwaway (Spec O §17.1). Lives on spike/k8s only; never merged.
on:
  push:
    branches: [spike/k8s]
permissions:
  contents: read
env:
  MISE_AUTO_INSTALL: "false"
jobs:
  kind:
    runs-on: ubuntu-24.04
    strategy:
      fail-fast: false
      matrix:
        node:
          - kindest/node:v1.37.0@sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5
          - kindest/node:v1.34.11@sha256:44e222ee2132dab25ff87301682f89eb82c7880ea3a1bf543bfe9708fd08d67d
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: mise install nono tmux jq
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
      - name: Runner kernel
        run: |
          uname -r
          cat /sys/kernel/security/lsm || true
      - name: Create the cluster
        env:
          NODE: ${{ matrix.node }}
        run: |
          sed "s|@SECCOMP@|$PWD/spikes/k8s/seccomp|" spikes/k8s/kind.yaml > "$RUNNER_TEMP/kind.yaml"
          mise x kind@0.33.0 -- kind create cluster --config "$RUNNER_TEMP/kind.yaml" --image "$NODE" --wait 180s
      - name: Probes
        env:
          SPIKE_KIND: "1"
        run: |
          # shellcheck disable=SC2046
          mise x kubectl@1.37.1 jq -- spikes/k8s/run.sh $(cat spikes/k8s/PROBES) || true
          cat spikes/k8s/RESULTS.md >> "$GITHUB_STEP_SUMMARY"
      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1
        if: always()
        with:
          name: results-${{ strategy.job-index }}
          path: spikes/k8s/RESULTS.md
```

`spikes/k8s/PROBES` is one line naming the probes to run (in the end `p1 p2 p3`); the workflow splits it into arguments. Tasks 3 and 4 extend it. For this task, write a trivial `p1-tmux.sh` so the harness itself is what is tested:

```bash
#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")"
source lib.sh
check P0.1 "the harness reaches the cluster" "${K[@]}" get serviceaccount default
check P0.2 "the harness records a failure" false
```

and `echo p1 > spikes/k8s/PROBES`.

- [ ] **Step 7: Lint what can be linted locally**

Run: `mise x -- shellcheck spikes/k8s/*.sh && mise x -- actionlint .github/workflows/spike-k8s.yml`
Expected: no output.

- [ ] **Step 8: Commit and push; confirm the push with the user first**

Pushing creates a branch on `balerix-ai/balerix` and starts a workflow run. Ask the user before the first push.

```bash
chmod +x spikes/k8s/*.sh
git add spikes .github/workflows/spike-k8s.yml
git commit -m "chore(spike): a kind harness for Spec O's open questions"
git push -u origin spike/k8s
gh run watch "$(gh run list --branch spike/k8s --workflow spike-k8s --limit 1 --json databaseId --jq '.[0].databaseId')"
```

Expected: both matrix legs finish; each job summary shows `P0.1 PASS` and `P0.2 FAIL`. That FAIL proves a failure is recorded and does not stop the run.

If `--image` with a digest is refused or the cluster does not come up, read the `Create the cluster` log; `kind export logs` is the next source.

---

### Task 2: Question 1 — tmux across two containers

**Files:**
- Create: `spikes/k8s/tmux-pod.yaml`
- Replace: `spikes/k8s/p1-tmux.sh`

**Interfaces:**
- Consumes: `K`, `check`, `info`, `inject` from `lib.sh`.
- Produces: rows `P1.1`–`P1.10` in `RESULTS.md`.

- [ ] **Step 1: Write `spikes/k8s/tmux-pod.yaml`**

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: tmux
spec:
  automountServiceAccountToken: false
  restartPolicy: Never
  securityContext:
    runAsNonRoot: true
    runAsUser: 10001
    runAsGroup: 10001
    fsGroup: 10001
    seccompProfile: { type: RuntimeDefault }
  initContainers:
    - name: sidecar
      restartPolicy: Always
      image: debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132
      command: ["sleep", "infinity"]
      env:
        - { name: HOME, value: /balerix/run/home-sidecar }
      securityContext: &hardened
        allowPrivilegeEscalation: false
        readOnlyRootFilesystem: true
        capabilities: { drop: ["ALL"] }
      volumeMounts: &mounts
        - { name: run, mountPath: /balerix/run }
        - { name: bin, mountPath: /spike/bin }
  containers:
    - name: agent
      image: debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132
      command: ["sleep", "infinity"]
      env:
        - { name: HOME, value: /balerix/run/home-agent }
      securityContext: *hardened
      volumeMounts: *mounts
  volumes:
    - { name: run, emptyDir: {} }
    - { name: bin, emptyDir: {} }
```

- [ ] **Step 2: Write `spikes/k8s/p1-tmux.sh`**

The pane runs a recorder that appends each line it reads to a file and echoes it, so both directions can be asserted. The format string in P1.6 is `TmuxRunner`'s `WINDOW_FORMAT`. P1.7 attaches the way `TmuxRunner::attach` does: a new session grouped with the crew's, inside a PTY (`script` allocates one).

```bash
#!/usr/bin/env bash
# Question 1 (Spec O §6.4): a tmux client in the sidecar container drives
# and attaches to a tmux server in the agent container.
set -uo pipefail
cd "$(dirname "$0")"
# shellcheck source=spikes/k8s/lib.sh
source lib.sh

SOCK=/balerix/run/tmux.sock
T=(/spike/bin/tmux -S "$SOCK")
ex() { local c=$1; shift; "${K[@]}" exec -i tmux -c "$c" -- "$@"; }
received() { ex agent grep -qxF "$1" /balerix/run/received; }

"${K[@]}" delete pod tmux --ignore-not-found --wait
"${K[@]}" apply -f tmux-pod.yaml
check P1.0 "a hardened two-container pod with a native sidecar is admitted and runs" \
  "${K[@]}" wait --for=condition=Ready pod/tmux --timeout=180s
inject tmux agent || info P1.0 "inject failed; every later row is void"

ex agent sh -c 'mkdir -p "$HOME" && cat > /balerix/run/record.sh && chmod +x /balerix/run/record.sh' <<'EOF'
#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> /balerix/run/received
  printf 'got:%s\n' "$line"
done
EOF
ex sidecar sh -c 'mkdir -p "$HOME"'

check P1.1 "the server starts in the agent container and outlives the exec that started it" \
  ex agent "${T[@]}" new-session -d -s crew -n agent -x 200 -y 50 /balerix/run/record.sh
ex agent "${T[@]}" set-option -t crew remain-on-exit on

check P1.2 "the sidecar sees the session through the shared socket" \
  ex sidecar "${T[@]}" has-session -t =crew

p1_3() {
  ex sidecar "${T[@]}" send-keys -t =crew:agent -l 'hello one' &&
    ex sidecar "${T[@]}" send-keys -t =crew:agent Enter && sleep 1 && received 'hello one'
}
check P1.3 "send-keys -l from the sidecar reaches the pane (send_text, short)" p1_3

p1_4() {
  printf 'line a\nline b' | ex sidecar "${T[@]}" load-buffer -b spike - &&
    ex sidecar "${T[@]}" paste-buffer -p -d -b spike -t =crew:agent &&
    ex sidecar "${T[@]}" send-keys -t =crew:agent Enter && sleep 1 &&
    received 'line a' && received 'line b'
}
check P1.4 "load-buffer from the client's stdin and paste-buffer -p work (send_text, long)" p1_4

p1_5() { ex sidecar "${T[@]}" capture-pane -p -t =crew:agent | grep -qF 'got:hello one'; }
check P1.5 "capture-pane from the sidecar reads the pane" p1_5

fmt='#{window_name}\t#{pane_dead}\t#{pane_pid}\t#{pane_dead_status}'
windows=$(ex sidecar "${T[@]}" list-windows -t =crew -F "$fmt" 2>&1)
pid=$(cut -f3 <<<"$windows")
if ex sidecar test -d "/proc/$pid"; then seen="visible"; else seen="NOT visible"; fi
info P1.6 "list-windows from the sidecar: '$windows'; pane pid $pid is $seen in the sidecar's /proc"

p1_7() {
  local out
  out=$( (sleep 3; printf 'from attach\r'; sleep 2; printf '\002d'; sleep 1) |
    ex sidecar env TERM=xterm-256color script -qfec \
      "/spike/bin/tmux -S $SOCK new-session -t crew -s balerix-attach-1" /dev/null 2>&1)
  received 'from attach' && grep -qF 'got:from attach' <<<"$out"
}
check P1.7 "a grouped attach session in a PTY in the sidecar types into and reads the pane" p1_7
ex sidecar "${T[@]}" kill-session -t =balerix-attach-1 2>/dev/null

p1_8() {
  ex sidecar "${T[@]}" send-keys -t =crew:agent C-d && sleep 1 &&
    ex sidecar "${T[@]}" list-windows -t =crew -F "$fmt" | cut -f2 | grep -qx 1
}
check P1.8 "the sidecar observes the pane's exit (pane_dead=1 under remain-on-exit)" p1_8

p1_9() {
  ex sidecar "${T[@]}" respawn-window -k -t =crew:agent /balerix/run/record.sh &&
    ex sidecar "${T[@]}" send-keys -t =crew:agent -l 'after respawn' &&
    ex sidecar "${T[@]}" send-keys -t =crew:agent Enter && sleep 1 && received 'after respawn'
}
check P1.9 "respawn-window from the sidecar restarts the command in the agent container" p1_9

p1_10() {
  ex sidecar "${T[@]}" kill-session -t =crew && ! ex sidecar "${T[@]}" has-session -t =crew
}
check P1.10 "kill-session from the sidecar ends the crew session" p1_10
```

- [ ] **Step 3: Lint**

Run: `mise x -- shellcheck spikes/k8s/*.sh`
Expected: no output.

- [ ] **Step 4: Commit, push, read the results**

```bash
git add spikes/k8s && git commit -m "chore(spike): tmux across two containers" && git push
gh run watch "$(gh run list --branch spike/k8s --workflow spike-k8s --limit 1 --json databaseId --jq '.[0].databaseId')"
gh run view --log | grep -E '^\S+\s+Probes.*\| P1\.' | sed 's/^.*| P1/| P1/'
```

Expected if the design holds: `P1.0`–`P1.5` and `P1.7`–`P1.10` PASS on both Kubernetes versions; `P1.6` INFO reads "NOT visible".

- [ ] **Step 5: Interpret**

- All PASS: §6.4's fallback is not taken.
- `P1.7` FAIL with the others passing: read the row's output tail. If the cause is the probe (no `xterm-256color` terminfo for the injected tmux, `script` flags), fix the probe and rerun; that is a harness defect, not an answer. If tmux itself refuses the attach across containers, the answer is that attach needs the fallback or another mechanism; record it as such.
- `P1.2` FAIL: the socket is not usable across containers. §6.4's fallback is taken.
- `P1.6` "NOT visible" is an expected finding to carry into sub-project 2: the sidecar must treat `pane_pid` as opaque, and anything that reads `/proc/<pid>` (`agent-supervise`) must run in the agent container.

---

### Task 3: Question 2 — nono and Landlock in a hardened pod

**Files:**
- Create: `spikes/k8s/landlock-pod.yaml`, `spikes/k8s/p2-landlock.sh`
- Modify: `spikes/k8s/PROBES`

**Interfaces:**
- Consumes: `K`, `check`, `info`, `inject`, `SPIKE_KIND` from `lib.sh`; the `no-landlock.json` profile mounted by `kind.yaml`.
- Produces: rows `P2.0`–`P2.7`.

- [ ] **Step 1: Write `spikes/k8s/landlock-pod.yaml`**

`@NAME@` and `@SECCOMP@` are substituted by the probe, so the same pod runs under `RuntimeDefault` and under the profile that denies Landlock.

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: "@NAME@"
spec:
  automountServiceAccountToken: false
  restartPolicy: Never
  securityContext:
    runAsNonRoot: true
    runAsUser: 10001
    runAsGroup: 10001
    fsGroup: 10001
    seccompProfile: @SECCOMP@
  containers:
    - name: agent
      image: debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132
      command: ["sleep", "infinity"]
      securityContext:
        allowPrivilegeEscalation: false
        readOnlyRootFilesystem: true
        capabilities: { drop: ["ALL"] }
      volumeMounts:
        - { name: agent, mountPath: /balerix/agent }
        - { name: bin, mountPath: /spike/bin }
  volumes:
    - { name: agent, emptyDir: {} }
    - { name: bin, emptyDir: {} }
```

- [ ] **Step 2: Write `spikes/k8s/p2-landlock.sh`**

The profile has the shape `sandbox.rs` renders: `filesystem.read` for read-only grants, `filesystem.allow` for read-write, `workdir.access: none`, `deny_vars: ["*"]`. nono's own `HOME` is `/balerix/agent/nono`, outside every grant, as the architecture requires.

```bash
#!/usr/bin/env bash
# Question 2 (Spec O O-4): nono under Landlock inside a hardened pod, and
# what happens where Landlock is denied.
set -uo pipefail
cd "$(dirname "$0")"
# shellcheck source=spikes/k8s/lib.sh
source lib.sh

A=/balerix/agent
start() { # start <name> <seccomp profile as inline yaml>
  "${K[@]}" delete pod "$1" --ignore-not-found --wait
  sed -e "s|@NAME@|$1|" -e "s|@SECCOMP@|$2|" landlock-pod.yaml | "${K[@]}" apply -f - &&
    "${K[@]}" wait --for=condition=Ready "pod/$1" --timeout=180s &&
    inject "$1" agent &&
    "${K[@]}" exec -i "$1" -- sh -c "mkdir -p $A/allowed $A/denied $A/nono && echo secret > $A/denied/secret && cat > $A/profile.json" <<EOF
{
  "meta": { "name": "balerix-spike", "description": "Spec O spike" },
  "filesystem": {
    "read": ["/usr", "/lib", "/lib64", "/bin", "/etc", "/spike/bin"],
    "allow": ["$A/allowed"]
  },
  "workdir": { "access": "none" },
  "network": { "block": true },
  "environment": { "deny_vars": ["*"], "set_vars": { "PATH": "/usr/bin:/bin" } }
}
EOF
}
# sandboxed <pod> <shell command>: as launch.sh invokes nono.
sandboxed() {
  "${K[@]}" exec "$1" -- env -i HOME=$A/nono PATH=/spike/bin:/usr/bin:/bin \
    nono -s --log-file $A/nono.log run --profile $A/profile.json -- /bin/bash -c "$2"
}
plain() { "${K[@]}" exec "$1" -- /bin/bash -c "$2"; }

check P2.0 "the hardened pod runs and takes the binaries" start landlock '{ type: RuntimeDefault }'
info P2.0 "kernel $(plain landlock 'uname -r' 2>&1); lsm: $(plain landlock 'cat /sys/kernel/security/lsm' 2>&1 | tail -c 120)"

check P2.1 "nono validates the profile" \
  "${K[@]}" exec landlock -- env -i HOME=$A/nono PATH=/spike/bin:/usr/bin:/bin nono -s profile validate $A/profile.json

p2_2() { sandboxed landlock "echo ok > $A/allowed/f" && plain landlock "grep -qx ok $A/allowed/f"; }
check P2.2 "a sandboxed write inside the read-write grant succeeds" p2_2

p2_3() { ! sandboxed landlock "echo no > $A/denied/f" && ! plain landlock "test -e $A/denied/f"; }
check P2.3 "a sandboxed write outside the grants is refused" p2_3

p2_4() { ! sandboxed landlock "cat $A/denied/secret"; }
check P2.4 "a sandboxed read outside the grants is refused" p2_4

p2_5() { ! sandboxed landlock "echo x > $A/profile.json" && plain landlock "grep -q balerix-spike $A/profile.json"; }
check P2.5 "the sandboxed process cannot rewrite its own profile" p2_5

if plain landlock 'exec 3<>/dev/tcp/1.1.1.1/53' 2>/dev/null; then
  p2_6() { ! sandboxed landlock 'exec 3<>/dev/tcp/1.1.1.1/53'; }
  check P2.6 "network.block refuses an outbound TCP connection the pod itself can make" p2_6
else
  info P2.6 "skipped: the pod has no egress to 1.1.1.1:53 even unsandboxed"
fi
info P2.6 "nono.log tail: $(plain landlock "tail -n 3 $A/nono.log" 2>&1 | tail -c 300)"

if [[ $SPIKE_KIND == 1 ]]; then
  if start no-landlock '{ type: Localhost, localhostProfile: profiles/no-landlock.json }'; then
    out=$(sandboxed no-landlock "echo ran > $A/allowed/f" 2>&1)
    code=$?
    p2_7() { [[ $code -ne 0 ]] && ! plain no-landlock "test -e $A/allowed/f"; }
    check P2.7 "where Landlock is denied, nono exits non-zero and the command does not run" p2_7
    info P2.7 "exit $code; nono said: $(tail -c 300 <<<"$out")"
  else
    info P2.7 "the no-landlock pod did not start; fail-closed is unproven"
  fi
else
  info P2.7 "skipped: needs a Localhost seccomp profile on the nodes (kind only)"
fi
```

- [ ] **Step 3: Enable the probe and lint**

```bash
echo "p1 p2" > spikes/k8s/PROBES
```

Run: `mise x -- shellcheck spikes/k8s/*.sh && mise x -- actionlint .github/workflows/spike-k8s.yml`
Expected: no output.

- [ ] **Step 4: Commit, push, read the results**

```bash
git add spikes/k8s
git commit -m "chore(spike): nono and Landlock in a hardened pod" && git push
gh run watch "$(gh run list --branch spike/k8s --workflow spike-k8s --limit 1 --json databaseId --jq '.[0].databaseId')"
```

Expected if the design holds: `P2.0`–`P2.6` PASS, `P2.7` PASS, and `P2.7`'s INFO row carries nono's exact message and exit code.

- [ ] **Step 5: Interpret**

- `P2.2` FAIL with a seccomp or permission message: Landlock is not usable under `RuntimeDefault` on this runtime. That contradicts O-4 for this cluster; record the runtime (containerd version from `kubectl get nodes -o wide`) and the message. Do not switch the pod to `Unconfined` to get a PASS.
- `P2.1` FAIL naming `/lib64`: the path does not exist on this architecture and nono refuses it. Record it; the real profile's `SYSTEM_READ` has the same entry.
- `P2.7` FAIL because the file exists: nono ran the command without a sandbox. O-4's "failing closed" then needs its own check in the sidecar before `launch.sh`; record that as a requirement for sub-project 2.
- `P2.7`'s message and exit code are what the sidecar will map to `SandboxUnavailable`; copy them verbatim into the spec in Task 6.

---

### Task 4: Question 3 — a ReadWriteMany class on kind

**Files:**
- Create: `spikes/k8s/rwx-claim.yaml`, `spikes/k8s/rwx-writer.yaml`, `spikes/k8s/rwx-reader.yaml`, `spikes/k8s/nfs-class.yaml`, `spikes/k8s/p3-rwx.sh`
- Modify: `spikes/k8s/PROBES`

**Interfaces:**
- Consumes: `K`, `check`, `info`, `SPIKE_KIND`.
- Produces: rows `P3.<class>.1`–`.5` for each candidate class.

- [ ] **Step 1: Write the claim, the writer and the reader**

The writer stands for the sync Job (read-write on the whole volume); the reader stands for an agent pod (a read-only sub-path of the crew's objects). `@CLASS@` and `@NODE@` are substituted by the probe.

`spikes/k8s/rwx-claim.yaml`:

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: shared
spec:
  accessModes: ["ReadWriteMany"]
  storageClassName: "@CLASS@"
  resources: { requests: { storage: 1Gi } }
```

`spikes/k8s/rwx-writer.yaml`:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: writer
spec:
  nodeName: "@NODE@"
  automountServiceAccountToken: false
  restartPolicy: Never
  securityContext:
    runAsNonRoot: true
    runAsUser: 10001
    runAsGroup: 10001
    fsGroup: 10001
    seccompProfile: { type: RuntimeDefault }
  containers:
    - name: c
      image: debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132
      command: ["sleep", "infinity"]
      securityContext:
        allowPrivilegeEscalation: false
        readOnlyRootFilesystem: true
        capabilities: { drop: ["ALL"] }
      volumeMounts:
        - { name: shared, mountPath: /shared }
  volumes:
    - name: shared
      persistentVolumeClaim: { claimName: shared }
```

`spikes/k8s/rwx-reader.yaml`:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: reader
spec:
  nodeName: "@NODE@"
  automountServiceAccountToken: false
  restartPolicy: Never
  securityContext:
    runAsNonRoot: true
    runAsUser: 10001
    runAsGroup: 10001
    fsGroup: 10001
    seccompProfile: { type: RuntimeDefault }
  containers:
    - name: c
      image: debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132
      command: ["sleep", "infinity"]
      securityContext:
        allowPrivilegeEscalation: false
        readOnlyRootFilesystem: true
        capabilities: { drop: ["ALL"] }
      volumeMounts:
        - name: shared
          mountPath: /cache/objects
          subPath: fleets/f/crews/c/repo/.git/objects
          readOnly: true
  volumes:
    - name: shared
      persistentVolumeClaim: { claimName: shared }
```

- [ ] **Step 2: Write `spikes/k8s/nfs-class.yaml`**

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: nfs-csi
provisioner: nfs.csi.k8s.io
parameters:
  server: nfs-server.default.svc.cluster.local
  share: /
reclaimPolicy: Delete
volumeBindingMode: Immediate
mountOptions: ["nfsvers=4.1"]
```

- [ ] **Step 3: Write `spikes/k8s/p3-rwx.sh`**

The reader starts only after the writer has created the sub-path, as an agent pod starts only after the sync Job. `standard` is kind's built-in class; it is tried across two nodes and then on one node, because a one-node answer is still useful for CI.

```bash
#!/usr/bin/env bash
# Question 3 (Spec O O-5, O-6): which ReadWriteMany class works on kind.
set -uo pipefail
cd "$(dirname "$0")"
# shellcheck source=spikes/k8s/lib.sh
source lib.sh

[[ $SPIKE_KIND == 1 ]] || { info P3 "skipped: kind only"; exit 0; }
KC=(kubectl ${KCONTEXT:+--context "$KCONTEXT"})
OBJ=fleets/f/crews/c/repo/.git/objects
apply() { sed -e "s|@CLASS@|${2:-}|" -e "s|@NODE@|${2:-}|" "$1" | "${K[@]}" apply -f -; }
clean() { "${K[@]}" delete pod writer reader --ignore-not-found --wait; "${K[@]}" delete pvc shared --ignore-not-found --wait; }

probe() { # probe <label> <class> <writer node> <reader node>
  local l=$1 class=$2 wn=$3 rn=$4
  clean
  apply rwx-claim.yaml "$class"
  apply rwx-writer.yaml "$wn"
  check "P3.$l.1" "class $class binds a ReadWriteMany claim and the writer starts on $wn" \
    "${K[@]}" wait --for=condition=Ready pod/writer --timeout=180s
  check "P3.$l.2" "uid 10001 creates the crew directory and writes an object" \
    "${K[@]}" exec writer -- sh -c "mkdir -p /shared/$OBJ && echo v1 > /shared/$OBJ/x"
  apply rwx-reader.yaml "$rn"
  p3_3() {
    "${K[@]}" wait --for=condition=Ready pod/reader --timeout=180s &&
      "${K[@]}" exec reader -- grep -qx v1 /cache/objects/x
  }
  check "P3.$l.3" "a reader on $rn sees the writer's object through a sub-path mount" p3_3
  p3_4() { ! "${K[@]}" exec reader -- sh -c 'echo evil > /cache/objects/y'; }
  check "P3.$l.4" "the reader cannot write through the read-only mount" p3_4
  p3_5() {
    "${K[@]}" exec writer -- sh -c "echo v2 > /shared/$OBJ/z" &&
      "${K[@]}" exec reader -- grep -qx v2 /cache/objects/z
  }
  check "P3.$l.5" "an object written after the reader started is visible to it" p3_5
}

probe standard-2node standard spike-worker spike-worker2
probe standard-1node standard spike-worker spike-worker

curl -fsSL https://raw.githubusercontent.com/kubernetes-csi/csi-driver-nfs/v4.13.4/deploy/install-driver.sh |
  bash -s v4.13.4 -- >/dev/null
"${KC[@]}" apply -f https://raw.githubusercontent.com/kubernetes-csi/csi-driver-nfs/v4.13.4/deploy/example/nfs-provisioner/nfs-server.yaml
"${KC[@]}" -n default rollout status deployment/nfs-server --timeout=180s
"${KC[@]}" -n kube-system rollout status deployment/csi-nfs-controller --timeout=180s
"${KC[@]}" -n kube-system rollout status daemonset/csi-nfs-node --timeout=180s
"${KC[@]}" apply -f nfs-class.yaml
probe nfs-2node nfs-csi spike-worker spike-worker2
clean
```

- [ ] **Step 4: Enable, lint, commit, push**

```bash
echo "p1 p2 p3" > spikes/k8s/PROBES
mise x -- shellcheck spikes/k8s/*.sh
git add spikes/k8s && git commit -m "chore(spike): ReadWriteMany candidates on kind" && git push
gh run watch "$(gh run list --branch spike/k8s --workflow spike-k8s --limit 1 --json databaseId --jq '.[0].databaseId')"
```

- [ ] **Step 5: Interpret**

Pick the class for CI by this order:

1. `standard-2node` all PASS: use kind's default class; no extra install.
2. else `nfs-2node` all PASS: use csi-driver-nfs v4.13.4 with the in-cluster server.
3. else `standard-1node` all PASS: CI uses a one-node cluster with the default class, and the spec records that multi-node sharing is not exercised in CI.
4. none: record the failing rows; question 3 stays open and blocks sub-project 3's end-to-end test, not sub-project 2.

Two rows need care:

- `P3.nfs-2node.2` FAIL with "Permission denied": the export squashes or roots the directory. That is Review Focus 4. Record it, and record what made it pass if anything did (the driver's `mountPermissions` class parameter is the first thing to try: add `mountPermissions: "0777"` under `parameters` and rerun). The fix belongs in the spec's storage requirements.
- `P3.*.1` FAIL with the claim `Pending`: `kubectl -n balerix-spike describe pvc shared` in a rerun gives the provisioner's reason; add that command's output to the row with `info`.

---

### Task 5: Questions 1 and 2 on a managed cluster

**Files:** none new.

**Interfaces:**
- Consumes: `run.sh`, `p1-tmux.sh`, `p2-landlock.sh`; a kubectl context from the user.

This task needs something only the user has: a managed cluster (GKE, EKS or AKS) and a context for it with permission to create a namespace and pods. This development container can reach a remote API server; it has no Docker, which the probes do not need.

- [ ] **Step 1: Ask the user for a context**

Ask: which managed cluster, and the kubectl context name, or how to obtain credentials (for example `! gcloud container clusters get-credentials …` typed in the prompt). If the user has none available, skip to Step 4 and record the question as open.

- [ ] **Step 2: Confirm the nodes can run the injected binaries**

```bash
mise x kubectl@1.37.1 -- kubectl --context "$CTX" get nodes \
  -o custom-columns=NAME:.metadata.name,ARCH:.status.nodeInfo.architecture,KERNEL:.status.nodeInfo.kernelVersion,RUNTIME:.status.nodeInfo.containerRuntimeVersion
uname -m
```

Expected: every node's architecture matches this machine's (`amd64` for `x86_64`). The binaries copied in are this machine's. If they differ, the user must run the probes from a machine of the nodes' architecture; say so and stop.

- [ ] **Step 3: Run the probes**

```bash
KCONTEXT="$CTX" RESULTS="$PWD/spikes/k8s/RESULTS-managed.md" \
  mise x kubectl@1.37.1 jq -- spikes/k8s/run.sh p1 p2
```

Expected if the design holds: the `P1` rows as on kind; `P2.0`–`P2.6` PASS; `P2.7` INFO "skipped".

A sandboxed runtime (GKE Sandbox, which is gVisor) is expected to FAIL `P2.2`; that is the documented limit in O-4, and worth one row if the cluster offers such a node pool.

- [ ] **Step 4: Clean up and commit the results file**

```bash
mise x kubectl@1.37.1 -- kubectl --context "$CTX" delete namespace balerix-spike
git add spikes/k8s/RESULTS-managed.md && git commit -m "chore(spike): results on a managed cluster" && git push
```

---

### Task 6: Record the answers in the spec

**Files:**
- Modify: `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md` (append §19; edit §6.4 and §17 item 1)
- Modify: `/home/dev/.claude/projects/-workspace/memory/spec-o-kubernetes.md`

**Interfaces:**
- Consumes: the job summaries of the last `spike-k8s` run (both Kubernetes versions) and `RESULTS-managed.md`.

- [ ] **Step 1: Collect the result tables**

```bash
run=$(gh run list --branch spike/k8s --workflow spike-k8s --limit 1 --json databaseId --jq '.[0].databaseId')
gh run download "$run" --dir "$TMPDIR/spike-results"
cat "$TMPDIR"/spike-results/*/RESULTS.md spikes/k8s/RESULTS-managed.md
git rev-parse spike/k8s
```

Keep the commit hash: the spec names it, since the branch is deleted.

- [ ] **Step 2: Append §19 to the spec, on `docs/spec-o-kubernetes`**

```bash
git checkout docs/spec-o-kubernetes
```

Append a section with this structure, filled from the tables. Every sentence states what was observed, on which cluster and version; nothing is inferred beyond the rows.

```markdown
## 19. Recorded at the spike (2026-10)

Probes: branch `spike/k8s` at `<commit>`, deleted after this section was
written. Run on kind 0.33.0 (Kubernetes 1.37.0 and 1.34.11, GitHub's
`ubuntu-24.04` runner, kernel `<uname -r>`) and on `<managed cluster,
version, kernel>`.

### 19.1 tmux across containers (§6.4)

<One paragraph: which of P1.0–P1.10 passed on which clusters. Then one
line: "The fallback in §6.4 is not taken." or "The fallback in §6.4 is
taken, because <row and message>.">

`#{pane_pid}` names a process in the agent container's PID namespace
(P1.6: <visible or not>). <What that means for the sidecar, one sentence.>

### 19.2 nono and Landlock in a hardened pod (O-4)

<Which of P2.0–P2.6 passed where.>

Where Landlock is denied (P2.7): nono exits `<code>` with `<message>`,
and the command <does not run | runs>. <If it runs: the sidecar must
check before launch; state the check.>

### 19.3 ReadWriteMany on kind (O-5)

<The class chosen by Task 4's order and why; the rows that decided it;
any class parameter that was needed for uid 10001.>

### 19.4 Still open

<Anything unanswered: no managed cluster available, a row that failed for
a harness reason that was not resolved. "Nothing." if nothing.>
```

- [ ] **Step 3: Bring §6.4 and §17 in line with the answers**

- §6.4: replace "To verify first" with the outcome: either the two-container shape stands, or the section describes the one-container shape as the design. If the fallback is taken, §6.1's table and §10.4's last sentence change with it; edit them in the same commit.
- §17 item 1: append "Done; see §19."
- If `P2.7` showed nono running the command unsandboxed, add to §6.2 step 1 the check the sidecar performs before `launch.sh`.
- If Task 4 needed a class parameter for uid 10001, add it to §4.1 as a requirement on `storage.shared`.

- [ ] **Step 4: Check the spec reads consistently**

Run: `grep -n 'To verify first\|fallback' docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md`
Expected: every hit agrees with §19.1's one-line verdict.

- [ ] **Step 5: Commit, then delete the spike branch after asking**

```bash
git add docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md
git commit -m "docs(spec): Spec O §19, what the spike found"
```

Deleting the remote branch is not reversible from here; ask the user, then:

```bash
git push origin --delete spike/k8s
git branch -D spike/k8s
```

- [ ] **Step 6: Update the memory file**

In `/home/dev/.claude/projects/-workspace/memory/spec-o-kubernetes.md`, replace the "How to apply" line with the spike's verdicts in one sentence each and "next: writing-plans for sub-project 2 (Daemon mode and sidecar)", and update the pointer line in `MEMORY.md` to match.
