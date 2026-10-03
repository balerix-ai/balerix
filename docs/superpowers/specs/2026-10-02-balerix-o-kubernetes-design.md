# Balerix — Spec O: Kubernetes

**Date:** 2026-10-02
**Status:** Design approved in brainstorm 2026-10-02; written spec approved
2026-10-02; the spike's findings recorded in §19; sub-project 2 decisions
in §7.4; sub-project 3a decisions in §20
**Scope:** running balerix on Kubernetes. A `balerix-operator` reconciles
five custom resources (Daemon, Fleet, Crew, Agent, Plugin) into pods,
claims, Secrets and Jobs. Each Daemon object is a `balerix serve` instance
in a new Kubernetes mode. Each agent is a pod whose `balerix-agent` sidecar
materialises the agent and links it to its Daemon. Operator and agent
images join the core release unit; two Helm charts form a new release unit
published to `balerix-ai/helm-charts`.

This is the umbrella design. It is implemented as five sub-projects, each
with its own plan (§17). The single-host tmux mode is unchanged and stays
supported.

---

## 1. Problem

Balerix runs control plane and data plane on one machine: one daemon, one
uid, tmux windows, nono for isolation, files under XDG roots. The
architecture spec kept a seam for a later split ("the future Kubernetes
split cuts between `server` and `runtime`; `core` is shared"), named
DNS-label identifiers for it, and made `LaunchPlan` and attach
runner-neutral. Nothing implements the split.

A team that runs its workloads on Kubernetes wants to declare a fleet with
`kubectl apply`, have each agent run as a pod with its own storage and
resource limits, see agents with `kubectl get`, and install the whole thing
with Helm. The fleet file they already have should keep its meaning.

## 2. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| O-1 | **The operator reconciles; a Daemon is an event hub.** The operator owns every Kubernetes object. A Daemon keeps hook ingress, the plugin chain, activations, KV, the proxy and attach. | Hook traffic never crosses the cluster-wide component, and a Daemon is a tenancy boundary. A daemon that created pods itself would need pod-creating RBAC per tenant and leave two reconcilers. |
| O-2 | **A Fleet is authored; Crew and Agent are generated**, owned children holding resolved settings. | The left-fold merge stays in one crate and one place. Independently authored children would spread the merge across objects that arrive in any order. Deleting the Fleet garbage-collects everything. |
| O-3 | **One standard agent image; mise installs `tools:` at start.** No per-agent image. | The same fleet file works on tmux and on Kubernetes. An image override is one field that can be added later without breaking anything. |
| O-4 | **nono inside a hardened pod, failing closed.** A node without Landlock yields `SandboxUnavailable`; the agent does not start. | The `sandbox:` block keeps its exact meaning and the tested launch path is reused. A silent fallback to pod-only isolation would make one fleet weaker on some nodes without saying so. |
| O-5 | **A claim per agent plus a shared ReadWriteMany volume per Daemon** for crew object caches and tool pools. | Matches today's layout (Specs E, N): fast clones, one download per tool, harvest of unpushed work. ReadWriteMany storage becomes a stated requirement. |
| O-6 | **Only operator-run Jobs write the shared volume.** Agent pods mount their slice read-only at the mount. | Spec N's rule that the cache has one writer, enforced by the kernel's mount flags instead of a nono grant. |
| O-7 | **One namespaced `Plugin` kind**; a Daemon's `spec.plugins` list gives the interceptor order. | One tenant model exists today. A cluster-scoped catalogue can be added later by letting a Plugin reference an entry instead of an image. |
| O-8 | **The Daemon holds no Kubernetes credentials.** The operator pushes resolved fleets and the plugin list to the Daemon's HTTP API and reads status back. | The Daemon hosts plugin traffic; it should not also hold cluster access. The existing API and its "a rejection lands nothing" semantics are reused, and every `status` has one writer. |
| O-9 | **The sidecar talks to the Daemon, never to the operator.** What the operator needs from a pod it reads from the Pod object (readiness, termination message). | Agent credentials and hook payloads stay off the cluster-wide component. Agent pods need no service-account token. |
| O-10 | **Agent pods accept no inbound connections.** The sidecar holds one outbound link to the Daemon; commands arrive over it. | One direction to authenticate, a deny-all ingress policy, and the Daemon knows liveness from the link. |
| O-11 | **TLS between pods**, from a certificate authority the operator creates per Daemon. | "Plain HTTP on loopback" (P3-1) held because an unprivileged process cannot read loopback. Pod-to-pod traffic carries tokens and hook payloads across the cluster network. |
| O-12 | **The operator and the agent are standalone projects** (`operator/`, `agent/`), like plugins. | `kube`, a WebSocket client and their trees stay out of the core workspace's feature resolution (Spec H's reason). |
| O-13 | **Operator, agent image and daemon image release as the core unit, at one version.** | They speak one protocol and must match. |
| O-14 | **Chart source lives in this repository; `helm-charts` is a publishing target.** Charts are a release unit of their own. | CRDs are generated from the Rust types; a CI check keeps the chart's copies equal. A change to a CRD field and its chart value is one pull request. |
| O-15 | **Two charts:** `balerix-operator` (CRDs and operator, once per cluster) and `balerix-daemon` (a Daemon and optional standard Plugins, per namespace). | A Daemon object cannot be created in the release that first installs its CRD, and the two have different installers and scopes. |
| O-16 | **`retain` mirrors today's commands:** `Branches` is `down --keep-repos`, `None` is plain `down`. | One meaning for both runners. |

## 3. Topology

```
            kubectl apply Fleet
                   │
        ┌──────────▼───────────┐   resolved spec, status    ┌──────────────────────┐
        │  balerix-operator    │ ─────────────────────────► │  Daemon (per Daemon  │
        │  (one per cluster)   │ ◄───────────────────────── │  object): serve      │
        └──────────┬───────────┘      Daemon HTTP API       └───▲──────────────┬───┘
   creates pods,   │                                            │ hooks, link  │ activate,
   claims, Secrets │                                            │ (outbound)   │ observe
        ┌──────────▼─────────────────────────┐                  │          ┌───▼────────┐
        │ Agent pod                          │                  │          │ Plugin pod │
        │  agent container: nono → claude    │                  │          └────────────┘
        │  sidecar: balerix-agent ───────────┼──────────────────┘
        └────────────────────────────────────┘
```

| Component | Runs as | Holds | Talks to |
|---|---|---|---|
| Operator | Deployment, leader-elected, one per cluster | Kubernetes credentials | the API server; each Daemon's admin API |
| Daemon | StatefulSet of one replica per Daemon object | plugin KV, vault, fleet records, activations | sidecars (inbound), plugins (both ways) |
| Agent pod | Pod per Agent object | the agent's claim, its credentials, its Daemon token | the Daemon (outbound only) |
| Plugin pod | Deployment of one replica per Plugin object | its token, config and secrets | the Daemon (both ways); its own upstreams |
| Crew Jobs | Job, one at a time per crew | the shared volume, read-write | the git remote, tool sources |

## 4. Custom resources

Group `balerix.ai`, version `v1alpha1`. All five kinds are namespaced. A
Fleet, its children, its Plugins and their pods live in the Daemon's
namespace.

Every `status` carries `observedGeneration` and `conditions`
(`type`, `status`, `reason`, `message`, `lastTransitionTime`). The operator
is the only writer of any status.

### 4.1 Daemon

```yaml
apiVersion: balerix.ai/v1alpha1
kind: Daemon
metadata: { name: default, namespace: team-a }
spec:
  version: 0.3.0                 # daemon and agent images; defaults to the operator's own version
  storage:
    state:  { storageClassName: standard, size: 5Gi }    # plugin KV, vault, records
    shared: { storageClassName: efs, size: 100Gi }       # ReadWriteMany: crew caches, tool pools
    agent:  { storageClassName: standard, size: 20Gi }   # default per-agent claim
  credentials:
    claude: { secretName: claude-credentials }           # key `credentials.json`
    github: { secretName: gh-token }                     # key `token`
  defaults:                      # the merge layer the host's settings.json is on one machine
    claude: { settings: { model: sonnet } }
  plugins: [flow, web, github]   # Plugin names; list order is interceptor order
  resources: {}                  # the daemon container's
```

- `version` must equal the operator's version in `v1alpha1`; another value
  sets `Ready=False`, reason `VersionMismatch`. The field exists so that a
  later version can allow skew without a schema change.
- `storage.shared` is required; its class must provision ReadWriteMany.
- `defaults` takes the place of the host `claude.settings` layer: the fold
  is `Daemon.defaults → Fleet.defaults → crew.defaults → agent`.
- Conditions: `StorageReady`, `SystemToolsReady` (the daemon pool, Spec F's
  readiness), `PluginsReady`, `Ready`.
- `status.endpoint` is the Daemon's in-cluster URL.

### 4.2 Fleet

The fleet file, with three differences: `name` is `metadata.name`, `daemon`
names the Daemon, and `runner` is `pod`.

```yaml
apiVersion: balerix.ai/v1alpha1
kind: Fleet
metadata: { name: payments, namespace: team-a }
spec:
  daemon: default
  retain: Branches               # or None (the default)
  defaults:
    claude: { settings: { permissions: { allow: ["Bash(git *)"] } }, resume: true }
    sandbox: { network: { block: false } }
    tools: { node: "22.11.0" }
    runner:
      type: pod
      resources: { requests: { cpu: "1", memory: 2Gi } }
      storage: { size: 40Gi }    # overrides the Daemon's agent claim size
      nodeSelector: {}
      tolerations: []
    plugins: { web: {} }
  crews:
    backend:
      repo: acme/payments-api
      ref: main
      git: { push: true, auth: gh }
      defaults: { tools: { python: "3.12.8" } }
      agents:
        alice: { plugins: { flow: { initial: working, states: { working: {} } } } }
        bob:   { claude: { settings: { model: opus } } }
```

- `defaults`, `crews.*.defaults` and each agent are open objects in the
  schema (`x-kubernetes-preserve-unknown-fields`) and are validated by
  `balerix-config`, exactly as the file is. The schema checks structure,
  names (`[a-z0-9-]+`) and the fixed fields.
- `runner.type` must be `pod`, and is `pod` when omitted. `tmux` is refused
  with the config path.
- `balerix` stays a reserved fleet name.
- `<fleet>-<crew>-<agent>` must be at most 63 characters; a longer name
  fails resolution with the config path.
- Conditions: `Resolved` (the resolver's message, config path first),
  `Accepted` (plugin activations), `Ready` (every agent `Ready`).
- A Fleet carries the finalizer `balerix.ai/fleet`.

### 4.3 Crew (generated)

Named `<fleet>-<crew>`, owned by the Fleet.

- `spec`: `fleet`, `crew`, `repo`, `ref`, `git`, and the two tool tables
  (the fleet's `defaults.tools` and this crew's), as `CrewTools` has them.
- `status`: `cacheRef` (the commit last fetched), conditions `CacheReady`
  and `ToolsReady`.

### 4.4 Agent (generated)

Named `<fleet>-<crew>-<agent>`, owned by the Fleet, labelled
`balerix.ai/fleet`, `balerix.ai/crew`, `balerix.ai/agent`.

- `spec`: the resolved `AgentSettings`, the daemon name, and `specHash`.
- `status`: `phase` (the existing phases), `pod`, `restarts`, `session`,
  `plugins` (name and activation state per plugin), and conditions
  `Scheduled`, `Materialized`, `Ready`.
- Printer columns: phase, restarts, age.
- An Agent carries the finalizer `balerix.ai/harvest`.

A hand edit to a Crew's or Agent's `spec` is reverted on the next
reconcile.

### 4.5 Plugin

```yaml
apiVersion: balerix.ai/v1alpha1
kind: Plugin
metadata: { name: github, namespace: team-a }   # must equal the plugin's manifest name
spec:
  image: ghcr.io/balerix-ai/balerix-plugin-github:0.2.0
  needs: [fleets, actions, kv, manage]          # the grant
  config: { appId: 12345 }
  secrets:
    privateKey: { secretName: gh-app, key: private-key }
  fleetDefaults: { claude: { settings: { model: sonnet } } }
  expose: { port: 8080 }                        # a Service for the plugin's own listener
  scratch: { size: 1Gi }                        # optional claim for the plugin's scratch directory
  resources: {}
```

- A Plugin runs only when a Daemon in its namespace lists it; a listed
  name with no Plugin object sets the Daemon's `PluginsReady=False`.
- A Plugin may be listed by at most one Daemon.
- Conditions: `Deployed`, `Ready` (the plugin said `hello` and its manifest
  fits the grant).

## 5. The operator

A standalone project `operator/`, binary `balerix-operator`, on `kube`. It
depends by path on `balerix-api`, `balerix-core` and `balerix-config`.

Every controller follows the repository's rule that the planner is pure
and the executor is dumb: a pure function takes the observed objects and
returns the desired objects; the controller applies them with server-side
apply under the field manager `balerix-operator`.

### 5.1 Daemon controller

Creates, per Daemon:

- a certificate authority and a serving certificate (Secrets), and a
  ConfigMap with the authority's certificate (§10.3);
- the admin token (Secret);
- the state claim and the shared claim;
- a StatefulSet of one replica running `balerix serve --mode kubernetes`
  from `ghcr.io/balerix-ai/balerix:<version>`, with the state claim at its
  home and the serving certificate mounted;
- a Service;
- a Job that installs the daemon tool pool on the shared volume (§8.3);
- NetworkPolicies (§10.2).

It then sends the Daemon its plugin list (§9.2) whenever the list, a
Plugin, or a referenced Secret changes.

### 5.2 Fleet controller

1. Resolve the Fleet through `balerix-config`, beneath the Daemon's
   `defaults`. A failure sets `Resolved=False` and changes nothing.
2. Read the credentials Secrets the Daemon names.
3. Mint a token for every Agent that has none (kept in a Secret per
   Agent; reused across reconciles, as hook secrets are today).
4. `PUT` the resolved spec, with each agent's token, to the Daemon (§7.2).
   A 400 sets `Accepted=False` with the Daemon's message and changes
   nothing: no child object is touched.
5. Apply the Crew and Agent objects; delete children the Fleet no longer
   declares.
6. Read the fleet's status from the Daemon and write `status`.

On deletion the finalizer waits until every Agent is gone (§8.5), deletes
the fleet on the Daemon, and with `retain: None` removes the crews'
directories from the shared volume through a cleanup Job.

A Daemon that does not answer sets `Ready=False`, reason
`DaemonUnavailable`, and the reconcile is retried with back-off. Existing
pods are left alone.

### 5.3 Crew controller

Runs the sync Job (§8.3) when the Crew is new, when `ref` or a tool table
changes, and before a new Agent's pod is created. One Job per crew at a
time; sync and harvest Jobs share that lock.

### 5.4 Agent controller

Once the Crew is `CacheReady` and `ToolsReady`, creates:

- the agent's claim, named after the Agent, kept across pod replacement;
- a Secret holding the resolved agent, the credential bundle and the
  Daemon token, mounted in the sidecar only;
- a NetworkPolicy;
- the Pod (§6.1), annotated with `specHash`.

A Pod whose `specHash` differs from the Agent's is deleted and recreated
on the same claim. Pod state maps to conditions: unschedulable or image
pull failure to `Scheduled=False`; a sidecar termination message to
`Materialized=False` with that message as the reason text
(`SandboxUnavailable`, a git error, a tool install error); the sidecar's
readiness to `Ready`.

### 5.5 Plugin controller

Creates the plugin's token (Secret), its serving certificate (§10.3), a
Secret with its resolved config, the optional scratch claim, a Deployment of one replica, a Service, and a
NetworkPolicy. A change to the Plugin's spec or to a Secret it references
rolls the Deployment, which is what a changed `ResolvedPlugin::hash` does
today.

### 5.6 Scope and permissions

By default the operator watches every namespace with a ClusterRole. With
the chart value `watchNamespaces` set, it watches only those, with a Role
in each. The CustomResourceDefinitions are installed by the chart, never by
the operator, so it needs no permission on them.

## 6. The agent pod

### 6.1 Shape

Two containers from `ghcr.io/balerix-ai/balerix-agent:<version>`.

| | `sidecar` | `agent` |
|---|---|---|
| Kind | native sidecar (init container, `restartPolicy: Always`) | main container |
| Command | `balerix-agent sidecar` | `balerix-agent run` |
| Mounts | agent claim; shared slice (read-only); run directory; the agent Secret; the authority's certificate | agent claim; shared slice (read-only); run directory |
| Resources | small fixed requests | the fleet's `runner.resources` |

Both run as uid 10001 with `runAsNonRoot`, a read-only root filesystem,
all capabilities dropped, `allowPrivilegeEscalation: false`, and
`seccompProfile: RuntimeDefault`. The pod sets
`automountServiceAccountToken: false` and meets the `restricted` Pod
Security Standard. Native sidecars need Kubernetes 1.29 or later; that is
the minimum supported version.

Volumes:

- **agent claim** at `/balerix/agent`: the existing agent directory
  (`home/`, `workspace/`, `nono/`, `logs/`, the profiles, `launch.sh`);
- **shared slice**, read-only sub-path mounts of the shared claim: the
  crew's `repo/.git/objects`, and the daemon, fleet and crew tool pools;
- **run directory**, an `emptyDir` at `/balerix/run`: the tmux socket and
  the start marker.

`StateLayout` gains a constructor for this layout so every path still
comes from it.

### 6.2 Start sequence

1. The sidecar materialises the agent with `balerix-runtime`, as the
   daemon does on one machine: the private clone with `--reference` to the
   crew cache, `home/` (settings with balerix's hooks, credentials,
   `hosts.yml`), `mise.toml` and the install of the agent's own tools into
   `home/`, the nono profile and its validation, `launch.sh`. It then
   runs a sandbox self-test in its own container, which has the same
   pod seccomp settings and the same node kernel as the agent container:
   `nono -s run --profile <the agent's profile> -- /bin/true`.
2. A failure ends the sidecar with a one-line reason as the container's
   termination message. A self-test that exits 1 with
   `Landlock not available` is `SandboxUnavailable`. That exit and
   message are what nono gave when the Landlock syscalls answered
   `ENOSYS` (§19.2), and `/sys/kernel/security/lsm` did not exist in the
   pods probed, so nono's own failure is the signal. The self-test is
   needed because the first `nono run` of `launch.sh` happens in the agent
   container, after these steps, where the sidecar does not see nono's
   message: a node without Landlock would otherwise look like a pane
   exiting 1 and be restarted with back-off.
3. The sidecar writes the start marker. The `agent` container, which has
   been waiting for it, starts a tmux server on the shared socket with the
   crew's anchor window only; the sidecar then creates the agent's window,
   running `launch.sh` under `remain-on-exit`, through the runner (§7.4).
4. Claude's `SessionStart` reaches the sidecar through `hook-relay`; the
   sidecar forwards it to the Daemon and its readiness probe turns true.

`launch.sh` is unchanged: `balerix agent-supervise → nono run → mise exec
→ claude`. The image therefore carries the `balerix` binary as well.

### 6.3 Running

The sidecar is the in-pod runner. It uses `TmuxRunner` against the shared
socket for `send_text`, `send_keys`, attach and respawn, and runs the core
planner for its one agent, so a dead Claude restarts with the existing
back-off and a `stop` holds until a `restart`.

Three rules follow from the spike (§19.1):

- Every tmux client call from the sidecar passes `-u`, or the sidecar's
  environment sets a UTF-8 locale. Without either, tmux replaces the tabs
  in `TmuxRunner`'s window format with `_`, and the line does not parse.
- `#{pane_pid}` names a process in the agent container's PID namespace,
  which the sidecar cannot see. The sidecar treats it as an opaque number:
  it reports it in `status` and reads nothing under `/proc` by it. What
  reads `/proc` of Claude's process (`balerix agent-supervise`) runs in
  the agent container, from `launch.sh`.
- On one machine, `TmuxRunner` waits for a process tree to end through
  `/proc`: the restart arm of `ensure_agent`, `stop_agent` and
  `stop_crew` take `ProcIdentity::of(pane_pid)`, which reads
  `/proc/<pid>/stat`, and `wait_gone` polls it (Spec N amendment §13.5).
  In the sidecar that read finds no process, so the wait returns at once
  and #107's bug is back, or it finds an unrelated sidecar process with
  the same pid and waits on that. Sub-project 2 therefore makes the
  runner's process waits in pod mode use no `/proc`. They rely on tmux's
  own view of the pane, where `pane_dead` under `remain-on-exit` is
  visible from the sidecar (P1.8), and on `balerix agent-supervise`, the
  pane's process, which runs in the agent container and exits only when
  its tree is empty. A wait on `pane_dead` needs the pane to exist until
  then, so a pod-mode stop does not remove the window before the pane is
  dead.

### 6.4 Verified at the spike

A tmux client in one container drives and attaches to a server in another
through a socket on a shared volume, including the PTY an attach needs.
This held on kind (Kubernetes 1.37.0 and 1.34.11) and on k3s 1.36.4
(§19.1). The fallback is not taken. That fallback was one
container running both processes, with nono as the only wall between
sidecar and agent; the two-container shape in §6.1 stands.

## 7. Sidecar and Daemon

### 7.1 Hooks

Claude posts hooks to the sidecar on `127.0.0.1`, with the same
`open_port` grant, `settings.json` shape and per-agent secret as today.
The sidecar forwards each to the Daemon's existing events route with the
agent's token and returns the Daemon's answer. When the Daemon cannot be
reached inside the hook budget the sidecar answers as an empty chain does
(the chain fails open) and counts it.

### 7.2 The link

The sidecar opens one WebSocket to the Daemon, authenticated with the
agent's token, and reconnects with back-off. JSON frames, one request and
one response per id:

| Daemon → sidecar | Meaning |
|---|---|
| `send_text`, `send_keys` | as the `AgentRunner` methods |
| `stop`, `restart` | per-agent desired state (plugins spec §16.4) |
| `attach` | the sidecar opens a second WebSocket carrying the PTY |
| `workspace.diff`, `.file`, `.tree`, `.version` | as `WorkspaceReader`; git runs in the sidecar with `inspect.rs`'s invocations |

| Sidecar → Daemon | Meaning |
|---|---|
| `status` | phase, pid, restarts, on every change |

On the Daemon, a `LinkHub` adapter implements `AgentRunner` and
`WorkspaceReader` over the link. A call for an agent whose link is down
fails with a `RunnerError` naming it. `ensure_agent`, `stop_crew` and the
`Materializer` have nothing to do in this mode: the Daemon's fleet actor
keeps the record, the secrets index, the stopped set and the activation
flow, and takes observed state from `status` frames.

### 7.3 The Daemon's Kubernetes mode

`balerix serve --mode kubernetes`:

- serves TLS on the pod address from the mounted certificate;
- accepts `PUT /v1/fleets/{name}` carrying a resolved spec and the
  operator-minted token per agent, activates plugins first as `apply` does
  today, and records the fleet with `owner: kubernetes`. The CLI's `up`
  and `down` on it answer 409, as for a plugin-owned fleet;
- accepts `PUT /v1/plugins` (§9.2) in place of reading `plugins.yaml`, and
  launches no plugin;
- serves the link and `GET /readyz`, which reads Spec F's system-pool
  channel and nothing else.

Everything else (the chain, KV, proxy, sessions, `plugin open`, attach,
watch, metrics) is unchanged. The CLI works against a Daemon through a
port-forward or an Ingress the user provides, with the admin token from
the Daemon's Secret.

### 7.4 Decided in sub-project 2 (2026-10)

- **The Daemon mirrors; it does not plan.** For a fleet with
  `owner: kubernetes` the actor runs no `reconcile_pass`. The sidecar's
  `status` frames are its observed state, written into `status.agents`
  as sent; `SetStopped` becomes a `stop` or `restart` frame. The stopped
  set is reconciled only on the first `status` frame of a link (a connect
  or a reconnect): a `Ready` agent the set holds, or a `Stopped` one it
  does not, gets the frame it missed, which is how a sidecar that was away
  during a `stop` learns of it. Later frames are mirrored only: a
  `Restart` is a stop then a restart, and a late `Stopped` frame must not
  trigger a second restart. For a downed fleet a first frame whose phase
  is not `Stopped` gets `stop` and its status is not mirrored. `Down`
  sends `stop` to every linked agent, clears the agents and is `Down`
  (the pods are the operator's to delete). An apply drops the agents the
  spec no longer has from the status and the stopped set, and frames for
  them are ignored. It keeps the stop of an agent still wanted: the
  operator re-sends its apply on every reconcile, so "an apply always
  wins" (plugins spec §16.4) would undo every plugin `stop` here. An
  apply that takes the fleet from Down to Up sends `restart` to each
  linked agent the set does not hold. A
  plugin sync leaves `kubernetes`-owned fleets alone. Two planners over
  one agent would double every restart.
- **A pod-mode stop is a respawn into a waiter.** From the sidecar
  nothing can signal the pane's process and `/proc` is another
  container's. `respawn-window -k` hangs the supervisor up as
  `kill-window` does today, into a command that runs in the agent
  container: `while kill -0 <pane_pid> 2>/dev/null; do sleep 0.02; done`.
  The supervisor exits when its tree is empty; the waiter exits when the
  supervisor is gone; tmux marks the pane dead; the sidecar polls
  `pane_dead` under the existing 5 s bound (`StillRunning` past it, the
  window kept), then kills the window so the agent is absent, which is
  what leaving the stopped set expects. The restart arm of
  `ensure_agent` does the same without the final `kill-window`.
- **In a pod the sidecar never removes or harvests.** Removal on a pod
  layout is a logged no-op (the harvest is a Job's, §8.4). The sidecar
  publishes its status on every hook event, and once more after the
  forward budget, so the hook-failure count reaches the Daemon.
- **Readiness is a file.** Agent pods accept no inbound connections
  (O-10), so the probe is `exec: test -f /balerix/run/ready`; the sidecar
  writes the marker on `SessionStart` and removes it when the phase
  leaves `Ready`.
- **The operator's apply is `PUT /v1/fleets/{name}` with `agent_tokens`**
  (`fleet/crew/agent` → token, at least 32 characters, one per agent of
  the spec). The fleet is recorded with `owner: kubernetes` and the tokens
  as the agents' hook secrets: one token per agent for the hook route
  and the link. The CLI's `PUT` without tokens, `POST` and `DELETE`
  without `force` answer 409 (`fleet <name> is managed by kubernetes;
  change it through its Fleet object`); the operator's down is
  `DELETE …?force=true`. For a fleet that is absent or has no owner,
  the CLI's `POST` and `PUT` answer 409 `this daemon is in kubernetes
  mode; create a Fleet object` (no pod would ever run it), and the
  operator's `PUT` does not adopt a record with no owner or another
  owner: 409, as a plugin's would be. A tmux-mode daemon answers a body
  with `agent_tokens` 400.
- **Kubernetes mode is flags on `serve`:** `--mode kubernetes --tls-cert
  --tls-key --admin-token-file`, with any `--bind` address and no `-d`.
  The Daemon reads no `plugins.yaml` and launches no plugin; its system
  pool is `Ready` without installing (the shared volume's daemon pool is
  a Job's, §8.3). `GET /readyz` is 200 when Spec F's channel says `Ready`,
  503 with the reason otherwise.
- **The sidecar's state survives it.** The stopped flag and the
  planner's status (`FleetStatus`, as the one-machine daemon persists it)
  are written atomically to `<agent>/.balerix/state/sidecar/state.json`
  and loaded before the first pass: a `stop` holds across a sidecar
  restart with the Daemon away, and the status's applied hash keeps the
  first pass from relaunching a healthy Claude. The link opens after that
  first pass, so its first `status` frame, which the Daemon reconciles
  its stopped set against, is the pass's answer.
- **The hook hop has a secret of its own.** Claude authenticates to
  the sidecar with a sidecar-local secret (32 random bytes, made on the
  first start and kept under `<agent>/.balerix/state/sidecar/`, outside
  the agent's sandbox grants); the operator's token authenticates the
  sidecar to the Daemon, on the events route, the link and the attach
  socket, and never reaches the agent's files.
- **The agent container learns its session from the start marker.**
  `balerix-agent run` waits for `<run>/started`, whose content is
  `<fleet>/<crew>`, starts the tmux server with only the crew's anchor
  window on `<run>/tmux.sock` and polls `has-session` until the server is
  gone; SIGTERM becomes `kill-server`. The sidecar creates the agent's
  window through the runner. It needs no Secret and no arguments. The
  sidecar never starts the server: every pod-mode tmux call carries `-N`,
  and a pass that finds no crew session fails until `run` is back, so
  Claude never runs in the sidecar's container.
- **Mount paths are flags with §6.1's defaults** (`--agent-dir
  /balerix/agent`, `--shared-dir /balerix/shared`, `--run-dir
  /balerix/run`, `--bundle /balerix/secret/agent.json`, `--ca
  /balerix/tls/ca.crt`, `--termination-log /dev/termination-log`,
  `--hook-port 7643`). Under the shared mount: `repo/.git/objects`,
  and the three pool directories mounted whole: `crew`, `fleet` and
  `daemon`, each holding `mise/` (§20.5). Sub-project 3 mounts the
  shared claim's sub-paths there and the Secret at the bundle path.
- **The link's wire shapes** are `balerix-api`'s `link` module: a
  Daemon → sidecar text frame is a `LinkRequest` (`id`, `op` tagged
  `kind`), a sidecar → Daemon frame is a `SidecarFrame` (`reply` with the
  id and a `result` tagged `kind`, or `status` with the agent's
  `AgentStatus`, the pane's pid and the hook-failure count). File bytes
  travel as a JSON array; the 1 MiB file cap keeps that small. Both
  sockets, the link and the attach socket, carry `balerix-link-protocol:
  1`, checked before the upgrade. An attach session is bound to the agent
  the `attach` request was sent to: another agent's socket is answered as
  an unknown session (close 1008). A call in flight when its link ends or
  is replaced fails `link down` at once. The sidecar drops a link that has
  been silent for three Daemon pings (90 s) and reconnects, and bounds its
  connect at 10 s. Workspace and runner error text crosses the link
  without the id prefix, which the Daemon adds back.

## 8. Storage

### 8.1 The shared volume

One ReadWriteMany claim per Daemon:

```
pools/daemon/                         the system tool table (claude, gh)
fleets/<fleet>/pool/                  the fleet's defaults.tools
fleets/<fleet>/crews/<crew>/pool/     the crew's defaults.tools
fleets/<fleet>/crews/<crew>/repo/     the crew's object cache
```

An agent pod mounts, read-only, its crew's `repo/.git/objects` and the
three pools on its path. `MISE_SHARED_INSTALL_DIRS` names them in the
existing order (crew, fleet, daemon).

### 8.2 The agent claim

ReadWriteOnce, sized by the Daemon's `storage.agent` or the fleet's
`runner.storage`. It outlives the pod. It is deleted with the Agent, after
the harvest.

### 8.3 Sync Job

`balerix-agent crew-sync`, with the shared volume read-write and the
GitHub token mounted: fetches the crew cache (`ensure_repo`, `gc.auto=0`,
`--no-auto-gc`) and installs the crew pool. The fleet pool and the
Daemon's `pools/daemon` are each installed by a Job of their own,
`balerix-agent pool-sync` (§20.3). A failure sets `CacheReady=False` or
`ToolsReady=False` with the tool's message and is retried with back-off;
the crew's agents wait.

### 8.4 Harvest Job

`balerix-agent harvest`, after the agent's pod is gone, with the agent
claim read-only and the crew cache read-write: `check_clone`, then the
fetch from inside the cache under the git nono profile, exactly as
`harvest_and_remove` does (Spec N). It needs Landlock like any agent.

### 8.5 Removal

| Event | Harvest | Claim | Cache |
|---|---|---|---|
| Agent dropped from a live Fleet | yes | deleted | kept |
| Agent's `branch` changed | yes | recreated | kept |
| Fleet deleted, `retain: Branches` | yes | deleted | kept |
| Fleet deleted, `retain: None` | no | deleted | deleted |

A failed harvest blocks the Agent's finalizer with a condition carrying
the message. The annotation `balerix.ai/purge: "true"` on the Agent or
Fleet skips the harvest, as `--purge` does. A Fleet re-created over a kept
cache seeds new clones from the harvested branches, as today.

## 9. Plugins

### 9.1 The pod

A Deployment of one replica from `spec.image`, hardened like an agent pod
but without nono: the in-tree images are distroless, and the pod is the
boundary. Mounted files give it its token, its config (secrets injected),
the Daemon's URL, the authority's certificate and its own serving
certificate. The optional scratch claim is its scratch directory.

### 9.2 The plugin list

`PUT /v1/plugins` (admin token) carries the ordered list: for each plugin
its name, grant, config with secrets injected, `fleetDefaults`, token and
Service address. The Daemon keeps the resolved config in memory only, as
`ResolvedPlugin` does today. A plugin dropped from the list has its fleets
downed, as `plugin remove` does.

### 9.3 Protocol changes

- `hello` carries the plugin's manifest. The Daemon refuses, with a
  message naming the capability, a manifest whose `needs` exceed the
  grant, and a manifest whose name differs from the Plugin's. On one
  machine the manifest is still read from the package; `hello`'s copy is
  ignored there.
- The SDK serves TLS when given a certificate and key, and its host client
  trusts a given authority. Both are transport settings; the protocol
  version stays 1.

### 9.4 Managed fleets

A plugin with `manage` still sends `PUT /v1/plugin-host/fleets/{name}`
with an unresolved file. In Kubernetes mode the Daemon stores the request
and lists it at `GET /v1/managed-fleets`; the operator writes it as a Fleet
labelled `balerix.ai/managed-by: <plugin>`, resolved beneath the Plugin's
`fleetDefaults` and held to the restricted surface (Spec M §12.1). The
operator never overwrites a Fleet that lacks the label; the request then
fails with the existing 409. A plugin's down deletes the Fleet.

## 10. Security

`docs/THREAT-MODEL.md` gains a Kubernetes section with these boundaries.

### 10.1 Who can do what

- **Operator:** its five kinds, plus pods, claims, Secrets, ConfigMaps,
  Jobs, Deployments, StatefulSets, Services and NetworkPolicies, in the
  namespaces it watches. It is the only component that reads credential
  Secrets from the API.
- **Daemon:** no Kubernetes access.
- **Agent pod:** no Kubernetes access; no inbound traffic.
- **Whoever may create a Fleet** in a namespace may run agents with that
  namespace's Daemon credentials. Cluster RBAC on Fleets is the control.

### 10.2 Network policy

- Agent pods: no ingress; egress to the Daemon, DNS and the internet.
  `sandbox.network` is enforced by nono inside the pod, so it holds where
  the network plugin ignores NetworkPolicy.
- Daemon: ingress from its agents, its plugins, the operator, and whatever
  the user's Ingress names.
- Plugin pods: ingress from the Daemon, plus `expose.port` from anywhere.

### 10.3 Transport and tokens

The operator creates one certificate authority per Daemon and issues
serving certificates for the Daemon and each Plugin, renewing them before
expiry. Sidecars, plugins and the operator trust only that authority when
calling the Daemon; the Daemon trusts only it when calling plugins. Tokens
(agent, plugin, admin) are minted by the operator, stored in Secrets, and
mounted as files. Nothing secret is in an environment variable, an
argument or a log.

TLS is a new dependency in the core workspace. `scripts/check-core-deps.sh`
is changed to assert the new exact feature set of the core `reqwest`, so
an accidental widening is still caught.

### 10.4 What is on disk

Claude's credentials and the GitHub token are written into `home/` on the
agent's claim, as they are on one machine. The agent can read them, as
today. The sidecar's Secret mount (the Daemon token, the credential
bundle) is in the sidecar container only.

## 11. Failures

| What fails | What happens |
|---|---|
| Operator down | Nothing changes; agents, hooks and plugins keep working. |
| Daemon down | Hooks pass; sidecars and plugins reconnect; the operator sets `DaemonUnavailable` and retries. After a restart the Daemon reloads its records from the state claim and the operator re-sends fleets and the plugin list. |
| Sync Job fails | `CacheReady=False` or `ToolsReady=False` with the message; the crew's agents wait; retried with back-off. |
| Node lacks Landlock | The sidecar's sandbox self-test (§6.2) fails: nono exits 1 and runs nothing, as observed with the Landlock syscalls answering `ENOSYS` (§19.2); the Agent is `Materialized=False`, reason `SandboxUnavailable`. |
| Pod evicted or node drained | Recreated on the same claim; Claude resumes. |
| Harvest fails | The finalizer blocks with a condition; the purge annotation skips it. |
| Shared class is not ReadWriteMany | The Daemon is `StorageReady=False`; nothing starts. |
| A plugin rejects an agent's config | The Fleet is `Accepted=False` with the config path; no pod changes. |
| A plugin's manifest exceeds its grant | The Plugin is `Ready=False`; its pairs stay `pending`. |

## 12. Code layout

```
operator/                     standalone project, own Cargo.lock
  src/api/                    the five kinds (kube CustomResource, schemars)
  src/desired/                pure: observed objects → desired objects
                              {common,names,fleet,daemon,jobs,agent}.rs
  src/controllers/            one module per kind
  src/daemon_client.rs        the Daemon admin API
  src/pki.rs                  the per-Daemon authority and serving certificates
  crds/                       generated definitions, until the chart holds them (§20.2)
  src/main.rs                 run | crds
agent/                        standalone project, own Cargo.lock
  src/{cli,bundle,tls,hooks,link,attach,sidecar,state,run,jobs}.rs (jobs.rs: the Jobs, §8.3, §8.4)
crates/balerix-runtime/src/jobs.rs   the Jobs' work: sync, pool install, harvest
crates/balerix-server/src/kube/   link.rs (LinkHub), plugins route, managed-fleets list
  kube/{link,pty,idle,tls}.rs
crates/balerix-api            link frames, the plugins request, the manifest in hello
crates/balerix-plugin-sdk     TLS serving and trust
charts/balerix-operator/      crds/, templates/, values.yaml, values.schema.json
charts/balerix-daemon/        a Daemon, credential references, optional standard Plugins
docker/operator/Dockerfile    distroless static
docker/agent/Dockerfile       the runtime image plus balerix-agent
```

`mise run operator` and `mise run agent` lint and test the two projects
the way `mise run plugin <name>` does, and CI runs them as their own jobs.
`check` still covers the core workspace only.

## 13. Images and the core release unit

The core unit (`balerix-v<ver>`) additionally ships:

| Artifact | Contents |
|---|---|
| `balerix-operator` binaries | static musl, x86_64 and aarch64 |
| `balerix-agent` binaries | static musl, x86_64 and aarch64 |
| `ghcr.io/balerix-ai/balerix-operator` | distroless static, the operator binary |
| `ghcr.io/balerix-ai/balerix-agent` | the runtime image's contents (mise, git, gh, nono, tmux, `balerix`) plus `balerix-agent` |

They go through Spec I's steps unchanged: build and smoke-test on a runner
of the binary's architecture, hadolint, trivy, push by digest, merge the
manifest, cosign and attest, and move `latest` only after the release is
verified. Smoke tests: `--version` on both images; `balerix-operator crds`
prints five documents; `balerix-agent sidecar` with no configuration exits
1 with a message.

`prepare.sh` writes the core version into `operator/Cargo.toml` and
`agent/Cargo.toml` with the other core manifests. A change under
`operator/`, `agent/`, `docker/operator/` or `docker/agent/` counts for
core. `affected-units.sh` learns the same paths, so `images.yml` builds the
new images on pull requests that can break them.

## 14. Charts

### 14.1 The charts

- **`balerix-operator`**: the five CustomResourceDefinitions, the operator
  Deployment, its service account and RBAC. Values: image, resources,
  `watchNamespaces`, `crds.install`. The definitions are templates with
  `helm.sh/resource-policy: keep`, so `helm upgrade` updates them and
  `helm uninstall` leaves them and every Fleet in place.
- **`balerix-daemon`**: one Daemon, and optionally Plugin objects for
  flow, web, matrix and github. Values: storage classes and sizes, the
  credential Secret names, `defaults`, per-plugin `enabled`, config and
  Secret references. It creates no credential Secret itself.

`charts/balerix-operator/crds/` is generated by `balerix-operator crds`.
`mise run crds` regenerates it; CI fails when the committed files differ.

### 14.2 The `charts` release unit

| Unit | Tag | Ships |
|---|---|---|
| charts | `balerix-charts-v<ver>` | both charts, at one version |

- Releasable changes are those under `charts/`.
- `release-prepare core` moves both charts' `appVersion` to the new core
  version. As with `common`, `release-prepare charts` answers
  `status=none` until that core version is tagged, so a chart never names
  images that do not exist.
- `release-prepare charts` writes `version` into both `Chart.yaml` files
  and the changelog section into `charts/CHANGELOG.md`.

### 14.3 Publishing

In `release.yml`, for the charts unit:

1. `helm lint`, render with default values and validate the output against
   the Kubernetes and CRD schemas, install both charts on `kind`.
2. `helm package` both charts; write `SHA256SUMS`; attest the archives.
3. Push both to `oci://ghcr.io/balerix-ai/charts`; cosign-sign by digest.
4. Publish the GitHub Release in this repository with the archives
   attached; this creates the tag, as for every unit.
5. Commit a regenerated `index.yaml` to `balerix-ai/helm-charts`
   (`helm repo index --merge`, URLs pointing at this repository's release
   assets) as the release bot. It follows the tag because the index names
   the release's asset URLs.
6. Verify: `helm repo add balerix https://balerix-ai.github.io/helm-charts`,
   `helm pull` each chart, compare with `SHA256SUMS`. A failure flags the
   release as a prerelease, as `verify-package` does.

Every step is idempotent: an existing OCI tag with the same digest and an
index that already lists the version are both skipped. A dry run does
steps 1 and 2 and uploads the archives to the run.

`helm-charts` holds `index.yaml`, a README and the licence. Nobody edits
its index by hand.

### 14.4 One-time setup

1. Enable GitHub Pages on `balerix-ai/helm-charts`, served from `main`.
2. Install the `balerix-release` App on `helm-charts` with *Contents: read
   and write*.
3. After the first pushes, make the ghcr packages `balerix-operator`,
   `balerix-agent` and `charts/*` public.
4. Add `charts` to the branch protection's required checks.

`docs/RELEASING.md` gains the unit, the steps and this setup.

## 15. Testing

- **Operator, pure:** insta snapshots of every `desired` function: Fleet
  to Crews and Agents, Agent to Pod, Secret and NetworkPolicy, Plugin to
  Deployment, Daemon to StatefulSet. Property test: resolving a Fleet's
  `spec` equals resolving the same content as a fleet file.
- **Operator, controllers:** against a pinned `kind` (a mise tool), with a
  stub Daemon: rejection lands nothing; a changed `specHash` replaces the
  pod and keeps the claim; finalizers; `watchNamespaces`.
- **Sidecar and link:** integration tests with the real tools under
  `BALERIX_REQUIRE_TOOLS`, no cluster: a Daemon in Kubernetes mode and a
  sidecar as two processes, hooks forwarded, `send_text` over the link,
  attach, workspace reads, link loss and reconnect, the fail-open hook.
  As built (`agent/tests/sidecar_it.rs`): against a fake Daemon, the
  hooks forwarded, `send_text`, workspace reads, attach, `stop` and
  `restart`, a dead Claude restarted, the fail-open hook and its count,
  and the link's return; against a real `balerix serve --mode kubernetes`
  over TLS, the link and `SessionStart` to `Ready`, the first-status
  reconciliation of a down made while the link was away and of an up after
  it (stop, then restart), and a forced down travelling the live link as a
  stop.
- **Jobs:** `crew-sync` and `harvest` against temp roots, reusing Spec N's
  cases (a nono that cannot run keeps the clone).
- **End to end:** `mise run e2e-k8s`, the Phase 3 journey on `kind` with
  locally built images and `dev fake-claude`: apply a Daemon and a Fleet,
  wait `Ready`, a flow rule fires, evict the pod and resume, drop an agent
  and find its branch in the cache.
- **Shared volume on kind:** every kind run (controllers, end to end,
  charts) gives the shared claim kind's local-path provisioner with
  `sharedFileSystemPath` set to one host directory mounted into every node
  (§19.3). kind's default class refuses ReadWriteMany. This class is for CI
  only; production needs a real ReadWriteMany class (§4.1).
- **Manual:** `mise run verify-k8s`, the same with the real `claude`. Not
  part of any CI tier.
- **Charts:** as §14.3 step 1, on pull requests that touch `charts/` or
  `operator/src/api/`.
- **Release scripts:** `release-test` scenarios for the charts unit: the
  `appVersion` gate, the pending-tag rule.

## 16. Deliberately deferred

- A per-agent image override.
- A cluster-scoped plugin catalogue.
- A validating admission webhook; `Resolved=False` is the feedback for now.
- Version skew between operator, Daemon and agents.
- More than one Daemon replica.
- Pod-only isolation as an explicit setting for nodes without Landlock.
- Conversion of a tmux fleet's state into a cluster.
- An Ingress for the Daemon in the chart.

## 17. Sub-projects, in build order

Each gets its own plan.

1. **Spike.** tmux across two containers on a shared socket, with attach;
   nono and Landlock on `kind` and on one managed cluster; a ReadWriteMany
   class on `kind` for CI. Output: answers, and §6.4's fallback taken or
   not. Done; see §19 — §19.4 for the managed cluster.
2. **Daemon mode and sidecar.** §6, §7, the pod layout in `StateLayout`,
   TLS. Done when the two-process integration tests pass. Done 2026-10
   (PR #125).
3. **Operator and CRDs.** §4, §5, §8. Done when `e2e-k8s` passes without
   plugins. Built as two plans, 3a without a cluster and 3b on one (§20). 3a done 2026-10.
4. **Plugins.** §9. Done when `e2e-k8s` passes with flow and web, and the
   managed journey passes with `dev fake-plugin`.
5. **Release and charts.** §13, §14. Done when a fork rehearsal publishes
   images and both charts and `helm install` from the published index
   brings up a Ready fleet.

## 18. Done when

- `helm install` of both charts from `https://balerix-ai.github.io/helm-charts`,
  then `kubectl apply` of a Fleet, yields agents whose `Ready` condition
  is true.
- The fleet in `examples/payments.yaml`, moved under a Fleet's `spec` with
  `runner.type: pod`, resolves to the same agent settings as the file.
- Hooks, flow rules, the web terminal and review page, and a managed fleet
  behave as they do on one machine.
- An evicted agent pod resumes its session; a removed agent's branch is in
  the crew cache.
- An agent on a node without Landlock reports `SandboxUnavailable` and
  runs nothing. nono's part of this is observed in §19.2.
- `mise run check`, `operator`, `agent`, `plugins`, `e2e` and `e2e-k8s`
  pass; the tmux mode's behaviour is unchanged.

## 19. Recorded at the spike (2026-10)

Probes: branch `spike/k8s` at `b61567a`, deleted after this section was
written. The kind tables are from GitHub Actions run 37016593694 at
`b2a4612`; the commits after it, up to `b61567a`, added only `run.sh`'s
namespace creation, the claude pin and the k3s results file. Run on kind
0.33.0 (Kubernetes 1.37.0 and 1.34.11, three nodes, GitHub's
`ubuntu-24.04` runner, kernel `6.17.0-1022-azure`) and on a k3s cluster
(`v1.36.4+k3s1`, one x86_64 node, kernel `6.12.90+deb13.1-amd64`,
Debian 13), reached through a namespaced tenant role. The k3s cluster
stands in for the managed cluster §17 asks for; no GKE, EKS or AKS cluster
was available. Every probe pod ran with §6.1's settings (uid 10001,
`runAsNonRoot`, read-only root filesystem, all capabilities dropped, no
privilege escalation, `RuntimeDefault` seccomp, no service-account token)
and with `runAsGroup: 10001` and `fsGroup: 10001`, in a namespace
enforcing `restricted`, from the pinned `debian:trixie-slim`, with tmux
3.7c and nono 0.79.0 copied in. kind 1.37.0 and 1.34.11 gave the same
verdict on every row.

### 19.1 tmux across containers (§6.4)

P1.0–P1.5 and P1.7–P1.10 passed on kind 1.37.0, kind 1.34.11 and k3s
1.36.4. The hardened pod with a native sidecar was admitted. A tmux server
started from the agent container outlived the exec that started it. From
the sidecar, through the socket in the shared `emptyDir`:

- `has-session` found the session;
- `send-keys -l` reached the pane, and so did `load-buffer -b <name> -`
  from the client's stdin followed by `paste-buffer -p -d`;
- `capture-pane` read the pane;
- a grouped attach session (`new-session -t crew -s balerix-attach-1`) in
  a PTY (`script`) typed into the pane and read it;
- `pane_dead=1` showed under `remain-on-exit` after the pane exited;
- `respawn-window -k` restarted the command in the agent container, and
  `kill-session` ended the session.

The fallback in §6.4 is not taken.

`#{pane_pid}` names a process in the agent container's PID namespace
(P1.6: not visible). `list-windows -u` from the sidecar printed pid 55 or
56 on kind and 51 on k3s, and `/proc/<pid>` did not exist in the sidecar.
The sidecar therefore treats the pid as opaque, what reads `/proc` of
Claude's process runs in the agent container, and `TmuxRunner`'s waits on
a process tree use no `/proc` in pod mode (§6.3).

The same `list-windows -F` without `-u`, in an environment with no UTF-8
locale, printed `agent_0_56_` on kind 1.34.11 (`agent_0_55_` on 1.37.0,
`agent_0_51_` on k3s): the client replaced each tab with `_` (P1.6a, not
in the plan). `TmuxRunner`'s window format is tab-separated, so the
sidecar passes `-u` or sets a UTF-8 locale (§6.3).

### 19.2 nono and Landlock in a hardened pod (O-4)

P2.0–P2.6 passed on kind 1.37.0, kind 1.34.11 and k3s 1.36.4. The pod took
the binaries. `nono -s profile validate` accepted a profile with system
read prefixes, one read-write directory, `workdir: none`, `network.block`
and `deny_vars: ["*"]`. Under `nono -s run`: a write inside the grant
succeeded; a write outside the grants and a read outside them were
refused; the process could not rewrite its own profile; an outbound TCP
connect to 1.1.1.1:53, which the pod itself could make, was refused.
nono's `HOME` was `/balerix/agent/nono`, outside every grant.

`/sys/kernel/security/lsm` did not exist in the pods on any of the three
clusters (securityfs is not mounted), so an in-pod check cannot read the
active security modules from it.

Where Landlock is denied (P2.7, kind only, a `Localhost` seccomp profile
answering `ENOSYS` to the three Landlock syscalls): nono exits `1` with
`nono: Sandbox initialization failed: Landlock not available. Requires Linux kernel 5.13+ with Landlock enabled.`,
and the command does not run (it wrote no file). The same on 1.37.0 and
1.34.11. That exit code and message are the signal the sidecar's sandbox
self-test maps to `SandboxUnavailable` (§6.2).

### 19.3 ReadWriteMany on kind (O-5)

The writer is a pod with the whole claim read-write, standing for the sync
Job. The reader is a pod with a read-only `subPath` mount of
`fleets/f/crews/c/repo/.git/objects`, standing for an agent. Both are
pinned to nodes by `kubernetes.io/hostname`. Kubernetes 1.37.0 and 1.34.11
gave the same results.

| Class, in the order tried | Result |
|---|---|
| kind's default `standard` (local-path, `WaitForFirstConsumer`); writer and reader on two nodes, then on one | FAIL. The claim stays `Pending`: `ProvisioningFailed: … NodePath only supports ReadWriteOnce and ReadWriteOncePod (1.22+) access modes`. |
| csi-driver-nfs v4.13.4 with the upstream example in-cluster `nfs-server`, class `nfs-csi` (`nfsvers=4.1`) | FAIL. Driver and server roll out, but the server logs `exportfs: /exports does not support NFS export` and exits; provisioning fails with `failed to mount nfs server … timeout after 110s`. The runner had the `nfs` and `nfsd` modules loaded. |
| kind's local-path provisioner with `config.json` `{"nodePathMap":[],"sharedFileSystemPath":"/var/local-path-shared"}`, that path one runner directory (mode 0777) mounted by `extraMounts` into every node; class `standard`; writer on `spike-worker`, reader on `spike-worker2` | PASS. The claim binds; uid 10001 creates the crew directory and writes an object; the reader sees it through the read-only sub-path; a write through that mount fails with `Read-only file system`; an object written after the reader started is visible to it. |

The third class was added during the spike; the plan named only the first
two classes. By the plan's own decision order (Task 4 Step 5) the outcome
was "none": all three planned candidates (`standard` on two nodes,
`nfs-csi` on two nodes, `standard` on one node) failed. The added class
replaces that outcome, and §15 commits CI to it. uid 10001 needed no class
parameter (the pods set `fsGroup: 10001`; the shared directory was 0777).
Its two nodes share one host directory, not a network filesystem, so it is
a CI class and not a production one. `fsGroup` on an
NFS class was not exercised, since NFS never provisioned.

### 19.4 Still open

- No GKE, EKS or AKS cluster was probed; the second cluster was a k3s
  tenant namespace. No sandboxed runtime (gVisor) was available to probe.
- An NFS-backed class on kind in CI: only the upstream example server was
  tried, and it could not export its in-pod directory. A real NFS export
  was not tried.
- A kernel with Landlock built in but disabled at boot, where the
  syscalls answer `EOPNOTSUPP` rather than `ENOSYS`, was not tested; P2.7
  denied Landlock only through a seccomp profile.

Every planned row has a verdict.

## 20. Decided in sub-project 3a (2026-10)

Sub-project 3 is built as two plans, split at the cluster. This host has
no container runtime, so no `kind`, and the k3s tenant cluster allows no
cluster-scoped object, so no CustomResourceDefinition: whatever needs a
cluster is verified in CI only. 3a is the part that needs none.

### 20.1 The cut

- **3a, no cluster:** the `operator/` project, the five kinds and the
  `crds` command, every `desired` function, the `pki` module, the Daemon
  client, and the Jobs' commands in `agent/` (`crew-sync`, `pool-sync`,
  `harvest`). Done when `mise run operator` and `mise run agent` pass,
  `balerix-operator crds` prints five documents, and `mise run check`
  passes with the tmux mode unchanged.
- **3b, on a cluster:** the controllers and `balerix-operator run`,
  `docker/operator` and `docker/agent`, the `kind` setup with the
  ReadWriteMany class (§19.3), and `e2e-k8s`. Done when `e2e-k8s` passes
  without plugins, which is §17's condition for sub-project 3.

In 3a the binary has `crds` only; `run` arrives with the controllers.

### 20.2 The operator project

- **Five kinds from the start.** The Plugin kind (§4.5) is defined and its
  definition generated in 3a; its controller (§5.5) and `PUT /v1/plugins`
  stay in sub-project 4. Until then the Daemon's `desired` function sets
  `PluginsReady=True` for an empty `spec.plugins` and `PluginsReady=False`,
  reason `PluginsUnsupported`, for any other: a Daemon never reports
  `Ready` over a plugin list nothing acts on. Sub-project 4 replaces that
  branch.
- **Generated definitions live in `operator/crds/`** until sub-project 5
  moves them into the chart (§14.1). `mise run crds` regenerates them; CI
  fails when the committed files differ.
- **`desired` returns typed objects.** Each function takes the observed
  objects and returns `k8s-openapi` objects and the status conditions:
  Daemon to claims, StatefulSet, Service, pool Job and NetworkPolicies;
  Fleet to Crews, Agents and the `PUT` body; Crew to its sync Job; Agent
  to claim, bundle Secret, NetworkPolicy, Pod and harvest Job; Pod state
  to the Agent's conditions.
- **Nothing random is made in `desired`.** Tokens and certificates are
  observed inputs: the controller reads them from their Secrets, or mints
  them and writes the Secret first. `src/pki.rs` makes the per-Daemon
  authority and the serving certificates (§10.3) with `rcgen` and decides
  renewal from a clock it is given.
- **A Fleet resolves through `balerix-config` as it is.** The Fleet's
  `spec` becomes a `FleetFile` through `file::from_value`, with
  `metadata.name` as the name. The Daemon's `defaults` go in as
  `ResolveOptions::operator_layer`, with `runner: { type: pod }` beneath
  them, which is how an omitted `runner.type` is `pod`. After resolving,
  an agent whose runner is `tmux`, and a `<fleet>-<crew>-<agent>` over 63
  characters, fail with the config path.
- **`src/daemon_client.rs`** speaks §7.4's routes: `PUT /v1/fleets/{name}`
  with `agent_tokens`, `DELETE …?force=true`, the fleet's status and
  `GET /readyz`. It trusts only the authority it is given.
- New dependencies, all in `operator/`'s own manifest: `kube`,
  `k8s-openapi`, `schemars`, `rcgen`. The plan pins the exact versions it
  has probed.

### 20.3 Outside `operator/`

- **`RunnerSettings` gains `Pod`** with `resources`, `storage`,
  `nodeSelector` and `tolerations`. In `balerix-api` the Kubernetes shapes
  are opaque JSON, so `k8s-openapi` stays out of the core workspace; the
  operator gives them their types when it builds the Pod. A tmux-mode
  daemon refuses a `pod` runner with the config path.
- **The Jobs see the shared volume as the agent pod does.** They mount
  the same sub-paths at the same places (`repo`, `crew`, `fleet`,
  `daemon` under `/balerix/shared`), read-write where the pod has them
  read-only. A clone made in a pod names
  `/balerix/shared/repo/.git/objects` in its `alternates`, and
  `check_clone` accepts only the line naming the crew cache, so the
  harvest must see the cache at that path; a pool is likewise read at the
  path it was installed at. The pod layout is widened to a crew with no
  agent. §8.1's directory names are the operator's sub-path mapping;
  nothing in `balerix-runtime` knows them, and a Job reaches only its own
  crew's slice.
- **`balerix-agent crew-sync`** makes the crew cache or fetches `ref`
  into it, creates the crew's `no-hooks` directory, and installs the crew
  pool with the fleet and daemon pools as read-only parents. On success
  its termination message is the commit fetched (`cacheRef`); a failure's
  message starts with `cache:` or `tools:`, which is how the operator
  picks `CacheReady` or `ToolsReady`.
- **`balerix-agent pool-sync --level daemon|fleet`** installs one upper
  pool. This amends §8.3, where each crew's Job installed the fleet pool:
  two crews of one fleet would write that pool at once, and §5.3's lock is
  per crew. The fleet pool has a Job of its own, run before the crews'.
- **`balerix-agent harvest`** is §8.4 through a harvest-only entry point
  split from `harvest_and_remove`, which keeps its behaviour on one
  machine. The entry point removes nothing (the operator deletes the
  claim) and writes its git profile to a scratch directory, since the
  claim is mounted read-only.
- **The sidecar writes nothing under the crew root**, which is a
  read-only mount in a pod: its git log moves to the agent claim, and
  `no-hooks` is `crew-sync`'s to create. Left open by sub-project 2's
  review; `e2e-k8s` cannot pass without it.

### 20.4 Testing in 3a

- insta snapshots of every `desired` function, and the property test of
  §15: a Fleet's `spec` resolves to what the same content resolves to as
  a fleet file.
- `pki`: issuance, the chain a client verifies, and renewal against a
  given clock.
- The Daemon client against a stub, and against a real
  `balerix serve --mode kubernetes` over TLS for the 400 and the 409s of
  §7.4. `mise run operator` therefore builds `balerix` first, as
  `mise run agent` does.
- The Jobs against temp roots with the real git, mise and nono under
  `BALERIX_REQUIRE_TOOLS`, reusing Spec N's cases: a nono that cannot run
  fails the harvest, and a harvest leaves the claim byte for byte as it
  found it.
- CI gains an `operator` job beside `agent`.

### 20.5 Decided by the plan

What 3a's plan decided beyond §20.1–§20.4, as built.

- **The runner check is the resolver's.** `ResolveOptions` gains
  `runner: RunnerKind` (default `Tmux`), and an agent whose `runner.type`
  differs fails with the config path. The CLI resolves with `Tmux`, the
  operator with `Pod`. This amends §20.2's "a Fleet resolves through
  `balerix-config` as it is … after resolving": `balerix-config` gains
  that one option, and the operator checks nothing after resolving but
  the object name's length. A tmux-mode daemon also refuses a posted
  `pod` runner, since it trusts what it is posted (#24); a
  Kubernetes-mode daemon checks nothing, and its tests post specs with
  the default runner.
- **The pod mounts whole pool directories.** `/balerix/shared/crew`,
  `/balerix/shared/fleet` and `/balerix/shared/daemon` are each one
  read-only sub-path mount (holding `mise/`, and for the crew
  `no-hooks/`), beside `/balerix/shared/repo/.git/objects`. §7.4's
  `crew/mise` and the like are inside them.
- **A crew with no agent is `layout::SharedSlice`**, which the pod
  layout's branches also use: one type gives the paths of the slice a Job
  or a pod mounts. This amends §20.3's "the pod layout is widened to a
  crew with no agent".
- **The volume's directories** are §8.1's: `pools/daemon`,
  `fleets/<fleet>/pool`, `fleets/<fleet>/crews/<crew>/pool` and
  `fleets/<fleet>/crews/<crew>/repo`.
- **Each Job has an init container `slice`** that mounts the whole shared
  claim at `/balerix/volume` and runs `mkdir -p` for the Job's
  directories as uid 10001: the kubelet creates a missing sub-path owned
  by root, which the Job's user could not write.
- **Tool tables are arguments** (`--tool node=22.11.0`, repeated); the
  GitHub token is a mounted file (`--gh-token-file`). The agent's three
  Job commands are in one file, `agent/src/jobs.rs`, over
  `balerix_runtime::jobs`.
- **`pool-sync --level daemon` installs the embedded system table**
  (`claude`, `gh`) unless `--tool` is given. The sidecar renders the same
  embedded table, and both come from one image version (O-13).
- **A crew sync fetches with `--prune`.** A branch deleted on the remote
  stops being synced, and the Crew's `CacheReady` goes false with `the
  remote has no branch <ref>`. Harvested branches live under the cache's
  `refs/heads` and are untouched.
- **A Job is re-run when its input changes:** every Job carries
  `balerix.ai/input-hash`; `backoffLimit: 0`, and the operator retries
  with back-off (3b).
- **Object names:** per Daemon `balerix-<daemon>` (StatefulSet, Service,
  NetworkPolicy), `balerix-<daemon>-state` and `balerix-<daemon>-shared`
  (claims), `balerix-<daemon>-ca` (Secret and ConfigMap),
  `balerix-<daemon>-tls` and `balerix-<daemon>-admin` (Secrets),
  `balerix-<daemon>-pool` (Job). Per fleet `<fleet>-pool` (Job). Per Crew
  `<fleet>-<crew>-sync` (Job). Per Agent `<fleet>-<crew>-<agent>` (claim,
  Pod, NetworkPolicy), `…-bundle` and `…-token` (Secrets), `…-harvest`
  (Job). A Job's name becomes a label value, so it is bounded to 63
  characters: a name that would be longer is the truncated base, `-`,
  eight hex characters of the base's hash, then the suffix
  (`operator/src/desired/names.rs`). Names that fit are as listed.
- **Certificates:** the authority is valid ten years, a serving
  certificate ninety days, renewed thirty days before expiry. The expiry
  is kept as the Secret annotation `balerix.ai/not-after` (unix seconds),
  so nothing parses X.509. The authority is reloaded from its key alone:
  its parameters are a function of the Daemon's namespace and name.
  `pki::issue_serving` refuses an empty name list (`PkiError::NoNames`).
- **The Daemon pod mounts no shared volume.** In Kubernetes mode it
  installs no pool and reads workspaces over the link (§7.4).
- **A Daemon's two claims carry no owner reference:** deleting a Daemon
  leaves its state and its shared volume, as a StatefulSet leaves its
  claims. Everything else the operator makes is owned and
  garbage-collected.
- **The Daemon's NetworkPolicy admits its own agents, Jobs and the
  operator.** §10.2's "whatever the user's Ingress names" has no field in
  §4.1 and is left for the chart (sub-project 5).
- **The sidecar's requests are fixed** at `cpu: 50m`, `memory: 64Mi`
  (§6.1 says "small fixed requests").
- **A renewed serving certificate rolls the daemon pod:** its expiry is
  an annotation on the pod template, since `serve` reads the certificate
  at start.
- **In a pod the crew's logs are `<claim>/.balerix/state/crew-logs`**,
  outside the agent's sandbox grants like the sidecar's own state.
- **Types are built against `k8s-openapi`'s `v1_32`**, the oldest it
  offers; §6.1's minimum of 1.29 is about the cluster, and nothing here
  uses a field newer than native sidecars.
- **`status.session` on an Agent stays unset in 3a:** the Daemon's
  `AgentStatus` carries no session id.
- **Crew and agent names are not pattern-checked by the schema:** a
  structural schema cannot constrain map keys; `balerix-config` checks
  them at resolution.
- **A type holding a token, key or credentials never derives `Debug`:**
  it has none, or a hand-written one printing `<redacted>`. The
  operator's `Material`, `DaemonSecrets`, `AgentInputs` and
  `AgentObjects` have none.
- **`desired::common::hash` takes a `serde_json::Value`**, and the claim
  builder (`common::claim`) is shared by the Daemon's and the Agent's
  claims.

Known in 3a and left for 3b:

- The `slice` init container's directories being writable by uid 10001,
  and sub-path mounts of directories a Job made, are unverified without a
  cluster.
- The agent and daemon containers have a read-only root and no writable
  `/tmp`.
- The Daemon client accepts a plain `http://` base (its tests use one);
  3b must build `https://` endpoints only.
- Names derived from a Daemon's (`balerix-<daemon>…`) have no length
  bound.
- A stale sidecar crash message can show `MaterializeFailed` after a
  successful restart, until something is reported.
