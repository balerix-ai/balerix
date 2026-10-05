# Spec O, part 3b: the operator on a cluster — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The second half of Spec O's sub-project 3 (§20.1, §21): `balerix-operator run` with the Daemon, Fleet, Crew and Agent controllers; controller tests on the envtest binaries, which need no cluster; `docker/operator/Dockerfile` and `docker/agent/Dockerfile`; `scripts/kind-up.sh`; and `mise run e2e-k8s`, the Phase 3 journey on a `kind` cluster in CI. Done when `mise run operator`, `mise run agent` and `mise run check` pass here, and the `e2e-k8s` CI job passes on the pull request (§17's condition for sub-project 3).

**Architecture:** The controllers are the dumb executor in front of 3a's pure `desired` functions: each reconcile observes (the owned objects by label, the Secrets it needs), calls `desired`, applies with server-side apply under the field manager `balerix-operator`, and patches status through the subresource. Four `kube_runtime::Controller`s share one `Client` and one `Context` in one process (one set of four per watched namespace). The Daemon is polled (`GET /v1/fleets/{name}`, a 15 s requeue) into a per-fleet record cache that the Agent controller reads on its own 15 s period and whenever its Fleet changes. Every Job goes through one rule (`controllers::jobs::ensure_job`): `backoffLimit: 0`, a failed Job is retried by the operator with a doubling delay carried on the owner's `balerix.ai/attempts` annotation, and a crew's sync, harvest and cleanup Jobs never run at once. The tests run the controllers in-process against a real `kube-apiserver` (envtest), each test in its own namespace, with the test standing in for the kubelet; `e2e-k8s` is the only thing that needs a cluster, and the operator runs outside it there.

**Tech Stack:** Rust 1.99.0 (edition 2024, `unsafe` forbidden); kube 4.2.0 (`client`, `runtime`, `derive`, `rustls-tls`, `ring`: kube's default TLS stack on the ring provider the Daemon client already uses); k8s-openapi 0.28.0 (`v1_32`, `schemars`); tokio 1.53.1 (adds `signal`, `sync`, `process`); futures-util 0.3.34; tracing 0.1.44, tracing-subscriber 0.3.23 (`env-filter`), the versions the core workspace pins; envtest `envtest-v1.34.1` from `kubernetes-sigs/controller-tools` (probed 2026-10-03: `etcd` and `kube-apiserver` start on this host as uid 1000 with no container runtime, `/readyz` in about 3 s, the five CRDs apply, server-side apply with a field manager and status subresource patches work; no controller-manager and no kubelet); kind 0.33.0, kubectl 1.34.12 (task-level mise tools); hadolint 2.15.1; cargo-insta 1.49.0 and insta 1.49.0.

**Spec:** `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md`: §21 (the decisions for this sub-project), with §5 as the controllers' design, §6.1 for the pod, §7.4 for the Daemon's routes, §8 for storage and removal, §10 for security, §15 for testing and §19.3 for the `kind` class. What this plan decides beyond §21 is listed under **Decisions this plan makes** and written into the spec as §21.6 by Task 12.

## Global Constraints

- Branch `feat/kube-operator-cluster`, cut from `docs/spec-o-3b-decisions` (16e434c, which holds §21). One pull request, squash-merged, title `feat(operator): the controllers, envtest tests, the images, kind and e2e-k8s (Spec O §21)`.
- The uncommitted `cargo-insta = "1.49.0"` hunk in `mise.toml` is Task 1's: it is staged and committed there, with the `insta` dev-dependency bumps. Until Task 1 commits, never stage `mise.toml` by accident: commit with explicit paths.
- Run cargo only through mise: `mise x -- cargo …` or `mise run <task>`, from the repository root. The operator project's cargo commands take `--manifest-path operator/Cargo.toml` and `CARGO_TARGET_DIR=operator/target`, as `scripts/operator.sh` does; the agent project's likewise with `agent/`.
- `mise run operator` passes before every commit that touches `operator/`; `mise run agent` before every commit that touches `agent/`; `mise run check` and `mise run test-it` before every commit that touches `crates/`.
- No `unsafe`. `unwrap`/`expect` are warnings outside tests and clippy runs with `-D warnings`; `#![allow(clippy::unwrap_used, clippy::expect_used)]` at the top of every test file.
- Exact versions for every new dependency, with a comment saying why, in the project that uses it. Nothing in this plan adds a dependency to the core workspace; `scripts/check-core-deps.sh` passes unchanged. `k8s-openapi` and `kube` never enter the core workspace or `agent/`.
- insta snapshots: read each `.snap.new`, compare it with the expected values the task lists, then `mise x -- cargo insta accept --manifest-path <project>/Cargo.toml`. Never blind-accept.
- Test roots under `target/tmp` (`CARGO_TARGET_TMPDIR`), never `/tmp`.
- `BALERIX_REQUIRE_TOOLS=1` turns every "skip: tool missing" into a failure. The envtest binaries, `kind`, `kubectl` and `docker` are tools in that sense.
- Nothing secret in argv, env or logs (§10.3). A type holding a token, a private key or credentials hand-implements `Debug` and prints `<redacted>`, or derives none. `tracing` fields are namespaces, names, reasons and counts.
- Nothing cluster-bound runs on this host: there is no container runtime, so no `kind`. Tasks 10 and 11 are verified by pushing the branch and reading the `e2e-k8s` job's log on the pull request (open it as a draft at Task 10). The envtest tests (Tasks 4–8) run here.
- Error, reason and annotation text that tests pin, verbatim:
  - annotations `balerix.ai/attempts` (JSON object, Job name to count), `balerix.ai/input-hash`, `balerix.ai/spec-hash`, `balerix.ai/not-after`, `balerix.ai/purge`;
  - finalizers `balerix.ai/fleet`, `balerix.ai/harvest`;
  - condition reasons `Rejected`, `DaemonUnavailable`, `SyncFailed`, `HarvestFailed`, `WaitingForCrew`, `PodMissing`, `ClaimPending`, `DaemonNotReady`;
  - the field manager `balerix-operator`;
  - `cache: f/c: the remote has no branch nope` (the message a failed sync Job's pod carries in the tests).

## Review Focus

Conditions the spec implies and no task's tests would otherwise exercise, most likely to bite first. Each has a test in the task that owns it.

1. **An Agent deleted before it ever had a claim.** A crew dropped from a Fleet while its sync Job still runs has Agents with no claim and no pod. A harvest Job over an absent claim would fail forever and hold the finalizer. Task 8 pins: no claim, no harvest; the finalizer drops at once.
2. **A failed Job that left no pod.** The kubelet garbage-collects pods; a failed Job whose pod is gone has no termination message. The condition must still be `False` with a readable message and the retry must still happen. Task 5 pins the pool Job with `status.failed: 1` and no pod: `SystemToolsReady=False`, reason `PoolSyncFailed`, message `the job failed and left no message`, and the retry after the delay.
3. **A Fleet whose Daemon does not exist.** `spec.daemon` names nothing. The Fleet must be `Resolved=False` with a message naming the Daemon, no child object, no `PUT`. Task 6 pins it.
4. **Two Fleets sharing a crew name.** `payments` and `billing` both with crew `backend` make Crews `payments-backend` and `billing-backend` and sync Jobs of their own; the lock is per crew of one fleet, not per crew name. Task 7 pins that both sync Jobs exist at once.
5. **A sidecar that crashed once and then ran.** The pod's `initContainerStatuses[sidecar].lastState.terminated.message` keeps the old crash text after a successful restart. §21.5: the message is read only while the sidecar is not running. Task 3 pins `Materialized=Unknown`, not `MaterializeFailed`, for a running sidecar with a stale `lastState`.

## Decisions this plan makes

Task 12 writes these into the spec as §21.6.

- **Owned objects are read by label-selected lists, not reflector stores.** A reconcile lists its children by `balerix.ai/*` labels through the API (`Api::list`), one call per kind it needs. Reflector stores for five child kinds would be shared mutable caches that lag a server-side apply by one watch event, and the lists are small. The main kinds are still watched. This amends §21.2's "from the controllers' reflector stores".
- **The attempt count is one annotation on the owner,** `balerix.ai/attempts`, a JSON object from Job name to count (`{"f-c-sync": 2}`), since a Fleet owns its pool Job and, while deleting, one cleanup Job per crew. An entry is removed when the Job succeeds or its input changes. The delay is measured from the failed Job's `Failed` condition's transition time, else its creation time.
- **The Fleet's agents are re-reconciled through a Fleet watch.** The Agent controller `watches` Fleets with a mapper that returns the Agent names the Fleet controller last planned, kept in the `Context` per fleet, so a Fleet change reaches its Agents at once; and the Agent period equals the Fleet's (15 s), so a phase change the poll brought lands within one period. (`Controller::reconcile_on` is unstable in kube-runtime 4.2; a mapper cannot list.)
- **`--watch-namespaces` with several names starts one set of controllers per namespace** in the same process, each on `Api::namespaced`, sharing the `Context`; with none it is one set on `Api::all`. A single `Controller` watches one namespace or all, and a per-namespace Role (§5.6) forbids a cluster-wide list.
- **Periods and the clock are configuration.** `RunConfig` carries the Fleet period (15 s), the other period (60 s) and a `Clock` (`Arc<dyn Fn() -> i64 + Send + Sync>`, unix seconds). Tests set periods of 1 s and a clock they can move, so the Job retry test needs no 30 s wait.
- **An operator outside the cluster reaches a Daemon through `--resolve host=ip:port`** (reqwest's resolver override on the Daemon client): the connection goes to a `kubectl port-forward` while TLS verifies the Service's name as in a pod. `e2e-k8s` uses it; the chart never does.
- **A test-only Daemon URL override.** `RunConfig::insecure_daemon_url: Option<String>` (the hidden flag `--insecure-daemon-url`, as `balerix-agent sidecar` hides `--allow-http`) makes every Daemon client `DaemonClient::insecure_for_tests` at that URL. The envtest tests' stub Daemon is plain HTTP; a pod's Daemon is `https`.
- **No claim, no harvest.** An Agent with no claim has nothing to harvest; its finalizer drops at once.
- **A failed harvest is `Ready=False`, reason `HarvestFailed`,** with the Job's message; the Agent keeps its finalizer until the retry succeeds.
- **An Agent waiting on its Crew is `Materialized=Unknown`, reason `WaitingForCrew`,** with the Crew's failing condition's message when it has one.
- **A token Secret is owned by the Fleet,** not the Agent: §5.2 keeps it across reconciles and pod replacements, and the Fleet's deletion removes it.
- **The Daemon's `Ready` asks `/readyz` once per reconcile** only when the StatefulSet reports a ready replica; without one `daemon_status` already says `DaemonNotReady`.
- **The test is the kubelet.** In envtest nothing runs pods: a test patches a Job's `status` and its pod's termination message, patches a Pod's `status`, and deletes a Pod with grace period 0 once the operator has set its deletion timestamp, since without a kubelet a deleted Pod stays `Terminating`.
- **The envtest binaries run under a parent-watching shell,** `bash -c '… & while kill -0 $PPID; do sleep 0.5; done; kill $!'`, so a test binary that exits leaves no `etcd` or `kube-apiserver` behind. One instance per test binary, started on free ports under `CARGO_TARGET_TMPDIR`.
- **`e2e-k8s` seeds its repository through a git server pod** from the agent image (`git daemon`, a Service), since nothing on the runner is reachable from the cluster; the crew uses `git: { push: false, auth: none }`. The journey asserts the Fleet object and the crew's directories are gone after a `retain: None` deletion; the Daemon's own list is not reachable from the test without a plugin token and is not asserted.
- **The cleanup Job is `balerix-agent crew-remove --crew <fleet>/<crew>`**, over `balerix_runtime::jobs::remove_crew`, which removes `repo/` and `crew/` (the pool and logs) under the crew's slice and leaves the mount points; its outcome line is `removed`. The Job is `names::remove_job(fleet, crew)` = `<fleet>-<crew>-remove`, bounded as the other Job names.
- **`/tmp` is an `emptyDir`** in the agent pod (both containers) and in every Job (the `job` container), mounted at `/tmp`. The daemon pod gets none unless `e2e-k8s` shows a need.
- **Names derived from a Daemon's are bounded by `names::job_name`'s rule** (truncate, `-`, eight hex characters of the hash, suffix), applied to every `balerix-<daemon>…` name through one function, so no label value can exceed 63 characters.
- **The retry delay** is `30 × 2^(n−1)` seconds for attempt `n`, capped at 600.

---

## File Structure

| File | Change |
|---|---|
| `mise.toml` | `cargo-insta` 1.49.0 (the user's hunk); task `e2e-k8s`, task `kind-up`; the `operator` task gains the envtest tool |
| `Cargo.toml`, `operator/Cargo.toml`, `plugins/{common,matrix,github}/Cargo.toml` and lockfiles | `insta` 1.49.0 |
| `crates/balerix-runtime/src/jobs.rs`, `tests/jobs_it.rs` | `remove_crew` |
| `agent/src/{cli,jobs,main}.rs`, `agent/tests/jobs_cli_it.rs` | `crew-remove` |
| `operator/Cargo.toml` | `kube` features, `tokio` features, `clap` `env`, `futures-util`, `tracing`, `tracing-subscriber`; dev: `axum` stays |
| `operator/src/daemon_client.rs` | `new_resolving` |
| `operator/src/desired/jobs.rs` | `remove_job`, `/tmp` emptyDir; `operator/src/desired/agent.rs`: `/tmp`, the termination-message rule; `operator/src/desired/names.rs`: `remove_job`, Daemon-derived bounds |
| `operator/src/controllers/mod.rs` | **new**: `Context`, `RunConfig`, `Clock`, `Error`, `apply`, `patch_status`, `run` |
| `operator/src/controllers/jobs.rs` | **new**: `ensure_job`, `Ensured`, the attempts annotation, the crew lock |
| `operator/src/controllers/daemon.rs` | **new** |
| `operator/src/controllers/fleet.rs` | **new** |
| `operator/src/controllers/crew.rs` | **new** |
| `operator/src/controllers/agent.rs` | **new** |
| `operator/src/main.rs` | `run` |
| `operator/src/lib.rs` | `pub mod controllers` |
| `operator/tests/support/envtest.rs` | **new**: the harness |
| `operator/tests/support/stub_daemon.rs` | **new**: the scripted Daemon |
| `operator/tests/support/mod.rs` | `pub mod envtest; pub mod stub_daemon;` and the kubelet helpers |
| `operator/tests/controllers_it.rs` | **new**: the envtest cases |
| `operator/tests/e2e_k8s.rs` | **new**: the journey |
| `docker/operator/Dockerfile`, `docker/agent/Dockerfile` | **new** |
| `scripts/kind-up.sh` | **new** |
| `scripts/operator.sh` | `e2e` mode |
| `.github/workflows/ci.yml` | job `e2e-k8s`; the `operator` job's install line |
| `AGENTS.md`, `ARCHITECTURE.md`, the spec | Task 12 |

---

### Task 1: cargo-insta and insta at 1.49.0

**Files:**
- Modify: `mise.toml:6` (the user's uncommitted hunk)
- Modify: `Cargo.toml:75`, `operator/Cargo.toml:71`, `plugins/common/Cargo.toml:34`, `plugins/matrix/Cargo.toml:45`, `plugins/github/Cargo.toml:50`, and the five lockfiles

**Interfaces:**
- Produces: nothing new; the snapshot tooling and library at one version.

- [ ] **Step 1: Cut the branch**

```bash
git checkout -b feat/kube-operator-cluster docs/spec-o-3b-decisions
git status --short   # exactly ` M mise.toml`
```

- [ ] **Step 2: Move the five `insta` dev-dependencies**

In each of the five manifests change `insta = { version = "1.48.0"` to `insta = { version = "1.49.0"`. Then refresh the lockfiles without touching anything else:

```bash
mise x -- cargo update -p insta --precise 1.49.0
mise x -- cargo update -p insta --precise 1.49.0 --manifest-path operator/Cargo.toml
for p in common matrix github; do mise x -- cargo update -p insta --precise 1.49.0 --manifest-path plugins/$p/Cargo.toml; done
git diff --stat   # mise.toml, five Cargo.toml, five Cargo.lock; nothing else
```

- [ ] **Step 3: Run every snapshot suite**

```bash
mise run check
mise run operator
mise run plugin common && mise run plugin matrix && mise run plugin github
```

Expected: all pass; no `.snap.new` appears (`git status --short | grep snap.new` prints nothing). insta 1.49 writes the same snapshot format as 1.48; if a snapshot does change, read the diff before accepting and name it in the commit.

- [ ] **Step 4: Commit**

```bash
git add mise.toml Cargo.toml Cargo.lock operator/Cargo.toml operator/Cargo.lock plugins/common/Cargo.toml plugins/common/Cargo.lock plugins/matrix/Cargo.toml plugins/matrix/Cargo.lock plugins/github/Cargo.toml plugins/github/Cargo.lock
git commit -m "chore(deps): cargo-insta and insta 1.49.0

The tool pin in mise.toml and the dev-dependency in the five projects that
snapshot move together, so a snapshot written by one is read by the other."
```

---

### Task 2: `crew-remove`, the cleanup Job's command

**Files:**
- Modify: `crates/balerix-runtime/src/jobs.rs`, `crates/balerix-runtime/tests/jobs_it.rs`
- Modify: `agent/src/cli.rs`, `agent/src/jobs.rs`, `agent/src/main.rs`, `agent/tests/jobs_cli_it.rs`

**Interfaces:**
- Consumes: `balerix_runtime::layout::SharedSlice` (`new(root)`, `crew() -> CrewPaths { repo, logs, root }`); `agent::cli::JobDirs`, `finish`.
- Produces: `balerix_runtime::jobs::remove_crew(slice: &SharedSlice) -> std::io::Result<()>`; `balerix-agent crew-remove --crew <fleet>/<crew> [--shared-dir …] [--scratch-dir …] [--termination-log …]`, outcome line `removed`.

- [ ] **Step 1: Write the failing runtime test**

Append to `crates/balerix-runtime/tests/jobs_it.rs` (it already has `mod support;`, `temp_root` and a `slice` helper; if it has no `slice` helper, use `SharedSlice::new(root.join("shared"))`):

```rust
#[test]
fn remove_crew_empties_the_slice_and_keeps_the_mount_points() {
    let root = support::temp_root("remove-crew");
    let slice = SharedSlice::new(root.join("shared"));
    let crew = slice.crew();
    // what a sync left behind: a cache, a pool, logs, and the fleet and
    // daemon pools the crew only reads
    std::fs::create_dir_all(crew.repo.join(".git/objects")).unwrap();
    std::fs::write(crew.repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::create_dir_all(crew.root.join("mise/installs")).unwrap();
    std::fs::create_dir_all(crew.root.join("no-hooks")).unwrap();
    std::fs::create_dir_all(&crew.logs).unwrap();
    std::fs::create_dir_all(slice.fleet().root.join("mise")).unwrap();
    std::fs::create_dir_all(slice.daemon_pool()).unwrap();

    balerix_runtime::jobs::remove_crew(&slice).unwrap();

    // the mount points stay (a Job cannot remove a mount), empty
    assert!(crew.repo.is_dir() && std::fs::read_dir(&crew.repo).unwrap().next().is_none());
    assert!(crew.root.is_dir() && std::fs::read_dir(&crew.root).unwrap().next().is_none());
    // what the crew only reads is untouched
    assert!(slice.fleet().root.join("mise").is_dir());
    assert!(slice.daemon_pool().is_dir());
    // a second run over an empty slice is fine
    balerix_runtime::jobs::remove_crew(&slice).unwrap();
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `mise x -- cargo nextest run -p balerix-runtime --test jobs_it remove_crew`
Expected: FAIL, `no function or associated item named remove_crew`.

- [ ] **Step 3: Implement `remove_crew`**

In `crates/balerix-runtime/src/jobs.rs`, after `harvest`:

```rust
/// The cleanup Job of a Fleet deleted with `retain: None` (Spec O §5.2,
/// §8.5): empties the crew's cache and its pool directory on the shared
/// volume. The two directories are mount points in the Job, so they are
/// emptied, not removed. Nothing the crew only reads (the fleet and
/// daemon pools) is touched.
pub fn remove_crew(slice: &SharedSlice) -> std::io::Result<()> {
    let crew = slice.crew();
    for dir in [&crew.repo, &crew.root] {
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Run the runtime tests**

Run: `mise x -- cargo nextest run -p balerix-runtime --test jobs_it`
Expected: PASS.

- [ ] **Step 5: Write the failing agent CLI test**

Append to `agent/tests/jobs_cli_it.rs`:

```rust
#[test]
fn crew_remove_ends_with_removed_and_empties_the_slice() {
    let root = support::temp_root("crew-remove-cli");
    let shared = root.join("shared");
    std::fs::create_dir_all(shared.join("repo/.git")).unwrap();
    std::fs::create_dir_all(shared.join("crew/mise")).unwrap();
    std::fs::create_dir_all(shared.join("fleet/mise")).unwrap();
    let log = root.join("termination-log");
    let out = Command::new(BIN)
        .args(["crew-remove", "--crew", "f/c"])
        .arg("--shared-dir").arg(&shared)
        .arg("--scratch-dir").arg(root.join("scratch"))
        .arg("--termination-log").arg(&log)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "removed\n");
    assert!(std::fs::read_dir(shared.join("repo")).unwrap().next().is_none());
    assert!(std::fs::read_dir(shared.join("crew")).unwrap().next().is_none());
    assert!(shared.join("fleet/mise").is_dir());

    // a bad crew name is the command's refusal, as the termination message
    let out = Command::new(BIN)
        .args(["crew-remove", "--crew", "not-a-crew"])
        .arg("--shared-dir").arg(&shared)
        .arg("--termination-log").arg(&log)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(std::fs::read_to_string(&log).unwrap().starts_with("--crew not-a-crew:"));
}
```

- [ ] **Step 6: Run it to see it fail**

Run: `CARGO_TARGET_DIR=agent/target mise x -- cargo nextest run --manifest-path agent/Cargo.toml --test jobs_cli_it crew_remove`
Expected: FAIL, `unrecognized subcommand 'crew-remove'`.

- [ ] **Step 7: Add the command**

In `agent/src/cli.rs`, in `enum Command` after `Harvest`:

```rust
    /// The cleanup Job: empty a crew's cache and pool after its Fleet is
    /// deleted with `retain: None` (§5.2, §8.5).
    CrewRemove(CrewRemoveArgs),
```

and after `HarvestArgs`:

```rust
#[derive(Debug, Args)]
pub struct CrewRemoveArgs {
    /// `<fleet>/<crew>`.
    #[arg(long)]
    pub crew: String,
    #[command(flatten)]
    pub dirs: JobDirs,
}
```

In `agent/src/jobs.rs`, import `CrewRemoveArgs` with the other args and add:

```rust
pub fn crew_remove(args: &CrewRemoveArgs) -> Result<String> {
    // parsed for its refusal: a Job over a mistyped crew must say so
    let _: CrewRef = args
        .crew
        .parse()
        .map_err(|e| anyhow!("--crew {}: {e}", args.crew))?;
    jobs::remove_crew(&SharedSlice::new(&args.dirs.shared_dir))
        .with_context(|| format!("cannot empty the slice at {}", args.dirs.shared_dir.display()))?;
    Ok("removed".to_string())
}
```

In `agent/src/main.rs`, after the `Harvest` arm:

```rust
        Command::CrewRemove(args) => balerix_agent::cli::finish(
            &args.dirs.termination_log,
            "crew-remove",
            balerix_agent::jobs::crew_remove(&args),
        ),
```

- [ ] **Step 8: Run the agent and runtime tiers**

```bash
mise run test-it
mise run agent
mise run check
```

Expected: all pass.

- [ ] **Step 9: Commit**

```bash
git add crates/balerix-runtime/src/jobs.rs crates/balerix-runtime/tests/jobs_it.rs agent/src/cli.rs agent/src/jobs.rs agent/src/main.rs agent/tests/jobs_cli_it.rs
git commit -m "feat(agent): crew-remove, the cleanup Job of a Fleet deleted with retain: None (Spec O §21)

Empties the crew's cache and pool on the shared volume and leaves the
mount points; the fleet and daemon pools the crew only reads are not
touched. Outcome line: removed."
```

---

### Task 3: §21.5 in `desired`: the cleanup Job, `/tmp`, bounded Daemon names, the termination-message rule

**Files:**
- Modify: `operator/src/desired/names.rs`, `operator/src/desired/jobs.rs`, `operator/src/desired/agent.rs`, `operator/src/desired/daemon.rs` (only if a snapshot moves)
- Test: the modules' own `tests`, the existing snapshots

**Interfaces:**
- Consumes: `desired::jobs::{JobContext, Parts, job, slice}` (private helpers in that file), `names::job_name`.
- Produces: `names::remove_job(fleet, crew) -> String`; `names::daemon_derived(daemon, suffix) -> String` used by every `balerix-<daemon>…` name; `desired::jobs::remove_job(ctx: &JobContext<'_>, fleet: &str, crew: &str) -> Result<Job, DesiredError>`; `desired::agent::agent_status` reads a termination message only when the sidecar is not running.

- [ ] **Step 1: Write the failing name tests**

In `operator/src/desired/names.rs`'s `tests` module, add:

```rust
    #[test]
    fn daemon_derived_names_are_bounded_like_job_names() {
        let long = "d".repeat(80);
        for n in [daemon(&long), state_claim(&long), shared_claim(&long), authority(&long), serving(&long), admin(&long), daemon_pool_job(&long)] {
            assert!(n.len() <= 63, "{n}");
            assert!(n.starts_with("balerix-"), "{n}");
        }
        assert!(state_claim(&long).ends_with("-state"));
        assert_ne!(state_claim(&long), shared_claim(&long));
        // a short name is unchanged
        assert_eq!(daemon("default"), "balerix-default");
        assert_eq!(admin("default"), "balerix-default-admin");
    }

    #[test]
    fn the_remove_job_is_named_and_bounded() {
        assert_eq!(remove_job("f", "c"), "f-c-remove");
        let l = "e".repeat(63);
        assert!(remove_job(&l, &l).len() <= 63);
        assert!(remove_job(&l, &l).ends_with("-remove"));
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `CARGO_TARGET_DIR=operator/target mise x -- cargo nextest run --manifest-path operator/Cargo.toml names::`
Expected: FAIL (`remove_job` not found; the long Daemon names exceed 63).

- [ ] **Step 3: Bound the Daemon-derived names and add `remove_job`**

In `operator/src/desired/names.rs`, add under `job_name`:

```rust
/// `balerix-<daemon><suffix>`, bounded as a Job name is: a Daemon's name
/// reaches labels (`balerix.ai/daemon`) and pod names, so it may not make
/// a value over 63 characters (§21.5).
fn daemon_derived(daemon: &str, suffix: &str) -> String {
    job_name(&format!("balerix-{daemon}"), suffix)
}
```

and rewrite the seven Daemon functions to use it:

```rust
pub fn daemon(daemon: &str) -> String {
    daemon_derived(daemon, "")
}
pub fn state_claim(daemon: &str) -> String {
    daemon_derived(daemon, "-state")
}
pub fn shared_claim(daemon: &str) -> String {
    daemon_derived(daemon, "-shared")
}
pub fn authority(daemon: &str) -> String {
    daemon_derived(daemon, "-ca")
}
pub fn serving(daemon: &str) -> String {
    daemon_derived(daemon, "-tls")
}
pub fn admin(daemon: &str) -> String {
    daemon_derived(daemon, "-admin")
}
pub fn daemon_pool_job(daemon: &str) -> String {
    daemon_derived(daemon, "-pool")
}
```

`job_name` with an empty suffix: `base.len() + 0 <= 63` returns the base unchanged, else the cut form with no suffix; `room` is `63 - 0 - 1 - 8`. Add after `harvest_job`:

```rust
/// The cleanup Job of a Fleet deleted with `retain: None` (§21.2).
pub fn remove_job(fleet: &str, crew: &str) -> String {
    job_name(&format!("{fleet}-{crew}"), "-remove")
}
```

(`sync_job` inlines the same base, `format!("{fleet}-{crew}")`; inline it here likewise, as `job_name(&format!("{fleet}-{crew}"), "-remove")`.)

- [ ] **Step 4: Run the name tests**

Run: `CARGO_TARGET_DIR=operator/target mise x -- cargo nextest run --manifest-path operator/Cargo.toml names::`
Expected: PASS.

- [ ] **Step 5: Write the failing Job and pod tests**

In `operator/src/desired/jobs.rs`'s `tests` module (it has a `ctx()` or similar fixture making a `JobContext`; reuse the one `crew_sync_job`'s snapshot test uses):

```rust
    #[test]
    fn the_remove_job_runs_crew_remove_over_the_crews_writable_slice() {
        let images = images();
        let job = remove_job(&ctx(&images), "f", "c").unwrap();
        insta::assert_yaml_snapshot!(job);
    }

    #[test]
    fn every_job_mounts_an_empty_dir_at_tmp() {
        let images = images();
        let job = daemon_pool_job(&ctx(&images)).unwrap();
        let spec = job.spec.unwrap().template.spec.unwrap();
        let mounts = spec.containers[0].volume_mounts.clone().unwrap();
        assert!(mounts.iter().any(|m| m.mount_path == "/tmp" && m.name == "tmp"));
        assert!(spec.volumes.unwrap().iter().any(|v| v.name == "tmp" && v.empty_dir.is_some()));
    }
```

In `operator/src/desired/agent.rs`'s `tests` module:

```rust
    #[test]
    fn both_containers_mount_an_empty_dir_at_tmp() {
        let spec = objects(json!({ "type": "pod" })).pod.spec.unwrap();
        for c in spec.init_containers.unwrap().iter().chain(spec.containers.iter()) {
            assert!(c.volume_mounts.as_ref().unwrap().iter().any(|m| m.mount_path == "/tmp"), "{}", c.name);
        }
    }

    #[test]
    fn a_running_sidecars_old_crash_is_not_a_failure() {
        let agent = agent(json!({ "type": "pod" }));
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": { "name": "f-c-a", "namespace": "ns" },
            "status": {
                "conditions": [{ "type": "PodScheduled", "status": "True" }],
                "initContainerStatuses": [{
                    "name": "sidecar", "image": "x", "imageID": "x", "ready": false, "restartCount": 1,
                    "state": { "running": { "startedAt": "2026-10-03T00:00:00Z" } },
                    "lastState": { "terminated": { "exitCode": 1, "message": "MaterializeFailed: git: boom" } }
                }]
            }
        })).unwrap();
        let status = agent_status(&agent, Some(&pod), None, &now());
        let materialized = status.conditions.iter().find(|c| c.type_ == "Materialized").unwrap();
        assert_eq!((materialized.status.as_str(), materialized.reason.as_str()), ("Unknown", "Materializing"));

        // the same message on a sidecar that is not running is the failure
        let mut waiting = pod.clone();
        waiting.status.as_mut().unwrap().init_container_statuses.as_mut().unwrap()[0].state =
            Some(serde_json::from_value(serde_json::json!({ "waiting": { "reason": "CrashLoopBackOff" } })).unwrap());
        let status = agent_status(&agent, Some(&waiting), None, &now());
        let materialized = status.conditions.iter().find(|c| c.type_ == "Materialized").unwrap();
        assert_eq!((materialized.status.as_str(), materialized.reason.as_str()), ("False", "MaterializeFailed"));
        assert_eq!(materialized.message, "MaterializeFailed: git: boom");
    }
```

`images()`, `ctx(&images)` are `jobs.rs`'s existing fixtures; `objects(runner)` and `agent(runner)` are `agent.rs`'s, and `now()` is the one its status tests use (`Time(k8s_openapi::jiff::Timestamp::from_second(secs).unwrap())` wrapped; read the module for its name and reuse it).

- [ ] **Step 6: Run them to see them fail**

Run: `CARGO_TARGET_DIR=operator/target mise x -- cargo nextest run --manifest-path operator/Cargo.toml desired::`
Expected: FAIL on the four new tests.

- [ ] **Step 7: Implement**

In `operator/src/desired/jobs.rs`:

```rust
/// The cleanup Job (§21.2): `crew-remove` over the crew's cache and pool,
/// read-write, after the Fleet's Agents are gone.
pub fn remove_job(ctx: &JobContext<'_>, fleet: &str, crew: &str) -> Result<Job, DesiredError> {
    let mut args = strings(&["crew-remove", "--crew"]);
    args.push(format!("{fleet}/{crew}"));
    job(
        ctx,
        Parts {
            name: names::remove_job(fleet, crew),
            component: "remove",
            extra_labels: vec![
                ("balerix.ai/fleet", fleet.to_string()),
                ("balerix.ai/crew", crew.to_string()),
            ],
            args,
            slices: vec![
                slice(names::vol_crew_repo(fleet, crew), "repo", true),
                slice(names::vol_crew_pool(fleet, crew), "crew", true),
            ],
            volumes: vec![],
            mounts: vec![],
        },
    )
}
```

In `job`, after `mounts.push(json!({ "name": "scratch", … }))` add `mounts.push(json!({ "name": "tmp", "mountPath": "/tmp" }));` and after the `scratch` volume add `json!({ "name": "tmp", "emptyDir": {} })`. The `input` hash covers mounts and volumes, so every existing Job snapshot changes: that is expected; read each `.snap.new` and check the only change is the `tmp` mount, volume and hash.

In `operator/src/desired/agent.rs`, in `both`, after the `run` mount push `json!({ "name": "tmp", "mountPath": "/tmp" })`, and add `{ "name": "tmp", "emptyDir": {} }` to the pod's `volumes`. Then change the `Materialized` branch of `agent_status`:

```rust
    // §21.5: a termination message is read only while the sidecar is not
    // running; a running sidecar's old crash in `lastState` is history.
    let running = side.is_some_and(|s| s.state.as_ref().is_some_and(|st| st.running.is_some()));
    let materialized = if past_materialize {
        Cond::yes("Materialized", "Materialized", "")
    } else if let Some(message) = side.filter(|_| !running).and_then(termination_message) {
```

(the rest of that `if` unchanged).

- [ ] **Step 8: Run, review the snapshots, accept**

```bash
CARGO_TARGET_DIR=operator/target mise x -- cargo nextest run --manifest-path operator/Cargo.toml desired::
```

Expected: the four new tests pass; the Job and pod snapshot tests fail with `.snap.new` files. Read each `.snap.new`: the Jobs gain the `tmp` mount and volume and a new `balerix.ai/input-hash`; the pod gains the `tmp` mount in both containers and the volume; the new `remove_job` snapshot shows `command: [balerix-agent]`, `args: [crew-remove, --crew, f/c]`, two `shared` mounts (`/balerix/shared/repo`, `/balerix/shared/crew`, both `readOnly: false`), the `slice` init container with `mkdir -p /balerix/volume/fleets/f/crews/c/repo /balerix/volume/fleets/f/crews/c/pool`, `backoffLimit: 0`. Then:

```bash
mise x -- cargo insta accept --manifest-path operator/Cargo.toml
mise run operator
```

Expected: PASS (the CRD check too: no kind changed).

- [ ] **Step 9: Commit**

```bash
git add operator/src/desired operator/src/desired/snapshots
git commit -m "feat(operator): the cleanup Job, /tmp in every pod, bounded Daemon names, a running sidecar's old crash is history (Spec O §21.5)"
```

---

### Task 4: `run`, the controllers' context, and the envtest harness

**Files:**
- Modify: `operator/Cargo.toml`, `operator/src/lib.rs`, `operator/src/main.rs`, `operator/src/daemon_client.rs`
- Create: `operator/src/controllers/mod.rs`
- Modify: `mise.toml` (the `operator` task's tool), `scripts/operator.sh`
- Create: `operator/tests/support/envtest.rs`, `operator/tests/controllers_it.rs`; modify `operator/tests/support/mod.rs`

**Interfaces:**
- Consumes: `desired::common::{OperatorConfig, Images, MANAGER}`, `daemon_client::{DaemonClient, ClientError}`, `desired::fleet::PlanError`, `desired::common::DesiredError`, `pki::PkiError`, `api::{Daemon, Fleet, Crew, Agent}`, `api::crd_files()`.
- Produces:
  - `controllers::Clock = Arc<dyn Fn() -> i64 + Send + Sync>`;
  - `controllers::RunConfig { version: String, images: Images, namespace: String, watch_namespaces: Option<Vec<String>>, fleet_period: Duration, period: Duration, clock: Clock, insecure_daemon_url: Option<String>, resolve: Vec<(String, SocketAddr)> }` with `RunConfig::new(version, images, namespace)` (15 s, 60 s, the system clock, no override, no resolve entries);
  - `daemon_client::DaemonClient::new_resolving(base_url, authority_pem, admin_token, timeout, resolve: &[(String, SocketAddr)])` (`new` is `new_resolving` with no entries; each entry is `reqwest::ClientBuilder::resolve`, so an out-of-cluster operator reaches a Service through a port-forward while TLS still verifies the Service's name);
  - `controllers::Context { client: Client, cfg: OperatorConfig, run: RunConfig, records: RwLock<BTreeMap<String, FleetRecord>>, fleet_agents: RwLock<BTreeMap<String, Vec<String>>>, … }` with `Context::now() -> i64`, `Context::k8s_now() -> Time`, `Context::daemon_client(namespace, daemon, endpoint, authority_pem, token) -> Result<Arc<DaemonClient>, Error>`, `Context::key(namespace, name) -> String` (`"<ns>/<name>"`);
  - `controllers::Error` (`Kube`, `Daemon`, `Desired`, `Plan`, `Pki`, `Finalizer`, `Missing`);
  - `controllers::apply<K>(client, &K) -> Result<K, Error>` (server-side apply, forced, manager `balerix-operator`); `controllers::patch_status<K, S: Serialize>(client, &K, &S) -> Result<(), Error>`; `controllers::error_policy<K>(Arc<K>, &Error, Arc<Context>) -> Action` (5 s doubling to 300 s per object, reset by `controllers::reconciled(&ctx, &K)`);
  - `controllers::run(client: Client, cfg: RunConfig) -> impl Future<Output = ()>`: builds the `Context`, starts the controllers Tasks 5–8 register (Task 4 starts none), runs until dropped;
  - `balerix-operator run [--watch-namespaces a,b] [--namespace ns] [--daemon-image i] [--agent-image i] [--resolve host=ip:port]… [--insecure-daemon-url u]`;
  - test support: `support::envtest::envtest() -> Option<&'static EnvTest>` (`EnvTest { client: Client, kubeconfig: PathBuf }`), `support::namespace(&Client, label) -> String`, `support::wait_for(Duration, F) -> T`, `support::TestClock { offset: Arc<AtomicI64> }` with `clock() -> Clock` and `advance(secs)`, `support::spawn_operator(&EnvTest, &str, Option<String>, &TestClock) -> tokio::task::AbortHandle`.

- [ ] **Step 1: Add the runtime dependencies**

In `operator/Cargo.toml` change the `kube` line and `tokio` line, and add four entries:

```toml
# The client, the controller runtime and the derive. `rustls-tls` + `ring`
# is kube 4.2's default TLS stack, on the provider the Daemon client uses.
kube = { version = "4.2.0", default-features = false, features = ["client", "runtime", "derive", "rustls-tls", "ring"] }
# The controllers' streams: `for_each` over a Controller's results.
futures-util = { version = "0.3.34", default-features = false, features = ["std"] }
# The controllers' logs: namespace, name, reason; never a token.
tracing = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }
# The controllers' runtime, SIGTERM, the record cache's locks, the
# envtest harness's child processes in tests.
tokio = { version = "1.53.1", features = ["rt-multi-thread", "macros", "time", "net", "signal", "sync", "process"] }
```

Also give `clap` the `env` feature (`features = ["derive", "env"]`: `--namespace` falls back to `POD_NAMESPACE`). Then `CARGO_TARGET_DIR=operator/target mise x -- cargo build --manifest-path operator/Cargo.toml` and `mise x -- cargo deny --manifest-path operator/Cargo.toml check` must both pass (new licences, if any, are added to `operator/deny.toml`'s allow list with a comment naming the crate).

- [ ] **Step 2: Write `controllers/mod.rs`**

Create `operator/src/controllers/mod.rs`:

```rust
//! The controllers (Spec O §5, §21.2): the dumb executor in front of
//! `desired`. A reconcile observes (the owned objects by label, the
//! Secrets it needs), calls the pure function, applies the result with
//! server-side apply under one field manager, and patches status. One
//! `Context` is shared by every controller of the process.

pub mod agent;
pub mod crew;
pub mod daemon;
pub mod fleet;
pub mod jobs;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use balerix_api::FleetRecord;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{Api, Client, Resource, ResourceExt};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::daemon_client::{ClientError, DaemonClient};
use crate::desired::common::{DesiredError, Images, MANAGER, OperatorConfig, hash};
use crate::desired::fleet::PlanError;
use crate::pki::PkiError;

/// Unix seconds. Tests move it; the binary reads the system clock.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub struct RunConfig {
    /// `CARGO_PKG_VERSION`: what a Daemon may ask for (§4.1).
    pub version: String,
    pub images: Images,
    /// The operator's own namespace (the Daemon's NetworkPolicy admits it).
    pub namespace: String,
    /// `None` watches every namespace (§5.6).
    pub watch_namespaces: Option<Vec<String>>,
    /// The Fleet's and the Agent's requeue: the status poll (§21.1).
    pub fleet_period: Duration,
    /// The Daemon's and the Crew's requeue: renewal and drift.
    pub period: Duration,
    pub clock: Clock,
    /// Tests only: every Daemon is this plain-HTTP stub. A pod's Daemon is
    /// `https` and the client refuses anything else.
    pub insecure_daemon_url: Option<String>,
    /// Service host to socket address, for an operator outside the
    /// cluster (`e2e-k8s`): the name still verifies against the
    /// certificate; only the connection goes elsewhere.
    pub resolve: Vec<(String, std::net::SocketAddr)>,
}

impl RunConfig {
    pub fn new(version: &str, images: Images, namespace: &str) -> Self {
        Self {
            version: version.to_string(),
            images,
            namespace: namespace.to_string(),
            watch_namespaces: None,
            fleet_period: Duration::from_secs(15),
            period: Duration::from_secs(60),
            clock: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            }),
            insecure_daemon_url: None,
            resolve: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Kube(#[from] kube::Error),
    #[error("{0}")]
    Daemon(#[from] ClientError),
    #[error("{0}")]
    Desired(#[from] DesiredError),
    #[error("{0}")]
    Plan(#[from] PlanError),
    #[error("{0}")]
    Pki(#[from] PkiError),
    #[error("{0}")]
    Finalizer(Box<kube::runtime::finalizer::Error<Error>>),
    /// An object the reconcile needs is not there: `{what} has no {field}`
    /// or `{what} {name} does not exist`.
    #[error("{0}")]
    Missing(String),
}

impl From<kube::runtime::finalizer::Error<Error>> for Error {
    fn from(e: kube::runtime::finalizer::Error<Error>) -> Self {
        Error::Finalizer(Box::new(e))
    }
}

/// A Daemon client and what it was built from, so a changed authority or
/// token rebuilds it.
struct CachedClient {
    fingerprint: String,
    client: Arc<DaemonClient>,
}

pub struct Context {
    pub client: Client,
    pub cfg: OperatorConfig,
    pub run: RunConfig,
    /// `<ns>/<fleet>` to the record the last Fleet reconcile read (§21.1).
    pub records: RwLock<BTreeMap<String, FleetRecord>>,
    /// `<ns>/<fleet>` to the Agent object names the last plan made: the
    /// Agent controller's Fleet watch maps through this.
    pub fleet_agents: RwLock<BTreeMap<String, Vec<String>>>,
    clients: Mutex<BTreeMap<String, CachedClient>>,
    /// Consecutive reconcile errors per object, for `error_policy`.
    errors: Mutex<BTreeMap<String, u32>>,
}

impl Context {
    pub fn new(client: Client, run: RunConfig) -> Self {
        let cfg = OperatorConfig {
            version: run.version.clone(),
            images: run.images.clone(),
            namespace: run.namespace.clone(),
        };
        Self {
            client,
            cfg,
            run,
            records: RwLock::new(BTreeMap::new()),
            fleet_agents: RwLock::new(BTreeMap::new()),
            clients: Mutex::new(BTreeMap::new()),
            errors: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn now(&self) -> i64 {
        (self.run.clock)()
    }

    pub fn k8s_now(&self) -> Time {
        // the epoch if the clock is out of jiff's range, which a test clock is not
        Time(k8s_openapi::jiff::Timestamp::from_second(self.now()).unwrap_or_default())
    }

    pub fn key(namespace: &str, name: &str) -> String {
        format!("{namespace}/{name}")
    }

    /// The client for one Daemon, rebuilt when its authority or token
    /// changed. Under `insecure_daemon_url` every Daemon is the stub.
    pub fn daemon_client(
        &self,
        namespace: &str,
        daemon: &str,
        endpoint: &str,
        authority_pem: &str,
        token: &str,
    ) -> Result<Arc<DaemonClient>, Error> {
        let fingerprint = hash(&serde_json::json!([endpoint, authority_pem, token]));
        let key = Self::key(namespace, daemon);
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = clients.get(&key).filter(|c| c.fingerprint == fingerprint) {
            return Ok(cached.client.clone());
        }
        let client = match &self.run.insecure_daemon_url {
            Some(url) => DaemonClient::insecure_for_tests(url, token)?,
            None => DaemonClient::new_resolving(endpoint, authority_pem, token, Duration::from_secs(10), &self.run.resolve)?,
        };
        let client = Arc::new(client);
        clients.insert(
            key,
            CachedClient {
                fingerprint,
                client: client.clone(),
            },
        );
        Ok(client)
    }
}

/// `<ns>/<name>` of any object, for the caches and the logs.
pub fn object_key<K: Resource>(object: &K) -> String {
    Context::key(&object.namespace().unwrap_or_default(), &object.name_any())
}

/// Server-side apply, forced, under the operator's field manager (§5).
pub async fn apply<K>(client: &Client, object: &K) -> Result<K, Error>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Clone
        + std::fmt::Debug
        + DeserializeOwned
        + Serialize,
    K::DynamicType: Default,
{
    let namespace = object
        .namespace()
        .ok_or_else(|| Error::Missing(format!("{} has no metadata.namespace", object.name_any())))?;
    let api: Api<K> = Api::namespaced(client.clone(), &namespace);
    Ok(api
        .patch(
            &object.name_any(),
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(object),
        )
        .await?)
}

/// A merge patch of `status` through the subresource.
pub async fn patch_status<K, S>(client: &Client, object: &K, status: &S) -> Result<(), Error>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Clone
        + std::fmt::Debug
        + DeserializeOwned,
    K::DynamicType: Default,
    S: Serialize,
{
    let namespace = object
        .namespace()
        .ok_or_else(|| Error::Missing(format!("{} has no metadata.namespace", object.name_any())))?;
    let api: Api<K> = Api::namespaced(client.clone(), &namespace);
    api.patch_status(
        &object.name_any(),
        &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "status": status })),
    )
    .await?;
    Ok(())
}

/// A reconcile that ended well resets the object's error count.
pub fn reconciled<K: Resource>(ctx: &Context, object: &K) {
    ctx.errors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&object_key(object));
}

/// 5 s, doubling per consecutive failure of the same object, at most 5 min.
pub fn error_policy<K: Resource>(object: Arc<K>, error: &Error, ctx: Arc<Context>) -> Action {
    let key = object_key(object.as_ref());
    let attempt = {
        let mut errors = ctx.errors.lock().unwrap_or_else(|e| e.into_inner());
        let n = errors.entry(key.clone()).or_insert(0);
        *n = n.saturating_add(1);
        *n
    };
    let delay = Duration::from_secs((5u64 << attempt.saturating_sub(1).min(6)).min(300));
    tracing::warn!(object = %key, attempt, "reconcile failed: {error}");
    Action::requeue(delay)
}

/// The Api a controller set works on: one namespace, or every one.
pub fn api_in<K>(client: &Client, namespace: Option<&str>) -> Api<K>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope>,
    K::DynamicType: Default,
{
    match namespace {
        Some(ns) => Api::namespaced(client.clone(), ns),
        None => Api::all(client.clone()),
    }
}

/// The controllers, one set per watched namespace, until the future is
/// dropped. Tasks 5–8 add a line per controller to `set`.
pub async fn run(client: Client, cfg: RunConfig) {
    let ctx = Arc::new(Context::new(client, cfg));
    let namespaces: Vec<Option<String>> = match &ctx.run.watch_namespaces {
        Some(list) if !list.is_empty() => list.iter().cloned().map(Some).collect(),
        _ => vec![None],
    };
    let sets = namespaces.into_iter().map(|ns| set(ctx.clone(), ns));
    futures_util::future::join_all(sets).await;
}

/// The four controllers over one namespace (or all).
async fn set(ctx: Arc<Context>, namespace: Option<String>) {
    tracing::info!(namespace = namespace.as_deref().unwrap_or("*"), "watching");
    let ns = namespace.as_deref();
    let controllers: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> = vec![
        // Task 5: Box::pin(daemon::controller(ctx.clone(), ns)),
        // Task 6: Box::pin(fleet::controller(ctx.clone(), ns)),
        // Task 7: Box::pin(crew::controller(ctx.clone(), ns)),
        // Task 8: Box::pin(agent::controller(ctx.clone(), ns)),
    ];
    let _ = ns;
    futures_util::future::join_all(controllers).await;
}
```

Create the four empty modules so it compiles: `operator/src/controllers/{daemon,fleet,crew,agent,jobs}.rs`, each holding only `//! Task N.` for now (Tasks 5–8 fill them). In `operator/src/lib.rs` add `pub mod controllers;` and change the doc comment's last sentence to "`controllers` ties them to a cluster (§21)."

- [ ] **Step 3: `DaemonClient::new_resolving`, then `run` in `main.rs`**

In `operator/src/daemon_client.rs`, rename `new` to `new_resolving` with a fifth parameter `resolve: &[(String, std::net::SocketAddr)]`, and after `.no_proxy()` add `.timeout(timeout)` as today, then before `.build()`:

```rust
        let mut builder = builder;
        for (host, addr) in resolve {
            builder = builder.resolve(host, *addr);
        }
```

(restructure the chain as `let builder = reqwest::Client::builder().use_preconfigured_tls(tls).no_proxy().timeout(timeout);` first). Add back `pub fn new(base_url, authority_pem, admin_token, timeout) -> Result<Self, ClientError> { Self::new_resolving(base_url, authority_pem, admin_token, timeout, &[]) }` with the old doc comment, so `tests/client_it.rs` is unchanged. Document on `new_resolving`: "`resolve` maps a Service host to a socket address for an operator outside the cluster; the certificate is still verified against the host."

Then `main.rs`:

Replace `operator/src/main.rs`'s `Command` and `main` with:

```rust
#[derive(Debug, Subcommand)]
enum Command {
    /// Print the five CustomResourceDefinitions, or write one file each.
    Crds {
        /// A directory for `<plural>.balerix.ai.yaml`; stdout when absent.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Run the controllers against the cluster the kubeconfig names
    /// (`KUBECONFIG`, or in-cluster).
    Run(RunArgs),
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// Namespaces to watch, comma-separated; every namespace when absent (§5.6).
    #[arg(long, value_delimiter = ',')]
    watch_namespaces: Option<Vec<String>>,
    /// The operator's own namespace; `POD_NAMESPACE` when absent.
    #[arg(long, env = "POD_NAMESPACE")]
    namespace: Option<String>,
    /// The daemon image; this operator's version of `ghcr.io/balerix-ai/balerix` when absent.
    #[arg(long)]
    daemon_image: Option<String>,
    /// The agent image; this operator's version of `ghcr.io/balerix-ai/balerix-agent` when absent.
    #[arg(long)]
    agent_image: Option<String>,
    /// `host=ip:port`, repeatable: reach a Daemon's Service at that address
    /// (an operator outside the cluster, through a port-forward).
    #[arg(long, value_parser = resolve_entry)]
    resolve: Vec<(String, std::net::SocketAddr)>,
    /// Tests only: talk to this plain-HTTP Daemon instead of each Daemon's Service.
    #[arg(long, hide = true)]
    insecure_daemon_url: Option<String>,
}

fn resolve_entry(s: &str) -> Result<(String, std::net::SocketAddr), String> {
    let (host, addr) = s.split_once('=').ok_or_else(|| format!("{s:?}: expected host=ip:port"))?;
    let addr = addr.parse().map_err(|e| format!("{addr:?}: {e}"))?;
    Ok((host.to_string(), addr))
}

async fn run(args: RunArgs) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let mut images = Images::for_version(version);
    if let Some(i) = args.daemon_image {
        images.daemon = i;
    }
    if let Some(i) = args.agent_image {
        images.agent = i;
    }
    let namespace = args
        .namespace
        .context("--namespace or POD_NAMESPACE is required: the Daemon's NetworkPolicy admits the operator's namespace")?;
    let mut cfg = RunConfig::new(version, images, &namespace);
    cfg.watch_namespaces = args.watch_namespaces;
    cfg.insecure_daemon_url = args.insecure_daemon_url;
    cfg.resolve = args.resolve;
    let client = kube::Client::try_default()
        .await
        .context("cannot connect to the cluster (KUBECONFIG, or in-cluster)")?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("cannot listen for SIGTERM")?;
    tokio::select! {
        () = balerix_operator::controllers::run(client, cfg) => {}
        _ = sigterm.recv() => tracing::info!("SIGTERM: stopping"),
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted: stopping"),
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();
    let result = match cli.command {
        Command::Crds { out } => crds(out),
        Command::Run(args) => match tokio::runtime::Runtime::new() {
            Ok(rt) => rt.block_on(run(args)),
            Err(e) => Err(e.into()),
        },
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

with `use balerix_operator::controllers::RunConfig; use balerix_operator::desired::common::Images;` at the top. Check: `CARGO_TARGET_DIR=operator/target mise x -- cargo run -q --manifest-path operator/Cargo.toml -- run --help` lists the four visible flags and not `--insecure-daemon-url`; `… -- run` with no cluster and no `--namespace` exits 1 with `--namespace or POD_NAMESPACE is required`.

- [ ] **Step 4: Pin the envtest tool on the `operator` task**

In `mise.toml`, the `[tasks.operator]` entry becomes:

```toml
[tasks.operator]
description = "Lint and test the standalone operator project (balerix-operator, Spec O §12): builds balerix first for its client test against a Kubernetes-mode daemon; the controller tests run on the envtest binaries this task pins (a kube-apiserver and etcd, no cluster); fails when operator/crds differs from the Rust types. Its own tier; not part of `check`"
# envtest (kube-apiserver, etcd, kubectl) is pinned here, not in [tools], so
# only this task downloads the ~160 MB tarball. The tag is the spike's 1.34
# line (§19); the binaries sit under controller-tools/envtest/ in it.
tools = { "github:kubernetes-sigs/controller-tools" = { version = "envtest-v1.34.1", bin_path = "controller-tools/envtest" } }
run = "scripts/operator.sh check"
```

Verify: `mise run operator` installs it (a one-time download) and, inside the task, `kube-apiserver` is on `PATH`. Check with a one-off: `mise task run operator` is the full check; quicker, `mise x -- bash -c 'command -v kube-apiserver'` will not see a task-level tool, so instead temporarily add `echo "envtest: $(command -v kube-apiserver || echo missing)"` as the first line of `scripts/operator.sh check` and run the task. If it prints `missing`, the `bin_path` option is not honoured by this mise: replace it with `extract_all = true` and look for the binary under `$(mise where github:kubernetes-sigs/controller-tools)/controller-tools/envtest`, exporting that directory as `ENVTEST_DIR` from `scripts/operator.sh` (the harness reads `ENVTEST_DIR` first, then `PATH`). Record which worked in the commit message. Remove the echo.

In `scripts/operator.sh`'s `check` arm, before the nextest line:

```bash
    # the envtest binaries the task pins; the harness skips (fails under
    # BALERIX_REQUIRE_TOOLS=1) without them
    if command -v kube-apiserver >/dev/null; then
      export ENVTEST_DIR="$(dirname "$(command -v kube-apiserver)")"
    fi
```

- [ ] **Step 5: Write the harness**

Create `operator/tests/support/envtest.rs`:

```rust
//! A real `kube-apiserver` and `etcd` (the envtest binaries, Spec O
//! §21.3) started once per test binary on free ports under `target/tmp`,
//! with the five definitions applied. No controller-manager, no kubelet:
//! a test patches Job and Pod status itself and force-deletes pods.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{ListParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};
use tokio::sync::OnceCell;

pub struct EnvTest {
    pub client: Client,
    pub kubeconfig: PathBuf,
}

static INSTANCE: OnceCell<Option<EnvTest>> = OnceCell::const_new();

/// The shared instance; `None` after printing a skip (a failure under
/// `BALERIX_REQUIRE_TOOLS=1`).
pub async fn envtest() -> Option<&'static EnvTest> {
    INSTANCE.get_or_init(start).await.as_ref()
}

fn binaries() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ENVTEST_DIR").map(PathBuf::from) {
        if dir.join("kube-apiserver").is_file() && dir.join("etcd").is_file() {
            return Some(dir);
        }
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .find(|d| d.join("kube-apiserver").is_file() && d.join("etcd").is_file())
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// `bash -c`: run the binary, and end it when this test process is gone.
/// A test binary that panics or is killed leaves no server behind.
const WATCHED: &str = r#""$0" "$@" & child=$!
while kill -0 "$PPID" 2>/dev/null && kill -0 "$child" 2>/dev/null; do sleep 0.5; done
kill "$child" 2>/dev/null; wait "$child""#;

fn spawn(bin: &Path, args: &[String], log: &Path) {
    let log = std::fs::File::create(log).unwrap();
    Command::new("bash")
        .arg("-c")
        .arg(WATCHED)
        .arg(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
}

async fn start() -> Option<EnvTest> {
    let Some(bin) = binaries() else {
        if std::env::var("BALERIX_REQUIRE_TOOLS").as_deref() == Ok("1") {
            panic!("envtest binaries missing and BALERIX_REQUIRE_TOOLS=1 (run through `mise run operator`)");
        }
        eprintln!("skip: envtest binaries (kube-apiserver, etcd) missing");
        return None;
    };
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("envtest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("etcd")).unwrap();
    std::fs::create_dir_all(root.join("certs")).unwrap();

    // the service-account signing key the API server insists on
    let key = rcgen::KeyPair::generate().unwrap();
    std::fs::write(root.join("certs/sa.key"), key.serialize_pem()).unwrap();
    std::fs::write(root.join("certs/sa.pub"), key.public_key_pem()).unwrap();
    std::fs::write(root.join("token.csv"), "envtest-token,admin,uid-admin,system:masters\n").unwrap();

    let (etcd_client, etcd_peer, api_port) = (free_port(), free_port(), free_port());
    spawn(
        &bin.join("etcd"),
        &[
            format!("--data-dir={}", root.join("etcd").display()),
            format!("--listen-client-urls=http://127.0.0.1:{etcd_client}"),
            format!("--advertise-client-urls=http://127.0.0.1:{etcd_client}"),
            format!("--listen-peer-urls=http://127.0.0.1:{etcd_peer}"),
            "--unsafe-no-fsync".to_string(),
        ],
        &root.join("etcd.log"),
    );
    spawn(
        &bin.join("kube-apiserver"),
        &[
            format!("--etcd-servers=http://127.0.0.1:{etcd_client}"),
            format!("--secure-port={api_port}"),
            "--bind-address=127.0.0.1".to_string(),
            "--advertise-address=127.0.0.1".to_string(),
            format!("--cert-dir={}", root.join("certs").display()),
            "--service-cluster-ip-range=10.0.0.0/24".to_string(),
            "--authorization-mode=RBAC".to_string(),
            format!("--token-auth-file={}", root.join("token.csv").display()),
            "--service-account-issuer=https://localhost".to_string(),
            format!("--service-account-key-file={}", root.join("certs/sa.pub").display()),
            format!("--service-account-signing-key-file={}", root.join("certs/sa.key").display()),
            "--disable-admission-plugins=ServiceAccount".to_string(),
            "--allow-privileged=true".to_string(),
        ],
        &root.join("apiserver.log"),
    );

    let kubeconfig = root.join("kubeconfig");
    std::fs::write(
        &kubeconfig,
        format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: envtest\n  cluster:\n    server: https://127.0.0.1:{api_port}\n    insecure-skip-tls-verify: true\nusers:\n- name: admin\n  user:\n    token: envtest-token\ncontexts:\n- name: envtest\n  context: {{ cluster: envtest, user: admin }}\ncurrent-context: envtest\n"
        ),
    )
    .unwrap();
    let kc = Kubeconfig::read_from(&kubeconfig).unwrap();
    let config = Config::from_custom_kubeconfig(kc, &KubeConfigOptions::default()).await.unwrap();
    let client = Client::try_from(config).unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    while client.apiserver_version().await.is_err() {
        assert!(Instant::now() < deadline, "kube-apiserver did not come up; see {}", root.join("apiserver.log").display());
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    for (_, yaml) in balerix_operator::api::crd_files().unwrap() {
        let crd: CustomResourceDefinition = serde_norway::from_str(&yaml).unwrap();
        crds.create(&PostParams::default(), &crd).await.unwrap();
    }
    // established: a list of each kind answers
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let ok = Api::<balerix_operator::api::Daemon>::all(client.clone()).list(&ListParams::default()).await.is_ok()
            && Api::<balerix_operator::api::Fleet>::all(client.clone()).list(&ListParams::default()).await.is_ok()
            && Api::<balerix_operator::api::Crew>::all(client.clone()).list(&ListParams::default()).await.is_ok()
            && Api::<balerix_operator::api::Agent>::all(client.clone()).list(&ListParams::default()).await.is_ok()
            && Api::<balerix_operator::api::Plugin>::all(client.clone()).list(&ListParams::default()).await.is_ok();
        if ok {
            break;
        }
        assert!(Instant::now() < deadline, "the definitions were not established");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Some(EnvTest { client, kubeconfig })
}
```

`rcgen::KeyPair::public_key_pem` is in rcgen 0.14 (it was probed in 3a's planning for `serialize_pem`; if `public_key_pem` is absent, use `key.public_key_der()` wrapped as `-----BEGIN PUBLIC KEY-----` base64 lines through the `pem` feature's `pem::encode`). `serde_norway` is already an operator dependency.

Add to `operator/tests/support/mod.rs` (keep `balerix()` and `temp_root`):

```rust
pub mod envtest;
pub mod stub_daemon;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use balerix_operator::controllers::{Clock, RunConfig};
use balerix_operator::desired::common::Images;
use k8s_openapi::api::core::v1::Namespace;
use kube::api::PostParams;
use kube::{Api, Client};

/// A namespace of this test's own: `t-<label>-<pid>`.
pub async fn namespace(client: &Client, label: &str) -> String {
    let name = format!("t-{label}-{}", std::process::id());
    let ns: Namespace = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": name }
    }))
    .unwrap();
    Api::<Namespace>::all(client.clone()).create(&PostParams::default(), &ns).await.unwrap();
    name
}

/// Polls `probe` every 200 ms until it answers `Some`, or panics with
/// `what` after `timeout`.
pub async fn wait_for<T, F, Fut>(what: &str, timeout: Duration, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(t) = probe().await {
            return t;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Holds `what` true for `hold`: panics the first time `probe` answers `Some`.
pub async fn hold_for<T, F, Fut>(what: &str, hold: Duration, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + hold;
    while Instant::now() < deadline {
        assert!(probe().await.is_none(), "{what} happened; it must not");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The system clock plus an offset the test moves.
#[derive(Clone, Default)]
pub struct TestClock {
    pub offset: Arc<AtomicI64>,
}

impl TestClock {
    pub fn clock(&self) -> Clock {
        let offset = self.offset.clone();
        Arc::new(move || {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            now + offset.load(Ordering::SeqCst)
        })
    }
    pub fn advance(&self, secs: i64) {
        self.offset.fetch_add(secs, Ordering::SeqCst);
    }
}

/// The controllers over one namespace, as a task the test aborts at its end.
pub fn spawn_operator(
    env: &envtest::EnvTest,
    namespace: &str,
    daemon_url: Option<String>,
    clock: &TestClock,
) -> tokio::task::AbortHandle {
    let mut cfg = RunConfig::new(
        "0.2.0",
        Images {
            daemon: "balerix:test".into(),
            agent: "balerix-agent:test".into(),
        },
        namespace,
    );
    cfg.watch_namespaces = Some(vec![namespace.to_string()]);
    cfg.fleet_period = Duration::from_secs(1);
    cfg.period = Duration::from_secs(1);
    cfg.clock = clock.clock();
    cfg.insecure_daemon_url = daemon_url;
    tokio::spawn(balerix_operator::controllers::run(env.client.clone(), cfg)).abort_handle()
}
```

Create `operator/tests/support/stub_daemon.rs` holding only `//! Task 6: the scripted Daemon.` for now (an empty module compiles; Task 6 fills it).

- [ ] **Step 6: Write the first envtest test**

Create `operator/tests/controllers_it.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The controllers against a real API server (Spec O §21.3). The test is
//! the kubelet: it patches Job and Pod status and force-deletes pods.
mod support;

use std::time::Duration;

use balerix_operator::api::{Daemon, DaemonSpec};
use kube::api::PostParams;
use kube::Api;
use support::envtest::envtest;
use support::{TestClock, namespace, spawn_operator, wait_for};

fn daemon_spec() -> DaemonSpec {
    serde_json::from_value(serde_json::json!({
        "storage": {
            "state": { "size": "1Gi" },
            "shared": { "size": "10Gi" },
            "agent": { "size": "2Gi" }
        }
    }))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_harness_serves_the_kinds_and_the_operator_starts_and_stops() {
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "harness").await;
    let daemons: Api<Daemon> = Api::namespaced(env.client.clone(), &ns);
    daemons
        .create(&PostParams::default(), &Daemon::new("default", daemon_spec()))
        .await
        .unwrap();
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, None, &clock);
    let got = wait_for("the Daemon to be listed", Duration::from_secs(10), || async {
        daemons.get_opt("default").await.unwrap()
    })
    .await;
    assert_eq!(got.spec, daemon_spec());
    tokio::time::sleep(Duration::from_millis(500)).await;
    operator.abort();
}
```

- [ ] **Step 7: Run it**

Run: `mise run operator`
Expected: PASS, with the harness's start visible as a few seconds in the test's time; `ls operator/target/tmp/` shows an `envtest-<pid>` directory with `apiserver.log`; after the run `pgrep -f kube-apiserver` finds nothing (the watched shell ended it). Also run with `BALERIX_REQUIRE_TOOLS=1 ENVTEST_DIR=/nonexistent PATH=/usr/bin:/bin CARGO_TARGET_DIR=operator/target mise x -- cargo nextest run --manifest-path operator/Cargo.toml --test controllers_it` and expect the panic `envtest binaries missing and BALERIX_REQUIRE_TOOLS=1`.

- [ ] **Step 8: Commit**

```bash
git add operator/Cargo.toml operator/Cargo.lock operator/deny.toml operator/src/lib.rs operator/src/main.rs operator/src/controllers operator/tests/support operator/tests/controllers_it.rs mise.toml scripts/operator.sh
git commit -m "feat(operator): run, the controllers' context, and the envtest harness (Spec O §21.2, §21.3)

kube gains client and runtime on its default TLS stack. The controllers'
Context carries the record cache, the per-Daemon clients, the clock and
the periods; run starts one set of controllers per watched namespace.
The tests start a real kube-apiserver and etcd (the envtest binaries,
pinned on the operator task) once per test binary."
```

---

### Task 5: The Job rule and the Daemon controller

**Files:**
- Create: `operator/src/controllers/jobs.rs`, `operator/src/controllers/daemon.rs`
- Modify: `operator/src/controllers/mod.rs` (`report`, the `set` line), `operator/tests/support/mod.rs` (the kubelet helpers), `operator/tests/controllers_it.rs`

**Interfaces:**
- Consumes: `desired::jobs::{JobContext, daemon_pool_job, job_outcome}`, `desired::common::{JobOutcome, owner_of}`, `desired::daemon::{Material, daemon_secrets, daemon_objects, daemon_status, version_ok, DaemonObserved, NOT_AFTER_ANNOTATION}`, `pki::{new_authority, issue_serving, needs_renewal, daemon_names, new_token, Issued}`, `desired::names`, Task 4's `Context`, `apply`, `patch_status`, `error_policy`, `reconciled`, `api_in`.
- Produces:
  - `controllers::jobs::ATTEMPTS_ANNOTATION = "balerix.ai/attempts"`; `controllers::jobs::Ensured { outcome: JobOutcome, again: Duration }`; `controllers::jobs::ensure_job<K>(ctx: &Context, owner: &K, wanted: Job, crew_lock: Option<(&str, &str)>) -> Result<Ensured, Error>`; `controllers::jobs::delay(attempt: u32) -> Duration`; `controllers::jobs::crew_busy(ctx, namespace, fleet, crew, except: &str) -> Result<bool, Error>`;
  - `controllers::daemon::controller(ctx: Arc<Context>, namespace: Option<&str>) -> impl Future<Output = ()>`; `controllers::daemon::reconcile(daemon: Arc<Daemon>, ctx: Arc<Context>) -> Result<Action, Error>`; `controllers::daemon::read_secret_string(secret: &Secret, key: &str) -> Option<String>` (shared by the other controllers); `controllers::daemon::authority_and_token(ctx, namespace, daemon) -> Result<(String, String), Error>` (the authority PEM from the ConfigMap and the admin token from the Secret, for the Fleet controller);
  - `controllers::report<K>(result)` the `for_each` logger;
  - test support: `support::finish_job(client, ns, name, succeeded: bool, message: Option<&str>)`.

- [ ] **Step 1: Write the Job rule's unit tests**

In `operator/src/controllers/jobs.rs` (the whole file, tests included; the async function comes in Step 3):

```rust
//! One rule for every Job the operator makes (Spec O §21.2): `backoffLimit:
//! 0`, a stale Job is replaced, a failed one is retried by the operator
//! after a doubling delay carried on its owner's `balerix.ai/attempts`
//! annotation, and a crew's sync, harvest and cleanup Jobs never run at
//! once (§5.3).

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Resource, ResourceExt};
use serde::de::DeserializeOwned;

use super::{Context, Error};
use crate::desired::common::JobOutcome;
use crate::desired::jobs::job_outcome;

/// On the owner: a JSON object from Job name to the attempt its next run
/// is. Absent or missing the name means attempt 1.
pub const ATTEMPTS_ANNOTATION: &str = "balerix.ai/attempts";

pub struct Ensured {
    pub outcome: JobOutcome,
    /// When to look again: soon while a Job runs or a lock holds, the
    /// rest of the delay for a failed one, the caller's period otherwise.
    pub again: Duration,
}

/// 30 s for attempt 1, doubling, at most 10 min (§21.2).
pub fn delay(attempt: u32) -> Duration {
    let n = attempt.max(1) - 1;
    Duration::from_secs((30u64 << n.min(5)).min(600))
}

fn attempts_of<K: Resource>(owner: &K) -> BTreeMap<String, u32> {
    owner
        .annotations()
        .get(ATTEMPTS_ANNOTATION)
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default()
}

/// When the Job failed: its `Failed` condition's transition, else its
/// creation. Unix seconds; `None` when the API server set neither.
fn failed_at(job: &Job) -> Option<i64> {
    let condition = job
        .status
        .as_ref()?
        .conditions
        .iter()
        .flatten()
        .find(|c| c.type_ == "Failed" && c.status == "True")
        .and_then(|c| c.last_transition_time.as_ref());
    condition
        .or(job.metadata.creation_timestamp.as_ref())
        .map(|t| t.0.as_second())
}

/// Whether a Job is still running: created and not yet succeeded or failed.
fn unfinished(job: &Job) -> bool {
    let status = job.status.as_ref();
    status.and_then(|s| s.succeeded).unwrap_or(0) == 0
        && status.and_then(|s| s.failed).unwrap_or(0) == 0
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_delay_doubles_from_thirty_seconds_to_a_ten_minute_cap() {
        assert_eq!(delay(0), Duration::from_secs(30));
        assert_eq!(delay(1), Duration::from_secs(30));
        assert_eq!(delay(2), Duration::from_secs(60));
        assert_eq!(delay(5), Duration::from_secs(480));
        assert_eq!(delay(6), Duration::from_secs(600));
        assert_eq!(delay(60), Duration::from_secs(600));
    }

    #[test]
    fn failed_at_prefers_the_failed_condition_over_creation() {
        let job: Job = serde_json::from_value(serde_json::json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": { "name": "j", "namespace": "ns", "creationTimestamp": "2026-10-03T00:00:00Z" },
            "status": { "failed": 1, "conditions": [{ "type": "Failed", "status": "True", "lastTransitionTime": "2026-10-03T00:10:00Z" }] }
        })).unwrap();
        assert_eq!(failed_at(&job), Some(1_790_986_200)); // 2026-10-03T00:10:00Z
        let mut no_condition = job.clone();
        no_condition.status.as_mut().unwrap().conditions = None;
        assert_eq!(failed_at(&no_condition), Some(1_790_985_600)); // creation, 2026-10-03T00:00:00Z
        assert!(unfinished(&Job::default()));
        assert!(!unfinished(&job));
    }

    #[test]
    fn attempts_are_read_from_the_owner_annotation_and_default_to_one() {
        let job: Job = serde_json::from_value(serde_json::json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": { "name": "o", "namespace": "ns", "annotations": { ATTEMPTS_ANNOTATION: "{\"f-c-sync\": 3}" } }
        })).unwrap();
        let attempts = attempts_of(&job);
        assert_eq!(attempts.get("f-c-sync"), Some(&3));
        assert_eq!(*attempts.get("other").unwrap_or(&1), 1);
        assert!(attempts_of(&Job::default()).is_empty());
    }
}
```

The two unix seconds are `date -u -d 2026-10-03T00:00:00Z +%s` (1790985600) and that plus 600.

- [ ] **Step 2: Run them to see them fail**

Run: `CARGO_TARGET_DIR=operator/target mise x -- cargo nextest run --manifest-path operator/Cargo.toml controllers::jobs`
Expected: PASS for the three (they only use the private helpers), which proves the helpers; the async rule is next.

- [ ] **Step 3: Write `ensure_job` and `crew_busy`**

Add to `operator/src/controllers/jobs.rs` above the tests:

```rust
/// Whether another unfinished Job of this crew exists (§5.3's lock).
pub async fn crew_busy(
    ctx: &Context,
    namespace: &str,
    fleet: &str,
    crew: &str,
    except: &str,
) -> Result<bool, Error> {
    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    let list = jobs
        .list(&ListParams::default().labels(&format!("balerix.ai/fleet={fleet},balerix.ai/crew={crew}")))
        .await?;
    Ok(list
        .items
        .iter()
        .any(|j| j.name_any() != except && unfinished(j)))
}

async fn set_attempts<K>(ctx: &Context, owner: &K, attempts: &BTreeMap<String, u32>) -> Result<(), Error>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope> + Clone + std::fmt::Debug + DeserializeOwned,
    K::DynamicType: Default,
{
    let namespace = owner.namespace().unwrap_or_default();
    let api: Api<K> = Api::namespaced(ctx.client.clone(), &namespace);
    let value = serde_json::to_string(attempts).map_err(crate::desired::common::DesiredError::from)?;
    api.patch(
        &owner.name_any(),
        &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "metadata": { "annotations": { ATTEMPTS_ANNOTATION: value } } })),
    )
    .await?;
    Ok(())
}

/// `wanted` exists and is current, or is on its way: creates an absent
/// one, replaces a stale one, leaves a running one, retries a failed one
/// after its delay. `crew_lock` is the `(fleet, crew)` whose other Jobs
/// must be finished before this one starts.
pub async fn ensure_job<K>(
    ctx: &Context,
    owner: &K,
    wanted: Job,
    crew_lock: Option<(&str, &str)>,
) -> Result<Ensured, Error>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope> + Clone + std::fmt::Debug + DeserializeOwned,
    K::DynamicType: Default,
{
    let namespace = wanted.namespace().unwrap_or_default();
    let name = wanted.name_any();
    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), &namespace);
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let existing = jobs.get_opt(&name).await?;
    let job_pods = pods
        .list(&ListParams::default().labels(&format!("job-name={name}")))
        .await?
        .items;
    let outcome = job_outcome(existing.as_ref(), &job_pods, &wanted);
    let soon = Duration::from_secs(5);
    let mut attempts = attempts_of(owner);
    match &outcome {
        JobOutcome::Absent => {
            if let Some((fleet, crew)) = crew_lock {
                if crew_busy(ctx, &namespace, fleet, crew, &name).await? {
                    tracing::debug!(job = %name, "waiting: another Job of the crew runs");
                    return Ok(Ensured { outcome, again: soon });
                }
            }
            match jobs.create(&PostParams::default(), &wanted).await {
                Ok(_) => tracing::info!(job = %name, attempt = attempts.get(&name).copied().unwrap_or(1), "created"),
                // made by a reconcile that raced this one: it is running
                Err(kube::Error::Api(e)) if e.code == 409 => {}
                Err(e) => return Err(e.into()),
            }
            Ok(Ensured { outcome: JobOutcome::Running, again: soon })
        }
        JobOutcome::Stale => {
            jobs.delete(&name, &DeleteParams::background()).await?;
            if attempts.remove(&name).is_some() {
                set_attempts(ctx, owner, &attempts).await?;
            }
            tracing::info!(job = %name, "stale: replaced");
            Ok(Ensured { outcome, again: Duration::from_secs(2) })
        }
        JobOutcome::Running => Ok(Ensured { outcome, again: soon }),
        JobOutcome::Succeeded(_) => {
            if attempts.remove(&name).is_some() {
                set_attempts(ctx, owner, &attempts).await?;
            }
            Ok(Ensured { outcome, again: ctx.run.period })
        }
        JobOutcome::Failed(message) => {
            let attempt = attempts.get(&name).copied().unwrap_or(1);
            let now = ctx.now();
            let due = failed_at(existing.as_ref().ok_or_else(|| Error::Missing(format!("Job {name} vanished")))?)
                .unwrap_or(now)
                + delay(attempt).as_secs() as i64;
            if now >= due {
                jobs.delete(&name, &DeleteParams::background()).await?;
                attempts.insert(name.clone(), attempt + 1);
                set_attempts(ctx, owner, &attempts).await?;
                tracing::warn!(job = %name, attempt = attempt + 1, "failed: {message}; retrying");
                return Ok(Ensured { outcome, again: Duration::from_secs(2) });
            }
            Ok(Ensured { outcome, again: Duration::from_secs((due - now).max(1) as u64) })
        }
    }
}
```

- [ ] **Step 4: Write the Daemon controller**

Create `operator/src/controllers/daemon.rs`:

```rust
//! The Daemon controller (Spec O §5.1, §21.2): mints what is missing
//! (authority, serving certificate, admin token), applies the objects
//! `desired::daemon` makes, runs the pool Job under the Job rule, and
//! writes status, asking `/readyz` once the StatefulSet has a ready pod.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::PostParams;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, ResourceExt};

use super::jobs::ensure_job;
use super::{Context, Error, api_in, apply, error_policy, patch_status, reconciled, report};
use crate::api::Daemon;
use crate::desired::common::owner_of;
use crate::desired::daemon::{
    DaemonObserved, Material, NOT_AFTER_ANNOTATION, daemon_objects, daemon_secrets, daemon_status, version_ok,
};
use crate::desired::jobs::{JobContext, daemon_pool_job};
use crate::desired::names;
use crate::pki::{self, Issued};

pub async fn controller(ctx: Arc<Context>, namespace: Option<&str>) {
    let client = &ctx.client;
    Controller::new(api_in::<Daemon>(client, namespace), watcher::Config::default())
        .owns(api_in::<StatefulSet>(client, namespace), watcher::Config::default())
        .owns(api_in::<Service>(client, namespace), watcher::Config::default())
        .owns(api_in::<Secret>(client, namespace), watcher::Config::default())
        .owns(api_in::<ConfigMap>(client, namespace), watcher::Config::default())
        .owns(api_in::<Job>(client, namespace), watcher::Config::default())
        .owns(api_in::<NetworkPolicy>(client, namespace), watcher::Config::default())
        .run(reconcile, error_policy, ctx.clone())
        .for_each(|r| async move { report("daemon", r) })
        .await;
}

/// One key of a Secret as text (`data` is already base64-decoded).
pub fn read_secret_string(secret: &Secret, key: &str) -> Option<String> {
    let bytes = secret.data.as_ref()?.get(key)?;
    String::from_utf8(bytes.0.clone()).ok()
}

fn read_issued(secret: Option<&Secret>, cert_key: &str, key_key: &str) -> Option<Issued> {
    let secret = secret?;
    let not_after = secret.annotations().get(NOT_AFTER_ANNOTATION)?.parse().ok()?;
    Some(Issued {
        cert_pem: read_secret_string(secret, cert_key)?,
        key_pem: read_secret_string(secret, key_key)?,
        not_after,
    })
}

/// The authority's certificate (from the ConfigMap) and the admin token:
/// what a client of this Daemon is built from.
pub async fn authority_and_token(
    ctx: &Context,
    namespace: &str,
    daemon: &str,
) -> Result<(String, String), Error> {
    let configmaps: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), namespace);
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    let authority = configmaps
        .get_opt(&names::authority(daemon))
        .await?
        .and_then(|c| c.data?.get("ca.crt").cloned())
        .ok_or_else(|| Error::Missing(format!("Daemon {daemon} has no authority ConfigMap yet")))?;
    let token = secrets
        .get_opt(&names::admin(daemon))
        .await?
        .as_ref()
        .and_then(|s| read_secret_string(s, "token"))
        .ok_or_else(|| Error::Missing(format!("Daemon {daemon} has no admin Secret yet")))?;
    Ok((authority, token))
}

/// Reads the material back, minting what is absent or due (§21.2), and
/// applies the four Secret-like objects. Returns the serving expiry.
async fn ensure_material(ctx: &Context, daemon: &Daemon, namespace: &str, name: &str) -> Result<i64, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    let now = ctx.now();
    let authority = match read_issued(secrets.get_opt(&names::authority(name)).await?.as_ref(), "ca.crt", "ca.key") {
        Some(a) => a,
        None => {
            tracing::info!(daemon = %name, "minting the authority");
            pki::new_authority(namespace, name, now)?
        }
    };
    let serving = match read_issued(secrets.get_opt(&names::serving(name)).await?.as_ref(), "tls.crt", "tls.key") {
        Some(s) if !pki::needs_renewal(s.not_after, now) => s,
        existing => {
            tracing::info!(daemon = %name, renewal = existing.is_some(), "issuing the serving certificate");
            pki::issue_serving(&authority, namespace, name, &pki::daemon_names(namespace, name), now)?
        }
    };
    let admin_token = match secrets.get_opt(&names::admin(name)).await?.as_ref().and_then(|s| read_secret_string(s, "token")) {
        Some(t) => t,
        None => pki::new_token(),
    };
    let material = Material { authority: &authority, serving: &serving, admin_token: &admin_token };
    let objects = daemon_secrets(daemon, &material)?;
    apply(&ctx.client, &objects.authority).await?;
    apply(&ctx.client, &objects.authority_config).await?;
    apply(&ctx.client, &objects.serving).await?;
    apply(&ctx.client, &objects.admin).await?;
    Ok(serving.not_after)
}

pub async fn reconcile(daemon: Arc<Daemon>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = daemon.namespace().unwrap_or_default();
    let name = daemon.name_any();
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &namespace);
    let statefulsets: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), &namespace);

    let (observed_pool, again) = if version_ok(&daemon, &ctx.cfg) {
        let not_after = ensure_material(&ctx, &daemon, &namespace, &name).await?;
        let objects = daemon_objects(&daemon, &ctx.cfg, not_after)?;
        // a claim is immutable once bound: created when absent, never re-applied
        for claim in &objects.claims {
            if claims.get_opt(&claim.name_any()).await?.is_none() {
                claims.create(&PostParams::default(), claim).await?;
            }
        }
        apply(&ctx.client, &objects.service).await?;
        apply(&ctx.client, &objects.statefulset).await?;
        apply(&ctx.client, &objects.policy).await?;
        let pool = ensure_job(ctx.as_ref(), daemon.as_ref(), objects.pool_job, None).await?;
        (pool.outcome, pool.again)
    } else {
        (crate::desired::common::JobOutcome::Absent, ctx.run.period)
    };

    let shared = claims.get_opt(&names::shared_claim(&name)).await?;
    let statefulset = statefulsets.get_opt(&names::daemon(&name)).await?;
    let observed = DaemonObserved {
        shared_claim: shared.as_ref(),
        statefulset: statefulset.as_ref(),
        pool: &observed_pool,
    };
    let mut status = daemon_status(&daemon, &ctx.cfg, &observed, &ctx.k8s_now());
    status.endpoint = Some(names::endpoint(&namespace, &name));
    // `Ready` additionally needs the daemon to answer (§21.2)
    if status.conditions.iter().any(|c| c.type_ == "Ready" && c.status == "True") {
        let (authority, token) = authority_and_token(&ctx, &namespace, &name).await?;
        let client = ctx.daemon_client(&namespace, &name, &names::endpoint(&namespace, &name), &authority, &token)?;
        if let Err(e) = client.ready().await {
            if let Some(ready) = status.conditions.iter_mut().find(|c| c.type_ == "Ready") {
                ready.status = "False".to_string();
                ready.reason = "DaemonNotReady".to_string();
                ready.message = e.to_string();
                ready.last_transition_time = ctx.k8s_now();
            }
        }
    }
    patch_status(&ctx.client, daemon.as_ref(), &status).await?;
    reconciled(&ctx, daemon.as_ref());
    Ok(Action::requeue(again.min(ctx.run.period)))
}
```

`daemon_objects` builds the pool Job itself (`DaemonObjects::pool_job`, 3a), so the controller never calls `daemon_pool_job`; drop the `JobContext`, `daemon_pool_job`, `owner_of` and `Duration` imports if clippy reports them unused.

In `operator/src/controllers/mod.rs` add the logger and the controller line:

```rust
/// What a controller's stream yields, as a log line.
pub fn report<K: Resource>(
    kind: &'static str,
    result: Result<(kube::runtime::reflector::ObjectRef<K>, Action), kube::runtime::controller::Error<Error, kube::runtime::watcher::Error>>,
) where
    K::DynamicType: std::fmt::Debug + std::hash::Hash + Eq + Clone,
{
    match result {
        Ok((object, _)) => tracing::debug!(kind, object = %object, "reconciled"),
        Err(e) => tracing::warn!(kind, "{e}"),
    }
}
```

and in `set`, replace the `// Task 5:` comment with `Box::pin(daemon::controller(ctx.clone(), ns)),` (keep the `ns` binding; remove `let _ = ns;` once a line uses it).

- [ ] **Step 5: Add the kubelet helper to the test support**

In `operator/tests/support/mod.rs`:

```rust
/// The kubelet's part for a Job in envtest: marks it succeeded or failed
/// (with a `Failed` condition stamped now), and when `message` is given,
/// leaves a pod labelled `job-name` whose container terminated with it.
pub async fn finish_job(client: &Client, namespace: &str, name: &str, succeeded: bool, message: Option<&str>) {
    use k8s_openapi::api::batch::v1::Job;
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Patch, PatchParams};
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let now = k8s_openapi::jiff::Timestamp::now().to_string();
    let status = if succeeded {
        serde_json::json!({ "status": { "succeeded": 1, "conditions": [{ "type": "Complete", "status": "True", "lastTransitionTime": now }] } })
    } else {
        serde_json::json!({ "status": { "failed": 1, "conditions": [{ "type": "Failed", "status": "True", "lastTransitionTime": now }] } })
    };
    jobs.patch_status(name, &PatchParams::default(), &Patch::Merge(&status)).await.unwrap();
    if let Some(message) = message {
        let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
        let pod_name = format!("{name}-pod");
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": { "name": pod_name, "namespace": namespace, "labels": { "job-name": name } },
            "spec": { "containers": [{ "name": "job", "image": "x" }], "restartPolicy": "Never" }
        })).unwrap();
        if pods.get_opt(&pod_name).await.unwrap().is_none() {
            pods.create(&PostParams::default(), &pod).await.unwrap();
        }
        let phase = if succeeded { "Succeeded" } else { "Failed" };
        pods.patch_status(&pod_name, &PatchParams::default(), &Patch::Merge(&serde_json::json!({
            "status": { "phase": phase, "containerStatuses": [{
                "name": "job", "image": "x", "imageID": "x", "ready": false, "restartCount": 0,
                "state": { "terminated": { "exitCode": if succeeded { 0 } else { 1 }, "message": message } }
            }] }
        }))).await.unwrap();
    }
}
```

- [ ] **Step 6: Write the failing Daemon tests**

Append to `operator/tests/controllers_it.rs` (add the imports it needs: `k8s_openapi::api::{apps::v1::StatefulSet, batch::v1::Job, core::v1::{ConfigMap, PersistentVolumeClaim, Secret, Service}, networking::v1::NetworkPolicy}`, `kube::api::{Patch, PatchParams}`, `support::{finish_job, hold_for}`):

```rust
fn condition<'a>(conditions: &'a [k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition], type_: &str) -> &'a k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition {
    conditions.iter().find(|c| c.type_ == type_).unwrap_or_else(|| panic!("no condition {type_}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_gets_its_objects_and_a_renewal_rolls_the_pod() {
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "daemon").await;
    let client = env.client.clone();
    let daemons: Api<Daemon> = Api::namespaced(client.clone(), &ns);
    daemons.create(&PostParams::default(), &Daemon::new("default", daemon_spec())).await.unwrap();
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, None, &clock);

    let sts = wait_for("the StatefulSet", Duration::from_secs(30), || async {
        Api::<StatefulSet>::namespaced(client.clone(), &ns).get_opt("balerix-default").await.unwrap()
    }).await;
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);
    let ca = secrets.get("balerix-default-ca").await.unwrap();
    assert!(ca.data.as_ref().unwrap().contains_key("ca.crt") && ca.data.as_ref().unwrap().contains_key("ca.key"));
    let tls = secrets.get("balerix-default-tls").await.unwrap();
    assert_eq!(tls.type_.as_deref(), Some("kubernetes.io/tls"));
    let not_after: i64 = tls.metadata.annotations.as_ref().unwrap()["balerix.ai/not-after"].parse().unwrap();
    let admin = secrets.get("balerix-default-admin").await.unwrap();
    assert_eq!(admin.data.as_ref().unwrap()["token"].0.len(), 64);
    assert!(Api::<ConfigMap>::namespaced(client.clone(), &ns).get("balerix-default-ca").await.unwrap().data.unwrap().contains_key("ca.crt"));
    Api::<Service>::namespaced(client.clone(), &ns).get("balerix-default").await.unwrap();
    Api::<NetworkPolicy>::namespaced(client.clone(), &ns).get("balerix-default").await.unwrap();
    Api::<Job>::namespaced(client.clone(), &ns).get("balerix-default-pool").await.unwrap();
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    claims.get("balerix-default-state").await.unwrap();
    claims.get("balerix-default-shared").await.unwrap();
    assert_eq!(sts.spec.as_ref().unwrap().template.metadata.as_ref().unwrap().annotations.as_ref().unwrap()["balerix.ai/not-after"], not_after.to_string());

    let status = wait_for("status", Duration::from_secs(10), || async {
        daemons.get("default").await.unwrap().status.filter(|s| !s.conditions.is_empty())
    }).await;
    assert_eq!(status.endpoint.as_deref(), Some(format!("https://balerix-default.{ns}.svc:7643").as_str()));
    let storage = condition(&status.conditions, "StorageReady");
    assert_eq!((storage.status.as_str(), storage.reason.as_str()), ("False", "ClaimPending"));
    assert_eq!(condition(&status.conditions, "Ready").status, "False");
    assert_eq!(condition(&status.conditions, "SystemToolsReady").reason, "PoolSyncRunning");

    // the serving certificate expires in ten days: inside the renewal window
    let soon = clock.clock()() + 10 * 86_400;
    secrets.patch("balerix-default-tls", &PatchParams::default(), &Patch::Merge(serde_json::json!({
        "metadata": { "annotations": { "balerix.ai/not-after": soon.to_string() } }
    }))).await.unwrap();
    let renewed = wait_for("a renewed certificate", Duration::from_secs(30), || async {
        let s = secrets.get("balerix-default-tls").await.unwrap();
        let t: i64 = s.metadata.annotations.as_ref().unwrap()["balerix.ai/not-after"].parse().unwrap();
        (t > soon + 60 * 86_400).then_some((t, s))
    }).await;
    assert_ne!(renewed.1.data.as_ref().unwrap()["tls.crt"], tls.data.as_ref().unwrap()["tls.crt"]);
    wait_for("the pod template to roll", Duration::from_secs(30), || async {
        let s = Api::<StatefulSet>::namespaced(client.clone(), &ns).get("balerix-default").await.unwrap();
        (s.spec.unwrap().template.metadata.unwrap().annotations.unwrap()["balerix.ai/not-after"] == renewed.0.to_string()).then_some(())
    }).await;
    // the authority and the admin token are kept
    assert_eq!(secrets.get("balerix-default-ca").await.unwrap().data, ca.data);
    assert_eq!(secrets.get("balerix-default-admin").await.unwrap().data, admin.data);
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_pool_job_is_reported_and_retried_after_the_delay() {
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "pooljob").await;
    let client = env.client.clone();
    let daemons: Api<Daemon> = Api::namespaced(client.clone(), &ns);
    daemons.create(&PostParams::default(), &Daemon::new("default", daemon_spec())).await.unwrap();
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, None, &clock);
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let first = wait_for("the pool Job", Duration::from_secs(30), || async { jobs.get_opt("balerix-default-pool").await.unwrap() }).await;

    // the kubelet ran it, it failed, and its pod is already gone
    finish_job(&client, &ns, "balerix-default-pool", false, None).await;
    let status = wait_for("SystemToolsReady=False", Duration::from_secs(10), || async {
        let s = daemons.get("default").await.unwrap().status?;
        (condition(&s.conditions, "SystemToolsReady").status == "False").then_some(s)
    }).await;
    let tools = condition(&status.conditions, "SystemToolsReady");
    assert_eq!(tools.reason, "PoolSyncFailed");
    assert_eq!(tools.message, "the job failed and left no message");
    // not retried before the delay
    hold_for("a retry before 30 s", Duration::from_secs(3), || async {
        (jobs.get("balerix-default-pool").await.unwrap().metadata.uid != first.metadata.uid).then_some(())
    }).await;

    clock.advance(31);
    let second = wait_for("the retry", Duration::from_secs(30), || async {
        let j = jobs.get_opt("balerix-default-pool").await.unwrap()?;
        (j.metadata.uid != first.metadata.uid).then_some(j)
    }).await;
    let attempts = daemons.get("default").await.unwrap().metadata.annotations.unwrap()["balerix.ai/attempts"].clone();
    assert_eq!(attempts, "{\"balerix-default-pool\":2}");

    finish_job(&client, &ns, "balerix-default-pool", true, Some("synced")).await;
    wait_for("SystemToolsReady=True", Duration::from_secs(10), || async {
        let s = daemons.get("default").await.unwrap().status?;
        (condition(&s.conditions, "SystemToolsReady").status == "True").then_some(())
    }).await;
    assert_eq!(condition(&daemons.get("default").await.unwrap().status.unwrap().conditions, "SystemToolsReady").reason, "PoolSynced");
    let attempts = daemons.get("default").await.unwrap().metadata.annotations.unwrap_or_default().get("balerix.ai/attempts").cloned();
    assert!(attempts.is_none() || attempts.as_deref() == Some("{}"), "{attempts:?}");
    let _ = second;
    operator.abort();
}
```

- [ ] **Step 7: Run them to see them fail, then pass**

Run: `mise run operator`
Expected first: FAIL (no controller registered: nothing is created). After Step 4's `set` line is in: PASS. If a `StatefulSet` apply is refused by the API server's validation, the message names the field; fix the `desired` manifest (3a's snapshot then changes, and that is a finding for the commit message), never the validation.

- [ ] **Step 8: Commit**

```bash
git add operator/src/controllers operator/tests
git commit -m "feat(operator): the Job rule and the Daemon controller (Spec O §5.1, §21.2)

A failed Job is retried after 30 s doubling to 10 min, the attempt kept
on the owner's balerix.ai/attempts; a stale one is replaced; a crew's
Jobs never run at once. The Daemon controller mints the authority, the
serving certificate (renewed thirty days before expiry) and the admin
token, applies the StatefulSet, Service and policies, and asks /readyz
before reporting Ready."
```

---

### Task 6: The Fleet controller and the stub Daemon

**Files:**
- Create: `operator/src/controllers/fleet.rs`
- Modify: `operator/src/controllers/mod.rs` (`Error::Waiting`, `daemon_fleets`, the `set` line), `operator/tests/support/stub_daemon.rs`, `operator/tests/controllers_it.rs`

**Interfaces:**
- Consumes: `desired::fleet::{plan_fleet, fleet_conditions, Accepted, FleetPlan, FLEET_FINALIZER, PlanError}`, `desired::jobs::{JobContext, fleet_pool_job, remove_job}`, `desired::common::{conditions, labels, owner_of, typed}`, `desired::names`, `pki::new_token`, `controllers::daemon::authority_and_token`, `controllers::jobs::ensure_job`, `DaemonClient::{apply, get, down}`, `balerix_api::AgentTokens`.
- Produces:
  - `controllers::fleet::controller(ctx, namespace)`, `controllers::fleet::reconcile(fleet: Arc<Fleet>, ctx) -> Result<Action, Error>`;
  - `controllers::fleet::daemon_of(ctx, namespace, daemon_name) -> Result<Option<Daemon>, Error>` and `controllers::fleet::client_for(ctx, &Daemon) -> Result<Arc<DaemonClient>, Error>` (the Agent controller reuses neither; the Daemon controller's `authority_and_token` is what both call);
  - `Context::daemon_fleets: RwLock<BTreeMap<String, Vec<String>>>` (`<ns>/<daemon>` to Fleet names), `Error::Waiting(String)` (requeued in 2 s by `error_policy`, no warning, no count);
  - `Context::records` entries keyed `<ns>/<fleet>` and `Context::fleet_agents` entries (Agent object names) written on every reconcile;
  - test support: `support::stub_daemon::StubDaemon` with `start() -> StubDaemon`, `url() -> String`, `puts() -> Vec<FleetRequest>`, `deletes() -> Vec<String>`, `reject(message: Option<&str>)`, `set_agent(fleet: &str, key: &str, status: AgentStatus)` (what the next `GET` reports), `stop(self)`.

- [ ] **Step 1: Write the stub Daemon**

Replace `operator/tests/support/stub_daemon.rs`:

```rust
//! A scripted Daemon over plain HTTP (Spec O §21.3): records every `PUT`
//! and `DELETE`, answers `GET` with the record it keeps, and can be told
//! to reject or to stop.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use balerix_api::{AgentStatus, ErrorBody, FleetRecord, FleetRequest};

#[derive(Default)]
struct Inner {
    puts: Vec<FleetRequest>,
    deletes: Vec<String>,
    records: BTreeMap<String, FleetRecord>,
    reject: Option<String>,
    /// `<fleet>` to (`fleet/crew/agent` to status), laid over every record read.
    agents: BTreeMap<String, BTreeMap<String, AgentStatus>>,
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Inner>>);

pub struct StubDaemon {
    url: String,
    state: Shared,
    server: tokio::task::JoinHandle<()>,
}

async fn put_fleet(State(s): State<Shared>, Path(name): Path<String>, Json(request): Json<FleetRequest>) -> axum::response::Response {
    let mut inner = s.0.lock().unwrap();
    inner.puts.push(request.clone());
    if let Some(message) = &inner.reject {
        return (StatusCode::BAD_REQUEST, Json(ErrorBody { error: message.clone() })).into_response();
    }
    let mut record = FleetRecord::with_owner(request.spec, Some("kubernetes".to_string()));
    record.status.agents = inner.agents.get(&name).cloned().unwrap_or_default();
    inner.records.insert(name, record.clone());
    Json(record).into_response()
}

async fn get_fleet(State(s): State<Shared>, Path(name): Path<String>) -> axum::response::Response {
    let inner = s.0.lock().unwrap();
    match inner.records.get(&name) {
        Some(record) => {
            let mut record = record.clone();
            record.status.agents = inner.agents.get(&name).cloned().unwrap_or_default();
            Json(record).into_response()
        }
        None => (StatusCode::NOT_FOUND, Json(ErrorBody { error: format!("no fleet {name}") })).into_response(),
    }
}

async fn delete_fleet(State(s): State<Shared>, Path(name): Path<String>) -> StatusCode {
    let mut inner = s.0.lock().unwrap();
    inner.deletes.push(name.clone());
    inner.records.remove(&name);
    StatusCode::NO_CONTENT
}

impl StubDaemon {
    pub async fn start() -> Self {
        let state = Shared::default();
        let router = Router::new()
            .route("/readyz", get(|| async { StatusCode::OK }))
            .route("/v1/fleets/{name}", get(get_fleet).put(put_fleet).delete(delete_fleet))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, state, server }
    }
    pub fn url(&self) -> String {
        self.url.clone()
    }
    pub fn puts(&self) -> Vec<FleetRequest> {
        self.state.0.lock().unwrap().puts.clone()
    }
    pub fn deletes(&self) -> Vec<String> {
        self.state.0.lock().unwrap().deletes.clone()
    }
    pub fn reject(&self, message: Option<&str>) {
        self.state.0.lock().unwrap().reject = message.map(str::to_string);
    }
    pub fn set_agent(&self, fleet: &str, key: &str, status: AgentStatus) {
        self.state.0.lock().unwrap().agents.entry(fleet.to_string()).or_default().insert(key.to_string(), status);
    }
    /// Ends the server: connections are refused from here on.
    pub fn stop(self) {
        self.server.abort();
    }
}
```

`FleetRequest` derives `Clone` (check `crates/balerix-api/src/request.rs`; if not, add `Clone` to its derive: a one-line change in the core workspace, `mise run check` before the commit).

- [ ] **Step 2: Add `Waiting` and `daemon_fleets` to the context**

In `operator/src/controllers/mod.rs`:

- to `enum Error` add `/// A cleanup that is not finished: requeued in 2 s, not an error. #[error("waiting: {0}")] Waiting(String),`;
- to `Context` add `/// `<ns>/<daemon>` to the Fleets naming it: the Fleet controller's Daemon watch maps through this. pub daemon_fleets: RwLock<BTreeMap<String, Vec<String>>>,` initialised empty in `new`;
- in `error_policy`, before counting: `if let Error::Waiting(why) = error { tracing::debug!(object = %key, "{why}"); return Action::requeue(Duration::from_secs(2)); }`;
- in `set`, replace the `// Task 6:` comment with `Box::pin(fleet::controller(ctx.clone(), ns)),`.

- [ ] **Step 3: Write the Fleet controller**

Create `operator/src/controllers/fleet.rs`:

```rust
//! The Fleet controller (Spec O §5.2, §21.2): resolve, mint the missing
//! agent tokens, `PUT` the Daemon, then the children; a rejection lands
//! nothing. The record it reads back is the status the Agents mirror. On
//! deletion it waits for the Agents, downs the fleet on the Daemon and,
//! for `retain: None`, runs one cleanup Job per crew.

use std::collections::BTreeMap;
use std::sync::Arc;

use balerix_api::AgentTokens;
use futures_util::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{DeleteParams, ListParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{Event, finalizer};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;
use kube::{Api, ResourceExt};

use super::daemon::{authority_and_token, read_secret_string};
use super::jobs::ensure_job;
use super::{Context, Error, api_in, apply, error_policy, patch_status, reconciled, report};
use crate::api::{Agent, Crew, Daemon, Fleet, FleetStatus, Retain};
use crate::daemon_client::{ClientError, DaemonClient};
use crate::desired::common::{JobOutcome, conditions, labels, owner_of, typed};
use crate::desired::fleet::{Accepted, FLEET_FINALIZER, FleetPlan, fleet_conditions, plan_fleet};
use crate::desired::jobs::{JobContext, fleet_pool_job, remove_job};
use crate::desired::names;
use crate::pki::new_token;

pub async fn controller(ctx: Arc<Context>, namespace: Option<&str>) {
    let client = &ctx.client;
    let mapper_ctx = ctx.clone();
    Controller::new(api_in::<Fleet>(client, namespace), watcher::Config::default())
        .owns(api_in::<Crew>(client, namespace), watcher::Config::default())
        .owns(api_in::<Agent>(client, namespace), watcher::Config::default())
        .owns(api_in::<Job>(client, namespace), watcher::Config::default())
        .watches(api_in::<Daemon>(client, namespace), watcher::Config::default(), move |daemon: Daemon| {
            let ns = daemon.namespace().unwrap_or_default();
            let key = Context::key(&ns, &daemon.name_any());
            let fleets = mapper_ctx.daemon_fleets.read().unwrap_or_else(|e| e.into_inner());
            fleets
                .get(&key)
                .into_iter()
                .flatten()
                .map(|f| ObjectRef::<Fleet>::new(f).within(&ns))
                .collect::<Vec<_>>()
        })
        .run(reconcile, error_policy, ctx.clone())
        .for_each(|r| async move { report("fleet", r) })
        .await;
}

pub async fn daemon_of(ctx: &Context, namespace: &str, name: &str) -> Result<Option<Daemon>, Error> {
    Ok(Api::<Daemon>::namespaced(ctx.client.clone(), namespace).get_opt(name).await?)
}

/// The client for a Fleet's Daemon; `Missing` until the Daemon controller
/// has minted the authority and token.
pub async fn client_for(ctx: &Context, daemon: &Daemon) -> Result<Arc<DaemonClient>, Error> {
    let namespace = daemon.namespace().unwrap_or_default();
    let name = daemon.name_any();
    let (authority, token) = authority_and_token(ctx, &namespace, &name).await?;
    let endpoint = daemon
        .status
        .as_ref()
        .and_then(|s| s.endpoint.clone())
        .unwrap_or_else(|| names::endpoint(&namespace, &name));
    ctx.daemon_client(&namespace, &name, &endpoint, &authority, &token)
}

pub async fn reconcile(fleet: Arc<Fleet>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = fleet.namespace().unwrap_or_default();
    let fleets: Api<Fleet> = Api::namespaced(ctx.client.clone(), &namespace);
    let ctx2 = ctx.clone();
    finalizer(&fleets, FLEET_FINALIZER, fleet, |event| async move {
        match event {
            Event::Apply(fleet) => apply_fleet(fleet, ctx2).await,
            Event::Cleanup(fleet) => cleanup_fleet(fleet, ctx2).await,
        }
    })
    .await
    .map_err(Error::from)
}

/// The tokens of every planned Agent, minted into a Secret owned by the
/// Fleet when absent (§5.2 step 3).
async fn ensure_tokens(ctx: &Context, fleet: &Fleet, plan: &FleetPlan) -> Result<AgentTokens, Error> {
    let namespace = fleet.namespace().unwrap_or_default();
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &namespace);
    let mut tokens = AgentTokens::new();
    for agent in &plan.agents {
        let key = format!("{}/{}/{}", agent.spec.fleet, agent.spec.crew, agent.spec.agent);
        let name = names::token(&agent.name_any());
        let token = match secrets.get_opt(&name).await?.as_ref().and_then(|s| read_secret_string(s, "token")) {
            Some(t) => t,
            None => {
                let token = new_token();
                let secret: Secret = typed(serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "Secret",
                    "metadata": {
                        "name": name,
                        "namespace": namespace,
                        "labels": labels(&agent.spec.daemon, "token", &[
                            ("balerix.ai/fleet", agent.spec.fleet.as_str()),
                            ("balerix.ai/crew", agent.spec.crew.as_str()),
                            ("balerix.ai/agent", agent.spec.agent.as_str()),
                        ]),
                        "ownerReferences": [owner_of(fleet)?],
                    },
                    "type": "Opaque",
                    "stringData": { "token": token },
                }))?;
                apply(&ctx.client, &secret).await?;
                token
            }
        };
        tokens.insert(key, token);
    }
    Ok(tokens)
}

async fn write_status(ctx: &Context, fleet: &Fleet, resolved: Result<(), String>, accepted: &Accepted, ready: usize, total: usize) -> Result<(), Error> {
    let old = fleet.status.as_ref().map(|s| s.conditions.as_slice()).unwrap_or(&[]);
    let status = FleetStatus {
        observed_generation: fleet.metadata.generation,
        conditions: conditions(old, &fleet_conditions(resolved, accepted, ready, total), fleet.metadata.generation, &ctx.k8s_now()),
    };
    patch_status(&ctx.client, fleet, &status).await
}

/// Agents of this fleet whose `Ready` condition is true.
async fn ready_agents(ctx: &Context, namespace: &str, fleet: &str) -> Result<usize, Error> {
    let agents: Api<Agent> = Api::namespaced(ctx.client.clone(), namespace);
    let list = agents.list(&ListParams::default().labels(&format!("balerix.ai/fleet={fleet}"))).await?;
    Ok(list
        .items
        .iter()
        .filter(|a| a.status.as_ref().is_some_and(|s| s.conditions.iter().any(|c| c.type_ == "Ready" && c.status == "True")))
        .count())
}

async fn apply_fleet(fleet: Arc<Fleet>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = fleet.namespace().unwrap_or_default();
    let name = fleet.name_any();
    let key = Context::key(&namespace, &name);
    let period = ctx.run.fleet_period;

    let Some(daemon) = daemon_of(&ctx, &namespace, &fleet.spec.daemon).await? else {
        let message = format!("spec.daemon: Daemon {} does not exist", fleet.spec.daemon);
        write_status(&ctx, &fleet, Err(message), &Accepted::NotAttempted, 0, 0).await?;
        return Ok(Action::requeue(period));
    };
    {
        let mut by_daemon = ctx.daemon_fleets.write().unwrap_or_else(|e| e.into_inner());
        let list = by_daemon.entry(Context::key(&namespace, &fleet.spec.daemon)).or_default();
        if !list.contains(&name) {
            list.push(name.clone());
        }
    }

    // resolve, then the tokens, then the plan with every token in it
    let first = match plan_fleet(&fleet, &daemon, &AgentTokens::new(), &ctx.cfg.images) {
        Ok(plan) => plan,
        Err(e) => {
            write_status(&ctx, &fleet, Err(e.to_string()), &Accepted::NotAttempted, 0, 0).await?;
            reconciled(&ctx, fleet.as_ref());
            return Ok(Action::requeue(period));
        }
    };
    let tokens = ensure_tokens(&ctx, &fleet, &first).await?;
    let plan = plan_fleet(&fleet, &daemon, &tokens, &ctx.cfg.images)?;
    let request = plan.request.as_ref().ok_or_else(|| Error::Missing(format!("Fleet {name}: a token is still missing after minting")))?;

    // the Daemon: a 400 lands nothing (§5.2 step 4)
    let accepted = match client_for(&ctx, &daemon).await {
        Err(Error::Missing(why)) => Accepted::DaemonUnavailable(why),
        Err(e) => return Err(e),
        Ok(client) => match client.apply(request).await {
            Ok(_) => Accepted::Yes,
            Err(ClientError::Rejected(m)) | Err(ClientError::Conflict(m)) => Accepted::Rejected(m),
            Err(e) => Accepted::DaemonUnavailable(e.to_string()),
        },
    };
    let total = plan.agents.len();
    if accepted != Accepted::Yes {
        let ready = ready_agents(&ctx, &namespace, &name).await?;
        write_status(&ctx, &fleet, Ok(()), &accepted, ready, total).await?;
        reconciled(&ctx, fleet.as_ref());
        return Ok(Action::requeue(period));
    }

    // the children
    let job_ctx = JobContext { namespace: &namespace, daemon: &fleet.spec.daemon, images: &ctx.cfg.images, owner: owner_of(fleet.as_ref())? };
    let pool = ensure_job(ctx.as_ref(), fleet.as_ref(), fleet_pool_job(&job_ctx, &name, &plan.spec.tools)?, None).await?;
    for crew in &plan.crews {
        apply(&ctx.client, crew).await?;
    }
    for agent in &plan.agents {
        apply(&ctx.client, agent).await?;
    }
    let crews: Api<Crew> = Api::namespaced(ctx.client.clone(), &namespace);
    let agents: Api<Agent> = Api::namespaced(ctx.client.clone(), &namespace);
    let selector = ListParams::default().labels(&format!("balerix.ai/fleet={name}"));
    let wanted_crews: Vec<String> = plan.crews.iter().map(ResourceExt::name_any).collect();
    for crew in crews.list(&selector).await?.items {
        if !wanted_crews.contains(&crew.name_any()) && crew.metadata.deletion_timestamp.is_none() {
            tracing::info!(fleet = %key, crew = %crew.name_any(), "dropped from the Fleet");
            crews.delete(&crew.name_any(), &DeleteParams::default()).await?;
        }
    }
    let wanted_agents: Vec<String> = plan.agents.iter().map(ResourceExt::name_any).collect();
    for agent in agents.list(&selector).await?.items {
        if !wanted_agents.contains(&agent.name_any()) && agent.metadata.deletion_timestamp.is_none() {
            tracing::info!(fleet = %key, agent = %agent.name_any(), "dropped from the Fleet");
            agents.delete(&agent.name_any(), &DeleteParams::default()).await?;
        }
    }
    ctx.fleet_agents.write().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), wanted_agents);

    // the record the Agents mirror (§21.1)
    if let Ok(client) = client_for(&ctx, &daemon).await {
        if let Some(record) = client.get(&name).await? {
            ctx.records.write().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), record);
        }
    }
    let ready = ready_agents(&ctx, &namespace, &name).await?;
    write_status(&ctx, &fleet, Ok(()), &accepted, ready, total).await?;
    reconciled(&ctx, fleet.as_ref());
    Ok(Action::requeue(period.min(pool.again)))
}

async fn cleanup_fleet(fleet: Arc<Fleet>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = fleet.namespace().unwrap_or_default();
    let name = fleet.name_any();
    let key = Context::key(&namespace, &name);
    let selector = ListParams::default().labels(&format!("balerix.ai/fleet={name}"));

    // 1. every Agent gone (their finalizers harvest, §8.5)
    let agents: Api<Agent> = Api::namespaced(ctx.client.clone(), &namespace);
    let remaining = agents.list(&selector).await?.items;
    if !remaining.is_empty() {
        for agent in &remaining {
            if agent.metadata.deletion_timestamp.is_none() {
                agents.delete(&agent.name_any(), &DeleteParams::default()).await?;
            }
        }
        return Err(Error::Waiting(format!("Fleet {key}: {} agents still exist", remaining.len())));
    }

    // 2. down on the Daemon; a Daemon that is gone has nothing to down
    if let Some(daemon) = daemon_of(&ctx, &namespace, &fleet.spec.daemon).await? {
        match client_for(&ctx, &daemon).await {
            Ok(client) => client.down(&name).await?,
            Err(Error::Missing(why)) => tracing::warn!(fleet = %key, "not downed: {why}"),
            Err(e) => return Err(e),
        }
    }

    // 3. `retain: None`: one cleanup Job per crew (§5.2)
    if fleet.spec.retain == Retain::None {
        let crews: Api<Crew> = Api::namespaced(ctx.client.clone(), &namespace);
        let job_ctx = JobContext { namespace: &namespace, daemon: &fleet.spec.daemon, images: &ctx.cfg.images, owner: owner_of(fleet.as_ref())? };
        let mut pending = Vec::new();
        for crew in crews.list(&selector).await?.items {
            let ensured = ensure_job(ctx.as_ref(), fleet.as_ref(), remove_job(&job_ctx, &name, &crew.spec.crew)?, Some((&name, &crew.spec.crew))).await?;
            if !matches!(ensured.outcome, JobOutcome::Succeeded(_)) {
                pending.push(crew.spec.crew.clone());
            }
        }
        if !pending.is_empty() {
            return Err(Error::Waiting(format!("Fleet {key}: cleanup of crews {} not finished", pending.join(", "))));
        }
    }

    ctx.records.write().unwrap_or_else(|e| e.into_inner()).remove(&key);
    ctx.fleet_agents.write().unwrap_or_else(|e| e.into_inner()).remove(&key);
    if let Some(list) = ctx.daemon_fleets.write().unwrap_or_else(|e| e.into_inner()).get_mut(&Context::key(&namespace, &fleet.spec.daemon)) {
        list.retain(|f| f != &name);
    }
    tracing::info!(fleet = %key, "removed");
    Ok(Action::await_change())
}

#[allow(dead_code)]
fn _types(_: BTreeMap<String, String>) {}
```

Remove the trailing `_types` helper and the `BTreeMap` import if clippy flags them (they exist only so the import list above is complete for a reader).

- [ ] **Step 4: Write the failing Fleet tests**

Append to `operator/tests/controllers_it.rs` (imports: `balerix_operator::api::{Agent, Crew, Fleet, FleetSpec}`, `support::stub_daemon::StubDaemon`):

```rust
fn fleet_spec(daemon: &str, crews: &[(&str, &[&str])], retain: &str) -> FleetSpec {
    let crews: serde_json::Map<String, serde_json::Value> = crews
        .iter()
        .map(|(crew, agents)| {
            let agents: serde_json::Map<String, serde_json::Value> =
                agents.iter().map(|a| (a.to_string(), serde_json::json!({}))).collect();
            (crew.to_string(), serde_json::json!({ "repo": "acme/api", "git": { "auth": "none" }, "agents": agents }))
        })
        .collect();
    serde_json::from_value(serde_json::json!({ "daemon": daemon, "retain": retain, "crews": crews })).unwrap()
}

/// A Daemon, a stub, an operator over a fresh namespace.
async fn world(label: &str) -> (&'static support::envtest::EnvTest, String, StubDaemon, TestClock, tokio::task::AbortHandle) {
    let env = envtest().await.expect("envtest");
    let ns = namespace(&env.client, label).await;
    Api::<Daemon>::namespaced(env.client.clone(), &ns)
        .create(&PostParams::default(), &Daemon::new("default", daemon_spec()))
        .await
        .unwrap();
    let stub = StubDaemon::start().await;
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, Some(stub.url()), &clock);
    (env, ns, stub, clock, operator)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fleet_becomes_crews_agents_and_tokens_and_the_put_carries_them() {
    if envtest().await.is_none() { return; }
    let (env, ns, stub, _clock, operator) = world("fleet").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets.create(&PostParams::default(), &Fleet::new("f", fleet_spec("default", &[("c", &["a", "b"])], "Branches"))).await.unwrap();

    let crews: Api<Crew> = Api::namespaced(client.clone(), &ns);
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    wait_for("the Crew", Duration::from_secs(30), || async { crews.get_opt("f-c").await.unwrap() }).await;
    let a = wait_for("Agent f-c-a", Duration::from_secs(10), || async { agents.get_opt("f-c-a").await.unwrap() }).await;
    agents.get("f-c-b").await.unwrap();
    assert_eq!(a.metadata.finalizers.as_deref(), Some(&["balerix.ai/harvest".to_string()][..]));
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);
    let token = secrets.get("f-c-a-token").await.unwrap();
    assert_eq!(token.data.as_ref().unwrap()["token"].0.len(), 64);
    assert_eq!(token.metadata.owner_references.as_ref().unwrap()[0].kind, "Fleet");
    Api::<Job>::namespaced(client.clone(), &ns).get("f-pool").await.unwrap();

    let puts = stub.puts();
    assert!(!puts.is_empty());
    let tokens = puts[0].agent_tokens.as_ref().unwrap();
    assert_eq!(tokens.len(), 2);
    assert_eq!(String::from_utf8(token.data.as_ref().unwrap()["token"].0.clone()).unwrap(), tokens["f/c/a"]);
    assert_eq!(puts[0].spec.crews["c"].agents.len(), 2);

    let status = wait_for("Fleet status", Duration::from_secs(10), || async {
        fleets.get("f").await.unwrap().status.filter(|s| s.conditions.len() == 3)
    }).await;
    assert_eq!(condition(&status.conditions, "Resolved").status, "True");
    assert_eq!(condition(&status.conditions, "Accepted").status, "True");
    let ready = condition(&status.conditions, "Ready");
    assert_eq!((ready.status.as_str(), ready.reason.as_str(), ready.message.as_str()), ("False", "AgentsNotReady", "0 of 2 agents ready"));
    assert_eq!(fleets.get("f").await.unwrap().metadata.finalizers.as_deref(), Some(&["balerix.ai/fleet".to_string()][..]));
    // the token is kept across reconciles
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(secrets.get("f-c-a-token").await.unwrap().data, token.data);
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_fleet_lands_no_child() {
    if envtest().await.is_none() { return; }
    let (env, ns, stub, _clock, operator) = world("rejected").await;
    stub.reject(Some("crews.c.agents.a.model: no such model"));
    let fleets: Api<Fleet> = Api::namespaced(env.client.clone(), &ns);
    fleets.create(&PostParams::default(), &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches"))).await.unwrap();
    let status = wait_for("Accepted=False", Duration::from_secs(30), || async {
        let s = fleets.get("f").await.unwrap().status?;
        (s.conditions.len() == 3 && condition(&s.conditions, "Accepted").status == "False").then_some(s)
    }).await;
    let accepted = condition(&status.conditions, "Accepted");
    assert_eq!((accepted.reason.as_str(), accepted.message.as_str()), ("Rejected", "crews.c.agents.a.model: no such model"));
    assert_eq!(condition(&status.conditions, "Ready").reason, "Rejected");
    let crews: Api<Crew> = Api::namespaced(env.client.clone(), &ns);
    hold_for("a Crew of a rejected Fleet", Duration::from_secs(3), || async { crews.get_opt("f-c").await.unwrap() }).await;
    assert!(Api::<Job>::namespaced(env.client.clone(), &ns).get_opt("f-pool").await.unwrap().is_none());
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_does_not_answer_leaves_the_children() {
    if envtest().await.is_none() { return; }
    let (env, ns, stub, _clock, operator) = world("unavailable").await;
    let fleets: Api<Fleet> = Api::namespaced(env.client.clone(), &ns);
    fleets.create(&PostParams::default(), &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches"))).await.unwrap();
    let crews: Api<Crew> = Api::namespaced(env.client.clone(), &ns);
    wait_for("the Crew", Duration::from_secs(30), || async { crews.get_opt("f-c").await.unwrap() }).await;
    stub.stop();
    let status = wait_for("DaemonUnavailable", Duration::from_secs(30), || async {
        let s = fleets.get("f").await.unwrap().status?;
        (condition(&s.conditions, "Ready").reason == "DaemonUnavailable").then_some(s)
    }).await;
    assert_eq!(condition(&status.conditions, "Accepted").status, "Unknown");
    crews.get("f-c").await.unwrap();
    Api::<Agent>::namespaced(env.client.clone(), &ns).get("f-c-a").await.unwrap();
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fleet_naming_no_daemon_is_not_resolved() {
    if envtest().await.is_none() { return; }
    let (env, ns, stub, _clock, operator) = world("ghost").await;
    let fleets: Api<Fleet> = Api::namespaced(env.client.clone(), &ns);
    fleets.create(&PostParams::default(), &Fleet::new("f", fleet_spec("ghost", &[("c", &["a"])], "Branches"))).await.unwrap();
    let status = wait_for("Resolved=False", Duration::from_secs(30), || async {
        let s = fleets.get("f").await.unwrap().status?;
        (!s.conditions.is_empty()).then_some(s)
    }).await;
    let resolved = condition(&status.conditions, "Resolved");
    assert_eq!((resolved.status.as_str(), resolved.message.as_str()), ("False", "spec.daemon: Daemon ghost does not exist"));
    assert!(stub.puts().is_empty());
    assert!(Api::<Crew>::namespaced(env.client.clone(), &ns).get_opt("f-c").await.unwrap().is_none());
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_namespaces_ignores_another_namespace() {
    if envtest().await.is_none() { return; }
    let (env, _ns, _stub, _clock, operator) = world("watched").await;
    // the operator watches `_ns`; this Daemon is elsewhere
    let other = namespace(&env.client, "unwatched").await;
    Api::<Daemon>::namespaced(env.client.clone(), &other)
        .create(&PostParams::default(), &Daemon::new("default", daemon_spec()))
        .await
        .unwrap();
    let sts: Api<StatefulSet> = Api::namespaced(env.client.clone(), &other);
    hold_for("a StatefulSet in an unwatched namespace", Duration::from_secs(4), || async { sts.get_opt("balerix-default").await.unwrap() }).await;
    operator.abort();
}
```

- [ ] **Step 5: Run them**

Run: `mise run operator`
Expected: the five new tests pass with Step 2's `set` line in; Tasks 4 and 5's tests still pass. The Agents of these tests keep their `balerix.ai/harvest` finalizer with no Agent controller yet; nothing deletes them here.

- [ ] **Step 6: Commit**

```bash
git add operator/src/controllers operator/tests
git commit -m "feat(operator): the Fleet controller and a scripted stub Daemon for the tests (Spec O §5.2, §21.2)

Resolve, mint the missing agent tokens into Secrets the Fleet owns, PUT
the Daemon, then the Crews and Agents; a 400 lands nothing and a Daemon
that does not answer leaves the children. The record read back is the
cache the Agents mirror. Deletion waits for the Agents, downs the fleet
and, for retain: None, runs the cleanup Jobs."
```

---

### Task 7: The Crew controller

**Files:**
- Create: `operator/src/controllers/crew.rs`
- Modify: `operator/src/controllers/mod.rs` (the `set` line), `operator/tests/controllers_it.rs`

**Interfaces:**
- Consumes: `desired::jobs::{JobContext, crew_sync_job, fleet_pool_job, job_outcome, crew_status}`, `desired::common::{JobOutcome, owner_of}`, `desired::names`, `controllers::jobs::ensure_job`, `controllers::fleet::daemon_of`.
- Produces: `controllers::crew::controller(ctx, namespace)`, `controllers::crew::reconcile(crew: Arc<Crew>, ctx) -> Result<Action, Error>`; `controllers::crew::crew_ready(crew: &Crew) -> Result<(), String>` (`Ok` when `CacheReady` and `ToolsReady` are both true, else the first failing condition's message; the Agent controller uses it).

- [ ] **Step 1: Write the Crew controller**

Create `operator/src/controllers/crew.rs`:

```rust
//! The Crew controller (Spec O §5.3, §21.2): the sync Job under the Job
//! rule, with the fleet pool Job's outcome read by name, into
//! `CacheReady` and `ToolsReady`.

use std::sync::Arc;

use futures_util::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::api::ListParams;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, ResourceExt};

use super::fleet::daemon_of;
use super::jobs::ensure_job;
use super::{Context, Error, api_in, error_policy, patch_status, reconciled, report};
use crate::api::{Crew, Fleet};
use crate::desired::common::{JobOutcome, owner_of};
use crate::desired::jobs::{JobContext, crew_status, crew_sync_job, fleet_pool_job, job_outcome};
use crate::desired::names;

pub async fn controller(ctx: Arc<Context>, namespace: Option<&str>) {
    let client = &ctx.client;
    Controller::new(api_in::<Crew>(client, namespace), watcher::Config::default())
        .owns(api_in::<Job>(client, namespace), watcher::Config::default())
        .run(reconcile, error_policy, ctx.clone())
        .for_each(|r| async move { report("crew", r) })
        .await;
}

/// `Ok` when the crew's cache and tools are ready; else why not.
pub fn crew_ready(crew: &Crew) -> Result<(), String> {
    let conditions = crew.status.as_ref().map(|s| s.conditions.as_slice()).unwrap_or(&[]);
    for type_ in ["CacheReady", "ToolsReady"] {
        match conditions.iter().find(|c| c.type_ == type_) {
            Some(c) if c.status == "True" => {}
            Some(c) => return Err(format!("{type_}: {}", if c.message.is_empty() { c.reason.clone() } else { c.message.clone() })),
            None => return Err(format!("{type_}: not reported yet")),
        }
    }
    Ok(())
}

/// What the fleet's pool Job came to, read by name: it is the Fleet's.
async fn fleet_pool_outcome(ctx: &Context, namespace: &str, crew: &Crew) -> Result<JobOutcome, Error> {
    let name = names::fleet_pool_job(&crew.spec.fleet);
    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), namespace);
    let Some(existing) = jobs.get_opt(&name).await? else {
        return Ok(JobOutcome::Absent);
    };
    let job_pods = pods.list(&ListParams::default().labels(&format!("job-name={name}"))).await?.items;
    // `job_outcome` compares the input hash with a wanted Job; the fleet
    // pool's wanted Job is the Fleet's to make, so the existing one is it
    Ok(job_outcome(Some(&existing), &job_pods, &existing))
}

pub async fn reconcile(crew: Arc<Crew>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = crew.namespace().unwrap_or_default();
    let name = crew.name_any();
    let Some(daemon) = daemon_of(&ctx, &namespace, &crew.spec.daemon).await? else {
        return Err(Error::Missing(format!("Crew {namespace}/{name}: Daemon {} does not exist", crew.spec.daemon)));
    };
    let gh_secret = daemon.spec.credentials.github.as_ref().map(|r| r.secret_name.clone());
    let job_ctx = JobContext { namespace: &namespace, daemon: &crew.spec.daemon, images: &ctx.cfg.images, owner: owner_of(crew.as_ref())? };
    let wanted = crew_sync_job(&job_ctx, &crew.spec, gh_secret.as_deref())?;
    let sync = ensure_job(ctx.as_ref(), crew.as_ref(), wanted, Some((&crew.spec.fleet, &crew.spec.crew))).await?;
    let fleet_pool = fleet_pool_outcome(&ctx, &namespace, &crew).await?;
    let status = crew_status(&crew, &fleet_pool, &sync.outcome, &ctx.k8s_now());
    patch_status(&ctx.client, crew.as_ref(), &status).await?;
    reconciled(&ctx, crew.as_ref());
    Ok(Action::requeue(sync.again.min(ctx.run.period)))
}

#[allow(dead_code)]
fn _unused(_: &Fleet, _: fn(&JobContext<'_>, &str, &std::collections::BTreeMap<String, String>) -> Result<Job, crate::desired::common::DesiredError>) {
    let _ = fleet_pool_job;
}
```

Remove `_unused` and the `Fleet`/`fleet_pool_job` imports if clippy flags them. In `mod.rs`'s `set`, replace the `// Task 7:` comment with `Box::pin(crew::controller(ctx.clone(), ns)),`.

- [ ] **Step 2: Write the failing Crew tests**

Append to `operator/tests/controllers_it.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_sync_is_reported_and_retried_as_attempt_two() {
    if envtest().await.is_none() { return; }
    let (env, ns, _stub, clock, operator) = world("sync").await;
    let client = env.client.clone();
    Api::<Fleet>::namespaced(client.clone(), &ns)
        .create(&PostParams::default(), &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches")))
        .await
        .unwrap();
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let crews: Api<Crew> = Api::namespaced(client.clone(), &ns);
    let first = wait_for("the sync Job", Duration::from_secs(30), || async { jobs.get_opt("f-c-sync").await.unwrap() }).await;
    let args = first.spec.as_ref().unwrap().template.spec.as_ref().unwrap().containers[0].args.clone().unwrap();
    assert_eq!(&args[..3], &["crew-sync".to_string(), "--crew".to_string(), "f/c".to_string()]);
    // the fleet pool succeeded; the sync failed on the cache
    finish_job(&client, &ns, "f-pool", true, Some("synced")).await;
    finish_job(&client, &ns, "f-c-sync", false, Some("cache: f/c: the remote has no branch nope")).await;
    let status = wait_for("CacheReady=False", Duration::from_secs(10), || async {
        let s = crews.get("f-c").await.unwrap().status?;
        (condition(&s.conditions, "CacheReady").status == "False").then_some(s)
    }).await;
    let cache = condition(&status.conditions, "CacheReady");
    assert_eq!((cache.reason.as_str(), cache.message.as_str()), ("SyncFailed", "cache: f/c: the remote has no branch nope"));

    clock.advance(31);
    let second = wait_for("the retry", Duration::from_secs(30), || async {
        let j = jobs.get_opt("f-c-sync").await.unwrap()?;
        (j.metadata.uid != first.metadata.uid).then_some(j)
    }).await;
    assert_eq!(crews.get("f-c").await.unwrap().metadata.annotations.unwrap()["balerix.ai/attempts"], "{\"f-c-sync\":2}");
    // the second run succeeds: both conditions true, cacheRef the commit
    let _ = second;
    finish_job(&client, &ns, "f-c-sync", true, Some("0123abcd")).await;
    let status = wait_for("CacheReady=True", Duration::from_secs(10), || async {
        let s = crews.get("f-c").await.unwrap().status?;
        (condition(&s.conditions, "CacheReady").status == "True" && condition(&s.conditions, "ToolsReady").status == "True").then_some(s)
    }).await;
    assert_eq!(status.cache_ref.as_deref(), Some("0123abcd"));
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn two_fleets_sharing_a_crew_name_sync_at_once() {
    if envtest().await.is_none() { return; }
    let (env, ns, _stub, _clock, operator) = world("twocrews").await;
    let fleets: Api<Fleet> = Api::namespaced(env.client.clone(), &ns);
    fleets.create(&PostParams::default(), &Fleet::new("payments", fleet_spec("default", &[("backend", &["a"])], "Branches"))).await.unwrap();
    fleets.create(&PostParams::default(), &Fleet::new("billing", fleet_spec("default", &[("backend", &["a"])], "Branches"))).await.unwrap();
    let jobs: Api<Job> = Api::namespaced(env.client.clone(), &ns);
    wait_for("both sync Jobs", Duration::from_secs(30), || async {
        let p = jobs.get_opt("payments-backend-sync").await.unwrap()?;
        let b = jobs.get_opt("billing-backend-sync").await.unwrap()?;
        Some((p, b))
    }).await;
    operator.abort();
}
```

The sync Job's pod labels carry `balerix.ai/fleet` and `balerix.ai/crew`; `finish_job`'s stand-in pod carries only `job-name`, which is what `job_outcome` reads.

- [ ] **Step 3: Run them**

Run: `mise run operator`
Expected: PASS, including the earlier tests.

- [ ] **Step 4: Commit**

```bash
git add operator/src/controllers operator/tests
git commit -m "feat(operator): the Crew controller (Spec O §5.3, §21.2)

The sync Job under the Job rule, with the fleet pool's outcome read by
name; a failed sync is CacheReady=False with the message and is retried
after the delay; the lock is per crew of one fleet."
```

---

### Task 8: The Agent controller

**Files:**
- Create: `operator/src/controllers/agent.rs`
- Modify: `operator/src/controllers/mod.rs` (the `set` line), `operator/tests/support/mod.rs` (`reap_pod`), `operator/tests/controllers_it.rs`

**Interfaces:**
- Consumes: `desired::agent::{agent_objects, agent_status, AgentInputs, AgentObjects, SPEC_HASH_ANNOTATION}`, `desired::jobs::{JobContext, harvest_job}`, `desired::fleet::HARVEST_FINALIZER`, `desired::common::{Cond, JobOutcome, conditions, owner_of}`, `desired::names`, `controllers::crew::crew_ready`, `controllers::fleet::daemon_of`, `controllers::daemon::read_secret_string`, `controllers::jobs::ensure_job`, `balerix_api::CredentialBundle`, `Context::{records, fleet_agents}`.
- Produces: `controllers::agent::controller(ctx, namespace)`, `controllers::agent::reconcile(agent: Arc<Agent>, ctx) -> Result<Action, Error>`; `controllers::agent::PURGE_ANNOTATION = "balerix.ai/purge"`; test support `support::reap_pod(client, ns, name)` (waits for the deletion timestamp, then deletes with grace period 0: the kubelet's part).

- [ ] **Step 1: Write the Agent controller**

Create `operator/src/controllers/agent.rs`:

```rust
//! The Agent controller (Spec O §5.4, §8.5, §21.2): once its Crew is
//! ready, the claim, the bundle Secret, the policy and the Pod; a Pod
//! whose `specHash` differs is replaced on the same claim; status is the
//! Pod plus the Daemon's record for the agent. On deletion: the Pod
//! gone, the harvest Job (unless purged, or the Fleet goes with
//! `retain: None`, or there is no claim), then the claim.

use std::sync::Arc;

use balerix_api::CredentialBundle;
use futures_util::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{DeleteParams, PostParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{Event, finalizer};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;
use kube::{Api, ResourceExt};

use super::crew::crew_ready;
use super::daemon::read_secret_string;
use super::fleet::daemon_of;
use super::jobs::ensure_job;
use super::{Context, Error, api_in, apply, error_policy, patch_status, reconciled, report};
use crate::api::{Agent, AgentStatus, Crew, Daemon, Fleet, Retain};
use crate::desired::agent::{AgentInputs, SPEC_HASH_ANNOTATION, agent_objects, agent_status};
use crate::desired::common::{Cond, JobOutcome, conditions, owner_of};
use crate::desired::fleet::HARVEST_FINALIZER;
use crate::desired::jobs::{JobContext, harvest_job};
use crate::desired::names;

/// On an Agent or its Fleet: skip the harvest, as `--purge` does (§8.5).
pub const PURGE_ANNOTATION: &str = "balerix.ai/purge";

pub async fn controller(ctx: Arc<Context>, namespace: Option<&str>) {
    let client = &ctx.client;
    let mapper_ctx = ctx.clone();
    Controller::new(api_in::<Agent>(client, namespace), watcher::Config::default())
        .owns(api_in::<Pod>(client, namespace), watcher::Config::default())
        .owns(api_in::<PersistentVolumeClaim>(client, namespace), watcher::Config::default())
        .owns(api_in::<Secret>(client, namespace), watcher::Config::default())
        .owns(api_in::<NetworkPolicy>(client, namespace), watcher::Config::default())
        .owns(api_in::<Job>(client, namespace), watcher::Config::default())
        .watches(api_in::<Fleet>(client, namespace), watcher::Config::default(), move |fleet: Fleet| {
            let ns = fleet.namespace().unwrap_or_default();
            let key = Context::key(&ns, &fleet.name_any());
            let agents = mapper_ctx.fleet_agents.read().unwrap_or_else(|e| e.into_inner());
            agents
                .get(&key)
                .into_iter()
                .flatten()
                .map(|a| ObjectRef::<Agent>::new(a).within(&ns))
                .collect::<Vec<_>>()
        })
        .run(reconcile, error_policy, ctx.clone())
        .for_each(|r| async move { report("agent", r) })
        .await;
}

pub async fn reconcile(agent: Arc<Agent>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = agent.namespace().unwrap_or_default();
    let agents: Api<Agent> = Api::namespaced(ctx.client.clone(), &namespace);
    let ctx2 = ctx.clone();
    finalizer(&agents, HARVEST_FINALIZER, agent, |event| async move {
        match event {
            Event::Apply(agent) => apply_agent(agent, ctx2).await,
            Event::Cleanup(agent) => cleanup_agent(agent, ctx2).await,
        }
    })
    .await
    .map_err(Error::from)
}

/// The Daemon's last word on this agent, from the Fleet's cached record.
fn reported(ctx: &Context, namespace: &str, agent: &Agent) -> Option<balerix_api::AgentStatus> {
    let key = Context::key(namespace, &agent.spec.fleet);
    let records = ctx.records.read().unwrap_or_else(|e| e.into_inner());
    records
        .get(&key)?
        .status
        .agents
        .get(&format!("{}/{}/{}", agent.spec.fleet, agent.spec.crew, agent.spec.agent))
        .cloned()
}

/// The credentials the Daemon names (§5.2 step 2), from their Secrets.
async fn credentials(ctx: &Context, namespace: &str, daemon: &Daemon) -> Result<CredentialBundle, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    let mut bundle = CredentialBundle::default();
    if let Some(r) = &daemon.spec.credentials.claude {
        let secret = secrets.get_opt(&r.secret_name).await?.ok_or_else(|| Error::Missing(format!("Secret {} (spec.credentials.claude) does not exist", r.secret_name)))?;
        let text = read_secret_string(&secret, "credentials.json").ok_or_else(|| Error::Missing(format!("Secret {} has no credentials.json", r.secret_name)))?;
        bundle.claude_credentials = Some(serde_json::from_str(&text).map_err(crate::desired::common::DesiredError::from)?);
    }
    if let Some(r) = &daemon.spec.credentials.github {
        let secret = secrets.get_opt(&r.secret_name).await?.ok_or_else(|| Error::Missing(format!("Secret {} (spec.credentials.github) does not exist", r.secret_name)))?;
        bundle.gh_token = Some(read_secret_string(&secret, "token").ok_or_else(|| Error::Missing(format!("Secret {} has no token", r.secret_name)))?.trim_end().to_string());
    }
    Ok(bundle)
}

/// `agent_status` with one condition replaced (its transition time kept
/// when the status is unchanged, as `conditions` does).
fn with_condition(ctx: &Context, agent: &Agent, pod: Option<&Pod>, reported: Option<&balerix_api::AgentStatus>, replace: Cond) -> AgentStatus {
    let mut status = agent_status(agent, pod, reported, &ctx.k8s_now());
    let old = agent.status.as_ref().map(|s| s.conditions.as_slice()).unwrap_or(&[]);
    for new in conditions(old, &[replace], agent.metadata.generation, &ctx.k8s_now()) {
        match status.conditions.iter_mut().find(|c| c.type_ == new.type_) {
            Some(c) => *c = new,
            None => status.conditions.push(new),
        }
    }
    status
}

async fn apply_agent(agent: Arc<Agent>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = agent.namespace().unwrap_or_default();
    let name = agent.name_any();
    let period = ctx.run.fleet_period;
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let reported = reported(&ctx, &namespace, &agent);

    let Some(daemon) = daemon_of(&ctx, &namespace, &agent.spec.daemon).await? else {
        return Err(Error::Missing(format!("Agent {namespace}/{name}: Daemon {} does not exist", agent.spec.daemon)));
    };
    let crews: Api<Crew> = Api::namespaced(ctx.client.clone(), &namespace);
    let crew = crews.get_opt(&names::crew(&agent.spec.fleet, &agent.spec.crew)).await?;
    let waiting = match crew.as_ref().map(crew_ready) {
        Some(Ok(())) => None,
        Some(Err(why)) => Some(why),
        None => Some("the Crew does not exist yet".to_string()),
    };
    if let Some(why) = waiting {
        let pod = pods.get_opt(&name).await?;
        let status = with_condition(&ctx, &agent, pod.as_ref(), reported.as_ref(), Cond::unknown("Materialized", "WaitingForCrew", &why));
        patch_status(&ctx.client, agent.as_ref(), &status).await?;
        reconciled(&ctx, agent.as_ref());
        return Ok(Action::requeue(period));
    }

    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &namespace);
    let token = secrets
        .get_opt(&names::token(&name))
        .await?
        .as_ref()
        .and_then(|s| read_secret_string(s, "token"))
        .ok_or_else(|| Error::Missing(format!("Agent {namespace}/{name}: its token Secret does not exist yet")))?;
    let credentials = credentials(&ctx, &namespace, &daemon).await?;
    let objects = agent_objects(&AgentInputs { agent: &agent, daemon: &daemon, token: &token, credentials: &credentials, cfg: &ctx.cfg })?;

    let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &namespace);
    if claims.get_opt(&name).await?.is_none() {
        claims.create(&PostParams::default(), &objects.claim).await?;
    }
    apply(&ctx.client, &objects.bundle).await?;
    apply(&ctx.client, &objects.policy).await?;

    // the Pod: created when absent, replaced when its specHash differs (§5.4)
    let mut again = period;
    let pod = match pods.get_opt(&name).await? {
        None => match pods.create(&PostParams::default(), &objects.pod).await {
            Ok(pod) => Some(pod),
            Err(kube::Error::Api(e)) if e.code == 409 => pods.get_opt(&name).await?,
            Err(e) => return Err(e.into()),
        },
        Some(pod) if pod.metadata.deletion_timestamp.is_some() => {
            again = std::time::Duration::from_secs(2);
            Some(pod)
        }
        Some(pod) if pod.annotations().get(SPEC_HASH_ANNOTATION) != Some(&agent.spec.spec_hash) => {
            tracing::info!(agent = %name, "specHash changed: replacing the pod");
            pods.delete(&name, &DeleteParams::default()).await?;
            again = std::time::Duration::from_secs(2);
            Some(pod)
        }
        Some(pod) => Some(pod),
    };

    let status = agent_status(&agent, pod.as_ref(), reported.as_ref(), &ctx.k8s_now());
    patch_status(&ctx.client, agent.as_ref(), &status).await?;
    reconciled(&ctx, agent.as_ref());
    Ok(Action::requeue(again))
}

fn purged(agent: &Agent, fleet: Option<&Fleet>) -> bool {
    let marked = |a: &std::collections::BTreeMap<String, String>| a.get(PURGE_ANNOTATION).is_some_and(|v| v == "true");
    marked(agent.annotations()) || fleet.is_some_and(|f| marked(f.annotations()))
}

async fn cleanup_agent(agent: Arc<Agent>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = agent.namespace().unwrap_or_default();
    let name = agent.name_any();
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &namespace);

    // 1. the Pod gone
    if let Some(pod) = pods.get_opt(&name).await? {
        if pod.metadata.deletion_timestamp.is_none() {
            pods.delete(&name, &DeleteParams::default()).await?;
        }
        return Err(Error::Waiting(format!("Agent {namespace}/{name}: its pod still exists")));
    }

    // 2. nothing to harvest without a claim
    if claims.get_opt(&name).await?.is_none() {
        tracing::info!(agent = %name, "removed; it never had a claim");
        return Ok(Action::await_change());
    }

    // 3. the harvest, unless skipped (§8.5)
    let fleets: Api<Fleet> = Api::namespaced(ctx.client.clone(), &namespace);
    let fleet = fleets.get_opt(&agent.spec.fleet).await?;
    let fleet_going_whole = fleet.as_ref().is_some_and(|f| f.metadata.deletion_timestamp.is_some() && f.spec.retain == Retain::None);
    if !purged(&agent, fleet.as_ref()) && !fleet_going_whole {
        let job_ctx = JobContext { namespace: &namespace, daemon: &agent.spec.daemon, images: &ctx.cfg.images, owner: owner_of(agent.as_ref())? };
        let ensured = ensure_job(ctx.as_ref(), agent.as_ref(), harvest_job(&job_ctx, &name, &agent.spec)?, Some((&agent.spec.fleet, &agent.spec.crew))).await?;
        match ensured.outcome {
            JobOutcome::Succeeded(message) => tracing::info!(agent = %name, "{message}"),
            JobOutcome::Failed(message) => {
                let reported = reported(&ctx, &namespace, &agent);
                let status = with_condition(&ctx, &agent, None, reported.as_ref(), Cond::no("Ready", "HarvestFailed", &message));
                patch_status(&ctx.client, agent.as_ref(), &status).await?;
                return Err(Error::Waiting(format!("Agent {namespace}/{name}: harvest failed: {message}")));
            }
            _ => return Err(Error::Waiting(format!("Agent {namespace}/{name}: harvest not finished"))),
        }
    }

    // 4. the claim
    claims.delete(&name, &DeleteParams::default()).await?;
    tracing::info!(agent = %name, "removed");
    Ok(Action::await_change())
}
```

In `mod.rs`'s `set`, replace the `// Task 8:` comment with `Box::pin(agent::controller(ctx.clone(), ns)),`.

- [ ] **Step 2: Add `reap_pod` to the test support**

In `operator/tests/support/mod.rs`:

```rust
/// The kubelet's part for a deleted Pod in envtest: once the operator has
/// set its deletion timestamp, remove it with grace period 0. Returns
/// when the Pod is gone. Panics after `timeout` if it was never deleted.
pub async fn reap_pod(client: &Client, namespace: &str, name: &str, timeout: Duration) {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::DeleteParams;
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    wait_for(&format!("pod {name} to be deleted"), timeout, || async {
        match pods.get_opt(name).await.unwrap() {
            None => Some(()),
            Some(p) if p.metadata.deletion_timestamp.is_some() => {
                let _ = pods.delete(name, &DeleteParams::default().grace_period(0)).await;
                None
            }
            Some(_) => None,
        }
    })
    .await;
}
```

- [ ] **Step 3: Write the failing Agent tests**

Append to `operator/tests/controllers_it.rs` (imports: `k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod}`, `support::reap_pod`, `balerix_api::{AgentPhase, AgentStatus as DaemonAgentStatus}`):

```rust
/// Both Jobs of crew `c` succeeded: its Agents may have pods.
async fn crew_ready(client: &kube::Client, ns: &str, fleet: &str, crew: &str) {
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);
    let pool = format!("{fleet}-pool");
    let sync = format!("{fleet}-{crew}-sync");
    wait_for("the Jobs", Duration::from_secs(30), || async {
        jobs.get_opt(&pool).await.unwrap()?;
        jobs.get_opt(&sync).await.unwrap()
    }).await;
    finish_job(client, ns, &pool, true, Some("synced")).await;
    finish_job(client, ns, &sync, true, Some("0123abcd")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crew_dropped_from_the_fleet_is_removed_and_its_agent_harvested() {
    if envtest().await.is_none() { return; }
    let (env, ns, _stub, _clock, operator) = world("dropped").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets.create(&PostParams::default(), &Fleet::new("f", fleet_spec("default", &[("c", &["a"]), ("d", &["a"])], "Branches"))).await.unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    crew_ready(&client, &ns, "f", "d").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    wait_for("the pods", Duration::from_secs(30), || async {
        pods.get_opt("f-c-a").await.unwrap()?;
        pods.get_opt("f-d-a").await.unwrap()
    }).await;
    claims.get("f-d-a").await.unwrap();
    Api::<Secret>::namespaced(client.clone(), &ns).get("f-d-a-bundle").await.unwrap();

    // crew d leaves the Fleet
    fleets.patch("f", &PatchParams::default(), &Patch::Merge(serde_json::json!({ "spec": { "crews": { "d": null } } }))).await.unwrap();
    reap_pod(&client, &ns, "f-d-a", Duration::from_secs(30)).await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    wait_for("the harvest Job", Duration::from_secs(30), || async { jobs.get_opt("f-d-a-harvest").await.unwrap() }).await;
    finish_job(&client, &ns, "f-d-a-harvest", true, Some("harvested balerix/a")).await;
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    wait_for("Agent f-d-a gone", Duration::from_secs(30), || async { agents.get_opt("f-d-a").await.unwrap().is_none().then_some(()) }).await;
    wait_for("claim f-d-a gone", Duration::from_secs(10), || async { claims.get_opt("f-d-a").await.unwrap().is_none().then_some(()) }).await;
    wait_for("Crew f-d gone", Duration::from_secs(10), || async { Api::<Crew>::namespaced(client.clone(), &ns).get_opt("f-d").await.unwrap().is_none().then_some(()) }).await;
    agents.get("f-c-a").await.unwrap();
    pods.get("f-c-a").await.unwrap();
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_with_no_claim_is_removed_without_a_harvest() {
    if envtest().await.is_none() { return; }
    let (env, ns, _stub, _clock, operator) = world("noclaim").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets.create(&PostParams::default(), &Fleet::new("f", fleet_spec("default", &[("c", &["a"]), ("d", &["a"])], "Branches"))).await.unwrap();
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    let a = wait_for("Agent f-d-a", Duration::from_secs(30), || async { agents.get_opt("f-d-a").await.unwrap() }).await;
    let status = wait_for("WaitingForCrew", Duration::from_secs(10), || async {
        let s = agents.get("f-d-a").await.unwrap().status?;
        (condition(&s.conditions, "Materialized").reason == "WaitingForCrew").then_some(s)
    }).await;
    assert_eq!(condition(&status.conditions, "Materialized").status, "Unknown");
    let _ = a;
    // dropped before its crew ever synced: no claim, no harvest
    fleets.patch("f", &PatchParams::default(), &Patch::Merge(serde_json::json!({ "spec": { "crews": { "d": null } } }))).await.unwrap();
    wait_for("Agent f-d-a gone", Duration::from_secs(30), || async { agents.get_opt("f-d-a").await.unwrap().is_none().then_some(()) }).await;
    assert!(Api::<Job>::namespaced(client.clone(), &ns).get_opt("f-d-a-harvest").await.unwrap().is_none());
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_spec_hash_replaces_the_pod_and_keeps_the_claim() {
    if envtest().await.is_none() { return; }
    let (env, ns, _stub, _clock, operator) = world("spechash").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets.create(&PostParams::default(), &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches"))).await.unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let first = wait_for("the pod", Duration::from_secs(30), || async { pods.get_opt("f-c-a").await.unwrap() }).await;
    let claim = Api::<PersistentVolumeClaim>::namespaced(client.clone(), &ns).get("f-c-a").await.unwrap();
    let old_hash = first.metadata.annotations.as_ref().unwrap()["balerix.ai/spec-hash"].clone();

    fleets.patch("f", &PatchParams::default(), &Patch::Merge(serde_json::json!({
        "spec": { "crews": { "c": { "agents": { "a": { "claude": { "settings": { "model": "opus" } } } } } } }
    }))).await.unwrap();
    reap_pod(&client, &ns, "f-c-a", Duration::from_secs(30)).await;
    let second = wait_for("the new pod", Duration::from_secs(30), || async {
        let p = pods.get_opt("f-c-a").await.unwrap()?;
        (p.metadata.uid != first.metadata.uid).then_some(p)
    }).await;
    let new_hash = &second.metadata.annotations.as_ref().unwrap()["balerix.ai/spec-hash"];
    assert_ne!(*new_hash, old_hash);
    assert_eq!(Api::<Agent>::namespaced(client.clone(), &ns).get("f-c-a").await.unwrap().spec.spec_hash, *new_hash);
    assert_eq!(Api::<PersistentVolumeClaim>::namespaced(client.clone(), &ns).get("f-c-a").await.unwrap().metadata.uid, claim.metadata.uid);
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_crew_lock_holds_the_harvest_while_a_sync_runs() {
    if envtest().await.is_none() { return; }
    let (env, ns, _stub, _clock, operator) = world("lock").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets.create(&PostParams::default(), &Fleet::new("f", fleet_spec("default", &[("c", &["a", "b"])], "Branches"))).await.unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    wait_for("the pods", Duration::from_secs(30), || async { pods.get_opt("f-c-a").await.unwrap()?; pods.get_opt("f-c-b").await.unwrap() }).await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let synced = jobs.get("f-c-sync").await.unwrap();

    // a changed crew table makes the sync Job stale: a new one runs, unfinished
    fleets.patch("f", &PatchParams::default(), &Patch::Merge(serde_json::json!({ "spec": { "crews": { "c": { "tools": { "node": "22.11.0" } } } } }))).await.unwrap();
    wait_for("a new sync Job", Duration::from_secs(30), || async {
        let j = jobs.get_opt("f-c-sync").await.unwrap()?;
        (j.metadata.uid != synced.metadata.uid).then_some(j)
    }).await;
    // agent b leaves while it runs
    fleets.patch("f", &PatchParams::default(), &Patch::Merge(serde_json::json!({ "spec": { "crews": { "c": { "agents": { "b": null } } } } }))).await.unwrap();
    reap_pod(&client, &ns, "f-c-b", Duration::from_secs(30)).await;
    hold_for("a harvest while the sync runs", Duration::from_secs(4), || async { jobs.get_opt("f-c-b-harvest").await.unwrap() }).await;
    finish_job(&client, &ns, "f-c-sync", true, Some("4567abcd")).await;
    wait_for("the harvest after the sync", Duration::from_secs(30), || async { jobs.get_opt("f-c-b-harvest").await.unwrap() }).await;
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn fleet_deletion_with_retain_none_downs_the_daemon_and_runs_the_cleanup_job() {
    if envtest().await.is_none() { return; }
    let (env, ns, stub, _clock, operator) = world("retainnone").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets.create(&PostParams::default(), &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "None"))).await.unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    wait_for("the pod", Duration::from_secs(30), || async { pods.get_opt("f-c-a").await.unwrap() }).await;

    fleets.delete("f", &kube::api::DeleteParams::default()).await.unwrap();
    reap_pod(&client, &ns, "f-c-a", Duration::from_secs(30)).await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    wait_for("Agent gone without a harvest", Duration::from_secs(30), || async { agents.get_opt("f-c-a").await.unwrap().is_none().then_some(()) }).await;
    assert!(jobs.get_opt("f-c-a-harvest").await.unwrap().is_none());
    wait_for("the Daemon's DELETE", Duration::from_secs(30), || async { stub.deletes().contains(&"f".to_string()).then_some(()) }).await;
    wait_for("the cleanup Job", Duration::from_secs(30), || async { jobs.get_opt("f-c-remove").await.unwrap() }).await;
    assert!(fleets.get_opt("f").await.unwrap().is_some(), "the finalizer waits for the cleanup");
    finish_job(&client, &ns, "f-c-remove", true, Some("removed")).await;
    wait_for("the Fleet gone", Duration::from_secs(30), || async { fleets.get_opt("f").await.unwrap().is_none().then_some(()) }).await;
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_status_mirrors_the_daemons_record_and_readiness_counts() {
    if envtest().await.is_none() { return; }
    let (env, ns, stub, _clock, operator) = world("mirror").await;
    let client = env.client.clone();
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    fleets.create(&PostParams::default(), &Fleet::new("f", fleet_spec("default", &[("c", &["a"])], "Branches"))).await.unwrap();
    crew_ready(&client, &ns, "f", "c").await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    wait_for("the pod", Duration::from_secs(30), || async { pods.get_opt("f-c-a").await.unwrap() }).await;
    stub.set_agent("f", "f/c/a", DaemonAgentStatus { phase: AgentPhase::Ready, restarts: 2, ..Default::default() });
    // the kubelet: the sidecar is ready
    pods.patch_status("f-c-a", &PatchParams::default(), &Patch::Merge(serde_json::json!({
        "status": { "phase": "Running", "conditions": [{ "type": "PodScheduled", "status": "True" }],
            "initContainerStatuses": [{ "name": "sidecar", "image": "x", "imageID": "x", "ready": true, "restartCount": 0, "state": { "running": { "startedAt": "2026-10-03T00:00:00Z" } } }] }
    }))).await.unwrap();
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    let status = wait_for("phase ready", Duration::from_secs(30), || async {
        let s = agents.get("f-c-a").await.unwrap().status?;
        (s.phase.as_deref() == Some("ready")).then_some(s)
    }).await;
    assert_eq!(status.restarts, Some(2));
    assert_eq!(status.pod.as_deref(), Some("f-c-a"));
    assert_eq!(condition(&status.conditions, "Ready").status, "True");
    assert_eq!(condition(&status.conditions, "Materialized").status, "True");
    let fleet = wait_for("Fleet Ready", Duration::from_secs(30), || async {
        let s = fleets.get("f").await.unwrap().status?;
        (condition(&s.conditions, "Ready").status == "True").then_some(s)
    }).await;
    assert_eq!(condition(&fleet.conditions, "Ready").reason, "AgentsReady");
    operator.abort();
}
```

- [ ] **Step 4: Run them**

Run: `mise run operator`
Expected: PASS for the six new tests and every earlier one. The whole `controllers_it` binary should finish in under three minutes; if the envtest start is counted once, the tests run concurrently in their own namespaces.

- [ ] **Step 5: Commit**

```bash
git add operator/src/controllers operator/tests
git commit -m "feat(operator): the Agent controller (Spec O §5.4, §8.5, §21.2)

Once the Crew is ready: the claim, the bundle, the policy, the Pod; a
changed specHash replaces the Pod on the same claim; status mirrors the
Daemon's record. Deletion: the Pod gone, the harvest Job under the crew
lock unless purged, the Fleet goes with retain: None, or there is no
claim; then the claim."
```

---

### Task 9: The operator and agent Dockerfiles

**Files:**
- Create: `docker/operator/Dockerfile`, `docker/agent/Dockerfile`

**Interfaces:**
- Produces: `docker/operator/Dockerfile` (context: the static `balerix-operator` binary as `balerix-operator`); `docker/agent/Dockerfile` (build argument `BASE`, context: `balerix-agent`). `kind-up.sh` (Task 10) builds both with the contexts named here; the release pipeline wires them in sub-project 5.

- [ ] **Step 1: Write the operator image**

Create `docker/operator/Dockerfile`:

```dockerfile
# The operator image (Spec O §13): one static binary on distroless static,
# as the plugin images are. The context from the release scripts
# (sub-project 5) or scripts/kind-up.sh holds it as `balerix-operator`.
# distroless/static carries the CA bundle the Kubernetes client needs.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:1c2c046bc09ed40fad370b599a0b1ae7987f55b01e247cf27a7c27cd97e5bbc7
COPY --chmod=0755 balerix-operator /usr/local/bin/balerix-operator
ENTRYPOINT ["/usr/local/bin/balerix-operator"]
CMD ["run"]
```

The digest is the one `docker/plugin/Dockerfile` pins today; Renovate moves both.

- [ ] **Step 2: Write the agent image**

Create `docker/agent/Dockerfile`:

```dockerfile
# The agent image (Spec O §6.1, §13): the runtime image's contents (mise,
# git, gh, nono, tmux, `balerix`) plus `balerix-agent`. `BASE` is the
# runtime image of the same version; scripts/kind-up.sh passes a locally
# built one. The context holds the binary as `balerix-agent`.
ARG BASE=ghcr.io/balerix-ai/balerix:0.2.0
# hadolint ignore=DL3006
FROM ${BASE}
COPY --chmod=0755 balerix-agent /usr/local/bin/balerix-agent
# The pod names the command (`balerix-agent sidecar`, `balerix-agent run`,
# a Job's subcommand); tini reaps for all of them as it does for `serve`.
ENTRYPOINT ["/usr/bin/tini", "--", "balerix-agent"]
CMD ["sidecar"]
```

`USER 10001:10001`, `HOME` and `LANG=C.UTF-8` come from the base; the sidecar's tmux calls carry `-u` as well (§6.3). `0.2.0` is `operator/Cargo.toml`'s version today; sub-project 5's `prepare.sh` learns to move it with the other core manifests.

- [ ] **Step 3: Lint**

Run: `mise x -- hadolint docker/operator/Dockerfile docker/agent/Dockerfile`
Expected: no output, exit 0. (`mise install hadolint` first if it is not installed; it is in `[tools]`.) A `DL3007`/`DL3006` warning on the `ARG`-based `FROM` is what the ignore line covers; anything else is fixed in the file.

- [ ] **Step 4: Commit**

```bash
git add docker/operator/Dockerfile docker/agent/Dockerfile
git commit -m "feat(docker): the operator and agent images (Spec O §13, §21.4)

distroless static for the operator, as the plugin images; the agent image
is the runtime image plus balerix-agent, its base a build argument so a
locally built runtime image serves e2e-k8s. Release wiring is sub-project 5."
```

---

### Task 10: `kind-up.sh`, the `kind-up` and `e2e-k8s` tasks, and the CI job

**Files:**
- Create: `scripts/kind-up.sh`
- Modify: `mise.toml` (tasks `kind-up`, `e2e-k8s`), `scripts/operator.sh` (`e2e` mode), `.github/workflows/ci.yml` (job `e2e-k8s`)

**Interfaces:**
- Produces: `mise run kind-up` (a two-node `kind` cluster `balerix-e2e` with the shared local-path class, the five definitions, and the images `balerix:e2e` and `balerix-agent:e2e` built from this tree and loaded; kubeconfig at `target/tmp/kind/kubeconfig`); `mise run kind-up -- down`; `mise run e2e-k8s` (Task 11's test with `KUBECONFIG` and `BALERIX_K8S_IMAGES=balerix:e2e,balerix-agent:e2e` set, `BALERIX_REQUIRE_TOOLS=1`); the CI job `e2e-k8s`.

- [ ] **Step 1: Write `scripts/kind-up.sh`**

```bash
#!/usr/bin/env bash
# A kind cluster for e2e-k8s (Spec O §15, §19.3, §21.4): two workers that
# share one host directory, the local-path provisioner told to provision
# ReadWriteMany claims from it, the five definitions applied, and the
# daemon and agent images built from this tree and loaded. Nothing here is
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
[[ -n ${GITHUB_TOKEN:-} ]] && secret=(--secret id=github_token,env=GITHUB_TOKEN)
docker build "${secret[@]}" -t balerix:e2e -f docker/balerix/Dockerfile "$context"
rm -rf "$root/context-agent" && mkdir -p "$root/context-agent"
cp "$repo/agent/target/release/balerix-agent" "$root/context-agent/balerix-agent"
docker build --build-arg BASE=balerix:e2e -t balerix-agent:e2e -f docker/agent/Dockerfile "$root/context-agent"
kind load docker-image --name "$name" balerix:e2e balerix-agent:e2e

echo "kind-up: cluster $name ready; KUBECONFIG=$KUBECONFIG"
```

`chmod +x scripts/kind-up.sh`. `shellcheck scripts/kind-up.sh` passes (`mise x -- shellcheck scripts/kind-up.sh`).

- [ ] **Step 2: The mise tasks and the `e2e` mode of `scripts/operator.sh`**

In `mise.toml`, after `[tasks.e2e]`:

```toml
[tasks.kind-up]
description = "A kind cluster for e2e-k8s with the shared local-path class, the CRDs and the daemon and agent images built from this tree (needs docker; `-- down` deletes it)"
# kind and kubectl are pinned here, not in [tools], so only this task and
# e2e-k8s download them. kind 0.33.0 ships the node image the script pins.
tools = { kind = "0.33.0", kubectl = "1.34.12" }
run = 'scripts/kind-up.sh "${usage_mode:-up}"'

[tasks.e2e-k8s]
description = "The Phase 3 journey on the kind cluster kind-up made: operator out of cluster, fake claude in the pods; fails (not skips) without the cluster"
tools = { kind = "0.33.0", kubectl = "1.34.12" }
env = { BALERIX_REQUIRE_TOOLS = "1" }
run = "scripts/operator.sh e2e"
```

If `mise run kind-up -- down` does not pass `down` through `usage_mode`, use `run = "scripts/kind-up.sh"` and document `mise run kind-up down` (positional) the way `verify-matrix-local -- down` is documented; check which form mise 2026.9 takes with `mise run kind-up -- down` and keep the one that works.

In `scripts/operator.sh`, add a mode:

```bash
  e2e)
    root="${CARGO_TARGET_DIR:-$repo/target}/tmp/kind"
    export KUBECONFIG="$root/kubeconfig"
    export BALERIX_K8S_IMAGES="${BALERIX_K8S_IMAGES:-balerix:e2e,balerix-agent:e2e}"
    CARGO_TARGET_DIR="$target" cargo nextest run \
      --config-file "$repo/.config/nextest.toml" \
      --manifest-path "$dir/Cargo.toml" --test e2e_k8s --no-capture
    ;;
```

and extend the `usage` line to `{build|fmt|check|crds|e2e}`. `--no-capture` keeps the operator's stderr in the CI log.

- [ ] **Step 3: The CI job**

In `.github/workflows/ci.yml`, after the `operator` job:

```yaml
  # The Phase 3 journey on a kind cluster (Spec O §21.4): three native
  # builds, two images, one cluster. Path-filtered on pull requests to what
  # can change it; always on main and the nightly.
  e2e-k8s:
    runs-on: ubuntu-24.04
    env:
      BALERIX_REQUIRE_TOOLS: "1"
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - id: changes
        env:
          EVENT: ${{ github.event_name }}
          BASE: ${{ github.event.pull_request.base.sha }}
        run: |
          if [[ $EVENT != pull_request ]]; then echo run=true >>"$GITHUB_OUTPUT"; exit 0; fi
          git fetch -q --depth=1 origin "$BASE"
          if git diff --name-only "$BASE" HEAD | grep -qE '^(operator/|agent/|crates/|docker/|scripts/kind-up\.sh|scripts/operator\.sh|scripts/release/image-context\.sh|mise\.toml|Cargo\.(toml|lock)|\.github/workflows/ci\.yml)'; then
            echo run=true >>"$GITHUB_OUTPUT"
          else
            echo "nothing e2e-k8s depends on changed"; echo run=false >>"$GITHUB_OUTPUT"
          fi
      - if: steps.changes.outputs.run == 'true'
        uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with: { version: 2026.9.2, install: false, cache: true }
      - if: steps.changes.outputs.run == 'true'
        run: mise install rust cargo:cargo-nextest kind@0.33.0 kubectl@1.34.12
      - if: steps.changes.outputs.run == 'true'
        uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
        with:
          workspaces: |
            . -> target
            agent -> target
            operator -> target
          key: e2e-k8s
      - if: steps.changes.outputs.run == 'true'
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}   # mise's downloads inside the image build
        run: mise run kind-up
      - if: steps.changes.outputs.run == 'true'
        run: mise run e2e-k8s
      - if: failure() && steps.changes.outputs.run == 'true'
        env:
          KUBECONFIG: target/tmp/kind/kubeconfig
        run: |
          for ns in $(kubectl get ns -o name | sed -n 's|namespace/\(e2e-.*\)|\1|p'); do
            echo "== $ns"; kubectl -n "$ns" get daemons,fleets,crews,agents,pods,jobs,pvc -o wide || true
            kubectl -n "$ns" describe pods || true
            for pod in $(kubectl -n "$ns" get pods -o name); do echo "-- $pod"; kubectl -n "$ns" logs "$pod" --all-containers --prefix || true; done
          done
```

`mise install kind@0.33.0 kubectl@1.34.12` installs the task-level pins by name under `MISE_AUTO_INSTALL=false`; if `mise run kind-up` then reports them missing, the task's `tools` table and the install line disagree on the backend name (`kind` is `aqua:kubernetes-sigs/kind` in the registry); use whatever `mise ls-remote kind` resolved in the `[tasks.kind-up]` entry and in the install line alike. `zizmor` and `actionlint` run in the `check` job (`mise run lint`); run `mise x -- zizmor .github/workflows/ci.yml` and `mise x -- actionlint` here before pushing.

- [ ] **Step 4: Push and open the draft pull request**

`mise run check` (lint covers the workflow). Then:

```bash
git add scripts/kind-up.sh scripts/operator.sh mise.toml .github/workflows/ci.yml
git commit -m "ci: kind-up, the e2e-k8s task and its job (Spec O §21.4)

A two-node kind cluster with the shared local-path class of §19.3, the
definitions and the two images built from the tree; the job runs it on
pull requests that can change it, on main and nightly."
git push -u origin feat/kube-operator-cluster
gh pr create --draft --title "feat(operator): the controllers, envtest tests, the images, kind and e2e-k8s (Spec O §21)" --body "Draft while e2e-k8s is wired; see the plan docs/superpowers/plans/2026-10-03-balerix-o3b-operator-on-a-cluster.md."
```

Watch the `e2e-k8s` job: `gh run watch` or `gh pr checks`. Expected at this task: `kind-up` succeeds (cluster, CRDs, both images loaded) and `e2e-k8s` fails with no `e2e_k8s` test (Task 11 adds it); read `kind-up`'s log for the shared class patch and both `docker build`s. Fix what the log shows (the task-tool install, the node image, the `docker build` secret) and push again until `kind-up` is green.

---

### Task 11: The `e2e-k8s` journey

**Files:**
- Create: `operator/tests/e2e_k8s.rs`

**Interfaces:**
- Consumes: `KUBECONFIG`, `BALERIX_K8S_IMAGES` (`<daemon>,<agent>`), `env!("CARGO_BIN_EXE_balerix-operator")`, `kubectl` on `PATH`; the kinds; `balerix dev fake-claude` in the image at `/usr/local/bin/balerix`.
- Produces: the test `the_phase_3_journey_on_kind` (§21.4's six steps).

- [ ] **Step 1: Write the test**

Create `operator/tests/e2e_k8s.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Phase 3 journey on a kind cluster (Spec O §15, §21.4): apply a
//! Daemon and a Fleet, wait Ready, evict a pod and see it resume on its
//! claim, drop an agent and find its branch in the crew cache, delete the
//! Fleet with `retain: None` and find the crew's directories gone. The
//! operator runs outside the cluster, as a child of this test. Needs
//! `KUBECONFIG` (scripts/kind-up.sh) and `BALERIX_K8S_IMAGES`; skips
//! without them, fails under `BALERIX_REQUIRE_TOOLS=1`.
mod support;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use balerix_operator::api::{Agent, Crew, Daemon, DaemonSpec, Fleet, FleetSpec};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Namespace, Pod, Service};
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::{Api, Client};
use support::wait_for;

const OPERATOR: &str = env!("CARGO_BIN_EXE_balerix-operator");

fn gate() -> Option<(String, String)> {
    let images = std::env::var("BALERIX_K8S_IMAGES").ok();
    let kubeconfig = std::env::var_os("KUBECONFIG").filter(|p| std::path::Path::new(p).is_file());
    match (images, kubeconfig) {
        (Some(images), Some(_)) => {
            let (daemon, agent) = images.split_once(',').expect("BALERIX_K8S_IMAGES is <daemon>,<agent>");
            Some((daemon.to_string(), agent.to_string()))
        }
        _ => {
            if std::env::var("BALERIX_REQUIRE_TOOLS").as_deref() == Ok("1") {
                panic!("KUBECONFIG and BALERIX_K8S_IMAGES are required (BALERIX_REQUIRE_TOOLS=1); run `mise run kind-up` first");
            }
            eprintln!("skip: no kind cluster (KUBECONFIG, BALERIX_K8S_IMAGES)");
            None
        }
    }
}

/// Ends a child (the operator, the port-forward) when the test ends,
/// however it ends.
struct Operator(Child);
impl Drop for Operator {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn exec(ns: &str, pod: &str, container: &str, script: &str) -> Result<String, String> {
    let out = Command::new("kubectl")
        .args(["-n", ns, "exec", pod, "-c", container, "--", "bash", "-c", script])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() { Ok(stdout) } else { Err(format!("{stdout}\n{}", String::from_utf8_lossy(&out.stderr))) }
}

fn condition_true(conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition], type_: &str) -> bool {
    conditions.iter().any(|c| c.type_ == type_ && c.status == "True")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_phase_3_journey_on_kind() {
    let Some((daemon_image, agent_image)) = gate() else { return };
    let client = Client::try_default().await.unwrap();
    let ns = format!("e2e-{}", std::process::id());
    let namespace: Namespace = serde_json::from_value(serde_json::json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": ns } })).unwrap();
    Api::<Namespace>::all(client.clone()).create(&PostParams::default(), &namespace).await.unwrap();

    // the operator runs here, not in the cluster: the Daemon's Service is
    // reached through a port-forward, under the Service's own name (§21.6)
    let forward_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let resolve = format!("balerix-default.{ns}.svc=127.0.0.1:{forward_port}");
    let _operator = Operator(
        Command::new(OPERATOR)
            .args(["run", "--watch-namespaces", &ns, "--namespace", &ns, "--daemon-image", &daemon_image, "--agent-image", &agent_image, "--resolve", &resolve])
            .env("RUST_LOG", "info,kube=warn")
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );

    // 1. a git server pod and its Service, seeded with one commit
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let git_pod: Pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "git", "namespace": ns, "labels": { "app": "git" } },
        "spec": {
            "containers": [{
                "name": "git", "image": agent_image, "command": ["bash", "-c",
                    "set -e; git init -q --bare /srv/repo.git; exec git daemon --base-path=/srv --export-all --enable=receive-pack --reuseaddr --listen=0.0.0.0 /srv"],
                "ports": [{ "containerPort": 9418 }],
                "volumeMounts": [{ "name": "srv", "mountPath": "/srv" }, { "name": "tmp", "mountPath": "/tmp" }]
            }],
            "volumes": [{ "name": "srv", "emptyDir": {} }, { "name": "tmp", "emptyDir": {} }]
        }
    })).unwrap();
    pods.create(&PostParams::default(), &git_pod).await.unwrap();
    let service: Service = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": "git", "namespace": ns },
        "spec": { "selector": { "app": "git" }, "ports": [{ "port": 9418, "targetPort": 9418 }] }
    })).unwrap();
    Api::<Service>::namespaced(client.clone(), &ns).create(&PostParams::default(), &service).await.unwrap();
    wait_for("the git pod", Duration::from_secs(180), || async {
        pods.get("git").await.unwrap().status.and_then(|s| s.phase).filter(|p| p == "Running")
    }).await;
    exec(&ns, "git", "git", "set -e; cd /tmp && git clone -q /srv/repo.git w && cd w && echo hi > README && git add . && git -c user.name=t -c user.email=t@t commit -qm init && git push -q origin HEAD:main").unwrap();
    let repo = format!("git://git.{ns}.svc:9418/repo.git");

    // 2. a Daemon whose claude is the fake one in the image
    let daemons: Api<Daemon> = Api::namespaced(client.clone(), &ns);
    let spec: DaemonSpec = serde_json::from_value(serde_json::json!({
        "storage": { "state": { "size": "1Gi" }, "shared": { "size": "2Gi" }, "agent": { "size": "1Gi" } },
        "defaults": {
            "claude": { "binary": "/usr/local/bin/balerix", "args": ["dev", "fake-claude", "--verbose"], "settings": { "model": "sonnet" } },
            "sandbox": { "network": { "block": false } }
        }
    })).unwrap();
    daemons.create(&PostParams::default(), &Daemon::new("default", spec)).await.unwrap();
    let services: Api<Service> = Api::namespaced(client.clone(), &ns);
    wait_for("the Daemon's Service", Duration::from_secs(120), || async { services.get_opt("balerix-default").await.unwrap() }).await;
    let _forward = Operator(
        Command::new("kubectl")
            .args(["-n", &ns, "port-forward", "svc/balerix-default", &format!("{forward_port}:7643")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    // the pool Job installs claude and gh with mise inside the cluster:
    // minutes, and GitHub's unauthenticated rate limit if the runner's
    // address is busy (a finding for §21.6 if it bites)
    wait_for("the Daemon Ready", Duration::from_secs(600), || async {
        let s = daemons.get("default").await.unwrap().status?;
        condition_true(&s.conditions, "Ready").then_some(())
    }).await;

    // 3. a Fleet of one crew and two agents, Ready when both forwarded SessionStart
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    let spec: FleetSpec = serde_json::from_value(serde_json::json!({
        "daemon": "default", "retain": "None",
        "crews": { "c": { "repo": repo, "ref": "main", "git": { "push": false, "auth": "none" },
            "agents": { "alice": {}, "bob": {} } } }
    })).unwrap();
    fleets.create(&PostParams::default(), &Fleet::new("f", spec)).await.unwrap();
    wait_for("the Fleet Ready", Duration::from_secs(900), || async {
        let s = fleets.get("f").await.unwrap().status?;
        condition_true(&s.conditions, "Ready").then_some(())
    }).await;
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    assert_eq!(agents.get("f-c-alice").await.unwrap().status.unwrap().phase.as_deref(), Some("ready"));
    assert!(Api::<Crew>::namespaced(client.clone(), &ns).get("f-c").await.unwrap().status.unwrap().cache_ref.is_some());

    // 4. evict alice's pod: recreated on the same claim, Ready again, home intact
    exec(&ns, "f-c-alice", "agent", "touch /balerix/agent/home/e2e-marker").unwrap();
    let first = pods.get("f-c-alice").await.unwrap();
    pods.delete("f-c-alice", &DeleteParams::default()).await.unwrap();
    wait_for("the new pod Ready", Duration::from_secs(600), || async {
        let p = pods.get_opt("f-c-alice").await.unwrap()?;
        if p.metadata.uid == first.metadata.uid { return None; }
        let a = agents.get("f-c-alice").await.unwrap().status?;
        condition_true(&a.conditions, "Ready").then_some(())
    }).await;
    exec(&ns, "f-c-alice", "agent", "test -f /balerix/agent/home/e2e-marker").unwrap();

    // 5. drop bob: harvested into the crew cache
    fleets.patch("f", &kube::api::PatchParams::default(), &kube::api::Patch::Merge(serde_json::json!({ "spec": { "crews": { "c": { "agents": { "bob": null } } } } }))).await.unwrap();
    wait_for("bob gone", Duration::from_secs(600), || async { agents.get_opt("f-c-bob").await.unwrap().is_none().then_some(()) }).await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let harvest = jobs.get("f-c-bob-harvest").await.unwrap();
    assert_eq!(harvest.status.as_ref().and_then(|s| s.succeeded), Some(1));
    let message = pods.list(&ListParams::default().labels("job-name=f-c-bob-harvest")).await.unwrap().items.iter()
        .find_map(|p| p.status.as_ref()?.container_statuses.as_ref()?.iter().find_map(|c| c.state.as_ref()?.terminated.as_ref()?.message.clone()))
        .expect("the harvest Job's message");
    let branch = message.trim().strip_prefix("harvested ").expect("harvested <branch>").to_string();
    let probe: Pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "probe", "namespace": ns },
        "spec": { "containers": [{ "name": "probe", "image": agent_image, "command": ["sleep", "infinity"],
            "volumeMounts": [{ "name": "shared", "mountPath": "/balerix/volume" }] }],
            "volumes": [{ "name": "shared", "persistentVolumeClaim": { "claimName": "balerix-default-shared" } }] }
    })).unwrap();
    pods.create(&PostParams::default(), &probe).await.unwrap();
    wait_for("the probe pod", Duration::from_secs(180), || async {
        pods.get("probe").await.unwrap().status.and_then(|s| s.phase).filter(|p| p == "Running")
    }).await;
    let listed = exec(&ns, "probe", "probe", &format!("git --git-dir=/balerix/volume/fleets/f/crews/c/repo branch --list {branch}")).unwrap();
    assert!(listed.contains(&branch), "branch {branch} not in the cache: {listed:?}");

    // 6. the Fleet deleted with retain: None: the crew's directories emptied
    fleets.delete("f", &DeleteParams::default()).await.unwrap();
    wait_for("the Fleet gone", Duration::from_secs(600), || async { fleets.get_opt("f").await.unwrap().is_none().then_some(()) }).await;
    exec(&ns, "probe", "probe", "test -z \"$(ls -A /balerix/volume/fleets/f/crews/c/repo)\" && test -z \"$(ls -A /balerix/volume/fleets/f/crews/c/pool)\"").unwrap();
}
```

`support::wait_for` is Task 4's; the `support` module's envtest and stub parts compile but are unused here (`#![allow(dead_code)]` is on them).

- [ ] **Step 2: Push and watch**

```bash
mise run operator      # the new test skips here (no KUBECONFIG); everything else passes
git add operator/tests/e2e_k8s.rs
git commit -m "test(operator): the Phase 3 journey on kind, e2e-k8s (Spec O §15, §21.4)"
git push
```

Watch the `e2e-k8s` job. Expected: green. Each failure is read from the job's log and the diagnostics step (pods, describes, logs of every container). Likely first findings, each a fix in the tree and a push: the `slice` init container's `mkdir` on the shared volume (§21.5: a finding for the plan, fixed in `desired::jobs`); the agent image's `/tmp`; the sidecar's self-test under kind's kernel; the time the pool Job takes to install `claude` and `gh` (the 600 s waits above are for that). A fix to `desired` changes a 3a snapshot: review and accept it, and name the finding in the commit. Iterate until the job is green twice in a row.

- [ ] **Step 3: Mark the pull request ready**

`gh pr ready`. Then the whole-branch review of the development workflow.

---

### Task 12: Docs: AGENTS.md, ARCHITECTURE.md, the spec's §21.6

**Files:**
- Modify: `AGENTS.md`, `ARCHITECTURE.md`, `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md` (§21.6, §15, §17)

- [ ] **Step 1: AGENTS.md**

In the tasks list, the `operator` entry gains: "The controller tests run on the envtest binaries the task pins (a `kube-apiserver` and `etcd`, no cluster, no kubelet: the test stands in for it)." After `e2e` add:

```
- `kind-up` — a kind cluster `balerix-e2e` for `e2e-k8s`: the shared
  local-path class (§19.3), the CRDs, and the daemon and agent images built
  from this tree and loaded (`balerix:e2e`, `balerix-agent:e2e`). Needs
  docker; `mise run kind-up -- down` deletes it. This host has no docker:
  CI only.
- `e2e-k8s` — the Phase 3 journey on that cluster (`operator/tests/e2e_k8s.rs`):
  the operator runs outside the cluster as the test's child; `dev fake-claude`
  in the pods. Fails, not skips, without the cluster. Its own CI job,
  path-filtered on pull requests.
```

In the conventions, after the `operator/` bullet: "The controllers (`operator/src/controllers/`) are the dumb executor: observe, call `desired`, apply with server-side apply under the field manager `balerix-operator`, patch status. Every Job goes through `controllers::jobs::ensure_job`."

In the gotchas: "A Pod deleted in envtest stays `Terminating`: there is no kubelet. The tests force-delete it with `support::reap_pod` once the operator has set its deletion timestamp. Jobs likewise never run: `support::finish_job` is the kubelet."

- [ ] **Step 2: ARCHITECTURE.md**

In the `balerix-operator` bullet, replace "the controllers (sub-project 3b) are the only part that touches a cluster" with "the controllers (`controllers/`, one `kube_runtime::Controller` per kind, one set per watched namespace) are the only part that touches a cluster: they observe, call `desired`, apply with server-side apply and patch status; the Daemon is polled into a per-fleet record cache every 15 s and the Agents mirror it. The cleanup Job (`crew-remove`) joins the sync, pool and harvest Jobs as a `balerix-agent` command."

- [ ] **Step 3: §21.6 and the small spec edits**

Replace §21.6's one line with the **Decisions this plan makes** list from this plan's header, each as a bullet in the spec's voice (as §20.5 was written from 3a's plan), plus a closing "Known in 3b and left open" list: whatever Task 11's CI iteration found and did not fix (write it from the pull request's commits), and the operator's RBAC, Deployment, metrics and `verify-k8s` (sub-project 5). In §15, the "End to end" bullet gains "(`operator/tests/e2e_k8s.rs`, the operator out of cluster; §21.4)". In §17, item 3 becomes "… 3a done 2026-10; 3b done 2026-10 (PR #<n>)" with the pull request's number.

- [ ] **Step 4: Commit and finish**

```bash
mise run check
git add AGENTS.md ARCHITECTURE.md docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md
git commit -m "docs: the controllers, envtest and e2e-k8s in AGENTS.md and ARCHITECTURE.md; Spec O §21.6"
git push
```

The pull request is then ready for the whole-branch review and the squash merge named in the Global Constraints.
