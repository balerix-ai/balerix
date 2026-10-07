# Balerix — Spec O: Kubernetes

**Date:** 2026-10-02
**Status:** Design approved in brainstorm 2026-10-02; written spec approved
2026-10-02; the spike's findings recorded in §19; sub-project 2 decisions
in §7.4; sub-project 3a decisions in §20; sub-project 4 decisions in
§23; sub-project 5 decisions in §24
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

`charts/balerix-operator/templates/crds/` is generated by
`balerix-operator crds` (§24.1: templates, not Helm's `crds/`).
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

`docs/RELEASING.md` (and §24.5's order) gains the unit, the steps and
this setup.

## 15. Testing

- **Operator, pure:** insta snapshots of every `desired` function: Fleet
  to Crews and Agents, Agent to Pod, Secret and NetworkPolicy, Plugin to
  Deployment, Daemon to StatefulSet. Property test: resolving a Fleet's
  `spec` equals resolving the same content as a fleet file.
- **Operator, controllers:** against the pinned envtest binaries (a mise
  tool; §21.3), with a stub Daemon: rejection lands nothing; a changed
  `specHash` replaces the pod and keeps the claim; finalizers;
  `watchNamespaces`. Needs no cluster.
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
  and find its branch in the cache (`operator/tests/e2e_k8s.rs`, the
  operator out of cluster; §21.4).
- **Shared volume on kind:** every kind run (end to end, charts) gives the shared claim kind's local-path provisioner with
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
   plugins. Built as two plans, 3a without a cluster and 3b on one (§20,
   §21). 3a done 2026-10; 3b done 2026-10 (PR #127).
4. **Plugins.** §9. Done when `e2e-k8s` passes with flow and web, and the
   managed journey passes with `dev fake-plugin`. Built as two plans, 4a
   without a cluster and 4b on one (§23). 4a done 2026-10 (PR #143); 4b
   done 2026-10 (PR #144).
5. **Release and charts.** §13, §14. Done when a fork rehearsal publishes
   images and both charts and `helm install` from the published index
   brings up a Ready fleet. Built as two plans, 5a charts and 5b release
   (§24).

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
  that one option, and the operator checks after resolving only what
  Kubernetes alone can refuse, each failure with its config path: the
  object name's length, and the pod runner's shapes (`desired::fleet::
  check_runner`). `resources` must be a `ResourceRequirements`,
  `tolerations` a list of `Toleration`, and `storage.size`, when
  `storage` is set, a non-empty string; it is not parsed as a quantity. A
  Daemon's `spec.resources` is checked the same way: one that is not a
  `ResourceRequirements` sets `Ready=False`, reason `InvalidResources`,
  with the message `spec.resources: ` and the typing error, and no
  object is built from it. A tmux-mode daemon also refuses a posted
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
- **The Daemon client refuses a base that is not `https://`** with a
  setup error naming the scheme, so the admin token is never sent in
  clear. Its stub tests use `DaemonClient::insecure_for_tests`, which no
  controller calls. A success answer whose body does not decode is
  `Unexpected`, not `DaemonUnavailable` (§5.2 keeps that for a Daemon
  that does not answer).
- **`desired::common::hash` takes a `serde_json::Value`**, and the claim
  builder (`common::claim`) is shared by the Daemon's and the Agent's
  claims.

Known in 3a and left for 3b:

- The `slice` init container's directories being writable by uid 10001,
  and sub-path mounts of directories a Job made, are unverified without a
  cluster.
- The agent and daemon containers have a read-only root and no writable
  `/tmp`.
- Names derived from a Daemon's (`balerix-<daemon>…`) have no length
  bound.
- A stale sidecar crash message can show `MaterializeFailed` after a
  successful restart, until something is reported.

## 21. Decided in sub-project 3b (2026-10)

The second half of §20.1's cut: the four controllers behind
`balerix-operator run`, the two Dockerfiles, a `kind` cluster with the
ReadWriteMany class of §19.3, and `e2e-k8s`. The probe that shaped it: the
envtest binaries (`kube-apiserver`, `etcd`) from
`kubernetes-sigs/controller-tools` start on a host with no container
runtime, as an ordinary user, answer `/readyz` in about three seconds, and
take the five definitions, server-side apply under a field manager and
status patches. The controller tests therefore run on the developer's
machine and in the `operator` CI job; only `e2e-k8s` needs a cluster.

### 21.1 The cut, refined

- **In 3b:** `run` and the Daemon, Fleet, Crew and Agent controllers; the
  envtest harness and the controller tests; `docker/operator/Dockerfile`
  and `docker/agent/Dockerfile`; `scripts/kind-up.sh`; `mise run
  e2e-k8s` and its CI job; a fourth Job command, `balerix-agent
  crew-remove`. Done when `e2e-k8s` passes without plugins (§17).
- **Not in 3b:** the Plugin controller and §15's flow-rule step
  (sub-project 4); the images' release wiring (`build.sh`, `images.yml`,
  cosign), the operator's Deployment, RBAC and `verify-k8s`
  (sub-project 5). The operator process has no HTTP endpoint, no metrics
  and no leader election: one replica, and its Deployment is the chart's.
- **The operator runs outside the cluster in `e2e-k8s`**, a child process
  of the test against `kind`'s kubeconfig. Only the daemon and agent
  images are loaded into `kind`; the operator image is built and linted,
  and runs in a cluster first with the chart.
- **The Daemon is polled, not watched.** The Fleet controller reads
  `GET /v1/fleets/{name}` on every reconcile and requeues every 15 s; the
  admin API gains no watch route (the plugin-host API's
  `fleets/watch` is under a plugin token). An event-driven feed can
  replace the poll later without touching the kinds.

### 21.2 `run` and the controllers

`balerix-operator run` takes `--watch-namespaces a,b` (every namespace
when absent, §5.6), `--namespace` (the operator's own, for the Daemon's
NetworkPolicy; default from `POD_NAMESPACE`), `--daemon-image` and
`--agent-image` (default `Images::for_version` of the operator's own
version), and the kubeconfig `kube` infers. `kube` gains `client`,
`runtime`, `rustls-tls` and `ring`: its default TLS stack, on the ring
provider the Daemon client already uses.

Four `kube_runtime::Controller`s on one `Client` in one runtime. A
reconcile is observe, `desired`, apply: the owned objects come from the
controllers' reflector stores and the Secrets from the API; the `desired`
functions of §20 are called as they are; the result is applied with
server-side apply under the field manager `balerix-operator`, and status
is a merge patch on the subresource with the object's generation as
`observedGeneration`. Each controller `owns` what its function returns
(Daemon: StatefulSet, Service, the three Secrets, the ConfigMap, the pool
Job, the NetworkPolicies; Fleet: Crews, Agents, the fleet pool Job;
Crew: its sync Job; Agent: claim, bundle Secret, NetworkPolicy, Pod,
harvest Job). Two cross-kind watches: a Daemon change maps to the Fleets
naming it; a Fleet change maps to its Agents, which is how an Agent sees
a fresh record. An error requeues with kube-runtime's exponential
back-off; a healthy reconcile requeues on a period, 15 s for a Fleet (the
status poll) and 60 s for the others (renewal and drift). A
`DaemonClient` is built from the Daemon's authority ConfigMap and admin
Secret, cached per Daemon and rebuilt when either changes.

**Daemon.** Mint what is missing first: the authority (Secret and
ConfigMap), the serving certificate when absent or `needs_renewal`
against the real clock, the admin token. Then `daemon_objects` with the
serving certificate, whose sha256 is the pod template's
`balerix.ai/serving-cert` annotation (not its `not-after`: two
certificates issued in one second share that, #142), applied in one wave, and the pool Job under
the Job rule. Status is `daemon_status` over the shared claim, the
StatefulSet and the pool Job, and `Ready` additionally needs one
`GET /readyz` answered 200 once the StatefulSet has a ready replica.
`status.endpoint` is `names::endpoint`.

**Fleet.** §5.2 in order. `plan_fleet`; a `PlanError` is `Resolved=False`
and the reconcile ends. A `…-token` Secret is minted for every planned
Agent that has none, all are read, and the `PUT` carries one token per
agent. A 400 is `Accepted=False` with the Daemon's message and no child
is touched; a Daemon that does not answer is `DaemonUnavailable`, the
children are left alone and the reconcile is retried. On success the
fleet pool Job, the Crews and the Agents are applied, and the Crews and
Agents labelled with this fleet that the plan no longer has are deleted.
The record from `get` goes into the per-fleet cache the Agent controller
reads, and `fleet_conditions` is written with the ready count. Deletion
under `balerix.ai/fleet`: delete the Agents and wait until none remain
(their finalizers harvest), `DELETE …?force=true` on the Daemon, and for
`retain: None` one cleanup Job per crew, `balerix-agent crew-remove
--crew <fleet>/<crew>`, which removes `repo/` and `pool/` under the
crew's slice; then the finalizer is dropped.

**Crew.** `crew_sync_job`, with the fleet pool Job read by name.
`job_outcome` decides: `Absent` or `Stale` creates (a stale Job is
deleted first), `Running` requeues, `Succeeded` and `Failed` become
`crew_status`. §5.3's lock: a Job is created only when no Job labelled
with the crew is active, sync or harvest; otherwise requeue in 5 s.

**Agent.** Waits for the Crew's `CacheReady` and `ToolsReady`. Then
`agent_objects` with the token from its Secret and the credentials from
the Secrets the Daemon names, applied. A Pod whose `specHash` annotation
differs is deleted; once it is gone the next reconcile makes the new one
on the same claim. Status is `agent_status` over the Pod and the cached
record's entry for the agent. Deletion under `balerix.ai/harvest`: delete
the Pod and wait, then the harvest Job unless `balerix.ai/purge` is on
the Agent or its Fleet, or the Fleet is being deleted with `retain:
None`. A failed harvest keeps the finalizer and the message in a
condition (§8.5); a success deletes the claim and drops the finalizer.
The other children go with the owner reference.

**The Job rule.** Every Job has `backoffLimit: 0` (§20.5). A `Failed`
outcome is written into status and retried by the operator: the owner
carries `balerix.ai/attempts: n`, the delay is 30 s doubling to a cap of
10 min, and once it has passed the failed Job is deleted and recreated
with the count plus one. A changed input hash resets the count. One
function serves the pool, sync, harvest and cleanup Jobs; it reads its
clock from the controller context so a test can move it.

**Errors.** A reconcile's error names its object and has three arms: a
Kubernetes API error (requeued with back-off); a `ClientError`, which
`Accepted` classifies into `Rejected` and `DaemonUnavailable`; and a
`DesiredError` or `PlanError`, written into status as `Resolved=False` or
the kind's `Ready=False` and not retried beyond the period, since the
same input cannot resolve differently. Logs carry the namespace and name
as `tracing` fields and never a token or a key; the secret-holding types
stay without `Debug` (§20.5).

### 21.3 The envtest harness and the controller tests

A task-level mise tool on the `operator` task,
`github:kubernetes-sigs/controller-tools` at `envtest-v1.34.1` (the
spike's 1.34 line; `mise ls-remote` lists the tag), the way tuwunel is on
`verify-matrix-local`. `operator/tests/support/envtest.rs` starts `etcd`
and `kube-apiserver` on free ports under a temp root with a static token
file, waits for `/readyz`, applies the five definitions from
`operator/crds/`, and hands back a `kube::Client`. One instance per test
binary, shared through a `OnceLock`; each test owns a namespace. The
operator runs in-process: the controllers' entry point takes the
`Client` and the config, so a test spawns it as a task with its namespace
in `watch_namespaces` and aborts it at the end. The Daemon is 3a's stub,
extended to count `PUT`s and serve a scripted record. The tests skip
without the tool and fail under `BALERIX_REQUIRE_TOOLS=1`.

The harness's limit, which the tests state: envtest has no
controller-manager and no kubelet. A test stands in for the kubelet by
patching a Job's `status.succeeded` or `failed` and its pod's termination
message, or a Pod's phase and the sidecar's readiness. Owner-reference
garbage collection never runs there and is proved by `e2e-k8s` alone.
Finalizers and deletion timestamps are the API server's and are real.

Cases:

- A Daemon gets its Secrets, ConfigMap, StatefulSet, Service, pool Job
  and policies; a `not-after` within the renewal window yields a new
  certificate and a rolled pod template.
- A Fleet becomes Crews, Agents and token Secrets, and the `PUT` carries
  one token per agent; a 400 from the stub lands no child; a refused
  connection leaves existing children.
- A crew dropped from the Fleet: its Crew and Agent are deleted, the
  Agent's finalizer runs the harvest Job, a patched success deletes the
  claim and the Agent is gone.
- A changed `specHash` replaces the Pod and keeps the claim.
- A failed sync Job is `CacheReady=False` with the message and is
  recreated as attempt two once the moved clock passes the delay.
- The crew lock: a running sync Job holds off the harvest.
- `watchNamespaces`: a Fleet in another namespace is never reconciled.
- Fleet deletion with `retain: None`: Agents gone, the stub's `DELETE`
  hit, the cleanup Job made, the finalizer dropped.

### 21.4 Images, `kind`, `e2e-k8s` and CI

- **`docker/operator/Dockerfile`** is the plugin image's shape: distroless
  static nonroot, one static binary. **`docker/agent/Dockerfile`** is
  `FROM` the runtime image, a build argument `BASE` (default
  `ghcr.io/balerix-ai/balerix:<version>`), adding `balerix-agent` under
  `/usr/local/bin` with the entrypoint on it. Both pass hadolint.
- **`scripts/kind-up.sh`** (`down` too), with `kind` and `kubectl` as
  task-level tools: a two-node cluster whose config `extraMounts` one
  host directory into every node, the local-path provisioner's ConfigMap
  patched to `sharedFileSystemPath` (§19.3), the five definitions
  applied, and the two locally built images loaded with `kind load`. The
  local build is `cargo build --release` of `balerix` and
  `balerix-agent` for the host's own architecture, then `docker build` of
  the runtime image through `image-context.sh`'s core context and of the
  agent image on top of it. No musl, no manifest, no scan.
- **`mise run e2e-k8s`** is `operator/tests/e2e_k8s.rs` under nextest,
  gated as `e2e` is: it skips unless `KUBECONFIG` names the cluster and
  `BALERIX_K8S_IMAGES` the loaded tags, and fails under
  `BALERIX_REQUIRE_TOOLS=1`. It starts `balerix-operator run` as a child
  with `--watch-namespaces` on a fresh namespace and the image overrides,
  and drives the cluster with the typed kinds. The journey:
  1. a git server pod from the agent image (`git daemon`, a Service),
     seeded with the Phase 3 repository over `kubectl exec`; the crew
     uses `git: { push: false, auth: none }` as the one-machine `e2e`
     does;
  2. a Daemon whose `defaults.claude` runs `balerix dev fake-claude` from
     the image, as the daemon's own tests wire it; wait `Ready`;
  3. a Fleet of one crew and two agents; wait `Ready`, which means both
     sidecars forwarded `SessionStart`;
  4. one agent's pod deleted; the operator recreates it on the same
     claim, the Agent is `Ready` again and the claim's `home/` is intact
     (read over `kubectl exec`);
  5. that agent dropped from the Fleet; its harvest Job succeeds and the
     branch is listed in the crew cache, read from a probe pod mounting
     the shared claim;
  6. the Fleet deleted with `retain: None`; the crew's directories are
     gone and the Daemon still lists the fleet, down (O-16: `retain: None`
     is a plain `down`, which keeps the record).
  `kind`'s network plugin does not enforce NetworkPolicy, so the policies
  are inert here; the test says so. §15's flow-rule step arrives with
  plugins; `verify-k8s` waits for a cluster.
- **CI:** `ci.yml` gains `e2e-k8s` on `ubuntu-24.04`, path-filtered to
  `operator/**`, `agent/**`, `crates/**`, `docker/**`, `scripts/kind*`,
  `mise.toml` and the workflow, and on the nightly; a required check
  like `operator`. The `operator` job installs the envtest tool.
- **The `cargo-insta` 1.49.0 pin in `mise.toml` lands with this
  sub-project**, and the `insta` dev-dependency in the root workspace,
  `operator/`, `plugins/common`, `plugins/matrix` and `plugins/github`
  moves to 1.49.0 with it, so tool and library match; the snapshot
  suites on the branch are the proof.

### 21.5 §20.5's open items

- The `slice` init container's ownership and the sub-path mounts of
  directories a Job made are what `e2e-k8s` step 3 exercises; a failure
  there is a finding for the plan, not a design change.
- The agent, sidecar and Job containers get an `emptyDir` at `/tmp`:
  git, mise and nono expect a writable temporary directory under a
  read-only root. The daemon container, whose home is its claim, needs
  none unless `e2e-k8s` shows otherwise.
- Names derived from a Daemon's are bounded as §20.5 bounds a Job's
  (`names.rs`), so a long Daemon name cannot make an invalid label.
- The sidecar's termination message is read only while the sidecar is
  not running: a recreated or restarted sidecar's last state no longer
  shows `MaterializeFailed` once it runs again.

### 21.6 Decided by the plan

What 3b's plan decided beyond §21.1–§21.5, as built.

- **Owned objects are read by label-selected lists, not reflector stores.**
  A reconcile lists its children by `balerix.ai/*` labels through the API
  (`Api::list`), one call per kind it needs. Reflector stores for five child
  kinds would be shared caches that lag a server-side apply by one watch
  event, and the lists are small. The main kinds are still watched. This
  amends §21.2's "from the controllers' reflector stores". A Job's pods are
  selected by `batch.kubernetes.io/controller-uid`, not `job-name`: a
  recreated Job's predecessor's pods keep `job-name` until they are
  collected.
- **The attempt count is one annotation on the owner,** `balerix.ai/attempts`,
  a JSON object from Job name to count (`{"f-c-sync": 2}`), since a Fleet
  owns its pool Job and, while deleting, one cleanup Job per crew. An entry
  is removed when the Job succeeds or its input changes. The delay is
  `30 × 2^(n−1)` seconds for attempt `n`, capped at 600, measured from the
  failed Job's `Failed` condition's transition time, else its creation time.
- **The Fleet's agents are re-reconciled through a Fleet watch.** The Agent
  controller `watches` Fleets with a mapper that returns the Agent names the
  Fleet controller last planned, kept in the `Context` per fleet, so a Fleet
  change reaches its Agents at once. The Agent period equals the Fleet's
  (15 s), so a phase change the poll brought lands within one period.
  (`Controller::reconcile_on` is unstable in kube-runtime 4.2; a mapper
  cannot list.) An Agent's `phase` therefore trails the Fleet's `Ready` by
  one Agent reconcile: the phase comes from the cached record, `Ready` from
  the pod.
- **`--watch-namespaces` with several names starts one set of controllers per
  namespace** in the same process, each on `Api::namespaced`, sharing the
  `Context`; with none it is one set on `Api::all`. A single `Controller`
  watches one namespace or all, and a per-namespace Role (§5.6) forbids a
  cluster-wide list.
- **Periods and the clock are configuration.** `RunConfig` carries the Fleet
  period (15 s), the other period (60 s) and a `Clock`
  (`Arc<dyn Fn() -> i64 + Send + Sync>`, unix seconds). Tests set periods of
  1 s and a clock they can move, so the Job retry test needs no 30 s wait.
- **An operator outside the cluster reaches a Daemon through
  `--resolve host=ip:port`** (reqwest's resolver override on the Daemon
  client). reqwest's override ignores the address's port, so `--resolve`
  rewrites the endpoint's port instead; TLS still verifies the Service's
  host name as in a pod. `e2e-k8s` runs `kubectl port-forward` in a restart
  loop, since it exits while the pod is Pending and on restarts. The chart
  never uses `--resolve`.
- **A test-only Daemon URL override.** `RunConfig::insecure_daemon_url:
  Option<String>` (the hidden flag `--insecure-daemon-url`, as
  `balerix-agent sidecar` hides `--allow-http`) makes every Daemon client
  `DaemonClient::insecure_for_tests` at that URL. The envtest tests' stub
  Daemon is plain HTTP; a pod's Daemon is `https`.
- **No claim, no harvest.** An Agent with no claim has nothing to harvest; its
  finalizer drops at once.
- **A failed harvest is `Ready=False`, reason `HarvestFailed`,** with the
  Job's message; the Agent keeps its finalizer until the retry succeeds.
- **A deletion that waits says why on the object.** While a Fleet's or an
  Agent's cleanup waits or fails, its `Ready` is `False`, reason `Deleting`,
  with the wait: the Agents that remain, the Daemon that did not take the
  down, the crews whose cleanup Job is not finished (with a failed Job's
  message), the Agent's pod, its unfinished harvest Job. The Fleet's other
  conditions stay as they were. The Agent's harvest messages, `Deleting` and
  `HarvestFailed` both, and the Fleet's "agents remain" name the escape: the
  annotation `balerix.ai/purge=true` on the Agent or its Fleet.
- **Every Job has `activeDeadlineSeconds: 3600`.** A pod that never starts
  (an image that cannot be pulled, a claim that is gone) fails the Job
  under the Job rule instead of leaving it running forever; a failed Job
  with no pod message reports its `Failed` condition (`DeadlineExceeded:
  …`). An hour, for a slow pool install.
- **A Job the namespace's deletion refuses is skipped.** A create that the
  API server refuses with the cause `NamespaceTerminating` is not retried:
  a harvest or cleanup Job counts as done (with a warning), since the
  namespace takes the claim and the objects with it.
- **The crew lock (§5.3) is an in-process mutex** per `<ns>/<fleet>/<crew>`
  in the controllers' `Context`, held by `ensure_job` across the busy check
  and the create, so a sync and a harvest that both see the crew idle
  cannot both start. The operator is one replica (§21.1); a second would
  need a lease. A stale Job is deleted with foreground propagation: it
  stays listed, deleting, until its pods are gone, and `ensure_job` and the
  busy check treat a deleting Job as running, so a predecessor's pod holds
  the crew.
- **A reconcile is bounded at 30 s** (`controllers::RECONCILE_TIMEOUT`,
  through `bounded`). A kube request now and then is lost on a pooled
  HTTP/1 connection and never answered, and kube-client 4.2 has no read
  timeout; the runtime never starts a second reconcile of an object whose
  first still runs, so the object would be stuck until a restart. On expiry
  the future is dropped (a held crew lock with it), a warning names the
  object, and `Error::TimedOut` requeues in 2 s with no back-off.
- **A credentials or token Secret the user must fix is `Materialized=False`,
  reason `CredentialsInvalid`,** on the Agent: a missing Secret, a missing
  key, or a `credentials.json` that does not parse (serde's line and
  column only, since its message can quote the input). The message names
  the Secret and the key, never what it holds; the reconcile requeues on
  its period. This amends §21.2's error arms: `plan_fleet` cannot see
  Secrets.
- **An Agent waiting on its Crew is `Materialized=Unknown`, reason
  `WaitingForCrew`,** with the Crew's failing condition's message when it has
  one. The Fleet and Agent controllers unwrap kube's finalizer error so
  `Error::Waiting` reaches `error_policy`: a 2 s requeue, not a warning and
  back-off.
- **A token Secret is owned by the Fleet,** not the Agent: §5.2 keeps it across
  reconciles and pod replacements, and the Fleet's deletion removes it.
- **The Daemon's `Ready` asks `/readyz` once per reconcile** only when the
  StatefulSet reports a ready replica; without one `daemon_status` already
  says `DaemonNotReady`. The override goes through `conditions()`, so an
  unchanged `Ready=False/DaemonNotReady` keeps its transition time;
  otherwise each status patch would re-trigger a reconcile and a probe.
- **A newly minted authority always re-issues the serving certificate.** A
  lost CA Secret would otherwise leave a certificate the new authority
  cannot verify. Amended: any serving certificate the authority read or
  minted cannot verify (`pki::verifies`) is re-issued, which also covers a
  reconcile dropped between applying the two Secrets.
- **The test is the kubelet.** In envtest nothing runs pods. A test patches a
  Job's `status` and its pod's termination message, patches a Pod's
  `status`, and deletes a Pod once the operator has set its deletion
  timestamp. There is no scheduler either, so an unscheduled Pod deletes at
  once; the helpers track the Pod's uid. There is no garbage collector: a
  Job deleted in the foreground keeps its `foregroundDeletion` finalizer
  until the test takes it off (`support::reap_job`). Kubernetes 1.34's Job status
  validation needs `startTime` and `FailureTarget=True` before
  `Failed=True`, and `startTime`, `completionTime` and
  `SuccessCriteriaMet=True` before `Complete=True`. The stand-in pod's
  termination message is written before the Job status, since the Job patch
  wakes the operator (Pods are not watched). A claim's
  `kubernetes.io/pvc-protection` finalizer is removed by the test: there is
  no controller-manager.
- **The envtest binaries run under a parent-watching shell,** `bash -c '… &
  while kill -0 $PPID; do sleep 0.5; done; kill $!'`, so a test process that
  exits leaves no `etcd` or `kube-apiserver` behind. There is one instance
  per test process, since nextest runs each test in its own, started on free
  ports under `CARGO_TARGET_TMPDIR` and bounded to four at once by a nextest
  test group in `operator/.config/nextest.toml`. Readiness is `GET /readyz`
  returning `ok`; a start that hits a port collision is retried on new
  ports; the watcher ends the servers with SIGKILL. A start first removes
  every `envtest-<pid>` root whose process is gone: each holds etcd's data
  directory, about 120 MB.
- **`e2e-k8s` seeds its repository through a git server pod** from the agent
  image (`git daemon`, a Service), since nothing on the runner is reachable
  from the cluster; the crew uses `git: { push: false, auth: none }`. After
  a `retain: None` deletion the journey asserts the Fleet object is gone, the
  crew's `repo/` and `pool/` exist and are empty, and the Daemon still lists
  the fleet, down with no agents (O-16: a plain `down` keeps the record):
  `GET /v1/fleets/f` answers it to a `DaemonClient` built from the
  `balerix-default-admin` Secret and the `balerix-default-ca` ConfigMap,
  through the port-forward under the Service's name.
- **`scripts/operator.sh check` leaves the journey out** with `-E 'not
  binary(e2e_k8s)'`; `e2e` runs it alone under a nextest profile `e2e-k8s`
  (45 minutes per test). The CI job has `timeout-minutes: 90`.
- **The cleanup Job is `balerix-agent crew-remove --crew <fleet>/<crew>`,**
  over `balerix_runtime::jobs::remove_crew`, which removes `repo/` and
  `crew/` (the pool and logs) under the crew's slice and leaves the mount
  points; its outcome line is `removed`. The Job is
  `names::remove_job(fleet, crew)` = `<fleet>-<crew>-remove`, bounded as the
  other Job names.
- **`/tmp` is an `emptyDir`** in the agent pod (both containers) and in every
  Job (the `job` container), mounted at `/tmp`. The daemon pod gets none.
- **The agent claim is mounted through the sub-path `agent/`,** made by a
  `claim` init container as uid 10001. A volume root is root-owned (local-path
  0777; a CSI volume root:10001 with `fsGroup`), and the sidecar's 0700 chmod
  of the root failed with EPERM. The sidecar, the agent and the harvest Job
  see it at `/balerix/agent`. This is §21.5's slice pattern applied to the
  agent claim.
- **The kind config mounts the shared directory into every node,** the control
  plane included, since local-path's helper pod may run there.
- **Names derived from a Daemon's are bounded by `names::job_name`'s rule**
  (truncate, `-`, eight hex characters of the hash, suffix), applied to every
  `balerix-<daemon>…` name through one function. The StatefulSet (and its
  Service) is bounded to 52 characters, since its pods carry
  `controller-revision-hash: <name>-<10 characters>`. The `balerix.ai/daemon`
  label value is `names::daemon_label`, the Daemon's name under the same
  rule at 63, wherever it is written or selected. The `balerix.ai/fleet`,
  `crew` and `agent` values are the user's names as they are: a Fleet, crew
  or agent name over 63 characters is refused at apply (a 422 in the log).
- **The operator image is linted, not built, in 3b.** `mise run lint` (CI's
  `check`) runs hadolint over every `docker/*/Dockerfile`. kind-up builds
  the daemon and agent images only, since the operator runs outside the
  cluster; the operator image needs the static (musl) release binary
  `distroless/static` can run, which is the release pipeline's, so it is
  first built there (sub-project 5). This amends §21.1's "built and
  linted".

Known in 3b and left open:

- On Kubernetes nothing purges a down record once the Fleet object is
  gone, so down records accumulate on the Daemon's volume until a purge
  path exists (an issue).
- `fsGroupChangePolicy` on a CSI class may re-add group bits to the agent's
  0700 tree. Not exercised: the journey runs on kind's local-path only.
- The harvest Job and its pod are collected with the Agent, so the outcome
  (`harvested <branch>`) survives only in the operator's log. Keep it
  somewhere (an Event) if anything needs it.
- A Fleet naming a Daemon that does not exist yet waits a period (15 s)
  instead of a Daemon watch.
- The envtest suite has load-sensitive waits (60 s `wait_for`). A lost
  request no longer hangs a test: probes are bounded at 5 s and every
  other request at 10 s (§22.1).
- The lost requests are hyper#4207 (a stale HTTP/1 want pools a connection
  mid-watch); until a fixed hyper ships, the watches run on an unpooled
  client (#129). The per-request timeout is §22.1.
- §8.5's "the Agent's branch changed" row is not built: a branch change is
  a spec change, so the Pod is replaced on the same claim, and the
  sidecar's `ensure_clone` then wants to recreate the clone, which
  harvests into a crew cache mounted read-only in the pod. The agent stays
  down with a `Materialized=False` git error; the old clone is kept. The
  fix is the controller's: note the branch the claim was made for, and on
  a change run the harvest Job, delete the claim and make a new one.
- What did not bite on kind (ubuntu-24.04): the `slice` init `mkdir`, the
  `/tmp` emptyDirs and the sidecar's sandbox self-test. The crew cache is a
  clone with a work tree (`repo/.git`). Pool Jobs took 4 to 19 s and the
  journey one to two minutes.
- The operator's RBAC, Deployment and metrics, and `verify-k8s`, are
  sub-project 5; the Plugin controller is sub-project 4.

## 22. Operator hardening (2026-10)

The follow-ups 3b left in the operator's request handling, its Jobs and
its authority, fixed together: #129, #130, #131, #132, #133, #134 and
#138. Each fix touches a flow that §21 built. What they add between them is
the operator writing Kubernetes Events, which the chart's RBAC must allow
(§22.5). Not in this item: running the Fleet reconcile's requests in
parallel, which is the real cure for a very large Fleet's slow reconcile
(§22.2 only stops it from looping); §8.5's branch-changed row (#128); the
down-record purge path (#135); the runtime flakes #136 and #137.

### 22.1 A bound on every kube request (#129, #138)

- **The reconcile client times out each request at 10 s.** Since #139 the
  controllers' watches run on their own unpooled client
  (`watch_client`), so the pooled client carries no watch, and a bound on
  every request it sends cuts nothing long-lived. The bound is tower's
  `TimeoutLayer` (tower's `timeout` feature; no new crate), added through
  kube's `ClientBuilder::with_layer` in one function,
  `request_client(config)`, which `main` uses in place of
  `Client::try_from`. 10 s sits well inside `RECONCILE_TIMEOUT` (30 s), so
  a lost request fails its reconcile with an ordinary error (`Error::Kube`,
  the usual back-off) before the reconcile bound has to cut it.
- **The bound covers the response head only.** The timeout wraps the
  service call, which resolves when the head arrives; kube then reads the
  body outside it. A lost request (hyper#4207) never gets a head, so it is
  covered. A body that stalls after its head is not, and stays under the
  reconcile bound. The layer sits outside kube's default stack, so the
  bound also covers kube's own 429/503/504 retries and a credential
  refresh: a throttled or restarting API server surfaces as `Error::Kube`
  and the usual back-off rather than a long wait.
- **The test harness builds its client through `request_client` too,** so
  no direct API call in a test can wait past 10 s, inside `wait_for` and
  `hold_for` or outside them (#138). The operator under test keeps sharing
  the test's client: with every request bounded, a lost one costs a retry,
  not the test.
- **`watch_client` stays** until a hyper with the fix for hyperium/hyper#4207
  (hyperium/hyper#4208 and seanmonstar/want#6) ships. #129 closes with this
  item; the upstream report is hyper#4207 itself.
- **This amends §21.6.** Its "per-request client timeout for production"
  item is done, and its "occasional 180 s hang" item no longer holds: a
  probe is bounded at 5 s and every other test request at 10 s.

### 22.2 Repeated reconcile timeouts back off (#130)

- **`error_policy` counts consecutive `TimedOut`s per object** in the same
  per-object map as other errors, which a reconcile that ends well clears
  (`reconciled`). The first two timeouts still requeue in 2 s, because one
  timeout most likely means one lost request. From the third on, the delay
  is the one other errors get: 5 s, doubling per attempt, at most 5 min.
  Attempts are counted from the first timeout, so the third waits 20 s.
- **The third consecutive timeout publishes a Warning Event** on the object,
  reason `ReconcileTimedOut`, message `the reconcile did not finish in
  30s, 3 or more times in a row: backing off`, and so does every later
  consecutive timeout. The message is fixed, so kube-runtime's `Recorder`
  (events.k8s.io/v1, reporter `balerix-operator`) folds the repeats into
  one Event whose series count is the number of repeats. `within` counts
  the timeout and publishes the Event, since it runs async and
  `error_policy` does not; `error_policy` reads the count.
  Publishing is best effort: an Event that cannot be written is logged and
  dropped, and never fails the reconcile.
- **The bound itself does not change.** A reconcile that needs more than
  30 s still never finishes; this only stops it from running nearly all the
  time and loading a slow API server, and makes the object say why.

### 22.3 Jobs and the crew lock (#131, #132, #133)

- **A Job is failed when `status.failed ≥ 1` or its `Failed` condition is
  `True`** (#131). One predicate serves `job_outcome`, `unfinished` and so
  `crew_holders`. A Job whose pod was never created, refused by a ResourceQuota
  or an admission webhook, reaches `Failed=True/DeadlineExceeded` under
  `activeDeadlineSeconds` with `failed: 0`. It is now retried under
  §21.6's Job rule like any failure, and reports its condition through
  `failed_condition`. This amends §21.6's deadline bullet, whose claim was
  broader than the code.
- **The crew lock also counts pods** (#132, gap 1). `crew_holders` lists pods
  labelled `balerix.ai/fleet=<fleet>,balerix.ai/crew=<crew>` that carry a
  `batch.kubernetes.io/job-name` label, so an agent pod never matches. A pod
  holds the crew while its phase is neither `Succeeded` nor `Failed`, or
  while it is being deleted. A dropped crew's sync pod, whose Job the
  garbage collector took when the Crew was deleted in the background, then
  keeps a harvest of that crew's agents from starting until the pod is gone.
  `crew_holders` leaves the wanted Job out of the Job list by name only, so
  pods labelled `job-name=<wanted>` that a same-named predecessor left
  (collected in the background) are counted while unfinished or deleting,
  which is the intended conservative behaviour.
- **A crew held by something being deleted for too long says so** (#132,
  gap 2). `crew_holders` returns what holds the crew, not a bare `bool`. When
  that is a Job or a pod whose deletion timestamp is older than
  `RunConfig::stuck_after` (5 min; tests set it lower), the owner of the
  wanted Job gets a Warning Event, reason `CrewLocked`, naming the object
  and its deletion time: the Crew for a sync Job, the Agent for a harvest
  Job, the Fleet for a cleanup Job. The lock still holds. Forcing the
  object out (a lost node's pod) is the user's call. The Event names the
  object so they can.
- **The busy check and the create outlive a dropped reconcile** (#132,
  gap 3). `ensure_job` takes the crew lock as an owned guard
  (`Mutex::lock_owned`) and runs the check and the create in a spawned task
  holding it, then awaits the task. When `RECONCILE_TIMEOUT` drops the
  reconcile, the task runs on to the create's answer, each of its requests
  (the Job list, the pod list and the create) bounded at 10 s under §22.1,
  and only then releases the lock. A second reconcile therefore
  either waits for the lock or lists the created Job. **Accepted:** a
  create the client gave up on at 10 s that the API server still commits
  later. Its write has passed the client's bound, and the API server's own
  request timeout (60 s) bounds it.
- **The lock map is pruned** (#133). On release, under the map's mutex, a
  crew's entry is removed when the releaser's `Arc` and the map's are the
  only two (`Arc::strong_count == 2`). A caller that cloned it in between
  keeps it. A caller that comes after the removal makes a new entry, and
  nothing holds the old one. The map then holds only the crews that are
  locked or awaited.

### 22.4 An authority whose key does not match its certificate (#134)

- **An authority is read back only if it works.** `read_issued`'s
  authority half also checks that the CA Secret's `ca.key` signs leaves
  that verify against its `ca.crt`. It issues a throwaway leaf and runs
  `pki::verifies` on it (`pki::authority_works`), so no x509 parser is
  added. An authority that fails this, or does not parse, is missing: it
  is re-minted, which re-issues the serving certificate (§21.6), and the
  Daemon pod rolls once. Before, the leaf never verified, so it was
  re-issued every reconcile and the pod rolled on each one.

### 22.5 Events and the chart's RBAC

The operator now writes Events (`events.k8s.io/v1`, verbs `create` and
`patch`) in every namespace it watches. Sub-project 5's chart grants this
in the operator's ClusterRole, or in each watched namespace's Role
(§5.6, §14.1). The events are `ReconcileTimedOut` (§22.2) and `CrewLocked`
(§22.3), both of type Warning. An Event lives about an hour, so a held
crew's Event is re-published while the hold lasts (the `Recorder` adds to
its series). A condition would have needed a new type on four kinds and
their status writers. An Event fits all four the same way and is where
`kubectl describe` looks for "why is this stuck".

### 22.6 Testing

- **Unit:** a client aimed at a socket that accepts and never answers
  fails within the bound; `error_policy` keeps 2 s for two timeouts, then
  backs off, and resets after a success; `job_outcome` and `unfinished` read
  a `Failed=True`, `failed: 0` Job as failed with the condition's message;
  the crew lock stays held until a spawned create finishes after its caller
  is dropped; the lock map drops a released entry and keeps one still
  cloned; `pki::authority_works` refuses a key paired with another
  authority's certificate.
- **envtest:** a Job patched to `FailureTarget=True` then
  `Failed=True/DeadlineExceeded`, with `failed: 0`, is retried and its
  owner's condition carries the message; a Pending pod with a crew's Job
  labels and no Job keeps a harvest Job from being created until it is
  deleted; a crew pod held by a finalizer past `stuck_after` puts a
  `CrewLocked` Event on the owner; a CA Secret rewritten with another
  authority's key is re-minted once, after which the pod template's
  annotation holds across reconciles.

## 23. Decided in sub-project 4 (2026-10)

Sub-project 4 is §9: plugins on Kubernetes. Brainstormed 2026-10-05. It
refines §4.5, §5.5 and §9 where the code showed them to be incomplete,
and it is built as two plans cut at the cluster, as 3 was:

- **4a, without a cluster:** `balerix-api`, the plugin SDK and the
  Daemon (§23.1–§23.3). Done when the two-process integration tests of
  §23.6 pass.
- **4b, on a cluster:** the Plugin controller, the Daemon controller's
  plugin list, the managed-Fleet writer, the plugin images and the plugin
  journey in `e2e-k8s` (§23.4–§23.6). Done when §17's condition holds:
  `e2e-k8s` passes with flow and web, and the managed journey passes with
  `dev fake-plugin`.

Not in this item: the chart, its RBAC (Events included) and publishing
(sub-project 5); github and matrix in `e2e-k8s` (they need no code change,
§23.1, but external accounts); a hot reload of renewed certificates.

### 23.1 Wire and SDK

- **`hello` carries the manifest.** `HelloRequest` gains
  `manifest: Option<PluginManifest>` (serde default, skipped when `None`);
  the protocol stays 1. The SDK's `Plugin` trait gains
  `fn manifest(&self) -> &'static str`, which each in-tree plugin
  implements as `include_str!("../package/balerix-plugin.yaml")` and
  `dev fake-plugin` builds inline; the SDK parses it and sends it. On one
  machine the Daemon still reads the package's manifest and ignores
  `hello`'s copy. In Kubernetes mode a `hello` without one is refused:
  `hello.manifest: required in kubernetes mode`.
- **Inputs stay environment variables; secrets are files they name.**
  This amends §9.1. The four variables of today keep their meaning. New,
  all optional, set by the Deployment (§23.4):

  | Variable | Meaning |
  |---|---|
  | `BALERIX_PLUGIN_TOKEN_FILE` | the token, read from a file; wins over `BALERIX_PLUGIN_TOKEN` |
  | `BALERIX_CA_FILE` | the only authority the host client trusts; `BALERIX_API_URL` must then be `https://` |
  | `BALERIX_PLUGIN_TLS_CERT`, `BALERIX_PLUGIN_TLS_KEY` | serve TLS with this certificate and key (both or neither) |
  | `BALERIX_PLUGIN_LISTEN` | the bind address; default `127.0.0.1:0`, a pod sets `0.0.0.0:7644` |

  The plugin's config is not mounted. It arrives in the `hello` reply as
  it does today, so it never lands on the pod's disk; §9.1's "its config
  (secrets injected)" among the mounted files is withdrawn.
- **TLS is always on in the core crates, and trusts one authority.** The
  SDK serves with axum-server (`tls-rustls-no-provider`, the Daemon's
  stack, §7.4); its host client is reqwest with `rustls-no-provider` and
  `use_preconfigured_tls`, a root store holding only the CA; its
  WebSockets use tokio-tungstenite's `Connector::Rustls` with the same
  config. The Daemon's plugin client (`plugins/client.rs`) gets the same
  client shape, trusting the authority the Daemon serves under. The ring
  provider is installed explicitly; there are no webpki or native roots.
  With no CA and no certificate given, both sides stay plain HTTP on
  loopback, exactly as today. A renewed certificate is picked up by a
  restart (the operator rolls the Deployment, §23.4), never reloaded.
  `scripts/check-core-deps.sh` asserts the new exact feature sets. A cargo
  feature was rejected: two SDK feature sets to test, and a plugin built
  without it would fail only in a pod.
- **`hello.listen` is ignored in Kubernetes mode.** The Daemon calls the
  address the operator gives it (§23.2). Plugins still send it, and on
  one machine it must still be loopback.
- **github and matrix need no code change.** github's webhook `listen`
  is set through `spec.config` to the `expose` port; it is plain HTTP
  behind its Service, and TLS toward GitHub is an ingress's. matrix keeps
  its store in `BALERIX_PLUGIN_SCRATCH`: the scratch claim when declared,
  an `emptyDir` otherwise.

### 23.2 The Daemon's plugins in Kubernetes mode

- **A plugin-source port behind `PluginHost`.** Where the Daemon's
  plugins come from becomes a seam with two adapters. `Packages` is
  today's code, unchanged: `plugins.yaml`, the materializer and the
  reserved `balerix` fleet's actor. `Declared` is Kubernetes mode's and
  lives in `kube/`, beside `NoFiles`, `NoPool` and `LinkHub`. The
  registry, the interceptor chain, the plugin client, the proxy and the
  `plugin-host` routes sit above the port and do not change. Rejected: a
  mirrored reserved fleet (mirroring assumes a sidecar link per agent,
  and a plugin's lifecycle is its Deployment's), and a sidecar per plugin
  (contradicts §9.1).
- **`PUT /v1/plugins` (admin token)** replaces the whole list; its order
  is interceptor order, the order of `Daemon.spec.plugins`. An entry is
  `{name, grant, config, fleetDefaults, token, url}`: `grant` a list of
  capabilities, `token` at least 32 characters, `url` `https://`. An
  unknown capability, a duplicate name, a reserved name or a non-https
  url is 400. `kubernetes` joins the reserved plugin names. A tmux-mode
  Daemon answers 409 `this daemon reads plugins.yaml`. In Kubernetes
  mode `POST /v1/plugins/sync` and `DELETE /v1/plugins/{name}` answer
  409 `this daemon is in kubernetes mode; change its plugins through the
  Daemon's spec.plugins`.
- **`Declared` holds the list in memory** (§9.2) and matches a
  presented token against it in constant time, as `plugin_for_token`
  does. `hello` is checked in order: the name equals the token's plugin;
  the protocol is 1; a manifest is present; its name equals the entry's;
  its `needs` are within the grant. The refusal names the first failure
  (`hello.manifest.needs: kv is not granted`) and is kept on the
  plugin's row. Routes go on checking the manifest's `needs`, which the
  grant now bounds.
- **Readiness.** A plugin is ready from an accepted `hello` until three
  consecutive missed health polls (the existing 10 s poll); a good poll
  makes it ready again only if a `hello` was accepted for its current
  entry. Activation is re-sent on `hello`, as today.
- **A Daemon restart does not need a new `hello`.** An accepted hello's
  manifest and version are written to
  `<state>/plugins/<name>/hello.json` with the hash of the list entry
  they were accepted under. When the operator re-sends the list after a
  restart (§23.4), an entry whose hash matches is re-checked against its
  grant and becomes ready on its first good health poll, with activation
  re-sent then. Any change to an entry changes its hash; the operator's
  Deployment hash covers the same fields, so the pod rolls and says
  `hello` again.
- **`GET /v1/plugins`** keeps its row, `PluginStatus`. In Kubernetes
  mode `phase` is `Ready` when ready and `Starting` before a first
  `hello`; a refused `hello` is `Failed` with the refusal in `message`;
  a ready plugin that misses its polls is `Failed` with `health: …`.
  `listen` is the entry's url. The operator reads these rows (§23.4).
- **Dropping a plugin** from the list takes it out of the chain and
  drops its stored managed requests (§23.3); the operator deletes its
  Fleets. The Daemon itself downs nothing for a dropped plugin in
  Kubernetes mode: the fleets are the operator's.

### 23.3 Managed fleets

This refines §9.4.

- **The plugin's `PUT plugin-host/fleets/{name}` answers at once.** The
  Daemon runs today's checks synchronously: reserved name, owner,
  resolution beneath the plugin's `fleetDefaults` held to the restricted
  surface (Spec M §12.1). A failure is the same 400 or 409 as on one
  machine. On success it stores `{name, plugin, file}` in memory and under
  `<state>/managed/<name>.json`, records the fleet with
  `owner: <plugin>`, the resolved spec and no agents, and answers as
  Spec M §12.2 says: the record if the plugin has `fleets`, 204
  otherwise. This is the SDK's documented contract ("the call returns
  before the fleet is ready"); `fleets/watch` and `GET fleets/{name}`
  see the record at once and its agents when the operator's pods link.
  Rejected: holding the PUT until the operator has applied (ties a
  plugin to the operator's poll and fails while the operator is down).
- **The plugin's `DELETE plugin-host/fleets/{name}`** marks the stored
  request down with its query, and is answered with the record.
- **`GET /v1/managed-fleets` (admin)** lists
  `[{name, plugin, file, down}]`, `down` being the query or absent.
- **Owner stays the plugin.** Mirroring is a property of the mode
  (`actor.rs`), not of `owner: kubernetes`, so a managed fleet's record
  keeps `owner: <plugin>`: the plugin downs it, watches it and gets the
  usual 409 on others'. The operator applies it with `PUT
  /v1/fleets/{name}` carrying `agent_tokens` and a new field
  `managed_by: <plugin>`; `Caller::Kubernetes` gains the plugin it acts
  for, and `check_owner` accepts the operator's apply or down of a record
  whose owner is that plugin. Without `managed_by` the operator's rules
  of §7.4 are unchanged. The CLI still gets 409.

### 23.4 The operator

- **The Plugin controller (§5.5)** acts only on a Plugin named in a
  Daemon's `spec.plugins` in its namespace. Unlisted, the Plugin is
  `Deployed=False/NotListed` and owns nothing; a name listed by two
  Daemons makes both Daemons `PluginsReady=False/PluginListedTwice`.
  Listed, it applies with server-side apply, owner-referenced to the
  Plugin:
  - a token Secret (`pki::new_token`);
  - a serving Secret issued by the listing Daemon's authority for the
    Service's names (`<plugin>`, `.<ns>`, `.<ns>.svc`,
    `.<ns>.svc.cluster.local`), renewed as the Daemon's is, reissued when
    `pki::verifies` fails;
  - the optional scratch claim;
  - a Deployment of one replica: §9.1's hardened pod (the agent pod's
    security context, read-only root, an `emptyDir` at `/tmp`), the
    variables of §23.1, the token, certificate, key and CA mounted under
    `/balerix/{token,tls,ca}`, the scratch claim or an `emptyDir` at
    `BALERIX_PLUGIN_SCRATCH`, `spec.resources`;
  - a Service: port 7644, plus `expose.port` when set;
  - a NetworkPolicy: ingress to 7644 from the Daemon's pod only, to the
    `expose` port from anywhere; egress to the Daemon, DNS and 443.
  A hash annotation on the pod template over the spec, the grant, the
  resolved config, the referenced Secrets' data, the token and the
  serving certificate rolls the Deployment (§5.5).
- **`spec.secrets` are resolved by the operator**, injected into
  `config` as `plugins.yaml`'s `secrets` are, and reach the Daemon only
  through `PUT /v1/plugins`, never a file in the plugin's pod.
- **The Daemon controller sends the list.** It builds it from
  `spec.plugins` and their Plugins (url
  `https://<plugin>.<ns>.svc:7644`) and sends `PUT /v1/plugins` on every
  reconcile; the call is idempotent, and it is how the list returns after
  a Daemon restart. A Plugin whose token or serving Secret is not yet
  made is left out of that pass. `PluginsReady` replaces
  `PluginsUnsupported` (§20.2): `True` when every listed plugin's row is
  `Ready`, otherwise `False` with `PluginMissing` (no Plugin object),
  `PluginRefused` (the row's message) or `PluginNotReady`.
- **The Plugin's conditions** are `Deployed` (the Deployment is
  available) and `Ready` (its row is `Ready`; the refusal as message
  when `Failed`), polled with `GET /v1/plugins` on the 15 s requeue.
- **The managed-Fleet writer** runs in the Daemon reconcile. For each
  live request of `GET /v1/managed-fleets` it writes a Fleet `<name>`
  with server-side apply: labelled `balerix.ai/managed-by: <plugin>`,
  owner-referenced to the Plugin (deleting the Plugin deletes its
  Fleets), `daemon` the listing Daemon, and the file's defaults and crews
  merged beneath the Plugin's `fleetDefaults`, `runner: pod`. It never
  writes a Fleet that lacks the label or carries another plugin's; that
  request gets a `FleetConflict` Event on the Plugin. A request marked
  down, a request no longer listed, or a plugin dropped from
  `spec.plugins` deletes the Fleet, `retain` taken from the down query
  (`keep-repos` → `Branches`, else `None`). The restricted surface is not
  checked again: the Daemon resolved the file.
- **The Fleet controller** sends `managed_by: <plugin>` when its Fleet
  carries the label.

### 23.5 Images

- `kind-up` also builds and loads `balerix-plugin-flow:e2e` and
  `balerix-plugin-web:e2e` from `docker/plugin/Dockerfile` with the musl
  binaries, and `balerix-fake-plugin:e2e`: a stage `FROM balerix:e2e`
  whose entrypoint is `balerix dev fake-plugin`.
- The released plugin images (`IMAGE_UNITS`) already use
  `docker/plugin/Dockerfile`; wiring them to a chart is sub-project 5's.

### 23.6 Testing

- **4a:** unit tests for `Declared` (token match, every `hello`
  refusal, readiness over missed polls, the persisted hello across a
  restart) and the managed-request store; SDK tests for the new
  variables and TLS on both sides; two-process integration tests with
  the Daemon in Kubernetes mode serving TLS and `dev fake-plugin` given
  CA, certificate and token files: `hello` accepted, a grant refusal, an
  interception over TLS, a managed PUT then `GET /v1/managed-fleets`
  then an operator-style apply with `managed_by`, and a Daemon restart
  that keeps the plugin ready without a new `hello`. The one-machine
  plugin tests and `plugin_manage_journey` stay green unchanged.
- **4b:** envtest tests for listed and unlisted Plugins, the objects and
  the hash roll, a changed authority reissuing the serving certificate,
  `PluginsReady` over a stub Daemon's rows, the managed writer's label
  rules, and drop → delete. `e2e-k8s` gains the plugin journey: a Daemon
  lists flow and web and both reach `Ready`; a flow rule acts on an
  agent's Stop; web's review page is fetched through the Daemon's
  `/v1/plugins/web/…` over the port-forward; `dev fake-plugin` with
  `manage` brings up a managed Fleet whose agents become Ready; removing
  it from `spec.plugins` deletes the Fleet; changing web's Plugin config
  mid-journey brings both pods back to `Ready` with no restart by hand
  (§23.8). The operator runs out of cluster as in §21.4, so the missing
  Events RBAC does not bite here.
- **4b, §23.8:** `Declared` unit tests (a revision mismatch is 409 and
  leaves the entry as it was; a missing revision is refused; a matching
  one is accepted); an SDK test where a fake host answers 409 twice, then
  accepts, and `configure` runs; an envtest check that the pod's
  `BALERIX_PLUGIN_REVISION` equals the list entry's `revision`.

### 23.7 Decided by the plan

What 4a's plan decided beyond §23.1–§23.3, and the rulings made while
building it, as built.

- **`Plugin::manifest()` returns `Option<&'static str>` and defaults to
  `None`.** This amends §23.1's `-> &'static str`. It is not a required
  method, so the SDK's own test plugins and third-party plugins keep
  compiling. Kubernetes mode refuses a `hello` without a manifest anyway.
- **The SDK sends the manifest only when it was given an authority
  (`BALERIX_CA_FILE`).** `HelloRequest` is `deny_unknown_fields`, so a
  released one-machine Daemon (0.2.0) would answer 400 to a `hello`
  carrying it. On one machine the manifest is ignored, so nothing is lost.
- **`PluginSource` is a closed enum (`Packages`, `Declared`), not a trait
  object.** There are two adapters and their methods are async; the enum
  needs no boxing.
- **`PUT /v1/plugins` registers every plugin at once,** with a placeholder
  manifest (no needs, no hooks, not ready). A fleet that names the plugin
  then applies with its pairs `pending`, as on one machine, where `sync`
  registers before any `hello`. Without this, an apply answers `no plugin
  "x" is installed` until the plugin's `hello`.
- **`serve --tls-ca <file>` is optional in Kubernetes mode.** The Daemon
  pods 3b ships do not mount it until 4b. Without it, `PUT /v1/plugins` is
  409 `this daemon was started without --tls-ca; it cannot call plugins`.
  The plugin client and the route proxy always use a preconfigured TLS
  config; with no `--tls-ca` it trusts nothing (`kube::tls::no_roots`), and
  `proxy::client` takes that config as an `Arc<ClientConfig>`.
- **`PluginAddr.listen` may hold a URL.** `PluginAddr::base()` gives
  `https://…` as-is and prefixes a bare `host:port` with `http://`. The
  client and the proxy build their URLs from it.
- **The proxy uses hyper-rustls's `https_or_http` connector.** reqwest's
  rustls feature already brings it into the lock. Loopback `http://` keeps
  working through the same client.
- **In Kubernetes mode a plugin's `DELETE fleets/{name}` still downs the
  fleet** through `down_as` (the mirror sends `stop`) and also marks the
  stored request down. A down request is kept until the plugin applies that
  name again, the plugin is dropped, or the record is purged.
- **`Caller::Kubernetes` becomes `Caller::Kubernetes { managed_by:
  Option<AgentName> }`.**
- **`dev fake-plugin` uses `serve`.** `configure` writes the hello file and
  spawns the manage step, so the fake gets TLS and the manifest with no code
  of its own.
- **A refused re-hello takes the plugin out of the chain and deletes its
  `hello.json`,** so a restart cannot bring back a hello that no longer
  holds. A restored hello lists as `starting` with its real version until
  the first health poll, which comes 10 s after the Daemon starts.

Rulings made while building it:

- **The fake plugin's manifest** is the union of the e2e fake's two
  packages: needs `[actions, fleets, kv, manage]`, `intercept: [PreToolUse,
  Stop]` and `observe: [SessionStart, Notification, PreToolUse, Stop]`.
  Task 8's grants and refusal text depend on those needs.
- **`sync` and `purge` refuse when the plugin source is `Declared`,** not
  on the mode, through `PluginError::Managed` (409). This keeps the
  existing error statuses; production Kubernetes mode always uses
  `Declared`.
- **The operator acting for a plugin (`managed_by`) never adopts an
  existing ownerless fleet:** 409 `fleet {name} is not managed by a
  plugin`. This is §7.4's rule that the operator never adopts a record it
  did not create, applied to §23.3's `check_owner`.
- **An `https://` `BALERIX_API_URL` without `BALERIX_CA_FILE` is
  refused** (`BALERIX_API_URL is https://, so BALERIX_CA_FILE must be
  set`), by `Env::from_env` and again by `Host::new`, since `Env`'s fields
  are public. Without an authority, reqwest would verify against the
  system's roots and tungstenite against webpki's bundle.
- **Every `PUT /v1/plugins` keeps only the listed plugins' managed
  requests,** not just those of the plugins it dropped since the previous
  list. After a restart the Daemon has no previous list, and a plugin
  dropped while it was down would keep its rows forever. A plugin's apply
  stores its request only while the plugin is still listed, checked under
  the store's lock, so a drop racing the apply leaves no row behind.
- **Two list entries with the same token are refused,** 400
  `plugins[<i>].token: listed twice`, beside §23.2's duplicate name. A
  token names its plugin; with two alike, the later plugin would
  authenticate as the earlier one.
- **`hello`'s manifest gets one machine's manifest rules,** all but
  `mise.toml`'s: `balerix_core::validate_manifest_fields`, split out of
  `validate_manifest`, runs before the name and grant checks, refusing as
  `hello.manifest.<field>: <reason>` (`hello.manifest.hooks.intercept:
  unknown event "Foo"`), the config-path form of the name and needs
  refusals. A restored `hello.json` is held to the same rules.

### 23.8 A hello belongs to one revision (4b, 2026-10-06)

4a's final review found a race. A changed Plugin rolls its Deployment and
the Daemon controller sends the new list, in separate steps. A new pod
whose `hello` reaches the Daemon before the list does is accepted under
the old entry; the new list then puts the entry back to `Waiting`, and
the pod never says hello again. The plugin stays `Starting`. Rather than
order the two controllers, a `hello` names the entry it was built for.

- **The revision is the pod template's hash.** The Plugin controller's
  hash annotation (§23.4: spec, grant, resolved config, referenced
  Secrets' data, token, serving certificate) is the plugin's revision. It
  reaches the pod as `BALERIX_PLUGIN_REVISION` and the list as
  `DeclaredPlugin.revision`, a required field. A new revision is a new
  pod, and that pod is the one that must say hello.
- **Wire.** `HelloRequest` gains an optional `revision`. The SDK reads it
  in `Env` and sends it only with an authority (`BALERIX_CA_FILE`), as
  the manifest (§23.7), so a 0.2.0 Daemon never sees it. A one-machine
  Daemon accepts and ignores it.
- **The Daemon** checks the revision after the protocol and before the
  manifest, in `Declared::hello`:
  - none: refused and kept on the row, as a missing manifest is —
    `hello.revision: required in kubernetes mode`;
  - another revision: 409 `hello.revision: this daemon holds <a>, the
    plugin is <b>; the list has not arrived yet`, and the entry is left
    as it was. An old pod still serving during the roll stays in the
    chain, and no `hello.json` is removed;
  - the same revision: the checks of §23.2 and §23.7, unchanged.
  The revision is part of the entry's hash, so a new one already sends
  the entry to `Waiting` and keeps a stale `hello.json` from being
  restored.
- **The SDK retries a 409 `hello`** with back-off from 1 s, doubling to
  30 s, without limit, the server bound meanwhile. Any other failure
  still ends `serve`. While it waits the row is `Starting` and the Daemon
  `PluginsReady=False/PluginNotReady`, so a roll that never completes is
  visible.
- `PluginStatus` is unchanged.

### 23.9 Decided by the 4b plan

What 4b's plan decided beyond §23.4–§23.6 and §23.8, and the rulings made
while building it, as built.

- **A plugin's objects are named for it:** `balerix-plugin-<p>` is the
  Deployment, `balerix-plugin-<p>-token` the token Secret,
  `balerix-plugin-<p>-tls` the serving Secret and
  `balerix-plugin-<p>-scratch` the optional claim. The Service is `<p>`,
  so the Daemon's list entry reaches it as `https://<p>.<namespace>.svc:…`.
- **`DaemonError::ListPending(String)`** is the Daemon's refusal of a
  `hello` built for another revision (§23.8), answered 409.
- **`PluginsReady` is `Unknown/Pending` ("judged once the daemon
  answers") until the Daemon answers.** `daemon_status` (pure) reports
  it so and does not hold `Ready` on it; once the Daemon answers, the
  controller judges `PluginsReady` from its rows and `Ready` follows it
  (a `PluginsReady` other than `True` makes `Ready=False` with its reason
  and message).
- **The Daemon requeues at the Fleet period while it lists plugins,**
  not the slower reconcile period: the rows and the managed requests are
  polled (§23.4).
- **The revision's inputs are the plugin's name, its spec, its resolved
  config, its token and its serving certificate.** A referenced Secret's
  data reaches it through the resolved config, so a changed Secret moves
  the revision without being hashed on its own.
- **`HOME=/tmp` in the plugin pod,** the pod's `emptyDir` mount, so a
  plugin's tools have a writable home.
- **No readiness probe.** `Deployed` is the Deployment's available
  replica, and `Ready` is the Daemon's row for the plugin (§23.2), which
  the plugin's own `hello` and the Daemon's health polls decide.
- **An unlisted Plugin's scratch claim is deleted with its other
  objects.** Nothing of a Plugin that no Daemon lists is kept.
- **A listed plugin whose grant or config cannot be built holds the whole
  list back for the pass.** The Daemon keeps its last list and with it
  every plugin's managed requests; sending a list without the plugin would
  drop them (see the second ruling below). Only a plugin whose token or
  serving Secret is not made yet, and that the Daemon has no row for, is
  left out: it has nothing to lose.
- **The fake plugin image is a heredoc stage in `kind-up`:** `FROM
  balerix:e2e`, entrypoint `balerix dev fake-plugin`. It has no
  Dockerfile of its own.
- **`e2e-k8s` runs its two journeys one at a time** (the `e2e-k8s`
  nextest profile sets `test-threads = 1`): both use the one three-node
  kind cluster.

Rulings made while building it:

- **A listed plugin whose grant or config cannot be built** puts that
  plugin's own reason (`SecretMissing` or `InvalidSpec`) on the Daemon's
  `PluginsReady`, with the message `<plugin>: <why>`, and no list is sent
  that pass.
- **A plugin listed by two Daemons also holds the list back**
  (`PluginsReady=False/PluginListedTwice`). Leaving it out would let the
  Daemon drop its managed requests and later delete its Fleets.
- **A managed request the operator cannot write or delete** (a
  `managed_fleet` error or a 4xx from the API server) is a Warning Event
  `ManagedFleetRefused` on the Plugin, and the pass goes on. Transport
  errors and 5xx fail the reconcile.
- **An unlisted Plugin deletes only the objects whose owner references
  carry its uid** (a uid precondition on the delete). A same-named object
  it does not own is left alone.
- **The managed Fleet's defaults are the file's over the Plugin's
  `fleetDefaults`,** which is Spec M's layer order: host settings, then
  `fleetDefaults`, then the file.
- **The SDK logs `hello accepted`** once a hello is accepted. The e2e
  journey's config roll reads it from the new pod's log, because a
  Plugin's `Ready` can still be the old pod's.
- **§5.5's "a Secret with its resolved config" is replaced by §23.4:**
  the config reaches the Daemon only in `PUT /v1/plugins`, never as a file
  in the plugin's pod.
- **A Plugin's name is checked before anything is made:** the Daemon's
  rule for a plugin's name (`balerix_core::name::validate_name`, at most
  63 characters of `a-z`, `0-9` and `-`, and `reserved_plugin_reason`,
  which refuses `kubernetes`) and a Service's (DNS-1035: it starts with a
  letter, so `1password` is refused). One that fails is
  `Deployed=False/InvalidSpec` with `metadata.name: <why>` and nothing is
  applied; on the Daemon it is `PluginsReady=False/InvalidSpec`
  (`<plugin>: metadata.name: <why>`) and the list is held back, like any
  `InvalidSpec`.
- **A list the Daemon refuses** (`PUT /v1/plugins` answers 400) is
  `PluginsReady=False/ListRefused` with the Daemon's message; the status
  is still written and no managed Fleet is touched that pass.
- **The plugin's Deployment is `strategy: Recreate`.** With one replica
  a rolling update keeps the old pod until the new one is ready, and a
  `ReadWriteOnce` scratch claim attached on another node would keep the
  new one from starting.
- **A Secret the operator manages is never a plugin's config.**
  `spec.secrets` naming a Secret labelled
  `app.kubernetes.io/managed-by: balerix-operator` (a Daemon's
  authority, its admin token, a plugin's token) is `InvalidSpec`,
  `spec.secrets.<key>: Secret <name> is managed by the operator`; the
  value is never read into a message.
- **A running plugin whose token or serving Secret is being remade holds
  the list back.** The Daemon controller asks for the Daemon's rows
  first; a plugin with a missing Secret that already has a row is
  `PluginsReady=False/PluginNotReady`, `<plugin>: its token or serving
  Secret is being remade`, instead of being left out (which would drop
  its managed requests and then delete its Fleets).
- **A Fleet of another Daemon is a `FleetConflict` too.** A managed
  request whose Fleet carries this plugin's label but whose `spec.daemon`
  is another Daemon is not written or re-pointed: a `FleetConflict` Event
  `Fleet <name> belongs to Daemon <other>, not <this>: left as it is`.

## 24. Decided in sub-project 5 (2026-10)

Sub-project 5 is §13 and §14: the release of the operator and agent, and
the charts. Brainstormed 2026-10-06. Chart sources stay in this
repository under `charts/`; `balerix-ai/helm-charts` holds only
`index.yaml`, a README and the licence (§14, as written). It is built as
two plans:

- **5a, charts:** both charts, the definitions moved into the operator
  chart, the RBAC (Events included), a `charts` check, and `e2e-k8s`
  installing both charts with the operator in the cluster (§24.1–§24.3).
  Done when `mise run charts` and `e2e-k8s` pass in CI.
- **5b, release:** the operator and agent in the core unit, the `charts`
  release unit, publishing, `verify-k8s` and the fork rehearsal
  (§24.4–§24.6). Done when §17's condition holds.

The operator runs as one replica with no leader election and no metrics
(§21.1); §3's "leader-elected" is superseded.

### 24.1 The `balerix-operator` chart

- **The definitions are templates.** They move from `operator/crds/` to
  `charts/balerix-operator/templates/crds/<plural>.balerix.ai.yaml`.
  `mise run crds` writes each from `balerix-operator crds` wrapped in
  `{{- if .Values.crds.install }}` and annotated
  `helm.sh/resource-policy: keep`. They are not in the chart's `crds/`
  directory, which Helm installs once and never upgrades; this amends
  §14.1's "`charts/balerix-operator/crds/`". `operator/crds/` is deleted;
  `crds_it` and the drift check read the chart's path.
- **The Deployment.** One replica, `strategy: Recreate`: with no leader
  election a rolling update would run two reconcilers for a moment. The
  pod is labelled `app.kubernetes.io/name: balerix-operator`
  (`desired::common::MANAGER`), which each Daemon's NetworkPolicy admits
  from the operator's namespace. `POD_NAMESPACE` comes from the downward
  API; `--watch-namespaces` from `watchNamespaces`;
  `--daemon-image`/`--agent-image` from `images.daemon`/`images.agent`
  when set. Restricted pod security: non-root, read-only root, every
  capability dropped, `RuntimeDefault` seccomp. No probes: the operator
  has no HTTP endpoint.
- **RBAC.** `watchNamespaces` empty: one ClusterRole and its binding.
  A list: a Role and a RoleBinding in each listed namespace (§5.6). The
  rules: the five `balerix.ai` kinds with `/status` and `/finalizers`;
  Secrets, Services, ConfigMaps, PersistentVolumeClaims and Pods;
  StatefulSets and Deployments; Jobs; NetworkPolicies; and
  `events.k8s.io` Events (`create`, `patch`, §22.5). Nothing on
  CustomResourceDefinitions.
- **Values:** `image` (`repository`, `tag` defaulting to `appVersion`,
  `pullPolicy`), `resources`, `watchNamespaces`, `crds.install`,
  `images.daemon`, `images.agent`.
- **Out-of-cluster plumbing goes.** `run --resolve` and the hidden
  `--insecure-daemon-url` are removed: nothing runs the operator outside
  a cluster any more.
- **`CrewLocked` is throttled.** Now that the chart grants Events, the
  Event §22.3 publishes for a stuck crew is published once per
  `stuck_after` per crew, not on every 5 s wait of every waiter.

### 24.2 The `balerix-daemon` chart

- **One Daemon object,** named after the release unless `name` is set.
  Its values mirror `DaemonSpec`: storage classes and sizes, the
  credential Secret names, `defaults`, `resources`. The chart creates no
  Secret.
- **Plugins.** `plugins.{flow,web,matrix,github}` each take `enabled`
  (default false), `image.repository`, `image.tag`, `config` and the
  Secret references `PluginSpec` takes. Each enabled one is one Plugin
  object. `image.tag` is pinned to that plugin's released version and
  moved by its release (§24.5).
- **No `values.schema.json`.** The definitions' schemas refuse a bad
  spec at apply, and the charts check validates against them (§24.3).

### 24.3 Testing 5a

- **`mise run charts`:** its own CI job, on pull requests touching
  `charts/`, `operator/`, `plugins/`, `scripts/operator.sh`,
  `scripts/kind-up.sh`, `mise.toml` or `.github/workflows/ci.yml`; not part of
  `check`. `helm lint` both charts; `helm template` with the default
  values, with `watchNamespaces: [a, b]`, and with every plugin enabled
  and `crds.install: false`; then a server-side dry-run apply of
  each rendering (§24.7) against the envtest API server the `operator` task pins,
  after applying the chart's definitions to it. That validates the
  built-in kinds and the definitions' schemas on a real 1.34 API server,
  without kubeconform. It also fails when the definition templates differ
  from `balerix-operator crds`. `helm` is pinned in the task's `tools`.
- **The NetworkPolicy label:** kind does not enforce NetworkPolicy, so a
  test asserts the rendered operator pod's labels match the `from`
  selector `desired::daemon` writes.
- **`e2e-k8s`, operator in the cluster.** `kind-up` builds a musl
  `balerix-operator` and loads `balerix-operator:e2e`, and no longer
  applies definitions. The harness `helm install`s
  `./charts/balerix-operator` into `balerix-system` with the `:e2e`
  images, instead of spawning the operator. The Phase 3 journey's Daemon
  is `helm install` of `./charts/balerix-daemon`; the plugin journey's
  enables flow and web at `:e2e`, and its "web's config rolls its pod"
  step becomes a `helm upgrade` changing `plugins.web.config`. The fake
  plugin is not one of the chart's four, so its Plugin object is still
  made from Rust. Fleets are still applied from Rust. On failure CI also
  prints `kubectl logs deploy/balerix-operator`. Every controller now runs
  under the chart's service account, so a missing verb fails the journey.
- **`CrewLocked`:** an envtest test that a stuck crew gets one Event per
  `stuck_after`.

### 24.4 The core unit (§13)

- **Binaries.** `build.sh core` also builds `balerix-operator` and
  `balerix-agent`, static musl, x86_64 and aarch64; core's archives gain
  both. `prepare.sh core` writes the version into `operator/Cargo.toml`
  and `agent/Cargo.toml` and updates their lockfiles. `unit_paths core`
  and `affected-units.sh` add `operator/**`, `agent/**`,
  `docker/operator/**` and `docker/agent/**`. 5a's `kind-up` operator
  build moves onto `build.sh`.
- **Images.** Core's image job builds three images per architecture, in
  order: `balerix`; `balerix-agent` `FROM` the `balerix` just built (the
  `BASE` build argument); `balerix-operator`. Each is hadolinted,
  scanned, smoke-tested, pushed by digest, merged, signed, attested and
  promoted as images are today. On a release, `balerix-agent`'s base is
  the pushed `balerix` digest, never `latest`.
- **Smoke tests:** `--version` on both new images; `balerix-operator
  crds` prints five documents; `balerix-agent sidecar` with no
  configuration exits 1 with a message.

### 24.5 The `charts` unit (§14.2, §14.3)

- **`lib.sh`** learns a unit kind with no crate: its version is
  `version` in `Chart.yaml` (both charts always share it), its changelog
  `charts/CHANGELOG.md`, its paths `charts/**`, its tag
  `balerix-charts-v<ver>`.
- **Pins.** `prepare.sh core` moves both charts' `appVersion`;
  `prepare.sh <plugin>` moves `plugins.<plugin>.image.tag` in the daemon
  chart. Both land in `chore(release)` commits, which are not releasable,
  so `prepare.sh charts` also counts a pin changed since the last charts
  tag as a change. The chart's bump is the larger of what the commits
  under `charts/` ask and the largest bump among the changed pins: a core
  minor is a chart minor.
- **The gate.** `prepare.sh charts` answers `status=none`, naming the
  missing tag, until `appVersion` and every pinned plugin version are
  tagged. github 0.1.0 has no release, so github's first release
  precedes the first charts release (RELEASING's order already puts it
  next).
- **`release.yml`, for the charts unit:**
  1. check: lint, render, validate (§24.3), and install both charts on
     kind with the published images at the pinned versions; the Daemon
     and the flow and web Plugins reach `Ready`. No Fleet: one needs
     claude credentials (`verify-k8s` covers it).
  2. package both charts; write `SHA256SUMS`; attest the archives.
  3. push to `oci://ghcr.io/<owner>/charts`; cosign-sign by digest.
  4. the shared `github-release` job attaches the archives and makes the
     tag.
  5. index: `helm repo index --merge` committed to `<owner>/helm-charts`
     with a release-bot App token scoped to that repository.
  6. verify: `helm repo add`, `helm pull` each chart, compare with
     `SHA256SUMS`; a failure flags the release a prerelease.

  Every step skips what is already done. A dry run does steps 1 and 2
  and uploads the archives.
- **Forks.** Image repositories and the index location derive from
  `GITHUB_REPOSITORY_OWNER`, as `unit_image` does: the package step
  rewrites `values.yaml`'s image repositories to `ghcr.io/<owner>/…`
  when the owner is not `balerix-ai`, so a fork's charts install the
  fork's images.
- **`release-test`** gains scenarios for the `appVersion` gate, the
  plugin-tag gate, a pin change with no commits under `charts/`, and the
  fork rewrite.

### 24.6 `verify-k8s` and the rehearsal

- **`mise run verify-k8s -- --from tree|index`** installs both charts on
  the cluster `KUBECONFIG` names (from `./charts`, or from the published
  index), makes the claude credential Secret from the environment,
  applies `examples/payments.yaml` as a Fleet with `runner.type: pod`,
  waits for `Ready` and walks the journey's checks with the real
  `claude`. Manual; no CI tier (§15).
- **The rehearsal** (§17's condition), on forks of `balerix` and
  `helm-charts` under one account: the one-time setup (§14.4 and
  RELEASING's), then release core, github and charts in that order, then
  `verify-k8s --from index` against a cluster the user provides. The
  setup and the cluster are the user's.
- **`docs/RELEASING.md`** gains the charts unit, its steps, the setup and
  the rehearsal.

### 24.7 Decided by the 5a plan

- **`extraPlugins`:** the daemon chart lists Plugin objects made outside
  it after its own four. The plugin journey's fake plugin goes through
  it. Order is fixed: flow, web, matrix, github, then the extras. With no
  plugin enabled and `extraPlugins` empty it renders `plugins: []`, not
  `null`.
- **Each plugin's `needs` is a chart value,** defaulted to its manifest's
  and held to it by `charts_it`. The test reads each
  `plugins/*/package/balerix-plugin.yaml`, so the CI `charts` job's path
  filter includes `plugins/` (§24.3).
- **matrix's `scratch`** defaults to 1Gi.
- **The checks are a Rust test binary, `operator/tests/charts_it.rs`, on
  the envtest harness.** It does a server-side dry-run apply through
  discovery, replacing `kubectl apply --dry-run=server`, and runs the
  controllers impersonating the chart's service account, so both RBAC
  modes are proven without a cluster. `mise run operator` leaves it out.
- **`crds --chart-dir`** replaces `crds --out`. The template carries a
  "generated" comment inside the guard.
- **ClusterRole names are `<namespace>-<release>`,** so two releases in
  different namespaces do not collide. The name stops the RBAC collision
  only: a second release must also set `crds.install=false` (the
  definitions are templates owned by the first release) and a
  `watchNamespaces` that overlaps no other release's, since two operators
  on the same objects, with no leader election between releases, fight.
  Documentation only: the chart has no `lookup` guard.
- **The Deployment's security context** sets `runAsUser: 65532`, which is
  distroless `nonroot`. The chart adds a `logLevel` value (`RUST_LOG`,
  default `info,kube=warn`).
- **`e2e-k8s` installs the operator chart once,** cluster-wide, into
  `balerix-system` from `scripts/operator.sh e2e`. The per-namespace Role
  mode is proven by `charts_it` only. Phase 3's fleet read goes through
  the test's own `admin_http` over the port-forward.
- **`RunConfig::insecure_daemon_url` stays** for the in-process tests (no
  flag sets it). `--resolve`, `RunConfig::resolve` and
  `DaemonClient::new_resolving` are gone.
- **Helm 4.3.0.**
- **No grant was added beyond §24.1's table.** Both RBAC journeys in
  `charts_it` (namespaced Roles and cluster-wide) and `e2e-k8s` in the
  cluster passed with the table as written.
- **A missing grant shows as a timeout, not a 403.** The controllers
  retry, so a journey fails with a `wait_for` timeout naming the step
  (for example "timed out waiting for the Fleet's Jobs" with the
  `networkpolicies` rule removed). The operator's log has the 403. Only
  the Events check and the outside-namespace check show the API error
  directly.

### 24.8 Decided by the 5b plan

- **Images by name.** `lib.sh` `unit_images` lists a unit's images in
  build order. Image scripts take an image name, and `merge-images` and
  `promote-images` run once per image. One composite action
  (`build-unit-images`) builds a unit's images in order. Digest artifacts
  stay one per unit and architecture, holding `<image>/<digest>`.
- **The agent's base.** The smoke build uses the docker driver with `BASE`
  set to the `balerix` just loaded on the runner. The push build uses the
  `balerix` per-architecture digest just pushed.
- **One archive.** Core's `balerix-v<ver>-<target>.tar.gz` holds all three
  binaries. `build.sh` checks each one's linkage and `--version`.
  `kind-up` builds its images through `build.sh core`, so `e2e-k8s` runs
  the static release binaries.
- **The charts job packages first** and uploads the archives before any
  cargo-built code runs. `mise run charts` then lints, renders and
  validates the tree's `charts/`, and the packaged archives are what gets
  installed on `kind-up cluster`. Upstream's staged charts equal the
  tree's byte for byte; a fork's differ only in their image repositories.
  What passes the check is what gets published.
- **Pins at release time.** A run can release core or a plugin together
  with the charts (release PRs merged together or queued behind one
  another), moving a pin after the gate passed. The charts job waits for
  `merge-images`, and first checks (`check-pins.sh`) that every pin is
  tagged or is the version a unit in the same run releases; otherwise it
  fails, naming the pin.
- **Pins in the changelog.** A charts release lists moved pins under
  `### Images`. A pin-only release carries no "Initial release." line.
- **OCI re-runs.** Pushing an archive that is already there is a no-op
  at the registry, and a re-run signs it again. The index step skips when
  `index.yaml` already lists both archive URLs.
- **verify-charts waits for Pages** for up to 10 minutes before it fails
  and flags.
- **verify-k8s** is a report-printing script, like `verify-claude.sh`. Its
  Fleet is `examples/payments.yaml` with the repo replaced by a git server
  pod, push off and a pod runner. It types the first prompt into alice's
  tmux window, re-pressing Enter until the transcript has it (#99). It
  reads flow's text from Claude's own transcript. It creates its namespace
  before the operator chart (whose `watchNamespaces` Role lives there) and
  streams the harvest pod with a watch.
- **The first charts release** follows the first core release that ships
  `balerix-operator`. `appVersion` 0.2.0 has no operator image, and the
  check job would fail on it.
