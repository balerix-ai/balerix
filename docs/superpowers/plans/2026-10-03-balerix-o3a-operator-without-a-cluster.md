# Spec O, part 3a: the operator without a cluster — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Everything in Spec O's sub-project 3 that needs no cluster: a standalone `operator/` project with the five kinds, a `crds` command, pure `desired` functions for every object the operator will apply, a `pki` module and a Daemon client; and in `agent/` the three Job commands (`crew-sync`, `pool-sync`, `harvest`). Done when `mise run operator`, `mise run agent` and `mise run check` pass and `balerix-operator crds` prints five documents (Spec O §20.1).

**Architecture:** The operator follows the repository's rule that the planner is pure and the executor is dumb: `operator/src/desired/` holds functions from observed objects to typed `k8s-openapi` objects and status conditions, with nothing random in them (tokens and certificates come in as inputs, made by `pki`). The controllers that call them are sub-project 3b. A Fleet resolves through `balerix-config` unchanged, with the Daemon's `defaults` as the existing `operator_layer`. The Jobs see the shared volume as the agent pod does: the same sub-paths at the same places under `/balerix/shared`, read-write, through a `SharedSlice` that the pod layout also uses, so a clone's `alternates` and a pool's install path are the same in the Job and in the pod.

**Tech Stack:** Rust 1.99.0 (edition 2024, `unsafe` forbidden), kube 4.2.0 (`derive` only: no client, no runtime), k8s-openapi 0.28.0 (`v1_32`, `schemars`), schemars 1.2.2, rcgen 0.14.10 (`pem`, `ring`), time 0.3.55, reqwest 0.13.4 (`json`, `rustls-no-provider`), rustls 0.23.45 (`ring`), rustls-pki-types 1.15.1, serde_norway 0.9.42, insta 1.48.0, proptest 1.11.0, clap 4.6.6, git 2.47.3, mise, nono 0.79.0. All of these were compiled and run together in a throwaway probe on 2026-10-03 (CRD generation with open objects and printer columns, typed Pods from JSON, the `pod` runner's serde shape, an authority reloaded from PEM signing a certificate that `rustls` verifies for a DNS name and for `127.0.0.1`).

**Spec:** `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md`: §20 (the decisions for this sub-project), with §4, §5, §8 as the design, §6.1 for the pod, §7.4 for the Daemon's routes and the sidecar's mount paths, §10 for security. What this plan decides beyond §20 is listed under **Decisions this plan makes** and written into the spec as §20.5 by Task 12.

## Global Constraints

- Branch `feat/kube-operator-offline`, cut from `docs/spec-o-3a-decisions` (2ed5361, which holds §20). One pull request, squash-merged, title `feat(operator): the operator project, its five kinds and pure desired state; the sync and harvest Jobs (Spec O §20)`.
- The uncommitted `cargo-insta = "1.49.0"` hunk in `mise.toml` is the user's. Never stage it: commit with explicit paths, and when a task edits `mise.toml`, stage with `git add -p mise.toml` and leave that hunk out.
- Run cargo only through mise: `mise x -- cargo …` or `mise run <task>`, from the repository root.
- `mise run check` passes before every commit that touches `crates/`. `mise run test-it` passes before the commits of Tasks 2 and 3. `mise run agent` passes before the commits of Tasks 2, 3 and 4. `mise run operator` passes before every commit from Task 5 on.
- No `unsafe`. `unwrap`/`expect` are warnings outside tests and clippy runs with `-D warnings`; `#![allow(clippy::unwrap_used, clippy::expect_used)]` at the top of every test file.
- Exact versions for every new dependency, with a comment saying why, in the project that uses it. Nothing in this plan adds a dependency to the core workspace; `scripts/check-core-deps.sh` passes unchanged.
- `k8s-openapi` and `kube` never enter the core workspace or `agent/`: in `balerix-api` the Kubernetes shapes are opaque JSON (§20.3).
- insta snapshots: read each `.snap.new`, compare it with the expected values the task lists, then `mise x -- cargo insta accept --manifest-path operator/Cargo.toml`. Never blind-accept.
- Test roots under `target/tmp` (`CARGO_TARGET_TMPDIR`), never `/tmp`.
- `BALERIX_REQUIRE_TOOLS=1` turns every "skip: tool missing" into a failure.
- Nothing secret in argv, env or logs (§10.3). A type holding a token, a private key or credentials hand-implements `Debug` and prints `<redacted>`. Tool tables and repository names are not secret and may be arguments.
- Every pod the operator describes: uid and gid 10001, `fsGroup` 10001, `runAsNonRoot`, read-only root filesystem, all capabilities dropped, `allowPrivilegeEscalation: false`, `seccompProfile: RuntimeDefault`, `automountServiceAccountToken: false` (§6.1).
- Error and message text that tests pin, verbatim:
  - `crews.<c>.agents.<a>.runner.type: \`pod\` runs only on Kubernetes; this daemon runs agents in tmux`
  - `crews.<c>.agents.<a>.runner.type: \`tmux\` is not available on Kubernetes; a Fleet's agents run as pods`
  - `crews.<c>.agents.<a>: the object name <fleet>-<crew>-<agent> is <n> characters; at most 63`
  - `cache: <fleet>/<crew>: the remote has no branch <ref>`
  - `nothing to harvest` and `harvested <branch>`
  - condition reasons `PluginsUnsupported`, `NoPlugins`, `VersionMismatch`, `ClaimPending`, `PoolSyncRunning`, `PoolSyncFailed`, `DaemonUnavailable`, `SandboxUnavailable`, `MaterializeFailed`.

## Review Focus

Conditions the spec implies and no task's tests would otherwise exercise, most likely to bite first. Each has a test in the task that owns it.

1. **The cache directory is a mount point.** In a Job `/balerix/shared/repo` is a volume mount: it exists, starts empty and cannot be removed. A cache whose making failed half-way must end up empty with git's own error reported, and the next run must succeed. Task 3 pins it with a directory whose parent is read-only.
2. **A `ref` the remote does not have.** `git rev-parse --quiet` answers with exit 1 and no text; the Crew's `CacheReady` message would be empty. Task 3 asserts the message is `cache: f/c: the remote has no branch nope`.
3. **A fleet with no `tools`.** The agent pod mounts `/balerix/shared/fleet` and `/balerix/shared/crew` as sub-paths, which must exist. Task 3 asserts that `pool-sync` and `crew-sync` with an empty table still leave `fleet/mise` and `crew/mise` directories and their markers.
4. **A harvest on a claim it may not write.** §8.4 mounts the claim read-only. Task 3 runs the harvest on a clone made read-only with `chmod`, asserts the branch is in the cache, and that no path under the claim changed.
5. **A Fleet the operator must refuse whole.** A `<fleet>-<crew>-<agent>` over 63 characters, a `tmux` runner, and a `runner.resources` that is not a Kubernetes `ResourceRequirements` each make `Resolved=False` with the config path, and no Crew, Agent or `PUT` body comes out. Task 7 pins all three.

## Decisions this plan makes

Task 12 writes these into the spec as §20.5.

- **The runner check is the resolver's.** `ResolveOptions` gains `runner: RunnerKind` (default `Tmux`): an agent whose `runner.type` differs fails with the config path. The CLI resolves with `Tmux`, the operator with `Pod`. A tmux-mode daemon also refuses a posted `pod` runner (it trusts what it is posted, #24). A Kubernetes-mode daemon checks nothing: its tests post specs with the default runner.
- **The pod mounts whole pool directories.** `/balerix/shared/crew`, `/balerix/shared/fleet` and `/balerix/shared/daemon` are each one read-only sub-path mount (holding `mise/`, and for the crew `no-hooks/`), beside `/balerix/shared/repo/.git/objects`. §7.4's paths (`crew/mise` and so on) are inside them.
- **The volume's directories** are §8.1's: `pools/daemon`, `fleets/<fleet>/pool`, `fleets/<fleet>/crews/<crew>/pool`, `fleets/<fleet>/crews/<crew>/repo`.
- **Each Job has an init container `slice`** that mounts the whole shared claim at `/balerix/volume` and runs `mkdir -p` for the Job's directories as uid 10001. The kubelet creates a missing sub-path owned by root, which the Job's user could not write. Unverified until 3b runs it on `kind`.
- **Tool tables are arguments** (`--tool node=22.11.0`, repeated); the GitHub token is a mounted file (`--gh-token-file`).
- **`pool-sync --level daemon` installs the embedded system table** (`claude`, `gh`) unless `--tool` is given; the sidecar renders the same embedded table, and both come from one image version (O-13).
- **A Job is re-run when its input changes:** every Job carries `balerix.ai/input-hash`; `backoffLimit: 0`, the operator retries with back-off (3b).
- **Object names:** per Daemon `balerix-<daemon>` (StatefulSet, Service, NetworkPolicy), `balerix-<daemon>-state`, `balerix-<daemon>-shared` (claims), `balerix-<daemon>-ca` (Secret and ConfigMap), `balerix-<daemon>-tls`, `balerix-<daemon>-admin` (Secrets), `balerix-<daemon>-pool` (Job). Per fleet `<fleet>-pool` (Job). Per Crew `<fleet>-<crew>-sync` (Job). Per Agent `<fleet>-<crew>-<agent>` (claim, Pod, NetworkPolicy), `…-bundle`, `…-token` (Secrets), `…-harvest` (Job).
- **Certificates:** the authority is valid ten years, a serving certificate ninety days, renewed thirty days before expiry. The expiry is kept as the Secret annotation `balerix.ai/not-after` (unix seconds), so nothing parses X.509. The authority is reloaded from its key alone: its parameters are a function of the Daemon's namespace and name.
- **The Daemon pod mounts no shared volume.** In Kubernetes mode it installs no pool and reads workspaces over the link (§7.4).
- **A Daemon's two claims carry no owner reference:** deleting a Daemon leaves its state and its shared volume, as a StatefulSet leaves its claims. Everything else the operator makes is owned and garbage-collected.
- **The Daemon's NetworkPolicy admits its own agents, Jobs and the operator.** §10.2's "whatever the user's Ingress names" has no field in §4.1 and is left for the chart (sub-project 5).
- **The sidecar's requests are fixed** at `cpu: 50m`, `memory: 64Mi` (§6.1 says "small fixed requests").
- **A renewed serving certificate rolls the daemon pod:** its expiry is an annotation on the pod template, since `serve` reads the certificate at start.
- **In a pod the crew's logs are `<claim>/.balerix/state/crew-logs`**, outside the agent's sandbox grants like the sidecar's own state.
- **Types are built against `k8s-openapi`'s `v1_32`**, the oldest it offers; §6.1's minimum of 1.29 is about the cluster, and nothing here uses a field newer than native sidecars.
- **`status.session` on an Agent stays unset in 3a:** the Daemon's `AgentStatus` carries no session id.
- **Crew and agent names are not pattern-checked by the schema:** a structural schema cannot constrain map keys; `balerix-config` checks them at resolution.

---

## File Structure

| File | Change |
|---|---|
| `crates/balerix-api/src/settings.rs` | `RunnerSettings::Pod { … }`, `RunnerKind`, `RunnerSettings::kind()` |
| `crates/balerix-api/src/lib.rs` | re-export `RunnerKind` |
| `crates/balerix-config/src/resolve.rs` | `ResolveOptions::runner`, the refusal |
| `crates/balerix-server/src/daemon.rs` | `refuse_pod_runner` in `apply_as` for a tmux-mode daemon |
| `crates/balerix-runtime/src/layout.rs` | `SharedSlice`; `CrewPaths::logs`; `AgentPaths::git_profile` as a field; the pod branches go through `SharedSlice` |
| `crates/balerix-runtime/src/workspace.rs` | `crew.logs`; `sync_cache`; `harvest_only`; `harvest` answers whether it fetched; `discard_half_made` accepts an emptied directory |
| `crates/balerix-runtime/src/{inspect,materializer,sandbox}.rs` | `crew.logs`, `git_profile` as a field |
| `crates/balerix-runtime/src/jobs.rs` | **new**: `sync_crew`, `sync_pool`, `harvest`, `SyncError`, `PoolLevel` |
| `crates/balerix-runtime/tests/jobs_it.rs` | **new** |
| `agent/src/cli.rs`, `agent/src/main.rs` | `crew-sync`, `pool-sync`, `harvest`; `terminate` takes the command's name |
| `agent/src/jobs.rs` | **new**: the three commands over `balerix_runtime::jobs` |
| `agent/tests/jobs_cli_it.rs` | **new** |
| `operator/Cargo.toml`, `deny.toml`, `clippy.toml` | **new** project files |
| `operator/src/main.rs`, `lib.rs` | **new**: `crds` |
| `operator/src/api/{mod,common,daemon,fleet,crew,agent,plugin}.rs` | **new**: the five kinds |
| `operator/crds/*.yaml` | **new**, generated |
| `operator/src/pki.rs` | **new** |
| `operator/src/desired/{mod,common,names,fleet,daemon,jobs,agent}.rs` | **new** |
| `operator/src/daemon_client.rs` | **new** |
| `operator/tests/{crds_it,client_it}.rs`, `operator/tests/support/mod.rs` | **new** |
| `scripts/operator.sh` | **new** |
| `mise.toml` | tasks `operator`, `crds`; `fmt` and `audit` entries |
| `.github/workflows/ci.yml` | job `operator` |
| `.gitignore` | `operator/target` |
| `AGENTS.md`, `ARCHITECTURE.md`, the spec | Task 12 |

---

### Task 1: The `pod` runner in the API, refused where it cannot run

**Files:**
- Modify: `crates/balerix-api/src/settings.rs`, `crates/balerix-api/src/lib.rs`
- Modify: `crates/balerix-config/src/resolve.rs`
- Modify: `crates/balerix-server/src/daemon.rs`

**Interfaces:**
- Produces: `balerix_api::RunnerKind { Tmux, Pod }` (`Copy`, `Default = Tmux`); `RunnerSettings::Pod { resources: Value, storage: Option<Value>, node_selector: BTreeMap<String, String>, tolerations: Vec<Value> }`; `RunnerSettings::kind(&self) -> RunnerKind`; `balerix_config::ResolveOptions::runner: RunnerKind`.

- [ ] **Step 1: Write the failing API tests**

In `crates/balerix-api/src/settings.rs`'s `tests` module:

```rust
    #[test]
    fn a_pod_runner_keeps_its_kubernetes_shapes_as_json() {
        let s: AgentSettings = serde_json::from_value(json!({
            "runner": {
                "type": "pod",
                "resources": { "requests": { "cpu": "1", "memory": "2Gi" } },
                "storage": { "size": "40Gi" },
                "nodeSelector": { "pool": "agents" },
                "tolerations": [{ "key": "agents", "operator": "Exists" }]
            }
        }))
        .unwrap();
        assert_eq!(s.runner.kind(), RunnerKind::Pod);
        let RunnerSettings::Pod {
            resources,
            storage,
            node_selector,
            tolerations,
        } = &s.runner
        else {
            panic!("{:?}", s.runner)
        };
        assert_eq!(resources["requests"]["memory"], "2Gi");
        assert_eq!(storage.as_ref().unwrap()["size"], "40Gi");
        assert_eq!(node_selector["pool"], "agents");
        assert_eq!(tolerations.len(), 1);
        // round trip, camelCase on the wire
        let back = serde_json::to_value(&s.runner).unwrap();
        assert_eq!(back["nodeSelector"]["pool"], "agents");
        assert_eq!(serde_json::from_value::<RunnerSettings>(back).unwrap(), s.runner);
    }

    #[test]
    fn a_bare_pod_runner_has_empty_shapes_and_an_unknown_key_is_refused() {
        let r: RunnerSettings = serde_json::from_value(json!({ "type": "pod" })).unwrap();
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({ "type": "pod", "resources": {} })
        );
        let e = serde_json::from_value::<RunnerSettings>(json!({ "type": "pod", "image": "x" }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown field `image`"), "{e}");
        assert_eq!(RunnerSettings::default().kind(), RunnerKind::Tmux);
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `mise x -- cargo nextest run -p balerix-api settings`
Expected: compile error, `RunnerKind` not found.

- [ ] **Step 3: Implement**

Replace the `RunnerSettings` enum in `crates/balerix-api/src/settings.rs`:

```rust
/// Which runner materializes the agent: a tmux window on one machine
/// (spec §6), or a pod (Spec O §4.2). The pod's Kubernetes shapes are
/// opaque here; `balerix-operator` gives them their types, so
/// `k8s-openapi` stays out of this crate (Spec O §20.3).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum RunnerSettings {
    #[default]
    Tmux,
    #[serde(rename_all = "camelCase")]
    Pod {
        /// A `ResourceRequirements` for the agent container.
        #[serde(default = "empty_object")]
        resources: Value,
        /// `{ size }`: overrides the Daemon's agent claim size.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage: Option<Value>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        node_selector: BTreeMap<String, String>,
        /// A list of `Toleration`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tolerations: Vec<Value>,
    },
}

/// `RunnerSettings` without its payload: what a resolver or a daemon is
/// willing to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunnerKind {
    #[default]
    Tmux,
    Pod,
}

impl RunnerSettings {
    pub fn kind(&self) -> RunnerKind {
        match self {
            Self::Tmux => RunnerKind::Tmux,
            Self::Pod { .. } => RunnerKind::Pod,
        }
    }
}
```

`RunnerSettings` is no longer `Copy` or `Eq`; nothing outside this file names it (`grep -rn RunnerSettings crates agent plugins`). In `crates/balerix-api/src/lib.rs` change the re-export to `pub use settings::{AgentSettings, ClaudeSettings, RunnerKind, RunnerSettings};`.

- [ ] **Step 4: Run the API tests**

Run: `mise x -- cargo nextest run -p balerix-api`
Expected: PASS, the existing `rejects_unknown_runner_type` included.

- [ ] **Step 5: Write the failing resolver test**

In `crates/balerix-config/src/resolve.rs`'s `tests` module:

```rust
    #[test]
    fn a_runner_this_resolver_does_not_serve_fails_with_the_config_path() {
        let pod = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ndefaults:\n  runner: { type: pod }\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a: {}\n";
        let err = resolve(&file(pod), &opts()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "crews.c.agents.a.runner.type: `pod` runs only on Kubernetes; this daemon runs agents in tmux"
        );
        let as_pod = ResolveOptions {
            runner: balerix_api::RunnerKind::Pod,
            ..opts()
        };
        assert!(resolve(&file(pod), &as_pod).is_ok());
        let err = resolve(&file(BASE), &as_pod).unwrap_err();
        assert_eq!(
            err.to_string(),
            "crews.c.agents.a.runner.type: `tmux` is not available on Kubernetes; a Fleet's agents run as pods"
        );
    }
```

Run: `mise x -- cargo nextest run -p balerix-config a_runner_this_resolver`
Expected: compile error, no field `runner`.

- [ ] **Step 6: Implement the resolver check**

In `ResolveOptions` add, after `restricted`:

```rust
    /// The runner this resolution is for: `Tmux` on one machine (the
    /// default), `Pod` for the operator (Spec O §4.2). An agent whose
    /// `runner.type` is the other one fails with its config path.
    pub runner: balerix_api::RunnerKind,
```

In `resolve`, directly after `validate_agent(&agent_path, &settings)?;`:

```rust
            if settings.runner.kind() != opts.runner {
                return Err(ConfigError::Invalid {
                    path: format!("{agent_path}.runner.type"),
                    message: match opts.runner {
                        balerix_api::RunnerKind::Tmux => {
                            "`pod` runs only on Kubernetes; this daemon runs agents in tmux"
                        }
                        balerix_api::RunnerKind::Pod => {
                            "`tmux` is not available on Kubernetes; a Fleet's agents run as pods"
                        }
                    }
                    .to_string(),
                });
            }
```

Every `ResolveOptions { … }` literal that lists all fields without `..` gains `runner: Default::default()`; the compiler names them (`crates/balerix/src/wiring.rs`, `commands/{fleet,config,dev}.rs`, `crates/balerix-config/tests/resolve_golden.rs`).

- [ ] **Step 7: Write the failing daemon test and implement**

In `crates/balerix-server/src/daemon.rs`, a free function beside `apply_as`'s `impl`:

```rust
/// A tmux-mode daemon runs no pod (Spec O §20.3). The CLI's resolver
/// refuses this first; the daemon does not rely on that (#24).
fn refuse_pod_runner(spec: &FleetSpec) -> Result<(), DaemonError> {
    for (crew_name, crew) in &spec.crews {
        for (agent_name, settings) in &crew.agents {
            if settings.runner.kind() == balerix_api::RunnerKind::Pod {
                return Err(DaemonError::Invalid(format!(
                    "crews.{crew_name}.agents.{agent_name}.runner.type: `pod` runs only on \
                     Kubernetes; this daemon runs agents in tmux"
                )));
            }
        }
    }
    Ok(())
}
```

Its test, in that file's `tests` module (write it first, see it fail to compile, then add the function):

```rust
    #[test]
    fn a_pod_runner_is_refused_with_its_config_path() {
        let spec: FleetSpec = serde_json::from_value(serde_json::json!({
            "name": "f",
            "crews": { "c": { "repo": "o/r", "ref": "main", "agents": {
                "a": {},
                "b": { "runner": { "type": "pod" } }
            } } }
        }))
        .unwrap();
        let DaemonError::Invalid(message) = refuse_pod_runner(&spec).unwrap_err() else {
            panic!("not Invalid")
        };
        assert_eq!(
            message,
            "crews.c.agents.b.runner.type: `pod` runs only on Kubernetes; this daemon runs agents in tmux"
        );
        let tmux_only: FleetSpec = serde_json::from_value(serde_json::json!({
            "name": "f",
            "crews": { "c": { "repo": "o/r", "ref": "main", "agents": { "a": {} } } }
        }))
        .unwrap();
        assert!(refuse_pod_runner(&tmux_only).is_ok());
    }
```

Call it in `apply_as`, directly after the `Fleet::try_from(spec.clone())…?;` line:

```rust
        if self.ports.kube.is_none() {
            refuse_pod_runner(&spec)?;
        }
```

- [ ] **Step 8: Run the gate and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add crates/balerix-api/src/settings.rs crates/balerix-api/src/lib.rs crates/balerix-config crates/balerix-server/src/daemon.rs crates/balerix/src
git commit -m "feat(api): a pod runner with opaque Kubernetes shapes, refused by the tmux resolver and daemon (Spec O §20.3)"
```

---

### Task 2: Crew logs and the git profile leave their fixed places

The sidecar's crew root is a read-only mount: its `git.log` is silently lost today. The harvest Job's claim is read-only: the git profile cannot be written beside the clone. Both paths become fields the layout fills.

**Files:**
- Modify: `crates/balerix-runtime/src/layout.rs`, `workspace.rs`, `inspect.rs`, `materializer.rs`, `sandbox.rs`
- Test: `crates/balerix-runtime/tests/layout_pod_it.rs`, `materialize_pod_it.rs`

**Interfaces:**
- Produces: `CrewPaths { pub root, pub repo, pub logs: PathBuf }`; `AgentPaths::git_profile: PathBuf` (a field; the method of that name is gone); `layout::SharedSlice { pub root: PathBuf }` with `new(impl Into<PathBuf>)`, `crew() -> CrewPaths`, `fleet() -> FleetPaths`, `daemon_root() -> PathBuf`, `daemon_pool() -> PathBuf`.

- [ ] **Step 1: Write the failing layout test**

Append to `crates/balerix-runtime/tests/layout_pod_it.rs`:

```rust
#[test]
fn crew_logs_follow_the_layout_and_the_git_profile_is_a_field() {
    let id: AgentId = "payments/backend/alice".parse().unwrap();
    let pod = StateLayout::pod(mounts(), &id);
    assert_eq!(
        pod.crew(&id.crew_ref()).logs,
        PathBuf::from("/balerix/agent/.balerix/state/crew-logs"),
        "the crew root is a read-only mount in a pod"
    );
    assert_eq!(
        pod.agent(&id).git_profile,
        PathBuf::from("/balerix/agent/nono-git-profile.json")
    );
    let xdg = StateLayout::xdg("/s".into(), "/d".into(), "/c".into());
    let crew = xdg.crew(&id.crew_ref());
    assert_eq!(crew.logs, crew.root.join("logs"));
}

#[test]
fn a_shared_slice_is_the_pods_view_without_an_agent() {
    use balerix_runtime::layout::SharedSlice;
    let id: AgentId = "payments/backend/alice".parse().unwrap();
    let pod = StateLayout::pod(mounts(), &id);
    let slice = SharedSlice::new("/balerix/shared");
    let crew = slice.crew();
    assert_eq!(crew.repo, pod.crew(&id.crew_ref()).repo);
    assert_eq!(crew.root, pod.crew(&id.crew_ref()).root);
    assert_eq!(crew.logs, PathBuf::from("/balerix/shared/crew/logs"));
    assert_eq!(slice.fleet().mise_pool(), pod.fleet(&id.fleet).mise_pool());
    assert_eq!(slice.daemon_pool(), pod.mise_data_dir());
    assert_eq!(slice.daemon_root(), PathBuf::from("/balerix/shared/daemon"));
}
```

Run: `mise x -- cargo nextest run -p balerix-runtime --test layout_pod_it`
Expected: compile error (`logs`, `git_profile`, `SharedSlice`).

- [ ] **Step 2: Implement in `layout.rs`**

Add the field to `CrewPaths` and `AgentPaths`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewPaths {
    pub root: PathBuf,
    pub repo: PathBuf,
    /// Where git calls and pool installs for this crew are logged:
    /// `<root>/logs` on one machine and in a Job; on the agent claim in a
    /// pod, where the crew root is a read-only mount (Spec O §20.3).
    pub logs: PathBuf,
}
```

In `AgentPaths` add after `launch`:

```rust
    /// The nono profile the daemon's own git runs under in this agent's
    /// clone (Spec N amendment 2026-10-01 §4). Daemon-owned: the agent
    /// root is outside every sandbox grant. A field so that the harvest
    /// Job, whose claim is read-only, can put it in its scratch directory
    /// (Spec O §20.3).
    pub git_profile: PathBuf,
```

and delete the `pub fn git_profile(&self)` method with its comment. In `StateLayout::agent` add `git_profile: root.join("nono-git-profile.json"),` to the literal.

Add `SharedSlice` after `PodLayout`'s `impl`:

```rust
/// The shared slice of one crew, as the agent pod and the Jobs both mount
/// it (Spec O §8.1, §20.3): `repo` (the crew cache), `crew`, `fleet` and
/// `daemon` (the three pool directories, each holding `mise/`). Nothing
/// here knows the directory names on the volume itself; those are the
/// operator's sub-path mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedSlice {
    pub root: PathBuf,
}

impl SharedSlice {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    /// The crew's paths with its logs beside its pool: right for a Job,
    /// which mounts the slice read-write.
    pub fn crew(&self) -> CrewPaths {
        let root = self.root.join("crew");
        CrewPaths {
            repo: self.root.join("repo"),
            logs: root.join("logs"),
            root,
        }
    }
    pub fn fleet(&self) -> FleetPaths {
        let root = self.root.join("fleet");
        FleetPaths {
            mise_toml: root.join("mise.toml"),
            root,
        }
    }
    pub fn daemon_root(&self) -> PathBuf {
        self.root.join("daemon")
    }
    pub fn daemon_pool(&self) -> PathBuf {
        self.daemon_root().join("mise")
    }
}
```

Route the pod branches through it. `PodLayout::shared_repo` becomes `SharedSlice::new(&self.mounts.shared).crew().repo`. In `StateLayout`:

```rust
    pub fn mise_data_dir(&self) -> PathBuf {
        match &self.pod {
            Some(p) => SharedSlice::new(&p.mounts.shared).daemon_pool(),
            None => self.data_root.join("mise"),
        }
    }
```

```rust
    pub fn crew(&self, c: &CrewRef) -> CrewPaths {
        if let Some(p) = &self.pod
            && p.id.crew_ref() == *c
        {
            return CrewPaths {
                logs: self.state_root.join("crew-logs"),
                ..SharedSlice::new(&p.mounts.shared).crew()
            };
        }
        let root = self.fleet_dir(&c.fleet).join("crews").join(c.crew.as_str());
        CrewPaths {
            repo: root.join("repo"),
            logs: root.join("logs"),
            root,
        }
    }
```

and in `fleet`, the pod branch returns `SharedSlice::new(&p.mounts.shared).fleet()`. Add `SharedSlice` to the `pub use layout::{…}` list in `crates/balerix-runtime/src/lib.rs`.

- [ ] **Step 3: Move the callers**

- `workspace.rs`: both `.log(&crew.root.join("logs").join("git.log"))` become `.log(&crew.logs.join("git.log"))`; `agent.git_profile()` in `sandbox_args` becomes `agent.git_profile`.
- `inspect.rs`: `crew.root.join("logs").join("git.log")` becomes `crew.logs.join("git.log")`.
- `materializer.rs` `install_pools`: `crew_paths.root.join("logs")` becomes `crew_paths.logs`.
- `sandbox.rs` `write_git_profile`: `let path = paths.git_profile();` becomes `let path = paths.git_profile.clone();`.
- Every other `git_profile()` call (`grep -rn 'git_profile()' crates agent`, fifteen today, most in tests) drops its parentheses; a test that takes it by value adds `.clone()`.

Run: `mise x -- cargo check --workspace --all-targets`
Expected: clean. Then `scripts/agent.sh build`: clean.

- [ ] **Step 4: Pin the sidecar's log on the claim**

In `crates/balerix-runtime/tests/materialize_pod_it.rs`, in `a_pod_materialize_clones_from_the_objects_only_cache_and_writes_nothing_into_it`, before the final `chmod_tree`:

```rust
    assert!(
        layout.crew(&id.crew_ref()).logs.join("git.log").is_file(),
        "the clone's git calls are logged on the claim"
    );
    assert!(
        !root.join("shared/crew/logs").exists(),
        "nothing is written under the crew root, a read-only mount in a pod"
    );
```

- [ ] **Step 5: Run and commit**

Run: `mise run check && mise run test-it && mise run agent`
Expected: PASS.

```bash
git add crates/balerix-runtime agent
git commit -m "refactor(runtime): crew logs and the git profile are layout fields; a pod logs its crew's git on the claim (Spec O §20.3)"
```

---

### Task 3: The Jobs' work in `balerix-runtime`

**Files:**
- Create: `crates/balerix-runtime/src/jobs.rs`
- Modify: `crates/balerix-runtime/src/workspace.rs`, `crates/balerix-runtime/src/lib.rs`
- Test: `crates/balerix-runtime/tests/jobs_it.rs`, `crates/balerix-runtime/tests/workspace_it.rs`

**Interfaces:**
- Consumes: `SharedSlice`, `CrewPaths::logs`, `AgentPaths::git_profile` (Task 2).
- Produces, in `balerix_runtime::jobs`:
  - `enum SyncError { Cache(MaterializeError), Tools(MaterializeError) }`, displayed `cache: …` and `tools: …`
  - `enum PoolLevel { Daemon, Fleet(FleetName) }`
  - `fn sync_crew(tools: &ToolPaths, slice: &SharedSlice, scratch: &Path, crew: &CrewRef, repo: &RepoRef, git_ref: &str, gh_token: Option<&str>, table: &BTreeMap<String, String>) -> Result<String, SyncError>` (the commit `origin/<git_ref>` is at)
  - `fn sync_pool(tools: &ToolPaths, slice: &SharedSlice, scratch: &Path, level: &PoolLevel, table: &BTreeMap<String, String>) -> Result<(), MaterializeError>`
  - `fn harvest(tools: &ToolPaths, slice: &SharedSlice, scratch: &Path, id: &AgentId, claim: &Path) -> Result<Option<String>, MaterializeError>` (the branch harvested, `None` when there was nothing)
- Produces, on `Workspace`: `sync_cache(&self, id: &str, crew: &CrewPaths, repo: &RepoRef, git_ref: &str) -> Result<String, MaterializeError>`; `harvest_only(&self, id: &str, crew: &CrewPaths, agent: &AgentPaths) -> Result<Option<String>, MaterializeError>`.

- [ ] **Step 1: Write the failing tests**

Create `crates/balerix-runtime/tests/jobs_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §8.3, §8.4, §20.3: what the sync and harvest Jobs do, on a
//! shared slice laid out as the pod mounts it.
mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use balerix_core::{AgentId, CrewRef, RepoRef};
use balerix_runtime::jobs::{PoolLevel, SyncError, harvest, sync_crew, sync_pool};
use balerix_runtime::layout::{PodMounts, SharedSlice};
use balerix_runtime::{StateLayout, Workspace};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_PREFIX")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare repository with one commit on `main`, and the work tree it came
/// from (to push more).
fn upstream(root: &Path) -> (RepoRef, PathBuf) {
    let work = root.join("upstream-work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("upstream.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    git(&work, &["remote", "add", "up", &bare.display().to_string()]);
    (
        RepoRef::parse(&format!("file://{}", bare.display())).unwrap(),
        work,
    )
}

/// The slice as a Job finds it: the four mount points exist and are empty.
fn slice(root: &Path) -> SharedSlice {
    let s = SharedSlice::new(root.join("shared"));
    for d in ["repo", "crew", "fleet", "daemon"] {
        std::fs::create_dir_all(s.root.join(d)).unwrap();
    }
    s
}

fn crew() -> CrewRef {
    "f/c".parse().unwrap()
}

fn none() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn listing(dir: &Path) -> BTreeMap<String, (u64, std::time::SystemTime)> {
    fn walk(dir: &Path, out: &mut BTreeMap<String, (u64, std::time::SystemTime)>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let m = e.metadata().unwrap();
            out.insert(e.path().display().to_string(), (m.len(), m.modified().unwrap()));
            if m.is_dir() {
                walk(&e.path(), out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, &mut out);
    out
}

fn chmod_tree(dir: &Path, writable: bool) {
    let mode = |m: u32| if writable { m | 0o200 } else { m & !0o222 };
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        let meta = std::fs::symlink_metadata(&p).unwrap();
        if meta.is_dir() {
            if writable {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode(meta.mode())))
                    .unwrap();
            }
            chmod_tree(&p, writable);
        }
        if !meta.file_type().is_symlink() {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode(meta.mode())))
                .unwrap();
        }
    }
}

use std::os::unix::fs::MetadataExt;

#[test]
fn crew_sync_makes_the_cache_fetches_the_ref_and_reports_its_commit() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise", false));
        return;
    };
    let root = support::temp_root("jobs-crew-sync");
    let (repo, work) = upstream(&root);
    let slice = slice(&root);
    let scratch = root.join("scratch");
    let first = sync_crew(&tools, &slice, &scratch, &crew(), &repo, "main", None, &none()).unwrap();
    assert_eq!(first, git(&work, &["rev-parse", "HEAD"]));
    let paths = slice.crew();
    assert!(paths.repo.join(".git").is_dir());
    assert_eq!(git(&paths.repo, &["config", "gc.auto"]), "0");
    assert!(paths.no_hooks().is_dir(), "the pod's git calls name it");
    // review focus 3: an empty table still leaves the pool and its marker
    assert!(paths.mise_pool().is_dir());
    assert!(paths.installed_marker().is_file());
    assert!(paths.logs.join("git.log").is_file());

    std::fs::write(work.join("more"), "x\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "more"]);
    git(&work, &["push", "-q", "up", "main"]);
    let second =
        sync_crew(&tools, &slice, &scratch, &crew(), &repo, "main", None, &none()).unwrap();
    assert_eq!(second, git(&work, &["rev-parse", "HEAD"]));
    assert_ne!(second, first, "an existing cache is fetched");
}

/// Review focus 2.
#[test]
fn a_ref_the_remote_lacks_is_a_cache_error_that_names_it() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise", false));
        return;
    };
    let root = support::temp_root("jobs-crew-sync-noref");
    let (repo, _) = upstream(&root);
    let slice = slice(&root);
    let e = sync_crew(
        &tools,
        &slice,
        &root.join("scratch"),
        &crew(),
        &repo,
        "nope",
        None,
        &none(),
    )
    .unwrap_err();
    assert!(matches!(e, SyncError::Cache(_)), "{e:?}");
    assert_eq!(e.to_string(), "cache: f/c: the remote has no branch nope");
}

#[test]
fn a_remote_that_is_not_there_is_a_cache_error_and_a_later_run_recovers() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise", false));
        return;
    };
    let root = support::temp_root("jobs-crew-sync-noremote");
    let (repo, _) = upstream(&root);
    let slice = slice(&root);
    let scratch = root.join("scratch");
    let gone = RepoRef::parse(&format!("file://{}/no-such.git", root.display())).unwrap();
    let e = sync_crew(&tools, &slice, &scratch, &crew(), &gone, "main", None, &none())
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("cache: f/c: "), "{e}");
    assert!(!slice.crew().repo.join(".git").exists());
    sync_crew(&tools, &slice, &scratch, &crew(), &repo, "main", None, &none()).unwrap();
}

/// Review focus 3, the upper pools.
#[test]
fn pool_sync_with_an_empty_table_still_leaves_the_pool_and_its_marker() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise", false));
        return;
    };
    let root = support::temp_root("jobs-pool-sync");
    let slice = slice(&root);
    let scratch = root.join("scratch");
    sync_pool(&tools, &slice, &scratch, &PoolLevel::Daemon, &none()).unwrap();
    assert!(slice.daemon_pool().is_dir());
    assert!(slice.daemon_root().join("mise.installed").is_file());
    let fleet = PoolLevel::Fleet("f".parse().unwrap());
    sync_pool(&tools, &slice, &scratch, &fleet, &none()).unwrap();
    assert!(slice.fleet().mise_pool().is_dir());
    assert!(slice.fleet().installed_marker().is_file());
    // a marker that outlived its pool must not short-circuit the install
    std::fs::remove_dir_all(slice.fleet().mise_pool()).unwrap();
    sync_pool(&tools, &slice, &scratch, &fleet, &none()).unwrap();
    assert!(slice.fleet().mise_pool().is_dir());
}

/// A clone as the sidecar makes it, on a pod layout over `slice`, with
/// one unpushed commit. Returns the claim and the commit.
fn pod_clone(
    tools: &balerix_runtime::ToolPaths,
    root: &Path,
    slice: &SharedSlice,
    repo: &RepoRef,
    id: &AgentId,
) -> (PathBuf, String) {
    let claim = root.join("agent");
    let layout = StateLayout::pod(
        PodMounts {
            agent: claim.clone(),
            shared: slice.root.clone(),
            run: root.join("run"),
        },
        id,
    );
    let paths = layout.agent(id);
    Workspace {
        tools,
        gh_config_dir: None,
        cache_is_read_only: true,
    }
    .ensure_clone(
        &id.to_string(),
        &layout.crew(&id.crew_ref()),
        &paths,
        repo,
        "balerix/f/c/a",
        "main",
    )
    .unwrap();
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    (claim, sha)
}

/// Review focus 4.
#[test]
fn a_harvest_reads_a_claim_it_cannot_write_and_leaves_it_as_it_was() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("jobs-harvest");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let (repo, _) = upstream(&root);
    let slice = slice(&root);
    let scratch = root.join("scratch");
    sync_crew(&tools, &slice, &scratch, &crew(), &repo, "main", None, &none()).unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    let (claim, sha) = pod_clone(&tools, &root, &slice, &repo, &id);
    chmod_tree(&claim, false);
    std::fs::set_permissions(&claim, std::fs::Permissions::from_mode(0o555)).unwrap();
    let before = listing(&claim);

    let got = harvest(&tools, &slice, &scratch, &id, &claim);
    let after = listing(&claim);
    std::fs::set_permissions(&claim, std::fs::Permissions::from_mode(0o755)).unwrap();
    chmod_tree(&claim, true); // let TempRoot remove it
    assert_eq!(got.unwrap().as_deref(), Some("balerix/f/c/a"));
    assert_eq!(
        git(&slice.crew().repo, &["rev-parse", "refs/heads/balerix/f/c/a"]),
        sha
    );
    assert_eq!(after, before, "the harvest wrote under the claim");
    assert!(scratch.join("nono-git-profile.json").is_file());
    assert!(claim.join("workspace/work.txt").is_file(), "nothing is removed");
}

#[test]
fn a_claim_with_no_clone_has_nothing_to_harvest() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("jobs-harvest-empty");
    let (repo, _) = upstream(&root);
    let slice = slice(&root);
    let scratch = root.join("scratch");
    sync_crew(&tools, &slice, &scratch, &crew(), &repo, "main", None, &none()).unwrap();
    let claim = root.join("agent");
    std::fs::create_dir_all(&claim).unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    assert_eq!(harvest(&tools, &slice, &scratch, &id, &claim).unwrap(), None);
}

/// Spec N's case, on the Job's path: a nono that cannot run fails the
/// harvest; it is never read as "nothing to harvest".
#[test]
fn a_harvest_without_a_working_nono_fails() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("jobs-harvest-no-nono");
    let (repo, _) = upstream(&root);
    let slice = slice(&root);
    let scratch = root.join("scratch");
    sync_crew(&tools, &slice, &scratch, &crew(), &repo, "main", None, &none()).unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    let (claim, _) = pod_clone(&tools, &root, &slice, &repo, &id);
    let no_nono = balerix_runtime::ToolPaths {
        nono: root.join("no-such-nono"),
        ..tools.clone()
    };
    let e = harvest(&no_nono, &slice, &scratch, &id, &claim)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/a: "), "{e}");
    assert!(e.contains("no-such-nono"), "{e}");
}
```

And review focus 1, in `crates/balerix-runtime/tests/workspace_it.rs`, after `a_cache_whose_pin_failed_is_removed_and_made_again`:

```rust
/// Spec O §20.3: in a Job the cache directory is a mount point, which can
/// be emptied and never removed. A cache whose making failed half-way is
/// emptied, git's own error is the one reported, and the next run makes it.
#[test]
fn a_half_made_cache_in_a_directory_that_cannot_be_removed_is_emptied() {
    use std::os::unix::fs::PermissionsExt;
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-cache-mountpoint");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let crew = layout.crew(&"f/c".parse().unwrap());
    std::fs::create_dir_all(&crew.repo).unwrap();
    // the parent refuses the rmdir, as a mount point's does
    std::fs::set_permissions(&crew.root, std::fs::Permissions::from_mode(0o555)).unwrap();
    let broken = failing_git(&root, &tools, "config");
    let e = Workspace {
        tools: &broken,
        gh_config_dir: None,
        cache_is_read_only: false,
    }
    .ensure_repo("f/c", &crew, &repo, "main")
    .unwrap_err()
    .to_string();
    let made = Workspace {
        tools: &tools,
        gh_config_dir: None,
        cache_is_read_only: false,
    }
    .ensure_repo("f/c", &crew, &repo, "main");
    std::fs::set_permissions(&crew.root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(e.contains("config refused"), "{e}");
    assert!(!e.contains("removing what it left behind failed"), "{e}");
    made.unwrap();
    assert_eq!(git(&crew.repo, &["config", "gc.auto"]).trim(), "0");
}
```

Run: `mise x -- cargo nextest run -p balerix-runtime --test jobs_it --test workspace_it a_half_made_cache`
Expected: compile error (`balerix_runtime::jobs`).

- [ ] **Step 2: `workspace.rs`: an emptied directory counts as removed**

In `discard_half_made`, add an arm before `Err(cleanup) =>`:

```rust
        // A mount point (the cache in a sync Job, Spec O §20.3) can be
        // emptied and never removed; empty is what the next pass needs.
        Err(_) if std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_none()) => failed,
```

- [ ] **Step 3: `workspace.rs`: `sync_cache`**

After `ensure_repo`:

```rust
    /// The sync Job's cache step (Spec O §8.3): the cache made when absent,
    /// then fetched, since in a pod nothing else ever fetches it
    /// (`create_clone` skips its own fetch there). Creates the crew's
    /// `no-hooks` directory, which the pod's git calls name and cannot
    /// make on a read-only mount. Returns the commit `origin/<git_ref>` is
    /// at, the Crew's `cacheRef`.
    pub fn sync_cache(
        &self,
        id: &str,
        crew: &CrewPaths,
        repo: &RepoRef,
        git_ref: &str,
    ) -> Result<String, MaterializeError> {
        self.ensure_repo(id, crew, repo, git_ref)?;
        let no_hooks = crew.no_hooks();
        std::fs::create_dir_all(&no_hooks).map_err(|e| MaterializeError::Io {
            id: id.to_string(),
            path: no_hooks,
            message: e.to_string(),
        })?;
        let cache = crew.repo.display().to_string();
        self.git(
            id,
            crew,
            &["-C", &cache, "fetch", "--quiet", "--no-auto-gc", "origin"],
        )?;
        let remote = format!("refs/remotes/origin/{git_ref}^{{commit}}");
        // `--quiet` answers a missing ref with exit 1 and no text
        if !self.git_probe(
            id,
            crew,
            &["-C", &cache, "rev-parse", "--verify", "--quiet", &remote],
        )? {
            return Err(MaterializeError::Invalid {
                id: id.to_string(),
                message: format!("the remote has no branch {git_ref}"),
            });
        }
        self.git(id, crew, &["-C", &cache, "rev-parse", "--verify", &remote])
            .map(|sha| sha.trim().to_string())
    }
```

- [ ] **Step 4: `workspace.rs`: a harvest that removes nothing**

`harvest` returns whether it fetched: change its signature's return type to `Result<bool, MaterializeError>`, the early `return Ok(());` for an absent branch to `return Ok(false);`, and its tail `.map(|_| ())` to `.map(|_| true)`.

Replace `harvest_and_remove_after`'s body and add the two functions around it:

```rust
    pub(crate) fn harvest_and_remove_after(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        after_checks: impl FnOnce(),
        before_fetch: impl FnOnce(),
    ) -> Result<(), MaterializeError> {
        self.harvest_checked(id, crew, agent, after_checks, before_fetch)?;
        remove_tree(id, &agent.workspace)
    }

    /// The harvest Job's step (Spec O §8.4): `harvest_and_remove` without
    /// the removal, since the Job's claim is mounted read-only and the
    /// operator deletes it afterwards. Returns the branch now in the
    /// cache; `None` when there was nothing to harvest (no clone, no
    /// cache, a detached HEAD, a branch the agent deleted).
    pub fn harvest_only(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
    ) -> Result<Option<String>, MaterializeError> {
        self.harvest_checked(id, crew, agent, || {}, || {})
    }

    /// The checks, the sandbox canary, the config check and the fetch, in
    /// the order `harvest_and_remove_after` documents.
    fn harvest_checked(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        after_checks: impl FnOnce(),
        before_fetch: impl FnOnce(),
    ) -> Result<Option<String>, MaterializeError> {
        if !(crew.repo.join(".git").is_dir() && agent.workspace.join(".git").is_dir()) {
            return Ok(None);
        }
        check_clone(id, crew, agent)?;
        self.prepare_sandbox(id, crew, agent)?;
        self.check_clone_config(id, crew, agent)?;
        after_checks();
        let Some(branch) = self.assigned_branch(id, crew, agent)? else {
            return Ok(None);
        };
        Ok(self
            .harvest(id, crew, agent, &branch, before_fetch)?
            .then_some(branch))
    }
```

The other caller of `harvest` (the changed-branch arm of `ensure_clone`) ignores the answer: append `.map(|_| ())` or bind it to `_` there, whichever the surrounding expression needs.

- [ ] **Step 5: Create `crates/balerix-runtime/src/jobs.rs`**

```rust
//! What the operator's Jobs do on the shared volume (Spec O §8.3, §8.4,
//! §20.3). Each sees the crew's slice where the agent pod mounts it, so
//! every path a clone or a pool records is the one the agent later reads.
//! `balerix-agent` wraps these as `crew-sync`, `pool-sync` and `harvest`.

use std::collections::BTreeMap;
use std::path::Path;

use balerix_core::{AgentId, CrewRef, FleetName, MaterializeError, RepoRef};

use crate::layout::{PodMounts, SharedSlice, StateLayout};
use crate::toolchain::Toolchain;
use crate::tools::ToolPaths;
use crate::workspace::Workspace;

/// Which half of a crew sync failed: the operator sets `CacheReady` or
/// `ToolsReady` from the prefix (§5.3).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyncError {
    #[error("cache: {0}")]
    Cache(MaterializeError),
    #[error("tools: {0}")]
    Tools(MaterializeError),
}

/// The two pools above a crew's. Each has a Job of its own, so two crews
/// of one fleet never install the fleet pool at once (§20.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolLevel {
    Daemon,
    Fleet(FleetName),
}

/// The crew cache, made or fetched, then the crew pool, with the fleet
/// and daemon pools as read-only parents. `gh_token` is written under
/// `scratch` for git's credential helper, never onto the volume. Returns
/// the commit `origin/<git_ref>` is at.
#[allow(clippy::too_many_arguments)]
pub fn sync_crew(
    tools: &ToolPaths,
    slice: &SharedSlice,
    scratch: &Path,
    crew: &CrewRef,
    repo: &RepoRef,
    git_ref: &str,
    gh_token: Option<&str>,
    table: &BTreeMap<String, String>,
) -> Result<String, SyncError> {
    let id = crew.to_string();
    let paths = slice.crew();
    let gh_config_dir = match gh_token {
        Some(token) => {
            let dir = scratch.join("gh");
            Workspace::write_fleet_gh_config(&dir, token, &id).map_err(SyncError::Cache)?;
            Some(dir)
        }
        None => None,
    };
    let commit = Workspace {
        tools,
        gh_config_dir,
        cache_is_read_only: false,
    }
    .sync_cache(&id, &paths, repo, git_ref)
    .map_err(SyncError::Cache)?;
    install(
        tools,
        scratch,
        &id,
        &format!("crew {crew}"),
        &paths.mise_toml(),
        &paths.mise_pool(),
        &[slice.fleet().mise_pool(), slice.daemon_pool()],
        &paths.installed_marker(),
        table,
        &paths.logs.join("mise.pools.log"),
    )
    .map_err(SyncError::Tools)?;
    Ok(commit)
}

pub fn sync_pool(
    tools: &ToolPaths,
    slice: &SharedSlice,
    scratch: &Path,
    level: &PoolLevel,
    table: &BTreeMap<String, String>,
) -> Result<(), MaterializeError> {
    match level {
        PoolLevel::Daemon => {
            let root = slice.daemon_root();
            install(
                tools,
                scratch,
                "system",
                "system",
                &root.join("mise.toml"),
                &slice.daemon_pool(),
                &[],
                &root.join("mise.installed"),
                table,
                &root.join("logs").join("mise.system.log"),
            )
        }
        PoolLevel::Fleet(name) => {
            let fleet = slice.fleet();
            install(
                tools,
                scratch,
                name.as_str(),
                &format!("fleet {name}"),
                &fleet.mise_toml,
                &fleet.mise_pool(),
                &[slice.daemon_pool()],
                &fleet.installed_marker(),
                table,
                &fleet.root.join("logs").join("mise.pools.log"),
            )
        }
    }
}

/// One level into its pool. The pool directory is made even for an empty
/// table: the agent pod mounts it as a sub-path, which must exist. A
/// marker that outlived its pool is dropped first, so `install_level`
/// cannot report an empty directory as installed (Spec F, F-5).
#[allow(clippy::too_many_arguments)]
fn install(
    tools: &ToolPaths,
    scratch: &Path,
    id: &str,
    label: &str,
    toml: &Path,
    pool: &Path,
    parents: &[std::path::PathBuf],
    marker: &Path,
    table: &BTreeMap<String, String>,
    log: &Path,
) -> Result<(), MaterializeError> {
    let io = |path: &Path, e: std::io::Error| MaterializeError::Io {
        id: id.to_string(),
        path: path.to_path_buf(),
        message: e.to_string(),
    };
    if !pool.exists() {
        match std::fs::remove_file(marker) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(marker, e)),
        }
        std::fs::create_dir_all(pool).map_err(|e| io(pool, e))?;
    }
    // `install_level` takes every path as an argument; the layout is only
    // what `Toolchain::install`, the agent's own step, reads.
    let layout = StateLayout::xdg(
        scratch.join("state"),
        scratch.join("data"),
        scratch.join("config"),
    );
    Toolchain {
        tools,
        layout: &layout,
    }
    .install_level(id, label, toml, pool, parents, marker, table, log, None)
}

/// The agent's branch into the crew cache, from a claim mounted read-only
/// at `claim`: the checks and the fetch of `harvest_and_remove`, with the
/// git profile, nono's home and its logs under `scratch`. Removes nothing.
pub fn harvest(
    tools: &ToolPaths,
    slice: &SharedSlice,
    scratch: &Path,
    id: &AgentId,
    claim: &Path,
) -> Result<Option<String>, MaterializeError> {
    let layout = StateLayout::pod(
        PodMounts {
            agent: claim.to_path_buf(),
            shared: slice.root.clone(),
            run: scratch.join("run"),
        },
        id,
    );
    let mut agent = layout.agent(id);
    agent.nono_home = scratch.join("nono");
    agent.logs = scratch.join("logs");
    agent.git_profile = scratch.join("nono-git-profile.json");
    Workspace {
        tools,
        gh_config_dir: None,
        cache_is_read_only: false,
    }
    .harvest_only(&id.to_string(), &slice.crew(), &agent)
}
```

Add `pub mod jobs;` to `crates/balerix-runtime/src/lib.rs`.

- [ ] **Step 6: Run the tests**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test jobs_it --test workspace_it`
Expected: PASS. If `a_harvest_reads_a_claim_it_cannot_write…` fails with a write under the claim, read `scratch/logs/nono-git.log` and the error's path: whatever still derives from `agent.root` must move to a field the Job overrides, as `git_profile` did. Do not make the claim writable.

- [ ] **Step 7: Gate and commit**

Run: `mise run check && mise run test-it && mise run agent`
Expected: PASS.

```bash
git add crates/balerix-runtime
git commit -m "feat(runtime): the sync and harvest Jobs' work on a shared slice (Spec O §8.3, §8.4, §20.3)"
```

---

### Task 4: `balerix-agent crew-sync`, `pool-sync`, `harvest`

**Files:**
- Create: `agent/src/jobs.rs`, `agent/tests/jobs_cli_it.rs`
- Modify: `agent/src/cli.rs`, `agent/src/main.rs`, `agent/src/lib.rs`, `agent/tests/cli_it.rs` (only if it names `terminate`)

**Interfaces:**
- Consumes: `balerix_runtime::jobs::{sync_crew, sync_pool, harvest, PoolLevel, SyncError}` (Task 3).
- Produces: the three subcommands. Each writes one line to `--termination-log` and exits 0 or 1:
  - `crew-sync`: the commit on success; `cache: …` or `tools: …` on failure.
  - `pool-sync`: `synced` on success; `tools: …` on failure.
  - `harvest`: `harvested <branch>` or `nothing to harvest`; the error on failure.
- Produces: `cli::finish(log: &Path, who: &str, outcome: anyhow::Result<String>) -> ExitCode`; `cli::terminate(log, who, e)` (the existing function with the command's name as a parameter).

- [ ] **Step 1: Write the failing CLI tests**

Create `agent/tests/jobs_cli_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Jobs' commands end with one line in the termination log, which is
//! all the operator reads of them (Spec O §20.3).
mod support;

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_balerix-agent");

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn upstream(root: &Path) -> (String, String) {
    let work = root.join("upstream-work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("upstream.git");
    git(
        root,
        &["clone", "-q", "--bare", &work.display().to_string(), &bare.display().to_string()],
    );
    (format!("file://{}", bare.display()), git(&work, &["rev-parse", "HEAD"]))
}

struct Dirs {
    shared: std::path::PathBuf,
    scratch: std::path::PathBuf,
    log: std::path::PathBuf,
}

fn dirs(root: &Path) -> Dirs {
    let shared = root.join("shared");
    for d in ["repo", "crew", "fleet", "daemon"] {
        std::fs::create_dir_all(shared.join(d)).unwrap();
    }
    Dirs {
        shared,
        scratch: root.join("scratch"),
        log: root.join("termination-log"),
    }
}

fn run(d: &Dirs, args: &[&str]) -> (Option<i32>, String) {
    let _ = std::fs::remove_file(&d.log);
    let out = Command::new(BIN)
        .args(args)
        .arg("--shared-dir")
        .arg(&d.shared)
        .arg("--scratch-dir")
        .arg(&d.scratch)
        .arg("--termination-log")
        .arg(&d.log)
        .output()
        .unwrap();
    let logged = std::fs::read_to_string(&d.log).unwrap_or_default();
    assert_eq!(logged.lines().count(), 1, "one line: {logged:?} / {out:?}");
    (out.status.code(), logged.trim_end().to_string())
}

#[test]
fn crew_sync_reports_the_commit_and_a_missing_ref_as_a_cache_failure() {
    if support::tools().is_none() {
        return;
    }
    let root = support::temp_root("jobs-cli-crew-sync");
    let (url, sha) = upstream(&root);
    let d = dirs(&root);
    let (code, line) = run(&d, &["crew-sync", "--crew", "f/c", "--repo", &url, "--ref", "main"]);
    assert_eq!((code, line.as_str()), (Some(0), sha.as_str()));
    assert!(d.shared.join("crew/no-hooks").is_dir());
    let (code, line) = run(&d, &["crew-sync", "--crew", "f/c", "--repo", &url, "--ref", "nope"]);
    assert_eq!(
        (code, line.as_str()),
        (Some(1), "cache: f/c: the remote has no branch nope")
    );
}

#[test]
fn pool_sync_says_synced_and_wants_a_fleet_for_the_fleet_level() {
    if support::tools().is_none() {
        return;
    }
    let root = support::temp_root("jobs-cli-pool-sync");
    let d = dirs(&root);
    // an explicit empty table: the embedded one would download claude
    let (code, line) = run(&d, &["pool-sync", "--level", "daemon", "--no-tools"]);
    assert_eq!((code, line.as_str()), (Some(0), "synced"));
    assert!(d.shared.join("daemon/mise").is_dir());
    let (code, line) = run(&d, &["pool-sync", "--level", "fleet", "--fleet", "f"]);
    assert_eq!((code, line.as_str()), (Some(0), "synced"));
    assert!(d.shared.join("fleet/mise.installed").is_file());
    let (code, line) = run(&d, &["pool-sync", "--level", "fleet"]);
    assert_eq!(code, Some(1));
    assert_eq!(line, "pool-sync --level fleet needs --fleet <name>");
}

#[test]
fn a_tool_that_is_not_name_equals_version_is_refused_by_the_parser() {
    let out = Command::new(BIN)
        .args(["crew-sync", "--crew", "f/c", "--repo", "o/r", "--ref", "main", "--tool", "node"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("expected <name>=<version>"), "{stderr}");
}

#[test]
fn harvest_with_no_clone_says_so() {
    if support::tools().is_none() {
        return;
    }
    let root = support::temp_root("jobs-cli-harvest");
    let (url, _) = upstream(&root);
    let d = dirs(&root);
    let (code, _) = run(&d, &["crew-sync", "--crew", "f/c", "--repo", &url, "--ref", "main"]);
    assert_eq!(code, Some(0));
    let claim = root.join("agent");
    std::fs::create_dir_all(&claim).unwrap();
    let (code, line) = run(
        &d,
        &["harvest", "--agent", "f/c/a", "--agent-dir", &claim.display().to_string()],
    );
    assert_eq!((code, line.as_str()), (Some(0), "nothing to harvest"));
    let (code, line) = run(
        &d,
        &["harvest", "--agent", "not-an-id", "--agent-dir", &claim.display().to_string()],
    );
    assert_eq!(code, Some(1));
    assert!(line.starts_with("--agent not-an-id: "), "{line}");
}
```

Run: `scripts/agent.sh check`
Expected: FAIL, unknown subcommand `crew-sync`.

- [ ] **Step 2: `agent/src/cli.rs`**

Add to `Command`:

```rust
    /// The sync Job: the crew cache and the crew pool (Spec O §8.3).
    CrewSync(CrewSyncArgs),
    /// A pool Job: the daemon pool or a fleet's pool (§20.3).
    PoolSync(PoolSyncArgs),
    /// The harvest Job: the agent's branch into the crew cache (§8.4).
    Harvest(HarvestArgs),
```

and below `RunArgs`:

```rust
/// Where a Job finds the slice, its scratch directory and its outcome.
#[derive(Debug, Args)]
pub struct JobDirs {
    /// The crew's slice, mounted read-write where the pod has it read-only.
    #[arg(long, default_value = "/balerix/shared")]
    pub shared_dir: PathBuf,
    /// An emptyDir: the gh config, the git profile, nono's home and logs.
    #[arg(long, default_value = "/balerix/scratch")]
    pub scratch_dir: PathBuf,
    /// One line: the outcome the operator reads.
    #[arg(long, default_value = "/dev/termination-log")]
    pub termination_log: PathBuf,
}

/// `--tool node=22.11.0`: one entry of a tool table.
fn tool(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((name, version)) if !name.is_empty() && !version.is_empty() => {
            Ok((name.to_string(), version.to_string()))
        }
        _ => Err(format!("{s:?}: expected <name>=<version>")),
    }
}

#[derive(Debug, Args)]
pub struct CrewSyncArgs {
    /// `<fleet>/<crew>`.
    #[arg(long)]
    pub crew: String,
    #[arg(long)]
    pub repo: String,
    #[arg(long = "ref")]
    pub git_ref: String,
    /// The GitHub token as a mounted file (`git.auth: gh`), never an argument.
    #[arg(long)]
    pub gh_token_file: Option<PathBuf>,
    /// The crew's own tool table.
    #[arg(long = "tool", value_parser = tool)]
    pub tools: Vec<(String, String)>,
    #[command(flatten)]
    pub dirs: JobDirs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Level {
    Daemon,
    Fleet,
}

#[derive(Debug, Args)]
pub struct PoolSyncArgs {
    #[arg(long, value_enum)]
    pub level: Level,
    /// The fleet's name; required with `--level fleet`.
    #[arg(long)]
    pub fleet: Option<String>,
    /// The table to install. With `--level daemon` and none given, the
    /// system table this binary embeds (`claude`, `gh`).
    #[arg(long = "tool", value_parser = tool)]
    pub tools: Vec<(String, String)>,
    /// Install an empty daemon table rather than the embedded one.
    #[arg(long, conflicts_with = "tools")]
    pub no_tools: bool,
    #[command(flatten)]
    pub dirs: JobDirs,
}

#[derive(Debug, Args)]
pub struct HarvestArgs {
    /// `<fleet>/<crew>/<agent>`.
    #[arg(long)]
    pub agent: String,
    /// The agent claim, mounted read-only.
    #[arg(long, default_value = "/balerix/agent")]
    pub agent_dir: PathBuf,
    #[command(flatten)]
    pub dirs: JobDirs,
}
```

Change `terminate` to take the command's name, and add `finish`:

```rust
/// Ends a command with its reason as the termination message: the first
/// line of `e`, written to `log` (best effort: the path may not exist off
/// a pod) and to stderr. Always exit 1.
pub fn terminate(log: &Path, who: &str, e: &anyhow::Error) -> ExitCode {
    let reason = format!("{e:#}");
    write_line(log, reason.lines().next().unwrap_or(""));
    eprintln!("balerix-agent {who}: {reason}");
    ExitCode::FAILURE
}

/// A Job's end: its one-line outcome on success, `terminate` otherwise.
pub fn finish(log: &Path, who: &str, outcome: anyhow::Result<String>) -> ExitCode {
    match outcome {
        Ok(line) => {
            write_line(log, &line);
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(e) => terminate(log, who, &e),
    }
}

fn write_line(log: &Path, line: &str) {
    if let Err(write) = std::fs::write(log, format!("{line}\n")) {
        tracing::debug!(path = %log.display(), "no termination log: {write}");
    }
}
```

- [ ] **Step 3: Create `agent/src/jobs.rs`**

```rust
//! The Jobs' commands (Spec O §8.3, §8.4, §20.3): thin over
//! `balerix_runtime::jobs`, each returning its one-line outcome.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow};
use balerix_core::{AgentId, CrewRef, FleetName, RepoRef};
use balerix_runtime::jobs::{self, PoolLevel};
use balerix_runtime::layout::SharedSlice;
use balerix_runtime::{ToolPaths, embedded_system_tools};

use crate::cli::{CrewSyncArgs, HarvestArgs, Level, PoolSyncArgs};

/// The tools on PATH. The `balerix` slot is this binary: a Job renders no
/// `launch.sh` and runs no relay, the slot only has to name a file.
fn tools() -> Result<ToolPaths> {
    let exe = std::env::current_exe().context("cannot name this binary")?;
    ToolPaths::discover_in(&std::env::var_os("PATH").unwrap_or_default(), &exe)
        .map_err(|e| anyhow!("{e} (a Job needs git, gh, mise, nono and tmux on PATH)"))
}

pub fn crew_sync(args: &CrewSyncArgs) -> Result<String> {
    let crew: CrewRef = args
        .crew
        .parse()
        .map_err(|e| anyhow!("--crew {}: {e}", args.crew))?;
    let repo = RepoRef::parse(&args.repo).map_err(|e| anyhow!("--repo: {e}"))?;
    let token = match &args.gh_token_file {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("cannot read the token file {}", path.display()))?
                .trim_end_matches('\n')
                .to_string(),
        ),
        None => None,
    };
    let table: BTreeMap<String, String> = args.tools.iter().cloned().collect();
    Ok(jobs::sync_crew(
        &tools()?,
        &SharedSlice::new(&args.dirs.shared_dir),
        &args.dirs.scratch_dir,
        &crew,
        &repo,
        &args.git_ref,
        token.as_deref(),
        &table,
    )?)
}

pub fn pool_sync(args: &PoolSyncArgs) -> Result<String> {
    let given: BTreeMap<String, String> = args.tools.iter().cloned().collect();
    let (level, table) = match args.level {
        Level::Daemon if given.is_empty() && !args.no_tools => {
            (PoolLevel::Daemon, embedded_system_tools())
        }
        Level::Daemon => (PoolLevel::Daemon, given),
        Level::Fleet => {
            let name = args
                .fleet
                .as_deref()
                .context("pool-sync --level fleet needs --fleet <name>")?;
            let fleet: FleetName = name.parse().map_err(|e| anyhow!("--fleet {name}: {e}"))?;
            (PoolLevel::Fleet(fleet), given)
        }
    };
    jobs::sync_pool(
        &tools()?,
        &SharedSlice::new(&args.dirs.shared_dir),
        &args.dirs.scratch_dir,
        &level,
        &table,
    )
    .map_err(|e| anyhow!("tools: {e}"))?;
    Ok("synced".to_string())
}

pub fn harvest(args: &HarvestArgs) -> Result<String> {
    let id: AgentId = args
        .agent
        .parse()
        .map_err(|e| anyhow!("--agent {}: {e}", args.agent))?;
    let branch = jobs::harvest(
        &tools()?,
        &SharedSlice::new(&args.dirs.shared_dir),
        &args.dirs.scratch_dir,
        &id,
        &args.agent_dir,
    )?;
    Ok(match branch {
        Some(b) => format!("harvested {b}"),
        None => "nothing to harvest".to_string(),
    })
}
```

`SyncError` converts into `anyhow::Error` through `std::error::Error`, and `{e:#}` prints its `cache: …` line. The three names' `FromStr::Err` is `balerix_core`'s `NameError`, which is `Display` (`agent/src/bundle.rs` already prints one).

Add `pub mod jobs;` to `agent/src/lib.rs`.

- [ ] **Step 4: `agent/src/main.rs`**

Add the arms, and pass `"sidecar"` to the two existing `terminate` calls:

```rust
        Command::CrewSync(args) => balerix_agent::cli::finish(
            &args.dirs.termination_log,
            "crew-sync",
            balerix_agent::jobs::crew_sync(&args),
        ),
        Command::PoolSync(args) => balerix_agent::cli::finish(
            &args.dirs.termination_log,
            "pool-sync",
            balerix_agent::jobs::pool_sync(&args),
        ),
        Command::Harvest(args) => balerix_agent::cli::finish(
            &args.dirs.termination_log,
            "harvest",
            balerix_agent::jobs::harvest(&args),
        ),
```

- [ ] **Step 5: Run and commit**

Run: `mise run agent`
Expected: PASS, the existing `a_sidecar_without_a_bundle_exits_one_with_the_reason` included (its stderr still starts `balerix-agent sidecar:`).

```bash
git add agent
git commit -m "feat(agent): crew-sync, pool-sync and harvest, each ending in a one-line termination message (Spec O §8.3, §8.4)"
```

---

### Task 5: The `operator/` project, the five kinds and `crds`

**Files:**
- Create: `operator/Cargo.toml`, `operator/deny.toml`, `operator/clippy.toml`
- Create: `operator/src/main.rs`, `operator/src/lib.rs`, `operator/src/api/{mod,common,daemon,fleet,crew,agent,plugin}.rs`
- Create: `operator/tests/crds_it.rs`, `operator/crds/*.yaml` (generated)
- Create: `scripts/operator.sh`
- Modify: `mise.toml`, `.github/workflows/ci.yml`, `.gitignore`

**Interfaces:**
- Produces, in `balerix_operator::api`: the kinds `Daemon`, `Fleet`, `Crew`, `Agent`, `Plugin` with spec structs `DaemonSpec`, `FleetSpec`, `CrewSpec`, `AgentSpec`, `PluginSpec` and status structs `DaemonStatus`, `FleetStatus`, `CrewStatus`, `AgentStatus`, `PluginStatus`; `GROUP`, `VERSION`; `crds() -> Vec<CustomResourceDefinition>`; `crd_files() -> Result<Vec<(String, String)>, serde_norway::Error>` (file name, YAML). Field names are the ones in the code below; later tasks use them exactly.
- These spec names shadow `balerix_api::FleetSpec`, `CrewSpec` and `AgentStatus`. Inside `operator/`, always write the `balerix_api::` ones with their crate path.

- [ ] **Step 1: The project files**

`operator/Cargo.toml`:

```toml
# A standalone project, not a workspace member (Spec H, Spec O-12): kube,
# k8s-openapi and their trees stay out of the core workspace's feature
# resolution. Reaches the core crates by path.
[workspace]
resolver = "3"

[package]
name = "balerix-operator"
description = "The balerix operator: five custom resources reconciled into pods, claims, Secrets and Jobs (Spec O §4, §5)"
# The core version (O-13): scripts/release/prepare.sh writes it with the
# other core manifests once sub-project 5 lands.
version = "0.2.0"
edition = "2024"
rust-version = "1.98"
license = "Apache-2.0"
repository = "https://github.com/balerix-ai/balerix"
publish = false

[lib]
name = "balerix_operator"
path = "src/lib.rs"

[[bin]]
name = "balerix-operator"
path = "src/main.rs"

[dependencies]
balerix-api = { path = "../crates/balerix-api" }
balerix-core = { path = "../crates/balerix-core" }
balerix-config = { path = "../crates/balerix-config" }
# Error context on the entrypoint's failures.
anyhow = "1.0.104"
# The command line: `crds` now, `run` with the controllers (sub-project 3b).
clap = { version = "4.6.6", features = ["derive"] }
# The CustomResource derive and the CRD type only: no client, no runtime
# until the controllers exist, which keeps hyper and tower out for now.
kube = { version = "4.2.0", default-features = false, features = ["derive"] }
# Typed Pods, Jobs, claims and policies. `v1_32` is the oldest API this
# release offers; `schemars` so `Condition` can sit in a status schema.
k8s-openapi = { version = "0.28.0", features = ["v1_32", "schemars"] }
# The kinds' OpenAPI schemas; the version kube 4.2 derives against.
schemars = "1.2.2"
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
# The generated CRD files, in the YAML crate the core already uses.
serde_norway = "0.9.42"
thiserror = "2.0.20"
# specHash and the Jobs' input hash.
sha2 = "0.11.0"
hex = "0.4.3"
# Tokens (agent, admin): 32 random bytes, as the daemon's vault mints them.
rand = "0.10.2"
# The per-Daemon authority and serving certificates (Spec O §10.3).
rcgen = { version = "0.14.10", default-features = false, features = ["pem", "ring"] }
# rcgen's validity dates.
time = "0.3.55"
# The Daemon admin API over TLS from the Daemon's own authority (§10.3):
# rustls with no provider of its own, ring below. The same stack as agent/.
reqwest = { version = "0.13.4", default-features = false, features = ["json", "rustls-no-provider"] }
rustls = { version = "0.23.45", default-features = false, features = ["ring", "std", "tls12", "logging"] }
rustls-pki-types = { version = "1.15.1", features = ["std"] }
# The client's async calls, and their tests.
tokio = { version = "1.53.1", features = ["rt-multi-thread", "macros", "time", "net"] }

[dev-dependencies]
# every `desired` function is pinned by a YAML snapshot (Spec O §15)
insta = { version = "1.48.0", features = ["yaml"] }
# a Fleet's spec resolves as the same content does as a file (§15)
proptest = "1.11.0"
# the stub Daemon the client tests talk to
axum = "0.8.9"
tempfile = "3.27.0"

[lints.rust]
unsafe_code = "forbid"

[lints.clippy]
all = { level = "warn", priority = -1 }
unwrap_used = "warn"
expect_used = "warn"

[profile.release]
strip = true
```

`operator/clippy.toml`: the same two lines as `agent/clippy.toml`. `operator/deny.toml`: a copy of `agent/deny.toml` with its first comment changed to `# The operator project's own policy (Spec H, Spec O-12).` and the `CDLA-Permissive-2.0` comment reworded to name only `webpki-root-certs` through `rustls-platform-verifier`, which reqwest pulls in; run `mise x -- cargo deny --manifest-path operator/Cargo.toml check advisories bans sources licenses` at the end of this task and, if a licence in the tree is not on the list, add it with a comment naming the crate.

Append `operator/target` to `.gitignore`.

`scripts/operator.sh` (mode 0755):

```bash
#!/usr/bin/env bash
# Build, format, lint and test the standalone operator project (Spec O
# §12), the way scripts/agent.sh does the agent: its own cargo workspace,
# lockfile and target directory. The Daemon client's test runs a real
# `balerix serve --mode kubernetes`, so `check` builds `balerix` from the
# core workspace first and hands its path over as BALERIX_BIN; without it
# that test skips (fails under BALERIX_REQUIRE_TOOLS=1).
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"

usage() { echo "usage: $0 {build|fmt|check|crds}" >&2; exit 2; }
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
    CARGO_TARGET_DIR="$target" cargo run -q --manifest-path "$dir/Cargo.toml" -- crds --out "$dir/crds"
    ;;
  check)
    (cd "$repo" && cargo build -q -p balerix)
    export BALERIX_BIN="${CARGO_TARGET_DIR:-$repo/target}/debug/balerix"
    CARGO_TARGET_DIR="$target" cargo fmt --manifest-path "$dir/Cargo.toml" --all --check
    CARGO_TARGET_DIR="$target" cargo clippy --manifest-path "$dir/Cargo.toml" --all-targets -- -D warnings
    CARGO_TARGET_DIR="$target" cargo nextest run \
      --config-file "$repo/.config/nextest.toml" \
      --manifest-path "$dir/Cargo.toml"
    ;;
  *)
    usage
    ;;
esac
```

`mise.toml`: after `[tasks.agent]` add

```toml
[tasks.operator]
description = "Lint and test the standalone operator project (balerix-operator, Spec O §12): builds balerix first for its client test against a Kubernetes-mode daemon; fails when operator/crds differs from the Rust types. Its own tier; not part of `check`"
run = "scripts/operator.sh check"

[tasks.crds]
description = "Regenerate operator/crds/ from the Rust types (Spec O §14.1)"
run = "scripts/operator.sh crds"
```

add `"scripts/operator.sh fmt",` as the last entry of `[tasks.fmt]`'s `run` and change its description to end `… every standalone plugin project, the agent project and the operator project`; add to `[tasks.audit]`'s `run`:

```toml
  "cargo audit --file operator/Cargo.lock",
  "cargo deny --manifest-path operator/Cargo.toml check advisories bans sources licenses",
```

`.github/workflows/ci.yml`: after the `agent` job, a copy of it named `operator` with the comment `# The operator project (Spec O §12): standalone like the agent, and like it runs the core balerix binary in one test.`, `workspaces` `. -> target` and `operator -> target`, `key: operator`, and `- run: mise run operator`. Keep the same `mise install rust cargo:cargo-nextest tmux nono gh` line: `balerix serve` discovers those tools at start.

- [ ] **Step 2: Write the failing test**

`operator/tests/crds_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §4, §13, §14.1: five namespaced kinds, printed by `crds`, and
//! the committed files are what the types generate.
use std::process::Command;

use balerix_operator::api;

const BIN: &str = env!("CARGO_BIN_EXE_balerix-operator");

#[test]
fn version_prints_the_core_version() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("balerix-operator {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn crds_prints_five_documents() {
    let out = Command::new(BIN).arg("crds").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let docs: Vec<serde_json::Value> = text
        .split("\n---\n")
        .map(|d| serde_norway::from_str(d).unwrap())
        .collect();
    let kinds: Vec<&str> = docs
        .iter()
        .map(|d| d["spec"]["names"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["Daemon", "Fleet", "Crew", "Agent", "Plugin"]);
    for d in &docs {
        assert_eq!(d["kind"], "CustomResourceDefinition");
        assert_eq!(d["spec"]["group"], "balerix.ai");
        assert_eq!(d["spec"]["scope"], "Namespaced");
        let v = &d["spec"]["versions"][0];
        assert_eq!(v["name"], "v1alpha1");
        assert!(v["subresources"]["status"].is_object(), "a status subresource");
        let status = &v["schema"]["openAPIV3Schema"]["properties"]["status"]["properties"];
        assert!(status["conditions"].is_object());
        assert!(status["observedGeneration"].is_object());
    }
}

#[test]
fn the_settings_layers_are_open_objects_and_an_agent_prints_its_phase() {
    let fleet = serde_json::to_value(&api::crds()[1]).unwrap();
    let spec = &fleet["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
        ["properties"];
    assert_eq!(spec["defaults"]["x-kubernetes-preserve-unknown-fields"], true);
    let crew = &spec["crews"]["additionalProperties"]["properties"];
    assert_eq!(crew["defaults"]["x-kubernetes-preserve-unknown-fields"], true);
    assert_eq!(crew["agents"]["x-kubernetes-preserve-unknown-fields"], true);
    assert_eq!(spec["retain"]["enum"], serde_json::json!(["None", "Branches"]));
    let agent = serde_json::to_value(&api::crds()[3]).unwrap();
    let columns: Vec<&str> = agent["spec"]["versions"][0]["additionalPrinterColumns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(columns, ["Phase", "Restarts", "Age"]);
}

/// `mise run crds` regenerates them; this is the check that fails when a
/// type changed and the files did not (§14.1).
#[test]
fn the_committed_files_are_what_the_types_generate() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crds");
    let files = api::crd_files().unwrap();
    assert_eq!(files.len(), 5);
    for (name, yaml) in &files {
        let committed = std::fs::read_to_string(dir.join(name))
            .unwrap_or_else(|e| panic!("{name}: {e}; run `mise run crds`"));
        assert_eq!(&committed, yaml, "{name} is stale; run `mise run crds`");
    }
    let on_disk = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(on_disk, 5, "a file in operator/crds no type generates");
}
```

- [ ] **Step 3: The kinds**

`operator/src/lib.rs`:

```rust
//! The balerix operator (Spec O §4, §5). `api` is the five kinds;
//! `desired` the pure functions from observed objects to the objects the
//! operator applies; `pki` the per-Daemon authority; `daemon_client` the
//! Daemon's admin API. The controllers that tie them to a cluster are
//! sub-project 3b.

pub mod api;
```

`operator/src/api/mod.rs`:

```rust
//! Group `balerix.ai`, version `v1alpha1`: all five kinds namespaced,
//! every status with `observedGeneration` and `conditions`, the operator
//! their only writer (Spec O §4).

pub mod agent;
pub mod common;
pub mod crew;
pub mod daemon;
pub mod fleet;
pub mod plugin;

pub use agent::{Agent, AgentSpec, AgentStatus};
pub use common::{ClaimSpec, SecretKeyRef, SecretRef};
pub use crew::{Crew, CrewSpec, CrewStatus};
pub use daemon::{Credentials, Daemon, DaemonSpec, DaemonStatus, DaemonStorage};
pub use fleet::{Fleet, FleetCrew, FleetSpec, FleetStatus, Retain};
pub use plugin::{Plugin, PluginSpec, PluginStatus};

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::CustomResourceExt;

pub const GROUP: &str = "balerix.ai";
pub const VERSION: &str = "v1alpha1";

/// In the order `crds` prints them: what a Fleet needs comes first.
pub fn crds() -> Vec<CustomResourceDefinition> {
    vec![
        Daemon::crd(),
        Fleet::crd(),
        Crew::crd(),
        Agent::crd(),
        Plugin::crd(),
    ]
}

/// `<plural>.balerix.ai.yaml` and its content, one per kind.
pub fn crd_files() -> Result<Vec<(String, String)>, serde_norway::Error> {
    crds()
        .iter()
        .map(|crd| {
            let name = crd.metadata.name.clone().unwrap_or_default();
            Ok((format!("{name}.yaml"), serde_norway::to_string(crd)?))
        })
        .collect()
}
```

`operator/src/api/common.rs`:

```rust
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A settings layer, validated by `balerix-config` exactly as the fleet
/// file's is, not by the schema (Spec O §4.2). Also used for maps of
/// such layers and for the Kubernetes shapes kept opaque.
pub fn open_object(_: &mut SchemaGenerator) -> Schema {
    json_schema!({ "type": "object", "x-kubernetes-preserve-unknown-fields": true })
}

pub fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// One claim's class and size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
    pub size: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretRef {
    pub secret_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeyRef {
    pub secret_name: String,
    pub key: String,
}
```

`operator/src/api/daemon.rs`:

```rust
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::common::{ClaimSpec, SecretRef, empty_object, open_object};

/// A `balerix serve` instance in Kubernetes mode and what it shares with
/// its agents (Spec O §4.1).
#[derive(CustomResource, Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "balerix.ai",
    version = "v1alpha1",
    kind = "Daemon",
    namespaced,
    status = "DaemonStatus",
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Endpoint","type":"string","jsonPath":".status.endpoint"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSpec {
    /// The daemon and agent images' version; must equal the operator's
    /// in `v1alpha1`, and is the operator's when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub storage: DaemonStorage,
    #[serde(default)]
    pub credentials: Credentials,
    /// The merge layer the host's `settings.json` is on one machine.
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub defaults: Value,
    /// Plugin names; list order is interceptor order.
    #[serde(default)]
    pub plugins: Vec<String>,
    /// The daemon container's `ResourceRequirements`.
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub resources: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStorage {
    /// Plugin KV, vault, records.
    pub state: ClaimSpec,
    /// ReadWriteMany: crew caches and tool pools.
    pub shared: ClaimSpec,
    /// The default per-agent claim.
    pub agent: ClaimSpec,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    /// Key `credentials.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<SecretRef>,
    /// Key `token`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<SecretRef>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// The Daemon's in-cluster URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}
```

`operator/src/api/fleet.rs`:

```rust
use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::common::{empty_object, open_object};

/// The fleet file, with `name` as `metadata.name`, `daemon` naming the
/// Daemon and `runner` a pod (Spec O §4.2).
#[derive(CustomResource, Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "balerix.ai",
    version = "v1alpha1",
    kind = "Fleet",
    namespaced,
    status = "FleetStatus",
    printcolumn = r#"{"name":"Daemon","type":"string","jsonPath":".spec.daemon"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct FleetSpec {
    pub daemon: String,
    #[serde(default)]
    pub retain: Retain,
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub defaults: Value,
    #[serde(default)]
    pub crews: BTreeMap<String, FleetCrew>,
}

/// `Branches` is `down --keep-repos`, `None` is plain `down` (O-16).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Retain {
    #[default]
    None,
    Branches,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FleetCrew {
    pub repo: String,
    /// `balerix-config`'s default when absent.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "open_object")]
    pub git: Option<Value>,
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub defaults: Value,
    /// Agent name to its settings layer.
    #[serde(default)]
    #[schemars(schema_with = "open_object")]
    pub agents: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FleetStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}
```

`operator/src/api/crew.rs`:

```rust
use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::common::open_object;

/// Generated: one crew of a Fleet, resolved (Spec O §4.3). A hand edit
/// is reverted on the next reconcile.
#[derive(CustomResource, Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "balerix.ai",
    version = "v1alpha1",
    kind = "Crew",
    namespaced,
    status = "CrewStatus",
    printcolumn = r#"{"name":"Cache","type":"string","jsonPath":".status.conditions[?(@.type==\"CacheReady\")].status"}"#,
    printcolumn = r#"{"name":"Tools","type":"string","jsonPath":".status.conditions[?(@.type==\"ToolsReady\")].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct CrewSpec {
    pub daemon: String,
    pub fleet: String,
    pub crew: String,
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// The crew's `GitSettings`.
    #[schemars(schema_with = "open_object")]
    pub git: Value,
    /// The fleet's `defaults.tools`: the fleet pool's table.
    #[serde(default)]
    pub fleet_tools: BTreeMap<String, String>,
    /// This crew's own table.
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CrewStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// The commit last fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ref: Option<String>,
}
```

`operator/src/api/agent.rs`:

```rust
use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::common::open_object;

/// Generated: one agent of a Fleet, resolved (Spec O §4.4).
#[derive(CustomResource, Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "balerix.ai",
    version = "v1alpha1",
    kind = "Agent",
    namespaced,
    status = "AgentStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Restarts","type":"integer","jsonPath":".status.restarts"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct AgentSpec {
    pub daemon: String,
    pub fleet: String,
    pub crew: String,
    pub agent: String,
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// The crew's `GitSettings`.
    #[schemars(schema_with = "open_object")]
    pub git: Value,
    /// The resolved `AgentSettings`.
    #[schemars(schema_with = "open_object")]
    pub settings: Value,
    /// Over everything the pod is made from; a pod annotated with another
    /// value is replaced (§5.4).
    pub spec_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// The Daemon's phase for the agent, as `balerix status` prints it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarts: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Plugin name to activation state.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, String>,
}
```

`operator/src/api/plugin.rs`:

```rust
use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::common::{SecretKeyRef, empty_object, open_object};

/// A plugin a Daemon in its namespace may list (Spec O §4.5). Defined
/// here so the five kinds are reviewed once; its controller is
/// sub-project 4, and until then a Daemon that lists any plugin is
/// `PluginsReady=False`, reason `PluginsUnsupported` (§20.2).
#[derive(CustomResource, Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "balerix.ai",
    version = "v1alpha1",
    kind = "Plugin",
    namespaced,
    status = "PluginStatus",
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct PluginSpec {
    pub image: String,
    /// The grant.
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub config: Value,
    /// Config key to the Secret key injected there.
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretKeyRef>,
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub fleet_defaults: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose: Option<Expose>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch: Option<Scratch>,
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub resources: Value,
}

/// A Service for the plugin's own listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Expose {
    pub port: i32,
}

/// A claim for the plugin's scratch directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Scratch {
    pub size: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}
```

`operator/src/main.rs`:

```rust
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "balerix-operator",
    version,
    about = "The balerix operator: Daemon, Fleet, Crew, Agent and Plugin objects into pods, claims, Secrets and Jobs (Spec O §5)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the five CustomResourceDefinitions, or write one file each.
    Crds {
        /// A directory for `<plural>.balerix.ai.yaml`; stdout when absent.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

fn crds(out: Option<PathBuf>) -> Result<()> {
    let files = balerix_operator::api::crd_files()?;
    match out {
        None => {
            let docs: Vec<&str> = files.iter().map(|(_, yaml)| yaml.trim_end()).collect();
            println!("{}", docs.join("\n---\n"));
        }
        Some(dir) => {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
            for (name, yaml) in &files {
                let path = dir.join(name);
                std::fs::write(&path, yaml)
                    .with_context(|| format!("cannot write {}", path.display()))?;
            }
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Crds { out } => crds(out),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("balerix-operator: {e:#}");
            ExitCode::FAILURE
        }
    }
}
```

- [ ] **Step 4: Generate, read, test**

Run: `mise run crds`, then read all five files under `operator/crds/`. Check in each: `scope: Namespaced`; `subresources: status: {}`; in `fleets.balerix.ai.yaml`, `defaults` and each crew's `defaults` and `agents` are `type: object` with `x-kubernetes-preserve-unknown-fields: true` and `daemon` is the one required field of `spec`; in `daemons.balerix.ai.yaml`, `storage` is required with `state`, `shared`, `agent` each requiring `size`. If kube rejects a schema as non-structural, the field is a `Value` without `schema_with = "open_object"`.

Run: `mise run operator`
Expected: PASS (four tests).

- [ ] **Step 5: Commit**

```bash
git add operator scripts/operator.sh .github/workflows/ci.yml .gitignore
git add -p mise.toml   # the two tasks, the fmt and audit entries; NOT the cargo-insta hunk
git commit -m "feat(operator): the operator project, its five kinds and the crds command (Spec O §4, §20.2)"
```

---

### Task 6: Names, shared builders, conditions, and the authority

**Files:**
- Create: `operator/src/desired/{mod,names,common}.rs`, `operator/src/pki.rs`
- Modify: `operator/src/lib.rs`

**Interfaces:**
- Produces, in `balerix_operator::desired::names`: every object name and volume path as a function (the list is the code below).
- Produces, in `desired::common`: `MANAGER`, `UID`, `DAEMON_PORT`, `HASH_ANNOTATION`, `DesiredError`, `typed<T>(Value) -> Result<T, DesiredError>`, `labels(daemon, component, extra) -> Value`, `pod_security() -> Value`, `container_security() -> Value`, `owner_of<K>(&K) -> Result<Value, DesiredError>`, `hash(&impl Serialize) -> String`, `OperatorConfig { version, images, namespace }`, `Images { daemon, agent }`, `Cond`, `conditions(old, new, generation, now) -> Vec<Condition>`, `JobOutcome`.
- Produces, in `balerix_operator::pki`: `Issued { cert_pem, key_pem, not_after }`, `PkiError`, `new_authority(namespace, daemon, now) -> Result<Issued, PkiError>`, `issue_serving(authority, namespace, daemon, names, now) -> Result<Issued, PkiError>`, `needs_renewal(not_after, now) -> bool`, `daemon_names(namespace, daemon) -> Vec<String>`, `new_token() -> String`. Times are unix seconds (`i64`).

- [ ] **Step 1: Write the failing tests**

At the bottom of `operator/src/pki.rs` (create the file with only this module first):

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::sync::Arc;

    use rustls::client::danger::ServerCertVerifier;
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, ServerName, UnixTime};

    use super::*;

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    fn verify(authority: &Issued, leaf: &Issued, name: &str, at: i64) -> Result<(), String> {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(authority.cert_pem.as_bytes()).unwrap())
            .unwrap();
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap();
        verifier
            .verify_server_cert(
                &CertificateDer::from_pem_slice(leaf.cert_pem.as_bytes()).unwrap(),
                &[],
                &ServerName::try_from(name.to_string()).unwrap(),
                &[],
                UnixTime::since_unix_epoch(std::time::Duration::from_secs(at as u64)),
            )
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    }

    #[test]
    fn a_serving_certificate_verifies_for_the_daemons_names_until_it_expires() {
        let ca = new_authority("team-a", "default", NOW).unwrap();
        let names = daemon_names("team-a", "default");
        assert_eq!(
            names,
            [
                "balerix-default",
                "balerix-default.team-a",
                "balerix-default.team-a.svc",
                "balerix-default.team-a.svc.cluster.local"
            ]
        );
        let leaf = issue_serving(&ca, "team-a", "default", &names, NOW).unwrap();
        assert_eq!(ca.not_after, NOW + 3650 * DAY);
        assert_eq!(leaf.not_after, NOW + 90 * DAY);
        for name in &names {
            verify(&ca, &leaf, name, NOW + DAY).unwrap();
        }
        // a clock five minutes behind the operator's still accepts it
        verify(&ca, &leaf, &names[2], NOW - 299).unwrap();
        let wrong = verify(&ca, &leaf, "other.team-a.svc", NOW + DAY).unwrap_err();
        assert!(wrong.contains("NotValidForName"), "{wrong}");
        let late = verify(&ca, &leaf, &names[2], NOW + 91 * DAY).unwrap_err();
        assert!(late.contains("Expired"), "{late}");
    }

    /// The controller keeps only the authority's PEM pair in a Secret: a
    /// certificate issued from that pair read back, on a later reconcile,
    /// must chain to the same authority.
    #[test]
    fn an_authority_read_back_from_its_secret_issues_the_same_chain() {
        let ca = new_authority("team-a", "default", NOW).unwrap();
        let read_back = Issued {
            cert_pem: ca.cert_pem.clone(),
            key_pem: ca.key_pem.clone(),
            not_after: ca.not_after,
        };
        let names = vec!["127.0.0.1".to_string()];
        let leaf = issue_serving(&read_back, "team-a", "default", &names, NOW + 60 * DAY).unwrap();
        verify(&ca, &leaf, "127.0.0.1", NOW + 61 * DAY).unwrap();
        let other = new_authority("team-a", "default", NOW).unwrap();
        assert!(verify(&other, &leaf, "127.0.0.1", NOW + 61 * DAY).is_err());
    }

    #[test]
    fn renewal_starts_thirty_days_before_expiry() {
        let not_after = NOW + 90 * DAY;
        assert!(!needs_renewal(not_after, NOW));
        assert!(!needs_renewal(not_after, NOW + 60 * DAY - 1));
        assert!(needs_renewal(not_after, NOW + 60 * DAY));
        assert!(needs_renewal(not_after, NOW + 200 * DAY));
    }

    #[test]
    fn a_key_and_a_token_are_never_printed() {
        let ca = new_authority("team-a", "default", NOW).unwrap();
        let shown = format!("{ca:?}");
        assert!(shown.contains("<redacted>"), "{shown}");
        assert!(!shown.contains("PRIVATE KEY"), "{shown}");
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
```

At the bottom of `operator/src/desired/common.rs`:

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use k8s_openapi::api::core::v1::ResourceRequirements;
    use serde_json::json;

    use super::*;

    fn at(secs: i64) -> Time {
        Time(k8s_openapi::jiff::Timestamp::from_second(secs).unwrap())
    }

    #[test]
    fn a_condition_keeps_its_transition_time_while_its_status_holds() {
        let first = conditions(&[], &[Cond::no("Ready", "ClaimPending", "x")], Some(1), &at(100));
        assert_eq!(first[0].status, "False");
        assert_eq!(first[0].last_transition_time, at(100));
        let same = conditions(&first, &[Cond::no("Ready", "PoolSyncRunning", "y")], Some(2), &at(200));
        assert_eq!(same[0].last_transition_time, at(100), "still False");
        assert_eq!(same[0].reason, "PoolSyncRunning");
        assert_eq!(same[0].observed_generation, Some(2));
        let flipped = conditions(&same, &[Cond::yes("Ready", "Ready", "")], Some(2), &at(300));
        assert_eq!(flipped[0].status, "True");
        assert_eq!(flipped[0].last_transition_time, at(300));
        let unknown = conditions(&flipped, &[Cond::unknown("Ready", "DaemonUnavailable", "z")], Some(2), &at(400));
        assert_eq!(unknown[0].status, "Unknown");
    }

    #[test]
    fn typed_names_a_wrong_shape_and_hash_is_stable() {
        let ok: ResourceRequirements =
            typed(json!({ "requests": { "cpu": "1" } })).unwrap();
        assert!(ok.requests.is_some());
        let e = typed::<ResourceRequirements>(json!({ "requests": 3 })).unwrap_err();
        assert!(e.to_string().contains("invalid type"), "{e}");
        assert_eq!(hash(&json!({ "a": 1, "b": 2 })), hash(&json!({ "b": 2, "a": 1 })));
        assert_ne!(hash(&json!({ "a": 1 })), hash(&json!({ "a": 2 })));
        assert_eq!(hash(&json!({})).len(), 64);
    }

    #[test]
    fn names_are_the_documented_ones() {
        use crate::desired::names;
        assert_eq!(names::daemon("default"), "balerix-default");
        assert_eq!(names::state_claim("default"), "balerix-default-state");
        assert_eq!(names::shared_claim("default"), "balerix-default-shared");
        assert_eq!(names::authority("default"), "balerix-default-ca");
        assert_eq!(names::serving("default"), "balerix-default-tls");
        assert_eq!(names::admin("default"), "balerix-default-admin");
        assert_eq!(names::daemon_pool_job("default"), "balerix-default-pool");
        assert_eq!(names::endpoint("team-a", "default"), "https://balerix-default.team-a.svc:7643");
        assert_eq!(names::fleet_pool_job("payments"), "payments-pool");
        assert_eq!(names::crew("payments", "backend"), "payments-backend");
        assert_eq!(names::sync_job("payments", "backend"), "payments-backend-sync");
        assert_eq!(names::agent("payments", "backend", "alice"), "payments-backend-alice");
        assert_eq!(names::bundle("payments-backend-alice"), "payments-backend-alice-bundle");
        assert_eq!(names::token("payments-backend-alice"), "payments-backend-alice-token");
        assert_eq!(names::harvest_job("payments-backend-alice"), "payments-backend-alice-harvest");
        assert_eq!(names::vol_daemon_pool(), "pools/daemon");
        assert_eq!(names::vol_fleet_pool("payments"), "fleets/payments/pool");
        assert_eq!(names::vol_crew_pool("payments", "backend"), "fleets/payments/crews/backend/pool");
        assert_eq!(names::vol_crew_repo("payments", "backend"), "fleets/payments/crews/backend/repo");
    }
}
```

Run: `scripts/operator.sh check`
Expected: compile errors.

- [ ] **Step 2: `operator/src/desired/names.rs`**

```rust
//! Every name the operator gives an object, and every directory on the
//! shared volume (Spec O §8.1). One place, so a controller that looks an
//! object up and the function that made it cannot disagree.

use super::common::DAEMON_PORT;

pub fn daemon(daemon: &str) -> String {
    format!("balerix-{daemon}")
}
pub fn state_claim(daemon: &str) -> String {
    format!("balerix-{daemon}-state")
}
pub fn shared_claim(daemon: &str) -> String {
    format!("balerix-{daemon}-shared")
}
/// The authority's Secret (`ca.crt`, `ca.key`) and its ConfigMap (`ca.crt`).
pub fn authority(daemon: &str) -> String {
    format!("balerix-{daemon}-ca")
}
pub fn serving(daemon: &str) -> String {
    format!("balerix-{daemon}-tls")
}
pub fn admin(daemon: &str) -> String {
    format!("balerix-{daemon}-admin")
}
pub fn daemon_pool_job(daemon: &str) -> String {
    format!("balerix-{daemon}-pool")
}
/// `status.endpoint`, and the `daemon_url` of every agent bundle.
pub fn endpoint(namespace: &str, daemon_name: &str) -> String {
    format!("https://{}.{namespace}.svc:{DAEMON_PORT}", daemon(daemon_name))
}
pub fn fleet_pool_job(fleet: &str) -> String {
    format!("{fleet}-pool")
}
pub fn crew(fleet: &str, crew: &str) -> String {
    format!("{fleet}-{crew}")
}
pub fn sync_job(fleet: &str, crew: &str) -> String {
    format!("{fleet}-{crew}-sync")
}
/// The Agent object, its claim, its pod and its NetworkPolicy.
pub fn agent(fleet: &str, crew: &str, agent: &str) -> String {
    format!("{fleet}-{crew}-{agent}")
}
pub fn bundle(agent: &str) -> String {
    format!("{agent}-bundle")
}
pub fn token(agent: &str) -> String {
    format!("{agent}-token")
}
pub fn harvest_job(agent: &str) -> String {
    format!("{agent}-harvest")
}

pub fn vol_daemon_pool() -> String {
    "pools/daemon".to_string()
}
pub fn vol_fleet_pool(fleet: &str) -> String {
    format!("fleets/{fleet}/pool")
}
pub fn vol_crew_pool(fleet: &str, crew: &str) -> String {
    format!("fleets/{fleet}/crews/{crew}/pool")
}
pub fn vol_crew_repo(fleet: &str, crew: &str) -> String {
    format!("fleets/{fleet}/crews/{crew}/repo")
}
```

- [ ] **Step 3: `operator/src/desired/common.rs`** (above its tests)

```rust
//! What every `desired` function shares: the hardened pod settings, the
//! labels, the conditions, and `typed`, which turns a manifest written as
//! JSON into the `k8s-openapi` type the function returns.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::Resource;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The server-side-apply field manager (Spec O §5).
pub const MANAGER: &str = "balerix-operator";
/// Every container's uid and gid, and the pods' `fsGroup` (§6.1).
pub const UID: i64 = 10001;
/// The Daemon's port, as the sidecar's hook port is on one machine.
pub const DAEMON_PORT: i32 = 7643;
/// On a Job: the hash of what it was made from. A Job whose hash is not
/// the wanted one is stale and is replaced.
pub const HASH_ANNOTATION: &str = "balerix.ai/input-hash";

#[derive(Debug, thiserror::Error)]
pub enum DesiredError {
    /// A manifest did not fit its type, or a user-supplied shape (a
    /// `resources` block, a toleration) is not what Kubernetes takes.
    #[error("{0}")]
    Shape(#[from] serde_json::Error),
    /// An object read from the cluster lacks what the API server always
    /// sets, or a Daemon lacks what a Fleet needs of it.
    #[error("{0} has no {1}")]
    Missing(&'static str, &'static str),
}

pub fn typed<T: DeserializeOwned>(manifest: Value) -> Result<T, DesiredError> {
    Ok(serde_json::from_value(manifest)?)
}

/// The operator's own settings: what it is, and the images it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorConfig {
    /// `CARGO_PKG_VERSION`: the one version a Daemon may ask for (§4.1).
    pub version: String,
    pub images: Images,
    /// The operator's own namespace, for the Daemon's NetworkPolicy.
    pub namespace: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Images {
    pub daemon: String,
    pub agent: String,
}

impl Images {
    pub fn for_version(version: &str) -> Self {
        Self {
            daemon: format!("ghcr.io/balerix-ai/balerix:{version}"),
            agent: format!("ghcr.io/balerix-ai/balerix-agent:{version}"),
        }
    }
}

/// `component` is `daemon`, `agent`, `pool`, `sync` or `harvest`.
pub fn labels(daemon: &str, component: &str, extra: &[(&str, &str)]) -> Value {
    let mut all = json!({
        "app.kubernetes.io/managed-by": MANAGER,
        "balerix.ai/daemon": daemon,
        "balerix.ai/component": component,
    });
    for (k, v) in extra {
        all[*k] = json!(v);
    }
    all
}

pub fn pod_security() -> Value {
    json!({
        "runAsUser": UID,
        "runAsGroup": UID,
        "fsGroup": UID,
        "runAsNonRoot": true,
        "seccompProfile": { "type": "RuntimeDefault" },
    })
}

pub fn container_security() -> Value {
    json!({
        "allowPrivilegeEscalation": false,
        "readOnlyRootFilesystem": true,
        "runAsNonRoot": true,
        "capabilities": { "drop": ["ALL"] },
        "seccompProfile": { "type": "RuntimeDefault" },
    })
}

/// The controller owner reference to `object`, as manifest JSON.
pub fn owner_of<K: Resource<DynamicType = ()>>(object: &K) -> Result<Value, DesiredError> {
    let reference = object
        .controller_owner_ref(&())
        .ok_or(DesiredError::Missing("the owner", "metadata.name and uid"))?;
    Ok(serde_json::to_value(reference)?)
}

/// sha256 over the value's JSON. `serde_json` keeps object keys sorted
/// (no `preserve_order` here), so equal values hash equal.
pub fn hash(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(&serde_json::to_value(value).unwrap_or(Value::Null))
        .unwrap_or_default();
    hex::encode(Sha256::digest(&bytes))
}

/// One condition as a function decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cond {
    pub type_: &'static str,
    /// `None` is `Unknown`.
    pub status: Option<bool>,
    pub reason: String,
    pub message: String,
}

impl Cond {
    pub fn yes(type_: &'static str, reason: &str, message: &str) -> Self {
        Self::new(type_, Some(true), reason, message)
    }
    pub fn no(type_: &'static str, reason: &str, message: &str) -> Self {
        Self::new(type_, Some(false), reason, message)
    }
    pub fn unknown(type_: &'static str, reason: &str, message: &str) -> Self {
        Self::new(type_, None, reason, message)
    }
    fn new(type_: &'static str, status: Option<bool>, reason: &str, message: &str) -> Self {
        Self {
            type_,
            status,
            reason: reason.to_string(),
            message: message.to_string(),
        }
    }
}

/// `new` as status conditions. A condition whose status is what `old`
/// had keeps its transition time; `now` is passed in, so this is pure.
pub fn conditions(
    old: &[Condition],
    new: &[Cond],
    generation: Option<i64>,
    now: &Time,
) -> Vec<Condition> {
    new.iter()
        .map(|c| {
            let status = match c.status {
                Some(true) => "True",
                Some(false) => "False",
                None => "Unknown",
            };
            let since = old
                .iter()
                .find(|o| o.type_ == c.type_ && o.status == status)
                .map(|o| o.last_transition_time.clone())
                .unwrap_or_else(|| now.clone());
            Condition {
                type_: c.type_.to_string(),
                status: status.to_string(),
                reason: c.reason.clone(),
                message: c.message.clone(),
                observed_generation: generation,
                last_transition_time: since,
            }
        })
        .collect()
}

/// What a Job came to, read from the Job and its pods (`jobs::job_outcome`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// No Job by that name.
    Absent,
    /// A Job made from other input: delete it and make the wanted one.
    Stale,
    Running,
    /// With the container's termination message.
    Succeeded(String),
    Failed(String),
}
```

`operator/src/desired/mod.rs`:

```rust
//! Pure: observed objects in, the objects the operator applies and the
//! status it writes out (Spec O §5). Nothing here reads a clock, makes a
//! random value or talks to anything; the controllers (3b) do that and
//! pass the results in.

pub mod common;
pub mod names;
```

`operator/src/lib.rs` gains `pub mod desired;` and `pub mod pki;`.

- [ ] **Step 4: `operator/src/pki.rs`** (above its tests)

```rust
//! One certificate authority per Daemon, and the serving certificates it
//! signs (Spec O §10.3). The controller keeps each pair in a Secret with
//! its expiry as an annotation, so nothing here or there parses X.509.
//! Times are unix seconds, given by the caller.

use rand::Rng;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use time::OffsetDateTime;

pub const AUTHORITY_DAYS: i64 = 3650;
pub const SERVING_DAYS: i64 = 90;
pub const RENEW_BEFORE_DAYS: i64 = 30;
/// A certificate is valid from this long before it is made: a pod whose
/// clock runs a little behind the operator's must still accept it.
const SKEW_SECS: i64 = 300;
const DAY_SECS: i64 = 86_400;

/// A certificate and its private key, PEM, and when it expires.
#[derive(Clone, PartialEq, Eq)]
pub struct Issued {
    pub cert_pem: String,
    pub key_pem: String,
    pub not_after: i64,
}

impl std::fmt::Debug for Issued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issued")
            .field("key_pem", &"<redacted>")
            .field("not_after", &self.not_after)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PkiError {
    #[error("certificate: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("certificate: the time {0} is out of range")]
    Time(i64),
}

fn at(unix: i64) -> Result<OffsetDateTime, PkiError> {
    OffsetDateTime::from_unix_timestamp(unix).map_err(|_| PkiError::Time(unix))
}

/// The authority's parameters are a function of the Daemon alone, so an
/// authority read back from its Secret signs with the same name and key
/// identifier as the one that was made.
fn authority_params(namespace: &str, daemon: &str) -> Result<CertificateParams, PkiError> {
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params
        .distinguished_name
        .push(DnType::CommonName, format!("balerix daemon {namespace}/{daemon}"));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    Ok(params)
}

pub fn new_authority(namespace: &str, daemon: &str, now: i64) -> Result<Issued, PkiError> {
    let mut params = authority_params(namespace, daemon)?;
    let not_after = now + AUTHORITY_DAYS * DAY_SECS;
    params.not_before = at(now - SKEW_SECS)?;
    params.not_after = at(not_after)?;
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok(Issued {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        not_after,
    })
}

/// A serving certificate for `names` (DNS names, or IP addresses as
/// text), signed by the Daemon's authority.
pub fn issue_serving(
    authority: &Issued,
    namespace: &str,
    daemon: &str,
    names: &[String],
    now: i64,
) -> Result<Issued, PkiError> {
    let issuer = Issuer::new(
        authority_params(namespace, daemon)?,
        KeyPair::from_pem(&authority.key_pem)?,
    );
    let mut params = CertificateParams::new(names.to_vec())?;
    params.distinguished_name.push(
        DnType::CommonName,
        names.first().cloned().unwrap_or_default(),
    );
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let not_after = now + SERVING_DAYS * DAY_SECS;
    params.not_before = at(now - SKEW_SECS)?;
    params.not_after = at(not_after)?;
    let key = KeyPair::generate()?;
    let cert = params.signed_by(&key, &issuer)?;
    Ok(Issued {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        not_after,
    })
}

pub fn needs_renewal(not_after: i64, now: i64) -> bool {
    now >= not_after - RENEW_BEFORE_DAYS * DAY_SECS
}

/// The names a sidecar, a plugin or the operator may call the Daemon by.
pub fn daemon_names(namespace: &str, daemon: &str) -> Vec<String> {
    let service = crate::desired::names::daemon(daemon);
    vec![
        service.clone(),
        format!("{service}.{namespace}"),
        format!("{service}.{namespace}.svc"),
        format!("{service}.{namespace}.svc.cluster.local"),
    ]
}

/// 32 random bytes as hex: an agent's token (the Daemon wants at least
/// 32 characters, §7.4) or a Daemon's admin token.
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}
```

- [ ] **Step 5: Run and commit**

Run: `mise run operator`
Expected: PASS.

```bash
git add operator
git commit -m "feat(operator): names, hardened-pod builders, conditions, and the per-Daemon authority (Spec O §10.3, §20.2)"
```

---

### Task 7: A Fleet into its resolved spec, its Crews, its Agents and its `PUT`

**Files:**
- Create: `operator/src/desired/fleet.rs`, `operator/src/desired/snapshots/` (insta writes it)
- Modify: `operator/src/desired/mod.rs`

**Interfaces:**
- Consumes: `api::{Fleet, Daemon, Crew, CrewSpec, Agent, AgentSpec}`, `names`, `common::{labels, owner_of, hash, Cond, Images, DesiredError}`.
- Produces:
  - `resolve_fleet(fleet: &Fleet, daemon_defaults: &Value) -> Result<balerix_api::FleetSpec, ConfigError>`
  - `struct FleetPlan { pub spec: balerix_api::FleetSpec, pub crews: Vec<Crew>, pub agents: Vec<Agent>, pub missing_tokens: Vec<String>, pub request: Option<balerix_api::FleetRequest> }`
  - `enum PlanError { Config(ConfigError), Desired(DesiredError) }`
  - `plan_fleet(fleet: &Fleet, daemon: &Daemon, tokens: &balerix_api::AgentTokens, images: &Images) -> Result<FleetPlan, PlanError>`
  - `enum Accepted { Yes, Rejected(String), DaemonUnavailable(String), NotAttempted }`
  - `fleet_conditions(resolved: Result<(), String>, accepted: &Accepted, ready: usize, total: usize) -> Vec<Cond>`
  - `pub const HARVEST_FINALIZER: &str = "balerix.ai/harvest"`, `FLEET_FINALIZER: &str = "balerix.ai/fleet"`

- [ ] **Step 1: Write the failing tests**

At the bottom of `operator/src/desired/fleet.rs`:

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use balerix_api::{AgentTokens, RunnerKind};
    use balerix_config::ResolveOptions;
    use proptest::prelude::*;
    use serde_json::{Value, json};

    use super::*;
    use crate::api::{Daemon, Fleet};

    fn fleet(name: &str, spec: Value) -> Fleet {
        serde_json::from_value(json!({
            "apiVersion": "balerix.ai/v1alpha1",
            "kind": "Fleet",
            "metadata": { "name": name, "namespace": "team-a", "uid": "fleet-uid", "generation": 3 },
            "spec": spec
        }))
        .unwrap()
    }

    fn daemon(defaults: Value) -> Daemon {
        serde_json::from_value(json!({
            "apiVersion": "balerix.ai/v1alpha1",
            "kind": "Daemon",
            "metadata": { "name": "default", "namespace": "team-a", "uid": "daemon-uid" },
            "spec": {
                "storage": {
                    "state": { "size": "5Gi" },
                    "shared": { "storageClassName": "efs", "size": "100Gi" },
                    "agent": { "size": "20Gi" }
                },
                "defaults": defaults
            }
        }))
        .unwrap()
    }

    fn payments() -> Value {
        json!({
            "daemon": "default",
            "retain": "Branches",
            "defaults": {
                "claude": { "settings": { "permissions": { "allow": ["Bash(git *)"] } }, "resume": true },
                "tools": { "node": "22.11.0" },
                "runner": { "resources": { "requests": { "cpu": "1", "memory": "2Gi" } }, "storage": { "size": "40Gi" } }
            },
            "crews": { "backend": {
                "repo": "acme/payments-api",
                "ref": "main",
                "git": { "push": true, "auth": "gh" },
                "defaults": { "tools": { "python": "3.12.8" } },
                "agents": { "alice": {}, "bob": { "claude": { "settings": { "model": "opus" } } } }
            } }
        })
    }

    fn images() -> Images {
        Images::for_version("0.2.0")
    }

    fn tokens(keys: &[&str]) -> AgentTokens {
        keys.iter()
            .map(|k| (k.to_string(), "0123456789abcdef0123456789abcdef".to_string()))
            .collect()
    }

    #[test]
    fn the_daemons_defaults_sit_beneath_the_fleets_and_the_runner_is_a_pod() {
        let f = fleet("payments", payments());
        let spec = resolve_fleet(&f, &json!({ "claude": { "settings": { "model": "sonnet", "theme": "dark" } } })).unwrap();
        assert_eq!(spec.name, "payments");
        let crew = &spec.crews["backend"];
        assert_eq!(crew.agents["alice"].claude.settings["model"], "sonnet");
        assert_eq!(crew.agents["bob"].claude.settings["model"], "opus");
        assert_eq!(crew.agents["bob"].claude.settings["theme"], "dark");
        assert_eq!(crew.agents["alice"].runner.kind(), RunnerKind::Pod, "omitted is pod");
        assert_eq!(spec.tools["node"], "22.11.0");
        assert_eq!(crew.tools["python"], "3.12.8");
    }

    /// Review focus 5: each of these refuses the whole Fleet.
    #[test]
    fn a_fleet_that_cannot_run_as_pods_fails_with_the_config_path() {
        let mut tmux = payments();
        tmux["crews"]["backend"]["agents"]["bob"]["runner"] = json!({ "type": "tmux" });
        let e = plan_fleet(&fleet("payments", tmux), &daemon(json!({})), &tokens(&[]), &images())
            .err()
            .unwrap();
        assert_eq!(
            e.to_string(),
            "crews.backend.agents.bob.runner.type: `tmux` is not available on Kubernetes; a Fleet's agents run as pods"
        );

        let long = "a-fleet-with-a-name-that-is-exactly-fifty-chars-xx";
        assert_eq!(long.len(), 50);
        let e = plan_fleet(&fleet(long, payments()), &daemon(json!({})), &tokens(&[]), &images())
            .err()
            .unwrap();
        assert_eq!(
            e.to_string(),
            format!("crews.backend.agents.alice: the object name {long}-backend-alice is 64 characters; at most 63")
        );

        let mut shape = payments();
        shape["defaults"]["runner"]["resources"] = json!({ "requests": "lots" });
        let e = plan_fleet(&fleet("payments", shape), &daemon(json!({})), &tokens(&[]), &images())
            .err()
            .unwrap()
            .to_string();
        assert!(e.starts_with("crews.backend.agents.alice.runner.resources: "), "{e}");

        let mut tolerations = payments();
        tolerations["defaults"]["runner"]["tolerations"] = json!(["not-a-toleration"]);
        let e = plan_fleet(&fleet("payments", tolerations), &daemon(json!({})), &tokens(&[]), &images())
            .err()
            .unwrap()
            .to_string();
        assert!(e.starts_with("crews.backend.agents.alice.runner.tolerations: "), "{e}");

        let e = plan_fleet(&fleet("balerix", payments()), &daemon(json!({})), &tokens(&[]), &images())
            .err()
            .unwrap()
            .to_string();
        assert!(e.starts_with("name: "), "the reserved name: {e}");
    }

    #[test]
    fn no_request_is_built_until_every_agent_has_a_token() {
        let f = fleet("payments", payments());
        let d = daemon(json!({}));
        let none = plan_fleet(&f, &d, &tokens(&[]), &images()).unwrap();
        assert_eq!(
            none.missing_tokens,
            ["payments/backend/alice", "payments/backend/bob"]
        );
        assert!(none.request.is_none(), "the Daemon refuses a partial map");
        let one = plan_fleet(&f, &d, &tokens(&["payments/backend/alice"]), &images()).unwrap();
        assert_eq!(one.missing_tokens, ["payments/backend/bob"]);
        assert!(one.request.is_none());
        // a token for an agent the Fleet no longer has is left out
        let all = plan_fleet(
            &f,
            &d,
            &tokens(&["payments/backend/alice", "payments/backend/bob", "payments/backend/gone"]),
            &images(),
        )
        .unwrap();
        assert!(all.missing_tokens.is_empty());
        let request = all.request.unwrap();
        assert_eq!(request.spec, all.spec);
        let sent: Vec<&String> = request.agent_tokens.as_ref().unwrap().keys().collect();
        assert_eq!(sent, ["payments/backend/alice", "payments/backend/bob"]);
    }

    #[test]
    fn crews_and_agents_are_owned_labelled_and_snapshot() {
        let f = fleet("payments", payments());
        let plan = plan_fleet(
            &f,
            &daemon(json!({ "claude": { "settings": { "model": "sonnet" } } })),
            &tokens(&["payments/backend/alice", "payments/backend/bob"]),
            &images(),
        )
        .unwrap();
        assert_eq!(plan.crews.len(), 1);
        assert_eq!(plan.agents.len(), 2);
        insta::assert_yaml_snapshot!("fleet_crews", plan.crews);
        insta::assert_yaml_snapshot!("fleet_agents", plan.agents);
        // the hash follows the settings and the image, and nothing else
        let again = plan_fleet(&f, &daemon(json!({ "claude": { "settings": { "model": "sonnet" } } })), &tokens(&[]), &images()).unwrap();
        assert_eq!(again.agents[0].spec.spec_hash, plan.agents[0].spec.spec_hash);
        let newer = plan_fleet(&f, &daemon(json!({ "claude": { "settings": { "model": "sonnet" } } })), &tokens(&[]), &Images::for_version("0.3.0")).unwrap();
        assert_ne!(newer.agents[0].spec.spec_hash, plan.agents[0].spec.spec_hash);
        let other = plan_fleet(&f, &daemon(json!({ "claude": { "settings": { "model": "haiku" } } })), &tokens(&[]), &images()).unwrap();
        assert_ne!(other.agents[0].spec.spec_hash, plan.agents[0].spec.spec_hash, "alice inherits the model");
        assert_eq!(other.agents[1].spec.spec_hash, plan.agents[1].spec.spec_hash, "bob sets his own");
    }

    #[test]
    fn conditions_follow_resolution_acceptance_and_the_agents() {
        let c = fleet_conditions(Err("crews.c.agents.a: x".into()), &Accepted::NotAttempted, 0, 0);
        assert_eq!((c[0].type_, c[0].status, c[0].reason.as_str()), ("Resolved", Some(false), "InvalidFleet"));
        assert_eq!(c[0].message, "crews.c.agents.a: x");
        assert_eq!((c[1].type_, c[1].status, c[1].reason.as_str()), ("Accepted", None, "NotResolved"));
        assert_eq!((c[2].type_, c[2].status), ("Ready", Some(false)));

        let c = fleet_conditions(Ok(()), &Accepted::Rejected("flow: states.x: unknown".into()), 0, 2);
        assert_eq!((c[1].status, c[1].reason.as_str(), c[1].message.as_str()), (Some(false), "Rejected", "flow: states.x: unknown"));
        assert_eq!(c[2].status, Some(false));

        let c = fleet_conditions(Ok(()), &Accepted::DaemonUnavailable("connection refused".into()), 1, 2);
        assert_eq!((c[1].status, c[1].reason.as_str()), (None, "DaemonUnavailable"));
        assert_eq!((c[2].status, c[2].reason.as_str()), (Some(false), "DaemonUnavailable"));

        let c = fleet_conditions(Ok(()), &Accepted::Yes, 1, 2);
        assert_eq!((c[2].status, c[2].reason.as_str(), c[2].message.as_str()), (Some(false), "AgentsNotReady", "1 of 2 agents ready"));
        let c = fleet_conditions(Ok(()), &Accepted::Yes, 2, 2);
        assert_eq!((c[2].status, c[2].reason.as_str()), (Some(true), "AgentsReady"));
    }

    /// Spec O §18: the example fleet, moved under a Fleet's `spec` with
    /// `runner.type: pod`, resolves to the same agent settings as the file.
    #[test]
    fn the_example_fleet_resolves_as_its_file_does() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/payments.yaml");
        let mut file: Value = serde_norway::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        file["defaults"]["runner"] = json!({ "type": "pod" });
        let from_file = balerix_config::resolve(
            &balerix_config::from_value(&file).unwrap(),
            &ResolveOptions { runner: RunnerKind::Pod, ..Default::default() },
        )
        .unwrap();
        let mut spec = file.clone();
        let name = spec["name"].as_str().unwrap().to_string();
        for key in ["apiVersion", "kind", "name"] {
            spec.as_object_mut().unwrap().remove(key);
        }
        spec["daemon"] = json!("default");
        assert_eq!(resolve_fleet(&fleet(&name, spec), &json!({})).unwrap(), from_file);
    }

    fn layer() -> impl Strategy<Value = Value> {
        (
            proptest::option::of(prop_oneof![Just("sonnet"), Just("opus"), Just("haiku")]),
            proptest::option::of(prop_oneof![Just("22.11.0"), Just("20.18.1")]),
            proptest::bool::ANY,
        )
            .prop_map(|(model, node, resume)| {
                let mut l = json!({});
                if let Some(m) = model {
                    l["claude"] = json!({ "settings": { "model": m }, "resume": resume });
                }
                if let Some(n) = node {
                    l["tools"] = json!({ "node": n });
                }
                l
            })
    }

    proptest! {
        /// Spec O §15: resolving a Fleet's `spec` equals resolving the
        /// same content as a fleet file.
        #[test]
        fn a_fleets_spec_resolves_as_the_same_content_does_as_a_file(
            defaults in layer(),
            crews in proptest::collection::btree_map(
                prop_oneof![Just("backend"), Just("web"), Just("infra")],
                (layer(), proptest::collection::btree_map(
                    prop_oneof![Just("alice"), Just("bob"), Just("carol")], layer(), 1..3)),
                1..3,
            ),
        ) {
            let crews: serde_json::Map<String, Value> = crews
                .into_iter()
                .map(|(name, (crew_defaults, agents))| {
                    (name.to_string(), json!({
                        "repo": "acme/api", "ref": "main", "git": { "auth": "none" },
                        "defaults": crew_defaults, "agents": agents,
                    }))
                })
                .collect();
            let mut file_defaults = defaults.clone();
            file_defaults["runner"] = json!({ "type": "pod" });
            let file = json!({
                "apiVersion": "balerix/v1", "kind": "Fleet", "name": "f",
                "defaults": file_defaults, "crews": crews,
            });
            let from_file = balerix_config::resolve(
                &balerix_config::from_value(&file).unwrap(),
                &ResolveOptions { runner: RunnerKind::Pod, ..Default::default() },
            ).unwrap();
            let object = fleet("f", json!({ "daemon": "default", "defaults": defaults, "crews": file["crews"] }));
            prop_assert_eq!(resolve_fleet(&object, &json!({})).unwrap(), from_file);
        }
    }
}
```

Run: `scripts/operator.sh check`
Expected: compile errors.

- [ ] **Step 2: Implement `operator/src/desired/fleet.rs`** (above its tests)

```rust
//! A Fleet into what the operator does with it (Spec O §5.2): the
//! resolved spec, through `balerix-config` beneath the Daemon's defaults;
//! the generated Crew and Agent objects; and the `PUT` body, built only
//! once every agent has a token.

use balerix_api::{AgentTokens, FleetRequest, RunnerKind, RunnerSettings};
use balerix_config::{ConfigError, ResolveOptions};
use k8s_openapi::api::core::v1::{ResourceRequirements, Toleration};
use serde_json::{Value, json};

use super::common::{Cond, DesiredError, Images, hash, labels, owner_of, typed};
use super::names;
use crate::api::{Agent, AgentSpec, Crew, CrewSpec, Daemon, Fleet};

/// A Fleet waits on this until every Agent is gone (§5.2, §8.5).
pub const FLEET_FINALIZER: &str = "balerix.ai/fleet";
/// An Agent waits on this until its branch is in the crew cache (§8.4).
pub const HARVEST_FINALIZER: &str = "balerix.ai/harvest";

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// `Resolved=False`, with this text: the config path comes first.
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("{0}")]
    Desired(#[from] DesiredError),
}

#[derive(Debug, Clone)]
pub struct FleetPlan {
    pub spec: balerix_api::FleetSpec,
    pub crews: Vec<Crew>,
    pub agents: Vec<Agent>,
    /// `fleet/crew/agent` of every agent with no token yet: the
    /// controller mints these, then plans again.
    pub missing_tokens: Vec<String>,
    /// The `PUT /v1/fleets/{name}` body; `None` while a token is missing.
    pub request: Option<FleetRequest>,
}

fn invalid(path: impl Into<String>, message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        path: path.into(),
        message: message.into(),
    }
}

/// The Fleet's `spec` as the fleet file it is (§4.2), resolved beneath
/// the Daemon's `defaults` with `runner: { type: pod }` at the bottom.
/// Then what only Kubernetes can refuse: an object name over 63
/// characters, and runner shapes that are not Kubernetes'.
pub fn resolve_fleet(
    fleet: &Fleet,
    daemon_defaults: &Value,
) -> Result<balerix_api::FleetSpec, ConfigError> {
    let name = fleet
        .metadata
        .name
        .clone()
        .ok_or_else(|| invalid("metadata.name", "required"))?;
    let crews: serde_json::Map<String, Value> = fleet
        .spec
        .crews
        .iter()
        .map(|(crew_name, crew)| {
            let mut c = json!({
                "repo": crew.repo,
                "defaults": crew.defaults,
                "agents": crew.agents,
            });
            if let Some(git_ref) = &crew.git_ref {
                c["ref"] = json!(git_ref);
            }
            if let Some(git) = &crew.git {
                c["git"] = git.clone();
            }
            (crew_name.clone(), c)
        })
        .collect();
    let file = balerix_config::from_value(&json!({
        "apiVersion": balerix_api::API_VERSION,
        "kind": "Fleet",
        "name": name,
        "defaults": fleet.spec.defaults,
        "crews": crews,
    }))?;
    let layer = balerix_config::merge(&json!({ "runner": { "type": "pod" } }), daemon_defaults);
    let spec = balerix_config::resolve(
        &file,
        &ResolveOptions {
            operator_layer: Some(layer),
            runner: RunnerKind::Pod,
            ..Default::default()
        },
    )?;
    for (crew_name, crew) in &spec.crews {
        for (agent_name, settings) in &crew.agents {
            let path = format!("crews.{crew_name}.agents.{agent_name}");
            let object = names::agent(&name, crew_name, agent_name);
            if object.len() > 63 {
                return Err(invalid(
                    path,
                    format!(
                        "the object name {object} is {} characters; at most 63",
                        object.len()
                    ),
                ));
            }
            check_runner(&path, &settings.runner)?;
        }
    }
    Ok(spec)
}

/// `balerix-api` keeps the pod's shapes opaque; here they get their
/// types, so a bad one is the Fleet's `Resolved=False` and never a pod
/// that fails to build.
fn check_runner(path: &str, runner: &RunnerSettings) -> Result<(), ConfigError> {
    let RunnerSettings::Pod {
        resources,
        storage,
        tolerations,
        ..
    } = runner
    else {
        return Ok(());
    };
    typed::<ResourceRequirements>(resources.clone())
        .map_err(|e| invalid(format!("{path}.runner.resources"), e.to_string()))?;
    typed::<Vec<Toleration>>(Value::Array(tolerations.clone()))
        .map_err(|e| invalid(format!("{path}.runner.tolerations"), e.to_string()))?;
    if let Some(storage) = storage {
        match storage.get("size").and_then(Value::as_str) {
            Some(size) if !size.is_empty() => {}
            _ => {
                return Err(invalid(
                    format!("{path}.runner.storage.size"),
                    "expected a quantity such as 40Gi",
                ));
            }
        }
    }
    Ok(())
}

pub fn plan_fleet(
    fleet: &Fleet,
    daemon: &Daemon,
    tokens: &AgentTokens,
    images: &Images,
) -> Result<FleetPlan, PlanError> {
    let spec = resolve_fleet(fleet, &daemon.spec.defaults)?;
    let namespace = fleet.metadata.namespace.clone();
    let daemon_name = fleet.spec.daemon.clone();
    let owner = owner_of(fleet)?;
    let mut crews = Vec::new();
    let mut agents = Vec::new();
    let mut wanted = Vec::new();
    for (crew_name, crew) in &spec.crews {
        let git = serde_json::to_value(&crew.git).map_err(DesiredError::from)?;
        let mut object = Crew::new(
            &names::crew(&spec.name, crew_name),
            CrewSpec {
                daemon: daemon_name.clone(),
                fleet: spec.name.clone(),
                crew: crew_name.clone(),
                repo: crew.repo.clone(),
                git_ref: crew.git_ref.clone(),
                git: git.clone(),
                fleet_tools: spec.tools.clone(),
                tools: crew.tools.clone(),
            },
        );
        object.metadata.namespace = namespace.clone();
        object.metadata.labels = Some(typed(labels(
            &daemon_name,
            "crew",
            &[
                ("balerix.ai/fleet", spec.name.as_str()),
                ("balerix.ai/crew", crew_name.as_str()),
            ],
        ))?);
        object.metadata.owner_references = Some(vec![typed(owner.clone())?]);
        crews.push(object);

        for (agent_name, settings) in &crew.agents {
            wanted.push(format!("{}/{crew_name}/{agent_name}", spec.name));
            let settings = serde_json::to_value(settings).map_err(DesiredError::from)?;
            // everything the pod is made from: a change here replaces it
            let spec_hash = hash(&json!({
                "repo": crew.repo,
                "ref": crew.git_ref,
                "git": git,
                "settings": settings,
                "image": images.agent,
            }));
            let mut object = Agent::new(
                &names::agent(&spec.name, crew_name, agent_name),
                AgentSpec {
                    daemon: daemon_name.clone(),
                    fleet: spec.name.clone(),
                    crew: crew_name.clone(),
                    agent: agent_name.clone(),
                    repo: crew.repo.clone(),
                    git_ref: crew.git_ref.clone(),
                    git: git.clone(),
                    settings,
                    spec_hash,
                },
            );
            object.metadata.namespace = namespace.clone();
            object.metadata.labels = Some(typed(labels(
                &daemon_name,
                "agent",
                &[
                    ("balerix.ai/fleet", spec.name.as_str()),
                    ("balerix.ai/crew", crew_name.as_str()),
                    ("balerix.ai/agent", agent_name.as_str()),
                ],
            ))?);
            object.metadata.owner_references = Some(vec![typed(owner.clone())?]);
            object.metadata.finalizers = Some(vec![HARVEST_FINALIZER.to_string()]);
            agents.push(object);
        }
    }
    let missing_tokens: Vec<String> = wanted
        .iter()
        .filter(|key| !tokens.contains_key(*key))
        .cloned()
        .collect();
    let request = missing_tokens.is_empty().then(|| FleetRequest {
        spec: spec.clone(),
        credentials: Default::default(),
        agent_tokens: Some(
            wanted
                .iter()
                .filter_map(|key| tokens.get(key).map(|t| (key.clone(), t.clone())))
                .collect(),
        ),
    });
    Ok(FleetPlan {
        spec,
        crews,
        agents,
        missing_tokens,
        request,
    })
}

/// What the Daemon said to the `PUT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    Yes,
    /// A 400: a plugin rejected an agent's config. Nothing landed, and
    /// the controller touches no child object (§5.2 step 4).
    Rejected(String),
    /// The Daemon did not answer; existing pods are left alone.
    DaemonUnavailable(String),
    /// The Fleet did not resolve, so nothing was sent.
    NotAttempted,
}

/// `Resolved`, `Accepted`, `Ready`, in that order (§4.2).
pub fn fleet_conditions(
    resolved: Result<(), String>,
    accepted: &Accepted,
    ready: usize,
    total: usize,
) -> Vec<Cond> {
    let resolved_ok = resolved.is_ok();
    let first = match &resolved {
        Ok(()) => Cond::yes("Resolved", "Resolved", ""),
        Err(message) => Cond::no("Resolved", "InvalidFleet", message),
    };
    let second = match accepted {
        Accepted::Yes => Cond::yes("Accepted", "Accepted", ""),
        Accepted::Rejected(message) => Cond::no("Accepted", "Rejected", message),
        Accepted::DaemonUnavailable(message) => Cond::unknown("Accepted", "DaemonUnavailable", message),
        Accepted::NotAttempted => Cond::unknown("Accepted", "NotResolved", ""),
    };
    let third = match accepted {
        _ if !resolved_ok => Cond::no("Ready", "InvalidFleet", ""),
        Accepted::DaemonUnavailable(message) => Cond::no("Ready", "DaemonUnavailable", message),
        Accepted::Rejected(_) => Cond::no("Ready", "Rejected", ""),
        Accepted::NotAttempted => Cond::no("Ready", "NotResolved", ""),
        Accepted::Yes if ready == total => Cond::yes("Ready", "AgentsReady", ""),
        Accepted::Yes => Cond::no(
            "Ready",
            "AgentsNotReady",
            &format!("{ready} of {total} agents ready"),
        ),
    };
    vec![first, second, third]
}
```

`FleetPlan` derives `Debug`: `FleetRequest`'s own `Debug` redacts the tokens. Add `pub mod fleet;` to `desired/mod.rs`.

- [ ] **Step 3: Run, read the snapshots, accept**

Run: `scripts/operator.sh check`
Expected: every test passes except the two snapshot assertions, which write `.snap.new` files under `operator/src/desired/snapshots/`.

Read both. `fleet_crews`: one Crew `payments-backend` in `team-a`; `ownerReferences` one entry, `kind: Fleet`, `name: payments`, `uid: fleet-uid`, `controller: true`; labels `balerix.ai/fleet: payments`, `balerix.ai/crew: backend`, `balerix.ai/daemon: default`, `balerix.ai/component: crew`; `spec.fleetTools: {node: 22.11.0}`, `spec.tools: {python: 3.12.8}`, `spec.ref: main`, `spec.git.auth: gh`. `fleet_agents`: `payments-backend-alice` and `payments-backend-bob`, each with `finalizers: [balerix.ai/harvest]`, the three `balerix.ai/` name labels, `spec.settings.runner.type: pod` with `resources.requests.memory: 2Gi` and `storage.size: 40Gi`, alice's `claude.settings.model: sonnet`, bob's `opus`, and a 64-character `specHash`.

Run: `mise x -- cargo insta accept --manifest-path operator/Cargo.toml`, then `mise run operator`.
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add operator
git commit -m "feat(operator): a Fleet resolves beneath its Daemon's defaults into Crews, Agents and the PUT body (Spec O §5.2)"
```

---

### Task 8: The Jobs, and what a Job came to

**Files:**
- Create: `operator/src/desired/jobs.rs`
- Modify: `operator/src/desired/mod.rs`

**Interfaces:**
- Consumes: `common::{typed, labels, pod_security, container_security, hash, HASH_ANNOTATION, Images, JobOutcome, DesiredError}`, `names`, `api::{CrewSpec, AgentSpec}`.
- Produces:
  - `struct JobContext<'a> { pub namespace: &'a str, pub daemon: &'a str, pub images: &'a Images, pub owner: Value }` (`owner` is `common::owner_of`'s manifest: the Daemon for its pool Job, the Fleet for a fleet pool Job, the Crew for a sync Job, the Agent for a harvest Job)
  - `daemon_pool_job(ctx: &JobContext) -> Result<Job, DesiredError>`
  - `fleet_pool_job(ctx: &JobContext, fleet: &str, table: &BTreeMap<String, String>) -> Result<Job, DesiredError>`
  - `crew_sync_job(ctx: &JobContext, crew: &CrewSpec, gh_secret: Option<&str>) -> Result<Job, DesiredError>`
  - `harvest_job(ctx: &JobContext, agent_name: &str, agent: &AgentSpec) -> Result<Job, DesiredError>`
  - `job_outcome(existing: Option<&Job>, pods: &[Pod], wanted: &Job) -> JobOutcome`
  - `crew_status(crew: &Crew, fleet_pool: &JobOutcome, sync: &JobOutcome, now: &Time) -> CrewStatus`
  - the mount paths `SHARED`, `SCRATCH`, `VOLUME`

- [ ] **Step 1: Write the failing tests**

At the bottom of `operator/src/desired/jobs.rs`:

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::api::{AgentSpec, CrewSpec};

    fn images() -> Images {
        Images::for_version("0.2.0")
    }

    fn ctx(images: &Images) -> JobContext<'_> {
        JobContext {
            namespace: "team-a",
            daemon: "default",
            images,
            owner: json!({
                "apiVersion": "balerix.ai/v1alpha1", "kind": "Crew", "name": "payments-backend",
                "uid": "crew-uid", "controller": true
            }),
        }
    }

    fn crew(auth: &str) -> CrewSpec {
        CrewSpec {
            daemon: "default".into(),
            fleet: "payments".into(),
            crew: "backend".into(),
            repo: "acme/payments-api".into(),
            git_ref: "main".into(),
            git: json!({ "push": true, "auth": auth }),
            fleet_tools: BTreeMap::from([("node".into(), "22.11.0".into())]),
            tools: BTreeMap::from([("python".into(), "3.12.8".into())]),
        }
    }

    fn agent() -> AgentSpec {
        AgentSpec {
            daemon: "default".into(),
            fleet: "payments".into(),
            crew: "backend".into(),
            agent: "alice".into(),
            repo: "acme/payments-api".into(),
            git_ref: "main".into(),
            git: json!({}),
            settings: json!({}),
            spec_hash: "h".into(),
        }
    }

    #[test]
    fn the_four_jobs() {
        let images = images();
        let c = ctx(&images);
        insta::assert_yaml_snapshot!("job_daemon_pool", daemon_pool_job(&c).unwrap());
        insta::assert_yaml_snapshot!(
            "job_fleet_pool",
            fleet_pool_job(&c, "payments", &crew("gh").fleet_tools).unwrap()
        );
        insta::assert_yaml_snapshot!("job_crew_sync", crew_sync_job(&c, &crew("gh"), Some("gh-token")).unwrap());
        insta::assert_yaml_snapshot!(
            "job_harvest",
            harvest_job(&c, "payments-backend-alice", &agent()).unwrap()
        );
    }

    #[test]
    fn a_public_crew_mounts_no_token_and_a_gh_crew_needs_the_daemons_secret() {
        let images = images();
        let c = ctx(&images);
        let public = serde_json::to_value(crew_sync_job(&c, &crew("none"), None).unwrap()).unwrap();
        let text = public.to_string();
        assert!(!text.contains("gh-token"), "{text}");
        let e = crew_sync_job(&c, &crew("gh"), None).unwrap_err().to_string();
        assert_eq!(
            e,
            "the Daemon has no spec.credentials.github (the crew's git.auth is gh)"
        );
    }

    #[test]
    fn a_jobs_hash_follows_its_input() {
        let images = images();
        let c = ctx(&images);
        let hash_of = |job: &Job| job.metadata.annotations.as_ref().unwrap()[HASH_ANNOTATION].clone();
        let a = crew_sync_job(&c, &crew("gh"), Some("gh-token")).unwrap();
        let same = crew_sync_job(&c, &crew("gh"), Some("gh-token")).unwrap();
        assert_eq!(hash_of(&a), hash_of(&same));
        let mut moved = crew("gh");
        moved.git_ref = "release".into();
        assert_ne!(hash_of(&a), hash_of(&crew_sync_job(&c, &moved, Some("gh-token")).unwrap()));
        let newer = Images::for_version("0.3.0");
        assert_ne!(
            hash_of(&a),
            hash_of(&crew_sync_job(&ctx(&newer), &crew("gh"), Some("gh-token")).unwrap())
        );
    }

    fn observed(job: &Job, status: serde_json::Value) -> Job {
        let mut v = serde_json::to_value(job).unwrap();
        v["status"] = status;
        serde_json::from_value(v).unwrap()
    }

    fn pod(statuses: serde_json::Value) -> Pod {
        serde_json::from_value(json!({ "metadata": { "name": "p" }, "status": statuses })).unwrap()
    }

    #[test]
    fn an_outcome_is_read_from_the_job_and_its_pods_termination_message() {
        let images = images();
        let c = ctx(&images);
        let wanted = daemon_pool_job(&c).unwrap();
        assert_eq!(job_outcome(None, &[], &wanted), JobOutcome::Absent);
        assert_eq!(job_outcome(Some(&wanted), &[], &wanted), JobOutcome::Running);

        let done = observed(&wanted, json!({ "succeeded": 1 }));
        let said = pod(json!({ "containerStatuses": [{
            "name": "job", "ready": false, "restartCount": 0, "image": "i", "imageID": "",
            "state": { "terminated": { "exitCode": 0, "message": "synced\n" } }
        }] }));
        assert_eq!(
            job_outcome(Some(&done), &[said], &wanted),
            JobOutcome::Succeeded("synced".into())
        );

        let failed = observed(&wanted, json!({ "failed": 1 }));
        let why = pod(json!({ "containerStatuses": [{
            "name": "job", "ready": false, "restartCount": 0, "image": "i", "imageID": "",
            "state": { "terminated": { "exitCode": 1, "message": "tools: system: mise install failed\n" } }
        }] }));
        assert_eq!(
            job_outcome(Some(&failed), &[why], &wanted),
            JobOutcome::Failed("tools: system: mise install failed".into())
        );
        // the init container could not make the directories
        let init = pod(json!({ "initContainerStatuses": [{
            "name": "slice", "ready": false, "restartCount": 0, "image": "i", "imageID": "",
            "state": { "terminated": { "exitCode": 1, "reason": "Error" } }
        }] }));
        assert_eq!(
            job_outcome(Some(&failed), &[init], &wanted),
            JobOutcome::Failed("the job failed and left no message".into())
        );

        let newer = Images::for_version("0.3.0");
        let other = daemon_pool_job(&ctx(&newer)).unwrap();
        assert_eq!(job_outcome(Some(&done), &[], &other), JobOutcome::Stale);
    }

    fn crew_object(cache_ref: Option<&str>) -> Crew {
        let mut c = Crew::new("payments-backend", crew("gh"));
        c.metadata.generation = Some(5);
        c.status = cache_ref.map(|r| CrewStatus {
            cache_ref: Some(r.to_string()),
            ..Default::default()
        });
        c
    }

    fn crew_cond<'a>(status: &'a CrewStatus, type_: &str) -> (&'a str, &'a str, &'a str) {
        let c = status.conditions.iter().find(|c| c.type_ == type_).unwrap();
        (c.status.as_str(), c.reason.as_str(), c.message.as_str())
    }

    /// §5.3, §20.3: the sync Job's message says which half failed.
    #[test]
    fn a_crews_conditions_follow_the_two_jobs_and_the_messages_prefix() {
        let now = Time(k8s_openapi::jiff::Timestamp::from_second(1_800_000_000).unwrap());
        let ok = JobOutcome::Succeeded("synced".into());
        let s = crew_status(&crew_object(None), &ok, &JobOutcome::Succeeded("abc123".into()), &now);
        assert_eq!(crew_cond(&s, "CacheReady"), ("True", "Synced", ""));
        assert_eq!(crew_cond(&s, "ToolsReady"), ("True", "Synced", ""));
        assert_eq!(s.cache_ref.as_deref(), Some("abc123"));
        assert_eq!(s.observed_generation, Some(5));

        let s = crew_status(&crew_object(Some("old")), &ok, &JobOutcome::Running, &now);
        assert_eq!(crew_cond(&s, "CacheReady"), ("False", "Syncing", ""));
        assert_eq!(crew_cond(&s, "ToolsReady"), ("False", "Syncing", ""));
        assert_eq!(s.cache_ref.as_deref(), Some("old"), "the last commit fetched stays");

        let cache = JobOutcome::Failed("cache: payments/backend: the remote has no branch nope".into());
        let s = crew_status(&crew_object(Some("old")), &ok, &cache, &now);
        assert_eq!(
            crew_cond(&s, "CacheReady"),
            ("False", "SyncFailed", "cache: payments/backend: the remote has no branch nope")
        );
        assert_eq!(crew_cond(&s, "ToolsReady"), ("Unknown", "SyncFailed", ""));

        let tools = JobOutcome::Failed("tools: payments/backend: crew payments/backend: mise install failed".into());
        let s = crew_status(&crew_object(None), &ok, &tools, &now);
        assert_eq!(crew_cond(&s, "CacheReady"), ("True", "Synced", ""));
        assert_eq!(crew_cond(&s, "ToolsReady").0, "False");
        assert_eq!(crew_cond(&s, "ToolsReady").1, "SyncFailed");

        // the fleet's pool comes first; the crew's Job has not run
        let s = crew_status(
            &crew_object(None),
            &JobOutcome::Failed("tools: payments: fleet payments: boom".into()),
            &JobOutcome::Absent,
            &now,
        );
        assert_eq!(
            crew_cond(&s, "ToolsReady"),
            ("False", "FleetPoolFailed", "tools: payments: fleet payments: boom")
        );
        assert_eq!(crew_cond(&s, "CacheReady"), ("False", "Syncing", ""));
    }
}
```

In the test module's imports, `use crate::api::{AgentSpec, CrewSpec};` becomes `use crate::api::{AgentSpec, Crew, CrewSpec, CrewStatus};`.

Run: `scripts/operator.sh check`
Expected: compile errors.

- [ ] **Step 2: Implement `operator/src/desired/jobs.rs`** (above its tests)

```rust
//! The Jobs that write the shared volume (Spec O §8.3, §8.4, O-6): each
//! mounts its crew's slice where the agent pod has it (§20.3), read-write
//! where it writes. `balerix-agent`'s `crew-sync`, `pool-sync` and
//! `harvest` are their commands; the outcome is the container's
//! termination message.

use std::collections::BTreeMap;

use balerix_api::{GitAuth, GitSettings};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use serde_json::{Value, json};

use super::common::{
    DesiredError, HASH_ANNOTATION, Images, JobOutcome, container_security, hash, labels,
    pod_security, typed,
};
use super::names;
use crate::api::{AgentSpec, CrewSpec};

/// Where a Job's init container sees the whole shared claim.
pub const VOLUME: &str = "/balerix/volume";
/// Where the slice is mounted, in a Job and in the agent pod (§7.4).
pub const SHARED: &str = "/balerix/shared";
/// A Job's emptyDir: the gh config, the git profile, nono's home, `HOME`.
pub const SCRATCH: &str = "/balerix/scratch";

pub struct JobContext<'a> {
    pub namespace: &'a str,
    pub daemon: &'a str,
    pub images: &'a Images,
    /// The owner reference, as `common::owner_of` makes it.
    pub owner: Value,
}

/// One directory of the volume and where under `SHARED` the Job has it.
struct Slice {
    sub_path: String,
    at: &'static str,
    writable: bool,
}

fn slice(sub_path: String, at: &'static str, writable: bool) -> Slice {
    Slice {
        sub_path,
        at,
        writable,
    }
}

struct Parts {
    name: String,
    component: &'static str,
    extra_labels: Vec<(&'static str, String)>,
    args: Vec<String>,
    slices: Vec<Slice>,
    volumes: Vec<Value>,
    mounts: Vec<Value>,
}

fn tool_args(table: &BTreeMap<String, String>) -> Vec<String> {
    table
        .iter()
        .flat_map(|(name, version)| ["--tool".to_string(), format!("{name}={version}")])
        .collect()
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_string()).collect()
}

fn job(ctx: &JobContext<'_>, parts: Parts) -> Result<Job, DesiredError> {
    let extra: Vec<(&str, &str)> = parts
        .extra_labels
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .collect();
    let labels = labels(ctx.daemon, parts.component, &extra);
    // The kubelet makes a missing sub-path owned by root; made here as
    // the Job's own user, the Job can write it.
    let mut mkdir = strings(&["mkdir", "-p"]);
    mkdir.extend(parts.slices.iter().map(|s| format!("{VOLUME}/{}", s.sub_path)));
    let mut mounts: Vec<Value> = parts
        .slices
        .iter()
        .map(|s| {
            json!({
                "name": "shared",
                "mountPath": format!("{SHARED}/{}", s.at),
                "subPath": s.sub_path,
                "readOnly": !s.writable,
            })
        })
        .collect();
    mounts.push(json!({ "name": "scratch", "mountPath": SCRATCH }));
    mounts.extend(parts.mounts);
    let mut volumes = vec![
        json!({ "name": "shared", "persistentVolumeClaim": { "claimName": names::shared_claim(ctx.daemon) } }),
        json!({ "name": "scratch", "emptyDir": {} }),
    ];
    volumes.extend(parts.volumes);
    let input = hash(&json!({
        "image": ctx.images.agent,
        "args": parts.args,
        "mounts": mounts,
        "volumes": volumes,
    }));
    typed(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": parts.name,
            "namespace": ctx.namespace,
            "labels": labels,
            "annotations": { HASH_ANNOTATION: input },
            "ownerReferences": [ctx.owner],
        },
        "spec": {
            // the operator retries with back-off (§8.3); the Job does not
            "backoffLimit": 0,
            "template": {
                "metadata": { "labels": labels },
                "spec": {
                    "restartPolicy": "Never",
                    "automountServiceAccountToken": false,
                    "enableServiceLinks": false,
                    "securityContext": pod_security(),
                    "initContainers": [{
                        "name": "slice",
                        "image": ctx.images.agent,
                        "command": mkdir,
                        "securityContext": container_security(),
                        "volumeMounts": [{ "name": "shared", "mountPath": VOLUME }],
                    }],
                    "containers": [{
                        "name": "job",
                        "image": ctx.images.agent,
                        "command": ["balerix-agent"],
                        "args": parts.args,
                        "env": [{ "name": "HOME", "value": SCRATCH }],
                        "securityContext": container_security(),
                        "volumeMounts": mounts,
                    }],
                    "volumes": volumes,
                },
            },
        },
    }))
}

/// `pools/daemon`: the system table this image embeds (§20.3).
pub fn daemon_pool_job(ctx: &JobContext<'_>) -> Result<Job, DesiredError> {
    job(
        ctx,
        Parts {
            name: names::daemon_pool_job(ctx.daemon),
            component: "pool",
            extra_labels: vec![],
            args: strings(&["pool-sync", "--level", "daemon"]),
            slices: vec![slice(names::vol_daemon_pool(), "daemon", true)],
            volumes: vec![],
            mounts: vec![],
        },
    )
}

/// The fleet's `defaults.tools`, once per fleet and before its crews'
/// Jobs: two crews' Jobs would otherwise write one pool at once (§20.3).
pub fn fleet_pool_job(
    ctx: &JobContext<'_>,
    fleet: &str,
    table: &BTreeMap<String, String>,
) -> Result<Job, DesiredError> {
    let mut args = strings(&["pool-sync", "--level", "fleet", "--fleet", fleet]);
    args.extend(tool_args(table));
    job(
        ctx,
        Parts {
            name: names::fleet_pool_job(fleet),
            component: "pool",
            extra_labels: vec![("balerix.ai/fleet", fleet.to_string())],
            args,
            slices: vec![
                slice(names::vol_fleet_pool(fleet), "fleet", true),
                slice(names::vol_daemon_pool(), "daemon", false),
            ],
            volumes: vec![],
            mounts: vec![],
        },
    )
}

/// The crew cache and the crew pool. `gh_secret` is the Daemon's
/// `credentials.github.secretName`; a crew whose `git.auth` is `gh`
/// cannot sync without it.
pub fn crew_sync_job(
    ctx: &JobContext<'_>,
    crew: &CrewSpec,
    gh_secret: Option<&str>,
) -> Result<Job, DesiredError> {
    let git: GitSettings = typed(crew.git.clone())?;
    let mut args = strings(&["crew-sync", "--crew"]);
    args.push(format!("{}/{}", crew.fleet, crew.crew));
    args.extend(["--repo".to_string(), crew.repo.clone()]);
    args.extend(["--ref".to_string(), crew.git_ref.clone()]);
    let (mut volumes, mut mounts) = (vec![], vec![]);
    if git.auth == GitAuth::Gh {
        let secret = gh_secret.ok_or(DesiredError::Missing(
            "the Daemon",
            "spec.credentials.github (the crew's git.auth is gh)",
        ))?;
        args.extend(strings(&["--gh-token-file", "/balerix/secret/gh-token"]));
        // 0440: the pod's fsGroup reads it; nothing else does
        volumes.push(json!({
            "name": "gh",
            "secret": {
                "secretName": secret,
                "items": [{ "key": "token", "path": "gh-token" }],
                "defaultMode": 0o440,
            },
        }));
        mounts.push(json!({ "name": "gh", "mountPath": "/balerix/secret", "readOnly": true }));
    }
    args.extend(tool_args(&crew.tools));
    job(
        ctx,
        Parts {
            name: names::sync_job(&crew.fleet, &crew.crew),
            component: "sync",
            extra_labels: vec![
                ("balerix.ai/fleet", crew.fleet.clone()),
                ("balerix.ai/crew", crew.crew.clone()),
            ],
            args,
            slices: vec![
                slice(names::vol_crew_repo(&crew.fleet, &crew.crew), "repo", true),
                slice(names::vol_crew_pool(&crew.fleet, &crew.crew), "crew", true),
                slice(names::vol_fleet_pool(&crew.fleet), "fleet", false),
                slice(names::vol_daemon_pool(), "daemon", false),
            ],
            volumes,
            mounts,
        },
    )
}

/// The agent's branch into the crew cache, after its pod is gone: the
/// claim read-only, the cache read-write (§8.4). It needs Landlock, like
/// any agent.
pub fn harvest_job(
    ctx: &JobContext<'_>,
    agent_name: &str,
    agent: &AgentSpec,
) -> Result<Job, DesiredError> {
    let mut args = strings(&["harvest", "--agent"]);
    args.push(format!("{}/{}/{}", agent.fleet, agent.crew, agent.agent));
    job(
        ctx,
        Parts {
            name: names::harvest_job(agent_name),
            component: "harvest",
            extra_labels: vec![
                ("balerix.ai/fleet", agent.fleet.clone()),
                ("balerix.ai/crew", agent.crew.clone()),
                ("balerix.ai/agent", agent.agent.clone()),
            ],
            args,
            slices: vec![
                slice(names::vol_crew_repo(&agent.fleet, &agent.crew), "repo", true),
                // the crew's logs and its `no-hooks` directory
                slice(names::vol_crew_pool(&agent.fleet, &agent.crew), "crew", true),
            ],
            volumes: vec![json!({
                "name": "agent",
                "persistentVolumeClaim": { "claimName": agent_name, "readOnly": true },
            })],
            mounts: vec![json!({ "name": "agent", "mountPath": "/balerix/agent", "readOnly": true })],
        },
    )
}

fn input_hash(job: &Job) -> Option<&String> {
    job.metadata.annotations.as_ref()?.get(HASH_ANNOTATION)
}

/// The last termination message any of the Job's containers left.
fn message(pods: &[Pod]) -> Option<String> {
    pods.iter().rev().find_map(|pod| {
        let status = pod.status.as_ref()?;
        status
            .container_statuses
            .iter()
            .flatten()
            .chain(status.init_container_statuses.iter().flatten())
            .find_map(|c| c.state.as_ref()?.terminated.as_ref()?.message.clone())
            .map(|m| m.trim_end().to_string())
    })
}

/// `existing` is the Job by `wanted`'s name, if there is one; `pods` are
/// that Job's pods.
pub fn job_outcome(existing: Option<&Job>, pods: &[Pod], wanted: &Job) -> JobOutcome {
    let Some(job) = existing else {
        return JobOutcome::Absent;
    };
    if input_hash(job) != input_hash(wanted) {
        return JobOutcome::Stale;
    }
    let status = job.status.as_ref();
    if status.and_then(|s| s.succeeded).unwrap_or(0) >= 1 {
        return JobOutcome::Succeeded(message(pods).unwrap_or_default());
    }
    if status.and_then(|s| s.failed).unwrap_or(0) >= 1 {
        return JobOutcome::Failed(
            message(pods).unwrap_or_else(|| "the job failed and left no message".to_string()),
        );
    }
    JobOutcome::Running
}

/// `CacheReady` and `ToolsReady` (§4.3) from the fleet's pool Job and the
/// crew's sync Job. A sync that failed says which half in its message's
/// prefix (`cache:` or `tools:`); the tools half runs after the cache, so
/// a `tools:` failure means the cache is synced. `cacheRef` is the commit
/// the last successful sync reported, and stays until the next one.
pub fn crew_status(
    crew: &Crew,
    fleet_pool: &JobOutcome,
    sync: &JobOutcome,
    now: &Time,
) -> CrewStatus {
    let syncing = || Cond::no("CacheReady", "Syncing", "");
    let (cache, tools, commit) = match (fleet_pool, sync) {
        (JobOutcome::Failed(message), _) => (
            syncing(),
            Cond::no("ToolsReady", "FleetPoolFailed", message),
            None,
        ),
        (JobOutcome::Succeeded(_), JobOutcome::Succeeded(commit)) => (
            Cond::yes("CacheReady", "Synced", ""),
            Cond::yes("ToolsReady", "Synced", ""),
            Some(commit.clone()),
        ),
        (JobOutcome::Succeeded(_), JobOutcome::Failed(message)) if message.starts_with("tools:") => (
            Cond::yes("CacheReady", "Synced", ""),
            Cond::no("ToolsReady", "SyncFailed", message),
            None,
        ),
        (JobOutcome::Succeeded(_), JobOutcome::Failed(message)) => (
            Cond::no("CacheReady", "SyncFailed", message),
            Cond::unknown("ToolsReady", "SyncFailed", ""),
            None,
        ),
        _ => (syncing(), Cond::no("ToolsReady", "Syncing", ""), None),
    };
    let old = crew.status.as_ref();
    CrewStatus {
        observed_generation: crew.metadata.generation,
        conditions: conditions(
            old.map(|s| s.conditions.as_slice()).unwrap_or_default(),
            &[cache, tools],
            crew.metadata.generation,
            now,
        ),
        cache_ref: commit.or_else(|| old.and_then(|s| s.cache_ref.clone())),
    }
}
```

Its imports: add `Cond` and `conditions` to the `super::common` list, `use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;`, and `Crew` and `CrewStatus` to the `crate::api` list.

Add `pub mod jobs;` to `desired/mod.rs`.

- [ ] **Step 3: Run, read the snapshots, accept**

Run: `scripts/operator.sh check`. Read the four `.snap.new` files. In every one: `backoffLimit: 0`; `restartPolicy: Never`; `automountServiceAccountToken: false`; pod `securityContext` with `runAsUser`, `runAsGroup`, `fsGroup` all `10001`, `runAsNonRoot: true`, `seccompProfile.type: RuntimeDefault`; both containers `readOnlyRootFilesystem: true`, `allowPrivilegeEscalation: false`, `capabilities.drop: [ALL]`; a `shared` volume on claim `balerix-default-shared` and a `scratch` emptyDir; the `ownerReferences` entry; a 64-character `balerix.ai/input-hash`. Then each:

- `job_daemon_pool`: name `balerix-default-pool`; `mkdir -p /balerix/volume/pools/daemon`; args `[pool-sync, --level, daemon]`; one shared mount, `/balerix/shared/daemon`, `subPath: pools/daemon`, `readOnly: false`.
- `job_fleet_pool`: name `payments-pool`; args end `--fleet, payments, --tool, node=22.11.0`; `/balerix/shared/fleet` writable, `/balerix/shared/daemon` read-only.
- `job_crew_sync`: name `payments-backend-sync`; args `[crew-sync, --crew, payments/backend, --repo, acme/payments-api, --ref, main, --gh-token-file, /balerix/secret/gh-token, --tool, python=3.12.8]`; `repo` and `crew` writable, `fleet` and `daemon` read-only; the `gh` Secret volume with `defaultMode: 288` and the one item `token` to `gh-token`. No token value anywhere.
- `job_harvest`: name `payments-backend-alice-harvest`; args `[harvest, --agent, payments/backend/alice]`; the `agent` claim volume and mount both `readOnly: true`; `repo` and `crew` writable.

Run: `mise x -- cargo insta accept --manifest-path operator/Cargo.toml && mise run operator`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add operator
git commit -m "feat(operator): the pool, sync and harvest Jobs on the pod's mount paths, and a Job's outcome (Spec O §8.3, §8.4, §20.3)"
```

---

### Task 9: A Daemon into its objects and its status

**Files:**
- Create: `operator/src/desired/daemon.rs`
- Modify: `operator/src/desired/mod.rs`

**Interfaces:**
- Consumes: `api::Daemon`, `pki::Issued`, `jobs::{JobContext, daemon_pool_job}`, `common::*`, `names`.
- Produces:
  - `pub const NOT_AFTER_ANNOTATION: &str = "balerix.ai/not-after"`
  - `struct Material<'a> { pub authority: &'a Issued, pub serving: &'a Issued, pub admin_token: &'a str }` (no `Debug`)
  - `struct DaemonSecrets { pub authority: Secret, pub authority_config: ConfigMap, pub serving: Secret, pub admin: Secret }` (no `Debug`: a `Secret`'s own `Debug` prints its data)
  - `struct DaemonObjects { pub claims: Vec<PersistentVolumeClaim>, pub service: Service, pub statefulset: StatefulSet, pub policy: NetworkPolicy, pub pool_job: Job }`
  - `struct DaemonObserved<'a> { pub shared_claim: Option<&'a PersistentVolumeClaim>, pub statefulset: Option<&'a StatefulSet>, pub pool: &'a JobOutcome }`
  - `version_ok(daemon: &Daemon, cfg: &OperatorConfig) -> bool`
  - `daemon_secrets(daemon: &Daemon, material: &Material) -> Result<DaemonSecrets, DesiredError>`
  - `daemon_objects(daemon: &Daemon, cfg: &OperatorConfig, serving_not_after: i64) -> Result<DaemonObjects, DesiredError>`
  - `daemon_status(daemon: &Daemon, cfg: &OperatorConfig, observed: &DaemonObserved, now: &Time) -> DaemonStatus`

- [ ] **Step 1: Write the failing tests**

At the bottom of `operator/src/desired/daemon.rs`:

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use serde_json::{Value, json};

    use super::*;
    use crate::desired::common::Images;

    fn daemon(extra: Value) -> Daemon {
        let mut spec = json!({
            "storage": {
                "state": { "storageClassName": "standard", "size": "5Gi" },
                "shared": { "storageClassName": "efs", "size": "100Gi" },
                "agent": { "storageClassName": "standard", "size": "20Gi" }
            },
            "credentials": { "claude": { "secretName": "claude-credentials" }, "github": { "secretName": "gh-token" } },
            "resources": { "requests": { "cpu": "250m", "memory": "256Mi" } }
        });
        for (k, v) in extra.as_object().unwrap() {
            spec[k] = v.clone();
        }
        serde_json::from_value(json!({
            "apiVersion": "balerix.ai/v1alpha1",
            "kind": "Daemon",
            "metadata": { "name": "default", "namespace": "team-a", "uid": "daemon-uid", "generation": 4 },
            "spec": spec
        }))
        .unwrap()
    }

    fn cfg() -> OperatorConfig {
        OperatorConfig {
            version: "0.2.0".into(),
            images: Images::for_version("0.2.0"),
            namespace: "balerix-system".into(),
        }
    }

    fn at(secs: i64) -> Time {
        Time(k8s_openapi::jiff::Timestamp::from_second(secs).unwrap())
    }

    fn status_of(daemon: &Daemon, claim_phase: Option<&str>, ready: i32, pool: JobOutcome) -> DaemonStatus {
        let claim: Option<PersistentVolumeClaim> = claim_phase.map(|p| {
            serde_json::from_value(json!({ "metadata": { "name": "c" }, "status": { "phase": p } })).unwrap()
        });
        let set: StatefulSet = serde_json::from_value(json!({
            "metadata": { "name": "s" }, "status": { "replicas": 1, "readyReplicas": ready }
        }))
        .unwrap();
        daemon_status(
            daemon,
            &cfg(),
            &DaemonObserved {
                shared_claim: claim.as_ref(),
                statefulset: Some(&set),
                pool: &pool,
            },
            &at(1_800_000_000),
        )
    }

    fn cond<'a>(status: &'a DaemonStatus, type_: &str) -> (&'a str, &'a str, &'a str) {
        let c = status.conditions.iter().find(|c| c.type_ == type_).unwrap();
        (c.status.as_str(), c.reason.as_str(), c.message.as_str())
    }

    #[test]
    fn the_daemons_objects() {
        let o = daemon_objects(&daemon(json!({})), &cfg(), 1_807_776_000).unwrap();
        insta::assert_yaml_snapshot!("daemon_claims", o.claims);
        insta::assert_yaml_snapshot!("daemon_service", o.service);
        insta::assert_yaml_snapshot!("daemon_statefulset", o.statefulset);
        insta::assert_yaml_snapshot!("daemon_policy", o.policy);
        assert_eq!(o.pool_job.metadata.name.as_deref(), Some("balerix-default-pool"));
        // a renewed certificate rolls the pod, which reads it only at start
        let renewed = daemon_objects(&daemon(json!({})), &cfg(), 1_815_552_000).unwrap();
        assert_ne!(
            serde_json::to_value(&renewed.statefulset).unwrap()["spec"]["template"]["metadata"]["annotations"],
            serde_json::to_value(&o.statefulset).unwrap()["spec"]["template"]["metadata"]["annotations"]
        );
    }

    #[test]
    fn the_daemons_secrets_hold_the_material_and_its_expiry() {
        let authority = Issued { cert_pem: "CA CERT".into(), key_pem: "CA KEY".into(), not_after: 2_115_360_000 };
        let serving = Issued { cert_pem: "TLS CERT".into(), key_pem: "TLS KEY".into(), not_after: 1_807_776_000 };
        let s = daemon_secrets(
            &daemon(json!({})),
            &Material { authority: &authority, serving: &serving, admin_token: "ADMIN TOKEN" },
        )
        .unwrap();
        insta::assert_yaml_snapshot!("daemon_secret_authority", s.authority);
        insta::assert_yaml_snapshot!("daemon_config_authority", s.authority_config);
        insta::assert_yaml_snapshot!("daemon_secret_serving", s.serving);
        insta::assert_yaml_snapshot!("daemon_secret_admin", s.admin);
        let config = serde_json::to_string(&s.authority_config).unwrap();
        assert!(config.contains("CA CERT") && !config.contains("CA KEY"), "{config}");
    }

    #[test]
    fn ready_needs_storage_the_pool_no_plugins_and_the_pod() {
        let d = daemon(json!({}));
        let s = status_of(&d, Some("Bound"), 1, JobOutcome::Succeeded("synced".into()));
        assert_eq!(cond(&s, "StorageReady"), ("True", "Bound", ""));
        assert_eq!(cond(&s, "SystemToolsReady"), ("True", "PoolSynced", ""));
        assert_eq!(cond(&s, "PluginsReady"), ("True", "NoPlugins", ""));
        assert_eq!(cond(&s, "Ready"), ("True", "Ready", ""));
        assert_eq!(s.endpoint.as_deref(), Some("https://balerix-default.team-a.svc:7643"));
        assert_eq!(s.observed_generation, Some(4));

        let s = status_of(&d, Some("Pending"), 1, JobOutcome::Succeeded(String::new()));
        assert_eq!(
            cond(&s, "StorageReady"),
            ("False", "ClaimPending", "claim balerix-default-shared is Pending; its class must provision ReadWriteMany")
        );
        assert_eq!(cond(&s, "Ready").0, "False");
        assert_eq!(cond(&s, "Ready").1, "ClaimPending");
        let s = status_of(&d, None, 1, JobOutcome::Absent);
        assert_eq!(cond(&s, "StorageReady").2, "claim balerix-default-shared does not exist yet");

        let s = status_of(&d, Some("Bound"), 1, JobOutcome::Running);
        assert_eq!(cond(&s, "SystemToolsReady"), ("False", "PoolSyncRunning", ""));
        let s = status_of(&d, Some("Bound"), 1, JobOutcome::Failed("tools: system: boom".into()));
        assert_eq!(cond(&s, "SystemToolsReady"), ("False", "PoolSyncFailed", "tools: system: boom"));
        assert_eq!(cond(&s, "Ready"), ("False", "PoolSyncFailed", "tools: system: boom"));

        let s = status_of(&d, Some("Bound"), 0, JobOutcome::Succeeded(String::new()));
        assert_eq!(cond(&s, "Ready"), ("False", "DaemonNotReady", "the daemon pod is not ready"));
    }

    /// §20.2: a Daemon never reports Ready over a plugin list nothing acts on.
    #[test]
    fn a_plugin_list_is_unsupported_until_sub_project_four() {
        let d = daemon(json!({ "plugins": ["flow", "web"] }));
        let s = status_of(&d, Some("Bound"), 1, JobOutcome::Succeeded(String::new()));
        assert_eq!(
            cond(&s, "PluginsReady"),
            ("False", "PluginsUnsupported", "this operator runs no plugins yet; remove spec.plugins (flow, web)")
        );
        assert_eq!(cond(&s, "Ready").1, "PluginsUnsupported");
    }

    #[test]
    fn another_version_is_a_mismatch_and_none_is_the_operators() {
        assert!(version_ok(&daemon(json!({})), &cfg()));
        assert!(version_ok(&daemon(json!({ "version": "0.2.0" })), &cfg()));
        let d = daemon(json!({ "version": "0.3.0" }));
        assert!(!version_ok(&d, &cfg()));
        let s = status_of(&d, Some("Bound"), 1, JobOutcome::Succeeded(String::new()));
        assert_eq!(
            cond(&s, "Ready"),
            ("False", "VersionMismatch", "spec.version 0.3.0 is not this operator's 0.2.0")
        );
        assert_eq!(cond(&s, "StorageReady").0, "Unknown");
    }
}
```

Run: `scripts/operator.sh check`
Expected: compile errors.

- [ ] **Step 2: Implement `operator/src/desired/daemon.rs`** (above its tests)

```rust
//! A Daemon object into what runs it (Spec O §5.1): two claims, a
//! StatefulSet of one `balerix serve --mode kubernetes`, its Service, its
//! NetworkPolicy and the Job that installs the daemon pool. The Secrets
//! come from material the controller minted (`pki`); nothing random is
//! made here.

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use serde_json::{Value, json};

use super::common::{
    Cond, DAEMON_PORT, DesiredError, JobOutcome, MANAGER, OperatorConfig, conditions,
    container_security, labels, owner_of, pod_security, typed,
};
use super::jobs::{JobContext, daemon_pool_job};
use super::names;
use crate::api::{ClaimSpec, Daemon, DaemonStatus};
use crate::pki::Issued;

/// On a certificate's Secret: when it expires, unix seconds. On the
/// daemon's pod template: its serving certificate's, so a renewal rolls
/// the pod (the daemon reads its certificate at start).
pub const NOT_AFTER_ANNOTATION: &str = "balerix.ai/not-after";

/// What the controller minted or read back for one Daemon.
pub struct Material<'a> {
    pub authority: &'a Issued,
    pub serving: &'a Issued,
    pub admin_token: &'a str,
}

pub struct DaemonSecrets {
    /// `ca.crt` and `ca.key`: the operator's alone.
    pub authority: Secret,
    /// `ca.crt`: what sidecars and plugins mount to trust the Daemon.
    pub authority_config: ConfigMap,
    /// `tls.crt` and `tls.key`, mounted in the daemon pod.
    pub serving: Secret,
    /// `token`: the admin token, for the daemon and the operator.
    pub admin: Secret,
}

#[derive(Debug, Clone)]
pub struct DaemonObjects {
    pub claims: Vec<PersistentVolumeClaim>,
    pub service: Service,
    pub statefulset: StatefulSet,
    pub policy: NetworkPolicy,
    pub pool_job: Job,
}

pub struct DaemonObserved<'a> {
    pub shared_claim: Option<&'a PersistentVolumeClaim>,
    pub statefulset: Option<&'a StatefulSet>,
    pub pool: &'a JobOutcome,
}

fn name_of(daemon: &Daemon) -> Result<(&str, &str), DesiredError> {
    let name = daemon
        .metadata
        .name
        .as_deref()
        .ok_or(DesiredError::Missing("the Daemon", "metadata.name"))?;
    let namespace = daemon
        .metadata
        .namespace
        .as_deref()
        .ok_or(DesiredError::Missing("the Daemon", "metadata.namespace"))?;
    Ok((name, namespace))
}

/// §4.1: in `v1alpha1` a Daemon's version is the operator's, or absent.
pub fn version_ok(daemon: &Daemon, cfg: &OperatorConfig) -> bool {
    daemon
        .spec
        .version
        .as_ref()
        .is_none_or(|v| *v == cfg.version)
}

fn secret(
    daemon: &Daemon,
    name: String,
    type_: &str,
    data: Value,
    not_after: Option<i64>,
) -> Result<Secret, DesiredError> {
    let (daemon_name, namespace) = name_of(daemon)?;
    let mut annotations = json!({});
    if let Some(t) = not_after {
        annotations[NOT_AFTER_ANNOTATION] = json!(t.to_string());
    }
    typed(json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": labels(daemon_name, "daemon", &[]),
            "annotations": annotations,
            "ownerReferences": [owner_of(daemon)?],
        },
        "type": type_,
        "stringData": data,
    }))
}

pub fn daemon_secrets(
    daemon: &Daemon,
    material: &Material<'_>,
) -> Result<DaemonSecrets, DesiredError> {
    let (name, namespace) = name_of(daemon)?;
    Ok(DaemonSecrets {
        authority: secret(
            daemon,
            names::authority(name),
            "Opaque",
            json!({ "ca.crt": material.authority.cert_pem, "ca.key": material.authority.key_pem }),
            Some(material.authority.not_after),
        )?,
        authority_config: typed(json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": names::authority(name),
                "namespace": namespace,
                "labels": labels(name, "daemon", &[]),
                "ownerReferences": [owner_of(daemon)?],
            },
            "data": { "ca.crt": material.authority.cert_pem },
        }))?,
        serving: secret(
            daemon,
            names::serving(name),
            "kubernetes.io/tls",
            json!({ "tls.crt": material.serving.cert_pem, "tls.key": material.serving.key_pem }),
            Some(material.serving.not_after),
        )?,
        admin: secret(
            daemon,
            names::admin(name),
            "Opaque",
            json!({ "token": material.admin_token }),
            None,
        )?,
    })
}

/// A claim carries no owner reference: deleting a Daemon leaves its
/// state and its shared volume, as a StatefulSet leaves its claims.
fn claim(
    daemon: &str,
    namespace: &str,
    name: String,
    spec: &ClaimSpec,
    mode: &str,
) -> Result<PersistentVolumeClaim, DesiredError> {
    let mut claim_spec = json!({
        "accessModes": [mode],
        "resources": { "requests": { "storage": spec.size } },
    });
    if let Some(class) = &spec.storage_class_name {
        claim_spec["storageClassName"] = json!(class);
    }
    typed(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": { "name": name, "namespace": namespace, "labels": labels(daemon, "daemon", &[]) },
        "spec": claim_spec,
    }))
}

pub fn daemon_objects(
    daemon: &Daemon,
    cfg: &OperatorConfig,
    serving_not_after: i64,
) -> Result<DaemonObjects, DesiredError> {
    let (name, namespace) = name_of(daemon)?;
    let owner = owner_of(daemon)?;
    let object = names::daemon(name);
    let labels = labels(name, "daemon", &[]);
    let selector = json!({ "balerix.ai/daemon": name, "balerix.ai/component": "daemon" });
    let metadata = json!({
        "name": object,
        "namespace": namespace,
        "labels": labels,
        "ownerReferences": [owner],
    });
    let storage = &daemon.spec.storage;
    Ok(DaemonObjects {
        claims: vec![
            claim(name, namespace, names::state_claim(name), &storage.state, "ReadWriteOnce")?,
            claim(name, namespace, names::shared_claim(name), &storage.shared, "ReadWriteMany")?,
        ],
        service: typed(json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": metadata,
            "spec": {
                "selector": selector,
                "ports": [{ "name": "https", "port": DAEMON_PORT, "targetPort": "https" }],
            },
        }))?,
        statefulset: typed(json!({
            "apiVersion": "apps/v1",
            "kind": "StatefulSet",
            "metadata": metadata,
            "spec": {
                "replicas": 1,
                "serviceName": object,
                "selector": { "matchLabels": selector },
                "template": {
                    "metadata": {
                        "labels": labels,
                        "annotations": { NOT_AFTER_ANNOTATION: serving_not_after.to_string() },
                    },
                    "spec": {
                        "automountServiceAccountToken": false,
                        "enableServiceLinks": false,
                        "securityContext": pod_security(),
                        "containers": [{
                            "name": "daemon",
                            "image": cfg.images.daemon,
                            "command": ["balerix"],
                            "args": [
                                "serve", "--mode", "kubernetes",
                                "--bind", format!("0.0.0.0:{DAEMON_PORT}"),
                                "--tmux-socket", "unused",
                                "--tls-cert", "/balerix/tls/tls.crt",
                                "--tls-key", "/balerix/tls/tls.key",
                                "--admin-token-file", "/balerix/admin/token",
                            ],
                            // its XDG roots: the state claim, the only writable path
                            "env": [{ "name": "HOME", "value": "/balerix/state" }],
                            "ports": [{ "name": "https", "containerPort": DAEMON_PORT }],
                            "readinessProbe": {
                                "httpGet": { "path": "/readyz", "port": "https", "scheme": "HTTPS" },
                                "periodSeconds": 5,
                            },
                            "resources": daemon.spec.resources,
                            "securityContext": container_security(),
                            "volumeMounts": [
                                { "name": "state", "mountPath": "/balerix/state" },
                                { "name": "tls", "mountPath": "/balerix/tls", "readOnly": true },
                                { "name": "admin", "mountPath": "/balerix/admin", "readOnly": true },
                            ],
                        }],
                        "volumes": [
                            { "name": "state", "persistentVolumeClaim": { "claimName": names::state_claim(name) } },
                            { "name": "tls", "secret": { "secretName": names::serving(name), "defaultMode": 0o440 } },
                            { "name": "admin", "secret": { "secretName": names::admin(name), "defaultMode": 0o440 } },
                        ],
                    },
                },
            },
        }))?,
        // §10.2: its agents and Jobs (and, later, its plugins) carry the
        // Daemon's label; the operator is named by its namespace and name.
        policy: typed(json!({
            "apiVersion": "networking.k8s.io/v1",
            "kind": "NetworkPolicy",
            "metadata": metadata,
            "spec": {
                "podSelector": { "matchLabels": selector },
                "policyTypes": ["Ingress"],
                "ingress": [{
                    "from": [
                        { "podSelector": { "matchLabels": { "balerix.ai/daemon": name } } },
                        {
                            "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": cfg.namespace } },
                            "podSelector": { "matchLabels": { "app.kubernetes.io/name": MANAGER } },
                        },
                    ],
                    "ports": [{ "protocol": "TCP", "port": DAEMON_PORT }],
                }],
            },
        }))?,
        pool_job: daemon_pool_job(&JobContext {
            namespace,
            daemon: name,
            images: &cfg.images,
            owner: owner_of(daemon)?,
        })?,
    })
}

/// `StorageReady`, `SystemToolsReady`, `PluginsReady`, `Ready` (§4.1).
pub fn daemon_status(
    daemon: &Daemon,
    cfg: &OperatorConfig,
    observed: &DaemonObserved<'_>,
    now: &Time,
) -> DaemonStatus {
    let name = daemon.metadata.name.as_deref().unwrap_or_default();
    let namespace = daemon.metadata.namespace.as_deref().unwrap_or_default();
    let new = if version_ok(daemon, cfg) {
        let shared = names::shared_claim(name);
        let phase = observed
            .shared_claim
            .and_then(|c| c.status.as_ref())
            .and_then(|s| s.phase.as_deref());
        let storage = match (observed.shared_claim, phase) {
            (Some(_), Some("Bound")) => Cond::yes("StorageReady", "Bound", ""),
            (Some(_), phase) => Cond::no(
                "StorageReady",
                "ClaimPending",
                &format!(
                    "claim {shared} is {}; its class must provision ReadWriteMany",
                    phase.unwrap_or("Pending")
                ),
            ),
            (None, _) => Cond::no(
                "StorageReady",
                "ClaimPending",
                &format!("claim {shared} does not exist yet"),
            ),
        };
        let tools = match observed.pool {
            JobOutcome::Succeeded(_) => Cond::yes("SystemToolsReady", "PoolSynced", ""),
            JobOutcome::Failed(message) => Cond::no("SystemToolsReady", "PoolSyncFailed", message),
            JobOutcome::Absent | JobOutcome::Stale | JobOutcome::Running => {
                Cond::no("SystemToolsReady", "PoolSyncRunning", "")
            }
        };
        // Sub-project 4 replaces this branch with the plugin list (§20.2).
        let plugins = if daemon.spec.plugins.is_empty() {
            Cond::yes("PluginsReady", "NoPlugins", "")
        } else {
            Cond::no(
                "PluginsReady",
                "PluginsUnsupported",
                &format!(
                    "this operator runs no plugins yet; remove spec.plugins ({})",
                    daemon.spec.plugins.join(", ")
                ),
            )
        };
        let pod_ready = observed
            .statefulset
            .and_then(|s| s.status.as_ref())
            .and_then(|s| s.ready_replicas)
            .unwrap_or(0)
            >= 1;
        let ready = match [&storage, &tools, &plugins]
            .into_iter()
            .find(|c| c.status != Some(true))
        {
            Some(failing) => Cond::no("Ready", &failing.reason, &failing.message),
            None if pod_ready => Cond::yes("Ready", "Ready", ""),
            None => Cond::no("Ready", "DaemonNotReady", "the daemon pod is not ready"),
        };
        vec![storage, tools, plugins, ready]
    } else {
        let message = format!(
            "spec.version {} is not this operator's {}",
            daemon.spec.version.as_deref().unwrap_or_default(),
            cfg.version
        );
        vec![
            Cond::unknown("StorageReady", "VersionMismatch", ""),
            Cond::unknown("SystemToolsReady", "VersionMismatch", ""),
            Cond::unknown("PluginsReady", "VersionMismatch", ""),
            Cond::no("Ready", "VersionMismatch", &message),
        ]
    };
    let old = daemon
        .status
        .as_ref()
        .map(|s| s.conditions.as_slice())
        .unwrap_or_default();
    DaemonStatus {
        observed_generation: daemon.metadata.generation,
        conditions: conditions(old, &new, daemon.metadata.generation, now),
        endpoint: Some(names::endpoint(namespace, name)),
    }
}
```

Add `pub mod daemon;` to `desired/mod.rs`.

- [ ] **Step 3: Run, read the snapshots, accept**

Run: `scripts/operator.sh check`. Read the eight `.snap.new` files:

- `daemon_claims`: `balerix-default-state` (`ReadWriteOnce`, `standard`, `5Gi`) and `balerix-default-shared` (`ReadWriteMany`, `efs`, `100Gi`); neither has `ownerReferences`.
- `daemon_service`: `balerix-default`, port `7643` named `https`, selector the two `balerix.ai/` labels.
- `daemon_statefulset`: `replicas: 1`; image `ghcr.io/balerix-ai/balerix:0.2.0`; args exactly `serve --mode kubernetes --bind 0.0.0.0:7643 --tmux-socket unused --tls-cert /balerix/tls/tls.crt --tls-key /balerix/tls/tls.key --admin-token-file /balerix/admin/token`; `HOME=/balerix/state`; the readiness probe on `/readyz` with `scheme: HTTPS`; the hardened security contexts; the template annotation `balerix.ai/not-after: "1807776000"`; three volumes; the `requests` from `spec.resources`. No token and no key in it.
- `daemon_policy`: `policyTypes: [Ingress]`, one rule with two peers (the Daemon's label; namespace `balerix-system` with `app.kubernetes.io/name: balerix-operator`), port `7643`.
- `daemon_secret_authority`: `stringData` `ca.crt: CA CERT`, `ca.key: CA KEY`; annotation `balerix.ai/not-after: "2115360000"`. `daemon_config_authority`: `ca.crt: CA CERT` only. `daemon_secret_serving`: `type: kubernetes.io/tls`, `tls.crt`, `tls.key`, annotation `"1807776000"`. `daemon_secret_admin`: `token: ADMIN TOKEN`, no expiry annotation. All four owned by the Daemon.

Run: `mise x -- cargo insta accept --manifest-path operator/Cargo.toml && mise run operator`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add operator
git commit -m "feat(operator): a Daemon into its claims, StatefulSet, Service, policy, Secrets and status (Spec O §5.1, §20.2)"
```

---

### Task 10: An Agent into its claim, its bundle, its policy, its pod and its status

**Files:**
- Create: `operator/src/desired/agent.rs`
- Modify: `operator/src/desired/mod.rs`

**Interfaces:**
- Consumes: `api::{Agent, Daemon, AgentStatus}`, `balerix_api::{AgentBundle, AgentSettings, CredentialBundle, RunnerSettings}`, `jobs::SHARED`, `common::*`, `names`.
- Produces:
  - `struct AgentInputs<'a> { pub agent: &'a Agent, pub daemon: &'a Daemon, pub token: &'a str, pub credentials: &'a balerix_api::CredentialBundle, pub cfg: &'a OperatorConfig }` (no `Debug`)
  - `struct AgentObjects { pub claim: PersistentVolumeClaim, pub bundle: Secret, pub policy: NetworkPolicy, pub pod: Pod }` (no `Debug`)
  - `agent_objects(inputs: &AgentInputs) -> Result<AgentObjects, DesiredError>`
  - `agent_status(agent: &Agent, pod: Option<&Pod>, reported: Option<&balerix_api::AgentStatus>, now: &Time) -> AgentStatus`
  - `pub const SPEC_HASH_ANNOTATION: &str = "balerix.ai/spec-hash"`

- [ ] **Step 1: Write the failing tests**

At the bottom of `operator/src/desired/agent.rs`:

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use balerix_api::{AgentPhase, CredentialBundle, PluginActivation};
    use serde_json::{Value, json};

    use super::*;
    use crate::desired::common::Images;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn agent(runner: Value) -> Agent {
        serde_json::from_value(json!({
            "apiVersion": "balerix.ai/v1alpha1",
            "kind": "Agent",
            "metadata": {
                "name": "payments-backend-alice", "namespace": "team-a", "uid": "agent-uid", "generation": 2,
                "labels": { "balerix.ai/fleet": "payments", "balerix.ai/crew": "backend", "balerix.ai/agent": "alice" }
            },
            "spec": {
                "daemon": "default", "fleet": "payments", "crew": "backend", "agent": "alice",
                "repo": "acme/payments-api", "ref": "main", "git": { "push": true, "auth": "gh" },
                "settings": { "claude": { "settings": { "model": "sonnet" } }, "runner": runner },
                "specHash": "abc123"
            }
        }))
        .unwrap()
    }

    fn daemon() -> Daemon {
        serde_json::from_value(json!({
            "apiVersion": "balerix.ai/v1alpha1",
            "kind": "Daemon",
            "metadata": { "name": "default", "namespace": "team-a", "uid": "daemon-uid" },
            "spec": { "storage": {
                "state": { "size": "5Gi" },
                "shared": { "storageClassName": "efs", "size": "100Gi" },
                "agent": { "storageClassName": "standard", "size": "20Gi" }
            } }
        }))
        .unwrap()
    }

    fn cfg() -> OperatorConfig {
        OperatorConfig {
            version: "0.2.0".into(),
            images: Images::for_version("0.2.0"),
            namespace: "balerix-system".into(),
        }
    }

    fn objects(runner: Value) -> AgentObjects {
        let credentials: CredentialBundle =
            serde_json::from_value(json!({ "gh_token": "gho_SECRET" })).unwrap();
        agent_objects(&AgentInputs {
            agent: &agent(runner),
            daemon: &daemon(),
            token: TOKEN,
            credentials: &credentials,
            cfg: &cfg(),
        })
        .unwrap()
    }

    #[test]
    fn the_agents_objects() {
        let o = objects(json!({
            "type": "pod",
            "resources": { "requests": { "cpu": "1", "memory": "2Gi" } },
            "storage": { "size": "40Gi" },
            "nodeSelector": { "pool": "agents" },
            "tolerations": [{ "key": "agents", "operator": "Exists" }]
        }));
        insta::assert_yaml_snapshot!("agent_claim", o.claim);
        insta::assert_yaml_snapshot!("agent_policy", o.policy);
        insta::assert_yaml_snapshot!("agent_pod", o.pod);
        // the pod names its secrets; it never holds them
        let pod = serde_json::to_string(&o.pod).unwrap();
        assert!(!pod.contains(TOKEN) && !pod.contains("gho_SECRET"), "{pod}");
    }

    #[test]
    fn the_bundle_is_what_the_sidecar_loads() {
        let o = objects(json!({ "type": "pod" }));
        let secret = serde_json::to_value(&o.bundle).unwrap();
        assert_eq!(secret["metadata"]["name"], "payments-backend-alice-bundle");
        assert_eq!(secret["metadata"]["ownerReferences"][0]["kind"], "Agent");
        let text = secret["stringData"]["agent.json"].as_str().unwrap();
        let bundle: balerix_api::AgentBundle = serde_json::from_str(text).unwrap();
        assert_eq!(bundle.agent, "payments/backend/alice");
        assert_eq!(bundle.repo, "acme/payments-api");
        assert_eq!(bundle.git_ref, "main");
        assert_eq!(bundle.daemon_url, "https://balerix-default.team-a.svc:7643");
        assert_eq!(bundle.token, TOKEN);
        assert_eq!(bundle.credentials.gh_token.as_deref(), Some("gho_SECRET"));
        assert_eq!(bundle.settings.claude.settings["model"], "sonnet");
    }

    #[test]
    fn the_claim_is_the_daemons_size_unless_the_runner_sets_one() {
        let size = |o: &AgentObjects| {
            serde_json::to_value(&o.claim).unwrap()["spec"]["resources"]["requests"]["storage"].clone()
        };
        assert_eq!(size(&objects(json!({ "type": "pod" }))), "20Gi");
        assert_eq!(size(&objects(json!({ "type": "pod", "storage": { "size": "40Gi" } }))), "40Gi");
    }

    #[test]
    fn a_tmux_agent_object_is_refused() {
        let credentials = CredentialBundle::default();
        let e = agent_objects(&AgentInputs {
            agent: &agent(json!({ "type": "tmux" })),
            daemon: &daemon(),
            token: TOKEN,
            credentials: &credentials,
            cfg: &cfg(),
        })
        .err()
        .unwrap();
        assert_eq!(e.to_string(), "the Agent has no pod runner in spec.settings");
    }

    fn at(secs: i64) -> Time {
        Time(k8s_openapi::jiff::Timestamp::from_second(secs).unwrap())
    }

    fn pod(status: Value) -> Pod {
        serde_json::from_value(json!({ "metadata": { "name": "payments-backend-alice" }, "status": status }))
            .unwrap()
    }

    fn sidecar(ready: bool, state: Value, last: Value) -> Value {
        json!({ "name": "sidecar", "ready": ready, "restartCount": 0, "image": "i", "imageID": "",
                "state": state, "lastState": last })
    }

    fn reported(phase: AgentPhase, message: &str, restarts: u32) -> balerix_api::AgentStatus {
        balerix_api::AgentStatus {
            phase,
            message: message.into(),
            restarts,
            plugins: [("flow".to_string(), PluginActivation::active())].into(),
            ..Default::default()
        }
    }

    fn cond<'a>(status: &'a AgentStatus, type_: &str) -> (&'a str, &'a str, &'a str) {
        let c = status.conditions.iter().find(|c| c.type_ == type_).unwrap();
        (c.status.as_str(), c.reason.as_str(), c.message.as_str())
    }

    #[test]
    fn a_ready_sidecar_is_a_ready_agent_with_the_daemons_phase() {
        let p = pod(json!({
            "conditions": [{ "type": "PodScheduled", "status": "True" }],
            "initContainerStatuses": [sidecar(true, json!({ "running": {} }), json!({}))]
        }));
        let s = agent_status(&agent(json!({ "type": "pod" })), Some(&p), Some(&reported(AgentPhase::Ready, "", 2)), &at(1));
        assert_eq!(cond(&s, "Scheduled"), ("True", "Scheduled", ""));
        assert_eq!(cond(&s, "Materialized"), ("True", "Materialized", ""));
        assert_eq!(cond(&s, "Ready"), ("True", "Ready", ""));
        assert_eq!(s.phase.as_deref(), Some("ready"));
        assert_eq!(s.restarts, Some(2));
        assert_eq!(s.pod.as_deref(), Some("payments-backend-alice"));
        assert_eq!(s.plugins["flow"], "active");
        assert_eq!(s.observed_generation, Some(2));
    }

    #[test]
    fn a_pod_that_cannot_be_placed_or_pulled_is_not_scheduled() {
        let a = agent(json!({ "type": "pod" }));
        let s = agent_status(&a, None, None, &at(1));
        assert_eq!(cond(&s, "Scheduled"), ("False", "PodMissing", ""));
        assert_eq!(cond(&s, "Ready").0, "False");
        assert_eq!(s.pod, None);

        let unschedulable = pod(json!({ "conditions": [{
            "type": "PodScheduled", "status": "False", "reason": "Unschedulable",
            "message": "0/3 nodes are available: 3 Insufficient memory."
        }] }));
        let s = agent_status(&a, Some(&unschedulable), None, &at(1));
        assert_eq!(
            cond(&s, "Scheduled"),
            ("False", "Unschedulable", "0/3 nodes are available: 3 Insufficient memory.")
        );

        let pull = pod(json!({
            "conditions": [{ "type": "PodScheduled", "status": "True" }],
            "initContainerStatuses": [sidecar(
                false,
                json!({ "waiting": { "reason": "ImagePullBackOff", "message": "Back-off pulling image" } }),
                json!({})
            )]
        }));
        let s = agent_status(&a, Some(&pull), None, &at(1));
        assert_eq!(cond(&s, "Scheduled"), ("False", "ImagePullBackOff", "Back-off pulling image"));
    }

    /// §5.4, §11: the sidecar's termination message is the reason text.
    #[test]
    fn a_sidecar_that_ended_with_a_message_is_not_materialized() {
        let a = agent(json!({ "type": "pod" }));
        let ended = |message: &str| {
            pod(json!({
                "conditions": [{ "type": "PodScheduled", "status": "True" }],
                "initContainerStatuses": [sidecar(
                    false,
                    json!({ "waiting": { "reason": "CrashLoopBackOff" } }),
                    json!({ "terminated": { "exitCode": 1, "message": format!("{message}\n") } })
                )]
            }))
        };
        let s = agent_status(&a, Some(&ended("SandboxUnavailable: Landlock not available")), None, &at(1));
        assert_eq!(
            cond(&s, "Materialized"),
            ("False", "SandboxUnavailable", "SandboxUnavailable: Landlock not available")
        );
        assert_eq!(cond(&s, "Ready").0, "False");
        let s = agent_status(&a, Some(&ended("payments/backend/alice: git clone failed")), None, &at(1));
        assert_eq!(
            cond(&s, "Materialized"),
            ("False", "MaterializeFailed", "payments/backend/alice: git clone failed")
        );
        // an old message does not outlive a later success
        let recovered = pod(json!({
            "conditions": [{ "type": "PodScheduled", "status": "True" }],
            "initContainerStatuses": [sidecar(
                false,
                json!({ "running": {} }),
                json!({ "terminated": { "exitCode": 1, "message": "payments/backend/alice: git clone failed" } })
            )]
        }));
        let s = agent_status(&a, Some(&recovered), Some(&reported(AgentPhase::Starting, "", 0)), &at(1));
        assert_eq!(cond(&s, "Materialized"), ("True", "Materialized", ""));
        assert_eq!(cond(&s, "Ready"), ("False", "starting", ""));
        // running, nothing reported yet
        let fresh = pod(json!({
            "conditions": [{ "type": "PodScheduled", "status": "True" }],
            "initContainerStatuses": [sidecar(false, json!({ "running": {} }), json!({}))]
        }));
        let s = agent_status(&a, Some(&fresh), None, &at(1));
        assert_eq!(cond(&s, "Materialized"), ("Unknown", "Materializing", ""));
        assert_eq!(cond(&s, "Ready"), ("False", "NotReady", ""));
    }
}
```

Run: `scripts/operator.sh check`
Expected: compile errors.

- [ ] **Step 2: Implement `operator/src/desired/agent.rs`** (above its tests)

```rust
//! An Agent object into its pod and what the pod needs (Spec O §5.4,
//! §6.1): the claim that outlives the pod, the bundle Secret the sidecar
//! alone mounts, a NetworkPolicy with no ingress, and the two-container
//! pod. And the other way: a Pod and the Daemon's word into the Agent's
//! status.

use balerix_api::{AgentBundle, AgentSettings, CredentialBundle, GitSettings, RunnerSettings};
use k8s_openapi::api::core::v1::{ContainerStatus, PersistentVolumeClaim, Pod, Secret};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use serde::Serialize;
use serde_json::{Value, json};

use super::common::{
    Cond, DesiredError, OperatorConfig, conditions, container_security, labels, owner_of,
    pod_security, typed,
};
use super::jobs::SHARED;
use super::names;
use crate::api::{Agent, AgentStatus, Daemon};

/// On the pod: the Agent's `specHash` it was made from (§5.4).
pub const SPEC_HASH_ANNOTATION: &str = "balerix.ai/spec-hash";

pub struct AgentInputs<'a> {
    pub agent: &'a Agent,
    pub daemon: &'a Daemon,
    /// The agent's token, from its Secret (minted by the controller).
    pub token: &'a str,
    /// Read from the Secrets the Daemon names (§5.2 step 2).
    pub credentials: &'a CredentialBundle,
    pub cfg: &'a OperatorConfig,
}

pub struct AgentObjects {
    pub claim: PersistentVolumeClaim,
    pub bundle: Secret,
    pub policy: NetworkPolicy,
    pub pod: Pod,
}

pub fn agent_objects(inputs: &AgentInputs<'_>) -> Result<AgentObjects, DesiredError> {
    let agent = inputs.agent;
    let spec = &agent.spec;
    let name = agent
        .metadata
        .name
        .as_deref()
        .ok_or(DesiredError::Missing("the Agent", "metadata.name"))?;
    let namespace = agent
        .metadata
        .namespace
        .as_deref()
        .ok_or(DesiredError::Missing("the Agent", "metadata.namespace"))?;
    let settings: AgentSettings = typed(spec.settings.clone())?;
    let RunnerSettings::Pod {
        resources,
        storage,
        node_selector,
        tolerations,
    } = &settings.runner
    else {
        return Err(DesiredError::Missing("the Agent", "pod runner in spec.settings"));
    };
    let owner = owner_of(agent)?;
    let labels = labels(
        &spec.daemon,
        "agent",
        &[
            ("balerix.ai/fleet", spec.fleet.as_str()),
            ("balerix.ai/crew", spec.crew.as_str()),
            ("balerix.ai/agent", spec.agent.as_str()),
        ],
    );
    let metadata = json!({
        "name": name,
        "namespace": namespace,
        "labels": labels,
        "ownerReferences": [owner],
    });

    // §8.2: sized by the fleet's `runner.storage`, else the Daemon's
    // `storage.agent`; deleted with the Agent, after the harvest.
    let default = &inputs.daemon.spec.storage.agent;
    let size = storage
        .as_ref()
        .and_then(|s| s.get("size"))
        .and_then(Value::as_str)
        .unwrap_or(&default.size);
    let mut claim_spec = json!({
        "accessModes": ["ReadWriteOnce"],
        "resources": { "requests": { "storage": size } },
    });
    if let Some(class) = &default.storage_class_name {
        claim_spec["storageClassName"] = json!(class);
    }

    let bundle = AgentBundle {
        agent: format!("{}/{}/{}", spec.fleet, spec.crew, spec.agent),
        repo: spec.repo.clone(),
        git_ref: spec.git_ref.clone(),
        git: typed::<GitSettings>(spec.git.clone())?,
        settings: settings.clone(),
        daemon_url: names::endpoint(namespace, &spec.daemon),
        token: inputs.token.to_string(),
        credentials: inputs.credentials.clone(),
    };
    let mut bundle_metadata = metadata.clone();
    bundle_metadata["name"] = json!(names::bundle(name));

    // §8.1: the crew's slice, read-only at the mount (O-6). Whole pool
    // directories: each holds `mise/`, and the crew's also `no-hooks/`.
    let shared = |sub_path: String, at: &str| {
        json!({ "name": "shared", "mountPath": format!("{SHARED}/{at}"), "subPath": sub_path, "readOnly": true })
    };
    let slice = [
        shared(
            format!("{}/.git/objects", names::vol_crew_repo(&spec.fleet, &spec.crew)),
            "repo/.git/objects",
        ),
        shared(names::vol_crew_pool(&spec.fleet, &spec.crew), "crew"),
        shared(names::vol_fleet_pool(&spec.fleet), "fleet"),
        shared(names::vol_daemon_pool(), "daemon"),
    ];
    let both = |extra: Vec<Value>| -> Vec<Value> {
        let mut mounts = vec![json!({ "name": "agent", "mountPath": "/balerix/agent" })];
        mounts.extend(slice.iter().cloned());
        mounts.push(json!({ "name": "run", "mountPath": "/balerix/run" }));
        mounts.extend(extra);
        mounts
    };
    let mut pod_metadata = metadata.clone();
    pod_metadata["annotations"] = json!({ SPEC_HASH_ANNOTATION: spec.spec_hash });

    Ok(AgentObjects {
        claim: typed(json!({
            "apiVersion": "v1",
            "kind": "PersistentVolumeClaim",
            "metadata": metadata,
            "spec": claim_spec,
        }))?,
        bundle: typed(json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": bundle_metadata,
            "type": "Opaque",
            "stringData": { "agent.json": serde_json::to_string(&bundle)? },
        }))?,
        // §10.2, O-10: no inbound connection at all; egress is nono's to
        // restrict (`sandbox.network`), so it holds where the network
        // plugin ignores policies.
        policy: typed(json!({
            "apiVersion": "networking.k8s.io/v1",
            "kind": "NetworkPolicy",
            "metadata": metadata,
            "spec": {
                "podSelector": { "matchLabels": {
                    "balerix.ai/fleet": spec.fleet,
                    "balerix.ai/crew": spec.crew,
                    "balerix.ai/agent": spec.agent,
                } },
                "policyTypes": ["Ingress"],
                "ingress": [],
            },
        }))?,
        pod: typed(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": pod_metadata,
            "spec": {
                "restartPolicy": "Always",
                "automountServiceAccountToken": false,
                "enableServiceLinks": false,
                "securityContext": pod_security(),
                "nodeSelector": node_selector,
                "tolerations": tolerations,
                // a native sidecar: an init container that keeps running
                "initContainers": [{
                    "name": "sidecar",
                    "image": inputs.cfg.images.agent,
                    "restartPolicy": "Always",
                    "command": ["balerix-agent", "sidecar"],
                    "resources": { "requests": { "cpu": "50m", "memory": "64Mi" } },
                    "securityContext": container_security(),
                    // agent pods accept no connection (O-10): a file
                    "readinessProbe": {
                        "exec": { "command": ["test", "-f", "/balerix/run/ready"] },
                        "periodSeconds": 5,
                    },
                    "volumeMounts": both(vec![
                        json!({ "name": "bundle", "mountPath": "/balerix/secret", "readOnly": true }),
                        json!({ "name": "authority", "mountPath": "/balerix/tls", "readOnly": true }),
                    ]),
                }],
                "containers": [{
                    "name": "agent",
                    "image": inputs.cfg.images.agent,
                    "command": ["balerix-agent", "run"],
                    "resources": resources,
                    "securityContext": container_security(),
                    "volumeMounts": both(vec![]),
                }],
                "volumes": [
                    { "name": "agent", "persistentVolumeClaim": { "claimName": name } },
                    { "name": "shared", "persistentVolumeClaim": {
                        "claimName": names::shared_claim(&spec.daemon), "readOnly": true } },
                    { "name": "run", "emptyDir": {} },
                    { "name": "bundle", "secret": { "secretName": names::bundle(name), "defaultMode": 0o440 } },
                    { "name": "authority", "configMap": { "name": names::authority(&spec.daemon) } },
                ],
            },
        }))?,
    })
}

/// A serde `lowercase` enum as its wire word (`ready`, `active`).
fn word(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn sidecar(pod: &Pod) -> Option<&ContainerStatus> {
    pod.status
        .as_ref()?
        .init_container_statuses
        .iter()
        .flatten()
        .find(|c| c.name == "sidecar")
}

/// The sidecar's last termination message, current state first.
fn termination_message(sidecar: &ContainerStatus) -> Option<String> {
    [sidecar.state.as_ref(), sidecar.last_state.as_ref()]
        .into_iter()
        .flatten()
        .find_map(|s| s.terminated.as_ref()?.message.clone())
        .map(|m| m.trim_end().to_string())
        .filter(|m| !m.is_empty())
}

const PULL_FAILURES: [&str; 3] = ["ImagePullBackOff", "ErrImagePull", "InvalidImageName"];

fn scheduled(pod: Option<&Pod>) -> Cond {
    let Some(status) = pod.and_then(|p| p.status.as_ref()) else {
        return Cond::no("Scheduled", "PodMissing", "");
    };
    let placed = status
        .conditions
        .iter()
        .flatten()
        .find(|c| c.type_ == "PodScheduled");
    if let Some(c) = placed
        && c.status == "False"
    {
        return Cond::no(
            "Scheduled",
            c.reason.as_deref().unwrap_or("Unschedulable"),
            c.message.as_deref().unwrap_or_default(),
        );
    }
    let pull = status
        .init_container_statuses
        .iter()
        .flatten()
        .chain(status.container_statuses.iter().flatten())
        .filter_map(|c| c.state.as_ref()?.waiting.as_ref())
        .find(|w| w.reason.as_deref().is_some_and(|r| PULL_FAILURES.contains(&r)));
    if let Some(w) = pull {
        return Cond::no(
            "Scheduled",
            w.reason.as_deref().unwrap_or_default(),
            w.message.as_deref().unwrap_or_default(),
        );
    }
    match placed {
        Some(_) => Cond::yes("Scheduled", "Scheduled", ""),
        None => Cond::unknown("Scheduled", "Pending", ""),
    }
}

/// `Scheduled`, `Materialized`, `Ready` from the Pod (§5.4), and the
/// phase, restarts and plugin states from the Daemon's status of the
/// agent, which is the sidecar's own (§7.4).
pub fn agent_status(
    agent: &Agent,
    pod: Option<&Pod>,
    reported: Option<&balerix_api::AgentStatus>,
    now: &Time,
) -> AgentStatus {
    use balerix_api::AgentPhase as P;
    let side = pod.and_then(sidecar);
    let past_materialize = reported
        .is_some_and(|r| matches!(r.phase, P::Starting | P::Ready | P::Dead | P::Stopped));
    let materialized = if past_materialize {
        Cond::yes("Materialized", "Materialized", "")
    } else if let Some(message) = side.and_then(termination_message) {
        let reason = if message.starts_with("SandboxUnavailable") {
            "SandboxUnavailable"
        } else {
            "MaterializeFailed"
        };
        Cond::no("Materialized", reason, &message)
    } else {
        Cond::unknown("Materialized", "Materializing", "")
    };
    let ready = if side.is_some_and(|s| s.ready) {
        Cond::yes("Ready", "Ready", "")
    } else {
        match reported {
            Some(r) => Cond::no("Ready", &word(&r.phase), &r.message),
            None => Cond::no("Ready", "NotReady", ""),
        }
    };
    let old = agent
        .status
        .as_ref()
        .map(|s| s.conditions.as_slice())
        .unwrap_or_default();
    AgentStatus {
        observed_generation: agent.metadata.generation,
        conditions: conditions(
            old,
            &[scheduled(pod), materialized, ready],
            agent.metadata.generation,
            now,
        ),
        phase: reported.map(|r| word(&r.phase)),
        pod: pod.and_then(|p| p.metadata.name.clone()),
        restarts: reported.map(|r| i64::from(r.restarts)),
        session: None,
        plugins: reported
            .map(|r| {
                r.plugins
                    .iter()
                    .map(|(name, activation)| (name.clone(), word(&activation.state)))
                    .collect()
            })
            .unwrap_or_default(),
    }
}
```

`DesiredError::Missing("the Agent", "pod runner in spec.settings")` displays as `the Agent has no pod runner in spec.settings`, which the test pins. Add `pub mod agent;` to `desired/mod.rs`.

- [ ] **Step 3: Run, read the snapshots, accept**

Run: `scripts/operator.sh check`. Read the three `.snap.new` files:

- `agent_claim`: `payments-backend-alice`, `ReadWriteOnce`, `storageClassName: standard`, `storage: 40Gi`, owned by the Agent.
- `agent_policy`: `policyTypes: [Ingress]`, `ingress: []`, the selector the three name labels.
- `agent_pod`: annotation `balerix.ai/spec-hash: abc123`; `automountServiceAccountToken: false`; pod security context as in every Job; `nodeSelector: {pool: agents}` and the toleration; `initContainers[0]` named `sidecar` with `restartPolicy: Always`, command `[balerix-agent, sidecar]`, the exec readiness probe on `/balerix/run/ready`, and mounts `/balerix/agent`, the four `/balerix/shared/…` sub-paths all `readOnly: true` (`fleets/payments/crews/backend/repo/.git/objects`, `fleets/payments/crews/backend/pool`, `fleets/payments/pool`, `pools/daemon`), `/balerix/run`, `/balerix/secret`, `/balerix/tls`; `containers[0]` named `agent`, command `[balerix-agent, run]`, the `requests` `cpu: "1"`, `memory: 2Gi`, and the same mounts without `/balerix/secret` and `/balerix/tls`; five volumes, the `shared` claim `readOnly: true`. Both containers hardened as in every Job.

Run: `mise x -- cargo insta accept --manifest-path operator/Cargo.toml && mise run operator`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add operator
git commit -m "feat(operator): an Agent into its claim, bundle, policy and two-container pod, and a Pod into its conditions (Spec O §5.4, §6.1)"
```

---

### Task 11: The Daemon client

**Files:**
- Create: `operator/src/daemon_client.rs`, `operator/tests/client_it.rs`, `operator/tests/support/mod.rs`
- Modify: `operator/src/lib.rs`

**Interfaces:**
- Consumes: `balerix_api::{FleetRequest, FleetRecord, DownQuery, ErrorBody}`, `pki` (tests).
- Produces:
  - `enum ClientError { Unavailable(String), Rejected(String), Conflict(String), Unexpected { status: u16, message: String }, Setup(String) }`
  - `struct DaemonClient` (hand-written `Debug`, token redacted)
  - `DaemonClient::new(base_url: &str, authority_pem: &str, admin_token: &str, timeout: Duration) -> Result<Self, ClientError>`
  - `async fn ready(&self) -> Result<(), ClientError>`
  - `async fn apply(&self, request: &FleetRequest) -> Result<FleetRecord, ClientError>`
  - `async fn get(&self, fleet: &str) -> Result<Option<FleetRecord>, ClientError>`
  - `async fn down(&self, fleet: &str) -> Result<(), ClientError>`

- [ ] **Step 1: Write the failing tests**

`operator/tests/support/mod.rs`:

```rust
//! The `balerix` binary `scripts/operator.sh` built, and a temp root
//! under `target/tmp`.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// From `BALERIX_BIN`; `None` after printing a skip (a failure under
/// `BALERIX_REQUIRE_TOOLS=1`).
pub fn balerix() -> Option<PathBuf> {
    let found = std::env::var_os("BALERIX_BIN")
        .map(PathBuf::from)
        .filter(|p| p.is_file());
    if found.is_none() {
        if std::env::var("BALERIX_REQUIRE_TOOLS").as_deref() == Ok("1") {
            panic!("BALERIX_BIN missing and BALERIX_REQUIRE_TOOLS=1 (run through scripts/operator.sh)");
        }
        eprintln!("skip: BALERIX_BIN missing");
    }
    found
}

pub fn temp_root(label: &str) -> PathBuf {
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}
```

`operator/tests/client_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Daemon's admin API as the operator calls it (Spec O §7.4): against
//! a stub for each answer's meaning, and against a real
//! `balerix serve --mode kubernetes` over TLS from the operator's own
//! authority.
mod support;

use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, put};
use balerix_api::{CrewSpec, ErrorBody, FleetRequest, FleetSpec};
use balerix_operator::daemon_client::{ClientError, DaemonClient};
use balerix_operator::pki;

const TOKEN: &str = "admin-0123456789abcdef0123456789abcdef";
const AGENT_TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn request(name: &str, token: &str) -> FleetRequest {
    let settings = serde_json::from_value(serde_json::json!({ "runner": { "type": "pod" } })).unwrap();
    FleetRequest {
        spec: FleetSpec {
            name: name.into(),
            tools: BTreeMap::new(),
            crews: BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "acme/api".into(),
                    git_ref: "main".into(),
                    agents: BTreeMap::from([("a".to_string(), settings)]),
                    ..Default::default()
                },
            )]),
        },
        credentials: Default::default(),
        agent_tokens: Some(BTreeMap::from([(format!("{name}/c/a"), token.to_string())])),
    }
}

async fn stub(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn refusal(status: StatusCode, message: &str) -> (StatusCode, Json<ErrorBody>) {
    (status, Json(ErrorBody { error: message.to_string() }))
}

fn client(base: &str) -> DaemonClient {
    let authority = pki::new_authority("team-a", "default", now()).unwrap();
    DaemonClient::new(base, &authority.cert_pem, TOKEN, Duration::from_secs(5)).unwrap()
}

#[tokio::test]
async fn each_answer_has_its_meaning() {
    let base = stub(
        Router::new()
            .route(
                "/v1/fleets/rejected",
                put(|| async { refusal(StatusCode::BAD_REQUEST, "flow: states.x: unknown state") }),
            )
            .route(
                "/v1/fleets/owned",
                put(|| async { refusal(StatusCode::CONFLICT, "fleet owned is managed by plugin github") }),
            )
            .route(
                "/v1/fleets/broken",
                put(|| async { refusal(StatusCode::INTERNAL_SERVER_ERROR, "disk full") }),
            )
            .route(
                "/v1/fleets/absent",
                get(|| async { refusal(StatusCode::NOT_FOUND, "not found") })
                    .delete(|| async { refusal(StatusCode::NOT_FOUND, "not found") }),
            )
            .route(
                "/readyz",
                get(|| async { refusal(StatusCode::SERVICE_UNAVAILABLE, "daemon pool: pending") }),
            ),
    )
    .await;
    let c = client(&base);
    assert_eq!(
        c.apply(&request("rejected", AGENT_TOKEN)).await.unwrap_err(),
        ClientError::Rejected("flow: states.x: unknown state".into())
    );
    assert_eq!(
        c.apply(&request("owned", AGENT_TOKEN)).await.unwrap_err(),
        ClientError::Conflict("fleet owned is managed by plugin github".into())
    );
    assert_eq!(
        c.apply(&request("broken", AGENT_TOKEN)).await.unwrap_err(),
        ClientError::Unexpected { status: 500, message: "disk full".into() }
    );
    assert_eq!(c.get("absent").await.unwrap(), None);
    c.down("absent").await.unwrap();
    assert_eq!(
        c.ready().await.unwrap_err(),
        ClientError::Unavailable("daemon pool: pending".into())
    );
}

#[tokio::test]
async fn the_admin_token_is_the_bearer_and_is_never_printed() {
    let base = stub(Router::new().route(
        "/readyz",
        get(|headers: HeaderMap| async move {
            let sent = headers.get("authorization").and_then(|v| v.to_str().ok());
            if sent == Some(&format!("Bearer {TOKEN}")) {
                (StatusCode::OK, "ready").into_response()
            } else {
                refusal(StatusCode::UNAUTHORIZED, "missing or invalid admin token").into_response()
            }
        }),
    ))
    .await;
    let c = client(&base);
    c.ready().await.unwrap();
    let shown = format!("{c:?}");
    assert!(shown.contains("<redacted>") && !shown.contains(TOKEN), "{shown}");
}

use axum::response::IntoResponse;

#[tokio::test]
async fn a_daemon_that_does_not_answer_is_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let c = client(&format!("http://{addr}"));
    assert!(matches!(c.ready().await.unwrap_err(), ClientError::Unavailable(_)));
    assert!(matches!(
        c.apply(&request("f", AGENT_TOKEN)).await.unwrap_err(),
        ClientError::Unavailable(_)
    ));
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `balerix serve --mode kubernetes` on a free port, serving a
/// certificate this operator's `pki` issued.
fn serve(balerix: &std::path::Path, root: &std::path::Path) -> (Kill, String, pki::Issued) {
    let authority = pki::new_authority("team-a", "default", now()).unwrap();
    let serving =
        pki::issue_serving(&authority, "team-a", "default", &["127.0.0.1".to_string()], now())
            .unwrap();
    std::fs::write(root.join("tls.crt"), &serving.cert_pem).unwrap();
    std::fs::write(root.join("tls.key"), &serving.key_pem).unwrap();
    std::fs::write(root.join("admin-token"), TOKEN).unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let log = std::fs::File::create(root.join("daemon.log")).unwrap();
    let child = Command::new(balerix)
        .args(["serve", "--mode", "kubernetes", "--tmux-socket", "unused", "--bind"])
        .arg(format!("127.0.0.1:{port}"))
        .arg("--tls-cert")
        .arg(root.join("tls.crt"))
        .arg("--tls-key")
        .arg(root.join("tls.key"))
        .arg("--admin-token-file")
        .arg(root.join("admin-token"))
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_DATA_HOME")
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    (Kill(child), format!("https://127.0.0.1:{port}"), authority)
}

#[tokio::test]
async fn a_real_kubernetes_mode_daemon_takes_the_operators_apply() {
    let Some(balerix) = support::balerix() else {
        return;
    };
    let root = support::temp_root("client-real-daemon");
    let (_daemon, base, authority) = serve(&balerix, &root);
    let c = DaemonClient::new(&base, &authority.cert_pem, TOKEN, Duration::from_secs(10)).unwrap();
    let mut waited = 0;
    while let Err(e) = c.ready().await {
        waited += 1;
        assert!(
            waited < 150,
            "the daemon never became ready: {e:?}\n{}",
            std::fs::read_to_string(root.join("daemon.log")).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert_eq!(c.get("payments").await.unwrap(), None);
    let record = c.apply(&request("payments", AGENT_TOKEN)).await.unwrap();
    assert_eq!(record.owner.as_deref(), Some("kubernetes"));
    // an upsert: the operator re-sends on every reconcile
    c.apply(&request("payments", AGENT_TOKEN)).await.unwrap();
    assert_eq!(
        c.get("payments").await.unwrap().unwrap().spec,
        request("payments", AGENT_TOKEN).spec
    );
    // §7.4: a token under 32 characters is a 400, and nothing lands
    let short = c.apply(&request("other", "short")).await.unwrap_err();
    assert_eq!(
        short,
        ClientError::Rejected("agent_tokens.other/c/a: a token is at least 32 characters".into())
    );
    assert_eq!(c.get("other").await.unwrap(), None);
    // §7.4's 409s: a body without tokens is the CLI's, on a fleet of the
    // operator's and on one that does not exist
    let mut as_cli = request("payments", AGENT_TOKEN);
    as_cli.agent_tokens = None;
    assert_eq!(
        c.apply(&as_cli).await.unwrap_err(),
        ClientError::Conflict(
            "fleet payments is managed by kubernetes; change it through its Fleet object".into()
        )
    );
    let mut absent = request("cli-made", AGENT_TOKEN);
    absent.agent_tokens = None;
    assert_eq!(
        c.apply(&absent).await.unwrap_err(),
        ClientError::Conflict("this daemon is in kubernetes mode; create a Fleet object".into())
    );
    c.down("payments").await.unwrap();

    // a client holding another authority does not trust this Daemon
    let stranger = pki::new_authority("team-a", "default", now()).unwrap();
    let untrusting =
        DaemonClient::new(&base, &stranger.cert_pem, TOKEN, Duration::from_secs(5)).unwrap();
    assert!(matches!(untrusting.ready().await.unwrap_err(), ClientError::Unavailable(_)));
    // and a wrong token is not "unavailable"
    let wrong = DaemonClient::new(&base, &authority.cert_pem, "not-the-token", Duration::from_secs(5)).unwrap();
    assert_eq!(
        wrong.get("payments").await.unwrap_err(),
        ClientError::Unexpected { status: 401, message: "missing or invalid admin token".into() }
    );
}
```

`balerix_api`'s `CrewSpec` and `FleetSpec` derive `Default`, and `FleetRecord` derives `PartialEq` and `Debug`, which the `assert_eq!`s on `Option<FleetRecord>` need.

Run: `scripts/operator.sh check`
Expected: compile error, no `daemon_client`.

- [ ] **Step 2: Implement `operator/src/daemon_client.rs`**

```rust
//! The Daemon's admin API, as the operator uses it (Spec O §5.2, §7.4):
//! `PUT /v1/fleets/{name}` with one token per agent, the forced `DELETE`,
//! the fleet's record, and `/readyz`. It trusts one authority, the
//! Daemon's own (§10.3), and nothing else; the Daemon holds no Kubernetes
//! credentials, so everything between the two goes through here (O-8).

use std::sync::Arc;
use std::time::Duration;

use balerix_api::{DownQuery, ErrorBody, FleetRecord, FleetRequest};
use reqwest::StatusCode;
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// No answer, a TLS failure, or `/readyz` saying 503: the Fleet is
    /// `DaemonUnavailable` and the reconcile is retried (§5.2).
    #[error("the Daemon is unavailable: {0}")]
    Unavailable(String),
    /// A 400: `Accepted=False` with this message; nothing landed.
    #[error("{0}")]
    Rejected(String),
    /// A 409: the fleet is another owner's.
    #[error("{0}")]
    Conflict(String),
    #[error("the Daemon answered {status}: {message}")]
    Unexpected { status: u16, message: String },
    /// The authority or the URL this client was given is unusable.
    #[error("{0}")]
    Setup(String),
}

pub struct DaemonClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl std::fmt::Debug for DaemonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonClient")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

fn unavailable(e: reqwest::Error) -> ClientError {
    // reqwest's own text names the URL, never a header
    ClientError::Unavailable(e.without_url().to_string())
}

async fn message(response: reqwest::Response) -> String {
    let text = response.text().await.unwrap_or_default();
    match serde_json::from_str::<ErrorBody>(&text) {
        Ok(body) => body.error,
        Err(_) => text,
    }
}

impl DaemonClient {
    /// `base_url` is the Daemon's `status.endpoint`; `authority_pem` the
    /// one certificate trusted. A plain `http://` base (tests) never
    /// touches the TLS settings.
    pub fn new(
        base_url: &str,
        authority_pem: &str,
        admin_token: &str,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        let setup = |what: &str, e: &dyn std::fmt::Display| ClientError::Setup(format!("{what}: {e}"));
        // one process-wide provider; a second install is a harmless `Err`
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(authority_pem.as_bytes()) {
            let cert = cert.map_err(|e| setup("the authority is not a PEM certificate", &e))?;
            roots
                .add(cert)
                .map_err(|e| setup("the authority is not a usable certificate", &e))?;
        }
        if roots.is_empty() {
            return Err(ClientError::Setup(
                "the authority holds no certificate".to_string(),
            ));
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| setup("TLS", &e))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let http = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .no_proxy()
            .timeout(timeout)
            .build()
            .map_err(|e| setup("the HTTP client", &e))?;
        Ok(Self {
            http,
            base: base_url.trim_end_matches('/').to_string(),
            token: admin_token.to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn refused(response: reqwest::Response) -> ClientError {
        let status = response.status();
        let message = message(response).await;
        match status {
            StatusCode::BAD_REQUEST => ClientError::Rejected(message),
            StatusCode::CONFLICT => ClientError::Conflict(message),
            other => ClientError::Unexpected {
                status: other.as_u16(),
                message,
            },
        }
    }

    /// `GET /readyz`: the Daemon's system pool and nothing else (§7.3).
    pub async fn ready(&self) -> Result<(), ClientError> {
        let response = self
            .http
            .get(self.url("/readyz"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(unavailable)?;
        match response.status() {
            StatusCode::OK => Ok(()),
            StatusCode::SERVICE_UNAVAILABLE => {
                Err(ClientError::Unavailable(message(response).await))
            }
            _ => Err(Self::refused(response).await),
        }
    }

    /// The operator's apply: an upsert, re-sent on every reconcile.
    pub async fn apply(&self, request: &FleetRequest) -> Result<FleetRecord, ClientError> {
        let response = self
            .http
            .put(self.url(&format!("/v1/fleets/{}", request.spec.name)))
            .bearer_auth(&self.token)
            .json(request)
            .send()
            .await
            .map_err(unavailable)?;
        if !response.status().is_success() {
            return Err(Self::refused(response).await);
        }
        response.json().await.map_err(unavailable)
    }

    /// The fleet's record, with its status; `None` when the Daemon has
    /// none by that name.
    pub async fn get(&self, fleet: &str) -> Result<Option<FleetRecord>, ClientError> {
        let response = self
            .http
            .get(self.url(&format!("/v1/fleets/{fleet}")))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(unavailable)?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(None),
            s if s.is_success() => response.json().await.map(Some).map_err(unavailable),
            _ => Err(Self::refused(response).await),
        }
    }

    /// The operator's down: `DELETE …?force=true` (§7.4). A fleet the
    /// Daemon does not have is already down.
    pub async fn down(&self, fleet: &str) -> Result<(), ClientError> {
        let query = DownQuery {
            force: true,
            ..Default::default()
        }
        .to_query_string();
        let response = self
            .http
            .delete(self.url(&format!("/v1/fleets/{fleet}?{query}")))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(unavailable)?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(()),
            s if s.is_success() => Ok(()),
            _ => Err(Self::refused(response).await),
        }
    }
}
```

Add `pub mod daemon_client;` to `operator/src/lib.rs`.

- [ ] **Step 3: Run**

Run: `BALERIX_REQUIRE_TOOLS=1 mise run operator`
Expected: PASS. If the real-daemon test fails at `ready`, the assertion prints `daemon.log`; a daemon that cannot find `git`, `gh`, `mise`, `nono` or `tmux` on `PATH` says so there (run through `mise`, which puts the pinned ones on it).

- [ ] **Step 4: Commit**

```bash
git add operator
git commit -m "feat(operator): the Daemon client, trusting the Daemon's own authority alone (Spec O §7.4, §10.3)"
```

---

### Task 12: The spec, the docs, the last gate

**Files:**
- Modify: `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md`, `AGENTS.md`, `ARCHITECTURE.md`

- [ ] **Step 1: The spec**

Append `### 20.5 Decided by the plan` to §20, holding every item of this plan's **Decisions this plan makes** as it was built (reword any that the code ended up departing from, and say how). In §12's layout add `src/desired/{common,names,fleet,daemon,jobs,agent}.rs` under `operator/` and `crates/balerix-runtime/src/jobs.rs`. In §7.4's mount-paths item, change "Under the shared mount: `repo/.git/objects`, `crew/mise`, `fleet/mise`, `daemon/mise`" to say the three pool directories are mounted whole (`crew`, `fleet`, `daemon`, each holding `mise/`). In §17 item 3, add `3a done 2026-10 (PR #<number>)` once the PR exists.

- [ ] **Step 2: `AGENTS.md`**

Under **Tasks**, after `agent`:

```markdown
- `operator` — lint and test the standalone `operator/` project
  (`balerix-operator`, Spec O §12); builds `balerix` first, since its
  client test runs `balerix serve --mode kubernetes`. Fails when
  `operator/crds/` differs from the Rust types. Its own CI job; not part
  of `check`.
- `crds` — regenerates `operator/crds/` from `operator/src/api/`.
```

Under **Conventions**, after the `agent/` item:

```markdown
- `operator/` is standalone like `agent/`, depending on `balerix-api`,
  `balerix-core` and `balerix-config` by path; `kube` and `k8s-openapi`
  never reach the core workspace or `agent/`, which is why a `pod`
  runner's Kubernetes shapes are opaque JSON in `balerix-api`.
  `operator/src/desired/` is pure: no clock, no random value, no I/O.
  Tokens and certificates are made by `pki` and passed in.
```

Under **Gotchas**:

```markdown
- A `desired` function writes its manifest as JSON and returns the
  `k8s-openapi` type (`common::typed`). Those types ignore unknown
  fields, so a misspelt key is dropped without an error: the insta
  snapshot is the check. Read a `.snap.new` for what is missing, not
  only for what is there.
- `ResolveOptions::runner` says which runner a resolution is for; the
  CLI's is `Tmux`, the operator's `Pod`, and an agent of the other kind
  fails with its config path. A Kubernetes-mode daemon does not check
  the runner; a tmux-mode daemon refuses `pod`.
- The Jobs (`balerix-agent crew-sync`, `pool-sync`, `harvest`) see the
  shared slice at the pod's paths (`SharedSlice`, `/balerix/shared`):
  `check_clone` compares a clone's `alternates` with the cache's path,
  so a harvest that saw the cache anywhere else would refuse every
  clone. `harvest` writes nothing under the claim; its git profile,
  nono's home and its logs are in the scratch directory, which is why
  `AgentPaths::git_profile` is a field.
- In a pod `CrewPaths::logs` is on the agent claim
  (`.balerix/state/crew-logs`): the crew root is a read-only mount.
  Never write `crew.root.join("logs")`.
- The cache directory can be a mount point (a sync Job): it can be
  emptied, never removed. `discard_half_made` accepts an emptied
  directory for that reason.
```

- [ ] **Step 3: `ARCHITECTURE.md`**

Read its section on the Kubernetes split (sub-project 2 added one; `grep -n -i 'kubernetes\|agent/' ARCHITECTURE.md`). Add `operator/` beside `agent/` in whatever list or diagram names the projects, in that section's own style, with one paragraph: the operator's `desired` functions are pure and its controllers (3b) are the only part that touches a cluster; the Daemon holds no Kubernetes credentials, so the operator pushes resolved fleets through `daemon_client`; the Jobs are `balerix-agent` commands over `balerix_runtime::jobs`.

- [ ] **Step 4: The last gate**

Run, each to completion:

```bash
mise run check
mise run test-it
BALERIX_REQUIRE_TOOLS=1 mise run agent
BALERIX_REQUIRE_TOOLS=1 mise run operator
mise run plugins
mise x -- cargo deny --manifest-path operator/Cargo.toml check advisories bans sources licenses
git status --short   # only ` M mise.toml` (the cargo-insta hunk) may remain
```

Expected: all pass; no `.snap.new` anywhere (`git status --short --ignored | grep snap.new` prints nothing).

- [ ] **Step 5: Commit and open the pull request**

```bash
git add docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md AGENTS.md ARCHITECTURE.md
git commit -m "docs: Spec O §20.5, and the operator project in AGENTS.md and ARCHITECTURE.md"
```

Push and open the pull request only when the user says so. Title: `feat(operator): the operator project, its five kinds and pure desired state; the sync and harvest Jobs (Spec O §20)`. The body lists what 3b still owes: the controllers and `run`, both Dockerfiles, `kind` with the ReadWriteMany class, `e2e-k8s`, and the two things this plan could not verify without a cluster (the `slice` init container's directories being writable by uid 10001, and sub-path mounts of directories a Job made).
