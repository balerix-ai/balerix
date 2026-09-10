# Balerix Spec A / Phase 2 — Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the ports and a pure planner/executor reconciler in `balerix-core`, the `balerix-runtime` adapters that make one agent exist on disk and in tmux, and a hidden `balerix dev materialize` command that renders an agent's generated files.

**Architecture:** `balerix-core` gains `ResolvedAgent`, two ports (`Materializer`, `AgentRunner`) plus `Clock`, in-crate fakes, and `reconcile::{plan, apply, execute}` — `plan` is a total function from (desired, status, observed, now) to an ordered `Vec<Step>`; `execute` walks it through the ports. `balerix-runtime` implements the ports over `git`/`gh`/`mise`/`nono`/`tmux` subprocesses, one module per materialization step, with every path derived from an injected `StateLayout` and every binary from an injected `ToolPaths`. The agent's environment is set through the nono profile's `environment` block (nono refuses grants that overlap its own state root under `$HOME`). The binary wires layout and tools and adds `dev materialize`.

**Tech Stack:** Rust 1.98.1 (edition 2024); serde/serde_json/serde_norway/toml; sha2 + hex (spec hash); thiserror; clap 4; insta (golden), proptest + proptest-state-machine (property/model-based); tempfile, assert_cmd, predicates; real `git`, `gh 2.100.0`, `mise`, `nono 0.75.0`, `tmux 3.7c` for integration tests.

**Spec:** `docs/superpowers/specs/2026-09-06-balerix-a2-runtime-design.md` (the *Phase 2 spec*), which amends `docs/superpowers/specs/2026-09-05-balerix-architecture-design.md` (the *architecture spec*). Read Phase 2 spec §2–§4 before starting any task in that area. Where this plan refines the spec (three places, all in Task 18's addendum): `Timestamp` and `SpecHash` live in `balerix-api` because `FleetStatus` carries them; the `MarkDead` step is folded into `NoteExit`; `RemoveCrew` carries a `Keep { repos, sessions }` instead of a bare `keep_repo`.

## Global Constraints

Copied from the specs; every task's requirements include these.

- Rust **1.98.1**, `edition = "2024"`, `rust-version = "1.98"`; every tool in `mise.toml` is an exact version. Run cargo as `mise x -- cargo …` or through `mise run <task>`.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass at every commit. `unsafe_code = "forbid"`. No `unwrap`/`expect` outside tests (clippy warns; `-D warnings` makes it an error). `std::env::set_var` is unsafe in edition 2024 — inject environment through parameters.
- Library crates return `thiserror` errors whose `Display` starts with the fleet/crew/agent id (runtime) or config path (config); only the `balerix` binary uses `anyhow`.
- Dependency direction: `api` leaf → `core` → `config` / `runtime` (adapters) → binary. `runtime` never depends on `config`; no adapter depends on another adapter.
- New Cargo dependencies go in `[workspace.dependencies]` with an exact version and a reason in the commit message. This plan adds exactly: `sha2 = "0.11.0"`, `hex = "0.4.3"`, `toml = "1.1.5"`, `proptest-state-machine = "0.8.0"`.
- Secrets never appear in `Debug` output, logs, argv, the outer environment, or `launch.sh`. `HookTarget` and anything holding a token hand-implements `Debug` with `<redacted>`.
- Subprocesses are argv arrays via `std::process::Command`; never a shell string. `launch.sh` values pass through `sh_quote`.
- Names are already validated (`FleetName` etc.); the runtime never re-validates them and never builds a path from an unvalidated string.
- Integration tests skip with a printed reason when a tool or Landlock is missing; if `BALERIX_REQUIRE_TOOLS=1` is set (CI), a would-be skip panics instead.
- Commit messages: imperative subject, body explains why, and end with the trailer line `Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU`.
- insta: read every `.snap.new`, compare against the expected values listed in the task, then `mise x -- cargo insta accept`. Never blind-accept.

## File structure

```
Cargo.toml                                        + balerix-runtime member/dep; sha2, hex, toml, proptest-state-machine
mise.toml                                         + tasks: mutants, test-it
.github/workflows/ci.yml                          + tmux/nono/gh install, BALERIX_REQUIRE_TOOLS, nightly mutants job
crates/balerix-api/src/status.rs                  Timestamp, SpecHash, FleetPhase, AgentPhase, AgentStatus, FleetStatus
crates/balerix-core/src/repo.rs                   + RepoRef::Local (file://)
crates/balerix-core/src/agent.rs                  CrewRef, ResolvedAgent, spec_hash
crates/balerix-core/src/ports.rs                  ProcessState, ObservedState, LaunchPlan, HookTarget, Keep, Materializer, AgentRunner, Clock, errors
crates/balerix-core/src/fakes.rs                  FakeMaterializer, FakeRunner, FakeClock
crates/balerix-core/src/reconcile/mod.rs          ReconcilePolicy, Step, Plan, plan(), backoff_secs()
crates/balerix-core/src/reconcile/status.rs       apply(), agent_ready(), set_desired(), finish_pass()
crates/balerix-core/src/reconcile/execute.rs      ReconcileContext, ExecuteReport, execute(), reconcile_pass()
crates/balerix-core/tests/plan_golden.rs          insta snapshots of plans for four situations
crates/balerix-core/tests/reconcile_model.rs      proptest-state-machine model test
crates/balerix-runtime/Cargo.toml
crates/balerix-runtime/src/lib.rs                 re-exports
crates/balerix-runtime/src/layout.rs              StateLayout, CrewPaths, AgentPaths
crates/balerix-runtime/src/tools.rs               ToolPaths, MissingTool, Cmd (logged subprocess runner)
crates/balerix-runtime/src/fsutil.rs              write_atomic, ensure_dir
crates/balerix-runtime/src/quote.rs               sh_quote
crates/balerix-runtime/src/env.rs                 agent_env (the profile set_vars)
crates/balerix-runtime/src/home.rs                settings.json / .credentials.json / .claude.json / hosts.yml
crates/balerix-runtime/src/toolchain.rs           mise.toml render, embedded default, mise install
crates/balerix-runtime/src/sandbox.rs             nono profile render, merge, conflict check, validate
crates/balerix-runtime/src/launch.rs              launch.sh + LaunchPlan
crates/balerix-runtime/src/workspace.rs           git clone / worktree
crates/balerix-runtime/src/materializer.rs        Runtime: render_agent, impl Materializer
crates/balerix-runtime/src/tmux.rs                TmuxRunner: impl AgentRunner
crates/balerix-runtime/tests/support/mod.rs       tool detection, skip-or-require, temp layout under CARGO_TARGET_TMPDIR
crates/balerix-runtime/tests/generated_golden.rs  four generated files for payments alice/bob
crates/balerix-runtime/tests/toolchain_it.rs      real mise, pre-seeded shared dir
crates/balerix-runtime/tests/sandbox_it.rs        real nono validate + enforcement
crates/balerix-runtime/tests/workspace_it.rs      real git against a local bare repo
crates/balerix-runtime/tests/materialize_it.rs    Runtime::materialize end to end
crates/balerix-runtime/tests/tmux_it.rs           real tmux on a private socket
crates/balerix/src/cli.rs                         + hidden `dev` group
crates/balerix/src/wiring.rs                      layout_from_env, tool_paths
crates/balerix/src/commands/dev.rs                `dev materialize`
crates/balerix/tests/cli_dev_materialize.rs
ARCHITECTURE.md AGENTS.md README.md docs/THREAT-MODEL.md   Phase 2 updates
docs/superpowers/specs/2026-09-05-balerix-architecture-design.md   dated addendum after §12
docs/superpowers/specs/2026-09-06-balerix-a2-runtime-design.md     §4.4 verdicts, three refinements
```

---

### Task 1: Workspace dependencies and `balerix-api` status types

**Files:**
- Modify: `Cargo.toml` (workspace)
- Create: `crates/balerix-api/src/status.rs`
- Modify: `crates/balerix-api/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct Timestamp(pub u64);            // seconds since the Unix epoch
  impl Timestamp { pub fn plus_secs(self, s: u64) -> Timestamp }
  pub struct SpecHash(String);               // hex sha-256
  impl SpecHash { pub fn new(hex: String) -> Self; pub fn as_str(&self) -> &str }
  pub enum FleetPhase { Pending, Reconciling, Ready, Degraded, Terminating }
  pub enum AgentPhase { Pending, Materializing, Starting, Ready, Dead, Stopped }
  pub struct AgentStatus { phase, message: String, last_event_at: Option<Timestamp>, applied_hash: Option<SpecHash>, restarts: u32, next_restart_at: Option<Timestamp> }
  pub struct FleetStatus { generation: u64, observed_generation: u64, phase: FleetPhase, agents: BTreeMap<String, AgentStatus> }
  impl FleetStatus { pub fn entry(&mut self, id: &str) -> &mut AgentStatus }   // inserts a Pending default
  ```

- [ ] **Step 1: Add the workspace dependencies**

In `Cargo.toml` `[workspace.dependencies]`, after `balerix-config`:
```toml
balerix-runtime = { path = "crates/balerix-runtime" }
```
after `clap`:
```toml
sha2 = "0.11.0"
hex = "0.4.3"
toml = "1.1.5"
```
under `# dev`, after `proptest`:
```toml
proptest-state-machine = "0.8.0"
```

- [ ] **Step 2: Write the failing tests**

`crates/balerix-api/src/status.rs`:
```rust
//! Fleet and agent status (spec §7 status model; Phase 2 spec §2.3). Wire
//! types: the daemon stores and returns these.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Seconds since the Unix epoch. Whole seconds are enough for backoff and
/// resync decisions and keep the wire form a plain integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp(pub u64);

impl Timestamp {
    pub fn plus_secs(self, s: u64) -> Self {
        Self(self.0.saturating_add(s))
    }
}

/// Hex SHA-256 of an agent's resolved spec. Computed in `balerix-core`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SpecHash(String);

impl SpecHash {
    pub fn new(hex: String) -> Self {
        Self(hex)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FleetPhase {
    Pending,
    Reconciling,
    Ready,
    Degraded,
    Terminating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentPhase {
    Pending,
    Materializing,
    Starting,
    Ready,
    Dead,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub phase: AgentPhase,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub last_event_at: Option<Timestamp>,
    #[serde(default)]
    pub applied_hash: Option<SpecHash>,
    #[serde(default)]
    pub restarts: u32,
    #[serde(default)]
    pub next_restart_at: Option<Timestamp>,
}

impl Default for AgentStatus {
    fn default() -> Self {
        Self {
            phase: AgentPhase::Pending,
            message: String::new(),
            last_event_at: None,
            applied_hash: None,
            restarts: 0,
            next_restart_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStatus {
    pub generation: u64,
    pub observed_generation: u64,
    pub phase: FleetPhase,
    /// Keyed by `AgentId` display form `fleet/crew/agent`.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentStatus>,
}

impl Default for FleetStatus {
    fn default() -> Self {
        Self {
            generation: 0,
            observed_generation: 0,
            phase: FleetPhase::Pending,
            agents: BTreeMap::new(),
        }
    }
}

impl FleetStatus {
    /// The status entry for `id`, created as `Pending` if absent.
    pub fn entry(&mut self, id: &str) -> &mut AgentStatus {
        self.agents.entry(id.to_string()).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timestamp_addition_saturates() {
        assert_eq!(Timestamp(10).plus_secs(5), Timestamp(15));
        assert_eq!(Timestamp(u64::MAX).plus_secs(5), Timestamp(u64::MAX));
    }

    #[test]
    fn phases_serialize_lowercase() {
        assert_eq!(serde_json::to_value(FleetPhase::Degraded).unwrap(), json!("degraded"));
        assert_eq!(serde_json::to_value(AgentPhase::Materializing).unwrap(), json!("materializing"));
    }

    #[test]
    fn entry_inserts_a_pending_default() {
        let mut s = FleetStatus::default();
        s.entry("f/c/a").restarts = 3;
        assert_eq!(s.agents["f/c/a"].phase, AgentPhase::Pending);
        assert_eq!(s.agents["f/c/a"].restarts, 3);
        assert_eq!(s.entry("f/c/a").restarts, 3, "second call returns the same entry");
    }

    #[test]
    fn status_round_trips_and_tolerates_missing_optional_fields() {
        let s: FleetStatus = serde_json::from_value(json!({
            "generation": 2, "observed_generation": 1, "phase": "reconciling",
            "agents": { "f/c/a": { "phase": "starting" } }
        }))
        .unwrap();
        assert_eq!(s.agents["f/c/a"].applied_hash, None);
        let back: FleetStatus = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
```

`crates/balerix-api/src/lib.rs`: add `pub mod status;` and
```rust
pub use status::{AgentPhase, AgentStatus, FleetPhase, FleetStatus, SpecHash, Timestamp};
```

- [ ] **Step 3: Run the tests**

Run: `mise x -- cargo nextest run -p balerix-api`
Expected: the four new tests PASS (the types are written alongside the tests; the failing state here is the compile error before Step 2's edits are complete).

- [ ] **Step 4: Lint and commit**

Run: `mise run lint`
```bash
git add Cargo.toml Cargo.lock crates/balerix-api
git commit -m "Add fleet/agent status wire types and Phase 2 workspace deps

sha2+hex hash the resolved agent spec (restart key), toml parses the
system tool table, proptest-state-machine drives the reconciler model
test. Timestamp and SpecHash live in balerix-api because FleetStatus
carries them on the wire.

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---

### Task 2: `balerix-core` — `RepoRef::Local`, `CrewRef`, `ResolvedAgent`, `spec_hash`

**Files:**
- Modify: `crates/balerix-core/Cargo.toml`, `crates/balerix-core/src/lib.rs`, `crates/balerix-core/src/repo.rs`
- Create: `crates/balerix-core/src/agent.rs`

**Interfaces:**
- Consumes: `Fleet`, `Crew`, `AgentId`, `RepoRef`, `SpecHash`.
- Produces:
  ```rust
  pub enum RepoRef { GitHub{..}, Url(String), Local(PathBuf) }   // `file:///abs/path` → Local
  pub struct CrewRef { pub fleet: FleetName, pub crew: CrewName }   // Display fleet/crew; FromStr
  impl AgentId { pub fn crew_ref(&self) -> CrewRef }
  pub struct ResolvedAgent { pub id: AgentId, pub repo: RepoRef, pub git_ref: String, pub git: GitSettings, pub settings: AgentSettings }
  impl ResolvedAgent { pub fn from_fleet(fleet: &Fleet) -> Vec<ResolvedAgent>;  // sorted by id
                       pub fn branch(&self) -> String;                          // balerix/<f>/<c>/<a>
                       pub fn hash(&self) -> SpecHash }
  ```

- [ ] **Step 1: Add dependencies**

`crates/balerix-core/Cargo.toml` `[dependencies]`: add
```toml
serde_json = { workspace = true }
sha2 = { workspace = true }
hex = { workspace = true }
```
(remove `serde_json` from `[dev-dependencies]` since it is now a normal dependency).

- [ ] **Step 2: Write the failing tests for `RepoRef::Local`**

Append to the `tests` module in `crates/balerix-core/src/repo.rs`:
```rust
    #[test]
    fn file_urls_parse_to_local_and_round_trip() {
        let r = RepoRef::parse("file:///srv/git/api.git").unwrap();
        assert_eq!(r, RepoRef::Local(PathBuf::from("/srv/git/api.git")));
        assert_eq!(r.clone_url(), "file:///srv/git/api.git");
        assert_eq!(r.github_slug(), None);
    }

    #[test]
    fn relative_file_urls_are_rejected() {
        let err = RepoRef::parse("file://relative/path").unwrap_err();
        assert_eq!(err.reason, "file:// repos must be absolute paths");
    }
```
Add `use std::path::PathBuf;` at the top of the tests module.

- [ ] **Step 3: Run to verify failure**

Run: `mise x -- cargo nextest run -p balerix-core repo`
Expected: compile error, `Local` is not a variant.

- [ ] **Step 4: Implement `Local`**

In `crates/balerix-core/src/repo.rs`: add `use std::path::PathBuf;` at the top; add the variant `Local(PathBuf)` with doc `/// A local repository reached through a file:// URL (tests, offline use).`; in `parse`, before the `contains("://")` check:
```rust
        if let Some(rest) = s.strip_prefix("file://") {
            if !rest.starts_with('/') {
                return Err(err("file:// repos must be absolute paths"));
            }
            return Ok(Self::Local(PathBuf::from(rest)));
        }
```
In `clone_url`: `Self::Local(p) => format!("file://{}", p.display()),`. `github_slug` already returns `None` for the catch-all; change its match arm to `Self::Url(_) | Self::Local(_) => None`.

- [ ] **Step 5: Run to verify pass**

Run: `mise x -- cargo nextest run -p balerix-core repo` — Expected: all PASS.

- [ ] **Step 6: Write the failing tests for `agent.rs`**

`crates/balerix-core/src/agent.rs`:
```rust
//! One agent as the runtime sees it: every crew-level fact folded in
//! (Phase 2 spec §2.1). `ResolvedAgent::from_fleet` is the only way the
//! runtime reaches the fleet tree.

use std::fmt;
use std::str::FromStr;

use balerix_api::{AgentSettings, GitSettings, SpecHash};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::name::{AgentId, CrewName, FleetName, NameError};
use crate::repo::RepoRef;
use crate::Fleet;

/// A crew's identity, written `fleet/crew`. Also the tmux session name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CrewRef {
    pub fleet: FleetName,
    pub crew: CrewName,
}

impl fmt::Display for CrewRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.fleet, self.crew)
    }
}

impl FromStr for CrewRef {
    type Err = NameError;
    fn from_str(s: &str) -> Result<Self, NameError> {
        let Some((f, c)) = s.split_once('/') else {
            return Err(NameError { kind: "crew ref", value: s.to_string(), reason: "expected fleet/crew" });
        };
        if c.contains('/') {
            return Err(NameError { kind: "crew ref", value: s.to_string(), reason: "expected fleet/crew" });
        }
        Ok(Self { fleet: f.parse()?, crew: c.parse()? })
    }
}

impl AgentId {
    pub fn crew_ref(&self) -> CrewRef {
        CrewRef { fleet: self.fleet.clone(), crew: self.crew.clone() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAgent {
    pub id: AgentId,
    pub repo: RepoRef,
    pub git_ref: String,
    pub git: GitSettings,
    pub settings: AgentSettings,
}

/// Exactly the fields that, when changed, must restart the agent.
#[derive(Serialize)]
struct HashInput<'a> {
    repo: String,
    git_ref: &'a str,
    git: &'a GitSettings,
    settings: &'a AgentSettings,
}

impl ResolvedAgent {
    /// Every agent of the fleet, sorted by id.
    pub fn from_fleet(fleet: &Fleet) -> Vec<ResolvedAgent> {
        let mut out = Vec::new();
        for (crew_name, crew) in &fleet.crews {
            for (agent_name, settings) in &crew.agents {
                out.push(ResolvedAgent {
                    id: AgentId { fleet: fleet.name.clone(), crew: crew_name.clone(), agent: agent_name.clone() },
                    repo: crew.repo.clone(),
                    git_ref: crew.git_ref.clone(),
                    git: crew.git.clone(),
                    settings: settings.clone(),
                });
            }
        }
        out
    }

    /// The per-agent branch, `balerix/<fleet>/<crew>/<agent>` (spec D3).
    pub fn branch(&self) -> String {
        format!("balerix/{}", self.id)
    }

    /// SHA-256 over canonical JSON. `serde_json` writes maps in key order
    /// (every map here is a `BTreeMap` or a `serde_json::Map` without
    /// `preserve_order`), so equal inputs hash equal regardless of source order.
    pub fn hash(&self) -> SpecHash {
        let input = HashInput { repo: self.repo.clone_url(), git_ref: &self.git_ref, git: &self.git, settings: &self.settings };
        // Serialization of these plain data types cannot fail.
        let bytes = serde_json::to_vec(&input).unwrap_or_default();
        SpecHash::new(hex::encode(Sha256::digest(bytes)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{CrewSpec, FleetSpec};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn fleet() -> Fleet {
        Fleet::try_from(FleetSpec {
            name: "payments".into(),
            crews: BTreeMap::from([(
                "backend".to_string(),
                CrewSpec {
                    repo: "acme/api".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: BTreeMap::from([
                        ("bob".to_string(), AgentSettings::default()),
                        ("alice".to_string(), AgentSettings::default()),
                    ]),
                },
            )]),
        })
        .unwrap()
    }

    #[test]
    fn crew_ref_displays_and_parses() {
        let c: CrewRef = "payments/backend".parse().unwrap();
        assert_eq!(c.to_string(), "payments/backend");
        assert!("payments".parse::<CrewRef>().is_err());
        assert!("a/b/c".parse::<CrewRef>().is_err());
    }

    #[test]
    fn from_fleet_folds_crew_facts_in_and_sorts_by_id() {
        let agents = ResolvedAgent::from_fleet(&fleet());
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].id.to_string(), "payments/backend/alice");
        assert_eq!(agents[1].id.to_string(), "payments/backend/bob");
        assert_eq!(agents[0].git_ref, "main");
        assert_eq!(agents[0].repo.clone_url(), "https://github.com/acme/api.git");
        assert_eq!(agents[0].branch(), "balerix/payments/backend/alice");
        assert_eq!(agents[0].id.crew_ref().to_string(), "payments/backend");
    }

    #[test]
    fn hash_ignores_json_key_order_and_changes_with_settings() {
        let mut a = ResolvedAgent::from_fleet(&fleet()).remove(0);
        a.settings.claude.settings = json!({ "model": "opus", "permissions": { "allow": ["x"] } });
        let mut b = a.clone();
        b.settings.claude.settings = json!({ "permissions": { "allow": ["x"] }, "model": "opus" });
        assert_eq!(a.hash(), b.hash());
        b.settings.claude.settings = json!({ "model": "sonnet" });
        assert_ne!(a.hash(), b.hash());
        assert_eq!(a.hash().as_str().len(), 64);
    }

    #[test]
    fn hash_changes_with_crew_level_facts() {
        let a = ResolvedAgent::from_fleet(&fleet()).remove(0);
        let mut b = a.clone();
        b.git_ref = "develop".into();
        assert_ne!(a.hash(), b.hash());
    }
}
```

`crates/balerix-core/src/lib.rs`: add `pub mod agent;` and
```rust
pub use agent::{CrewRef, ResolvedAgent};
```

- [ ] **Step 7: Run, lint, commit**

Run: `mise x -- cargo nextest run -p balerix-core && mise run lint`
Expected: all PASS, no warnings.
```bash
git add crates/balerix-core Cargo.lock
git commit -m "Add ResolvedAgent, CrewRef, spec hash and file:// repos to balerix-core

The runtime sees one agent with the crew's repo/ref folded in; the hash
of that view is the restart key (spec §7). file:// repos let the
integration tests and offline use clone from a local bare repo.

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---
### Task 3: `balerix-core` — ports, observation types, errors

**Files:**
- Create: `crates/balerix-core/src/ports.rs`
- Modify: `crates/balerix-core/src/lib.rs`

**Interfaces:**
- Produces (Phase 2 spec §2.1–2.2):
  ```rust
  pub enum ProcessState { Running { pid: u32 }, Exited { code: Option<i32> } }
  pub struct ObservedState { pub crews: BTreeMap<CrewName, BTreeMap<AgentName, ProcessState>> }
  impl ObservedState { pub fn get(&self, id: &AgentId) -> Option<&ProcessState>;
                       pub fn set(&mut self, id: &AgentId, s: ProcessState);
                       pub fn remove(&mut self, id: &AgentId);
                       pub fn agent_ids(&self, fleet: &FleetName) -> Vec<AgentId> }
  pub struct LaunchPlan { pub cwd: PathBuf, pub env: BTreeMap<String, String>, pub argv: Vec<String>, pub script: PathBuf }
  pub struct HookTarget { pub url: String, pub secret: String }        // Debug redacts secret
  pub struct Keep { pub repos: bool, pub sessions: bool }              // spec D6
  pub trait Materializer: Send + Sync { ensure_crew, materialize, remove_agent, remove_crew }
  pub trait AgentRunner: Send + Sync { ensure_crew, ensure_agent, stop_agent, stop_crew, observe, send_text }
  pub trait Clock: Send + Sync { fn now(&self) -> Timestamp }
  pub enum MaterializeError { Tool{..}, Io{..}, SandboxConflict{..}, Invalid{..} }
  pub enum RunnerError { Tool{..}, Parse{..} }
  pub fn first_line(s: &str) -> &str
  ```

- [ ] **Step 1: Write the file with its tests**

`crates/balerix-core/src/ports.rs`:
```rust
//! Ports (Phase 2 spec §2.2) and the value types that cross them. Adapters
//! in `balerix-runtime` implement the traits; the reconciler drives them.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use balerix_api::{CredentialBundle, GitSettings, Timestamp};

use crate::agent::{CrewRef, ResolvedAgent};
use crate::name::{AgentId, AgentName, CrewName, FleetName};
use crate::repo::RepoRef;

/// What the runner can see about one agent's process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Running { pid: u32 },
    Exited { code: Option<i32> },
}

/// Sessions and windows the runner found for one fleet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedState {
    pub crews: BTreeMap<CrewName, BTreeMap<AgentName, ProcessState>>,
}

impl ObservedState {
    pub fn get(&self, id: &AgentId) -> Option<&ProcessState> {
        self.crews.get(&id.crew)?.get(&id.agent)
    }
    pub fn set(&mut self, id: &AgentId, s: ProcessState) {
        self.crews.entry(id.crew.clone()).or_default().insert(id.agent.clone(), s);
    }
    pub fn remove(&mut self, id: &AgentId) {
        if let Some(c) = self.crews.get_mut(&id.crew) {
            c.remove(&id.agent);
        }
    }
    /// Every observed agent, as ids in `fleet`, sorted.
    pub fn agent_ids(&self, fleet: &FleetName) -> Vec<AgentId> {
        self.crews
            .iter()
            .flat_map(|(crew, agents)| {
                agents.keys().map(move |agent| AgentId { fleet: fleet.clone(), crew: crew.clone(), agent: agent.clone() })
            })
            .collect()
    }
}

/// Everything a runner needs to start one agent. Runner-agnostic (spec §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub cwd: PathBuf,
    /// The OUTER environment: `PATH` and nono's own `HOME`. The agent's
    /// environment is inside the nono profile (Phase 2 spec P2-5).
    pub env: BTreeMap<String, String>,
    pub argv: Vec<String>,
    /// Rendered `launch.sh`; what the runner actually executes.
    pub script: PathBuf,
}

/// Where an agent's Claude hooks post to, and the per-agent secret.
#[derive(Clone, PartialEq, Eq)]
pub struct HookTarget {
    pub url: String,
    pub secret: String,
}

impl fmt::Debug for HookTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookTarget").field("url", &self.url).field("secret", &"<redacted>").finish()
    }
}

/// What `down` leaves behind (spec D6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Keep {
    pub repos: bool,
    pub sessions: bool,
}

/// First line of a tool's stderr, for one-line error displays.
pub fn first_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim()
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MaterializeError {
    #[error("{id}: {tool} {subcommand}: {}", first_line(stderr))]
    Tool { id: String, tool: String, subcommand: String, args: Vec<String>, stderr: String },
    #[error("{id}: {path}: {message}")]
    Io { id: String, path: PathBuf, message: String },
    #[error("{id}: sandbox.{path}: {message}")]
    SandboxConflict { id: String, path: String, message: String },
    #[error("{id}: {message}")]
    Invalid { id: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunnerError {
    #[error("{id}: tmux {subcommand}: {}", first_line(stderr))]
    Tool { id: String, subcommand: String, args: Vec<String>, stderr: String },
    #[error("{id}: cannot parse tmux output: {message}")]
    Parse { id: String, message: String },
}

/// Makes files exist (or not) for crews and agents.
pub trait Materializer: Send + Sync {
    fn ensure_crew(&self, crew: &CrewRef, repo: &RepoRef, git_ref: &str, git: &GitSettings, creds: &CredentialBundle) -> Result<(), MaterializeError>;
    fn materialize(&self, agent: &ResolvedAgent, creds: &CredentialBundle, hooks: &HookTarget) -> Result<LaunchPlan, MaterializeError>;
    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError>;
    fn remove_crew(&self, crew: &CrewRef, keep: Keep) -> Result<(), MaterializeError>;
}

/// Makes processes exist (or not) and reports what it sees.
pub trait AgentRunner: Send + Sync {
    fn ensure_crew(&self, crew: &CrewRef) -> Result<(), RunnerError>;
    /// Creates the agent's window, or respawns it if it exists.
    fn ensure_agent(&self, agent: &AgentId, plan: &LaunchPlan) -> Result<(), RunnerError>;
    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError>;
    fn stop_crew(&self, crew: &CrewRef) -> Result<(), RunnerError>;
    fn observe(&self, fleet: &FleetName) -> Result<ObservedState, RunnerError>;
    fn send_text(&self, agent: &AgentId, text: &str, submit: bool) -> Result<(), RunnerError>;
}

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }

    #[test]
    fn observed_state_set_get_remove_and_list() {
        let mut o = ObservedState::default();
        o.set(&id("f/c/b"), ProcessState::Running { pid: 2 });
        o.set(&id("f/c/a"), ProcessState::Exited { code: Some(1) });
        assert_eq!(o.get(&id("f/c/a")), Some(&ProcessState::Exited { code: Some(1) }));
        let ids: Vec<String> = o.agent_ids(&"f".parse().unwrap()).iter().map(ToString::to_string).collect();
        assert_eq!(ids, vec!["f/c/a", "f/c/b"]);
        o.remove(&id("f/c/a"));
        assert_eq!(o.get(&id("f/c/a")), None);
    }

    #[test]
    fn hook_target_debug_redacts_the_secret() {
        let h = HookTarget { url: "https://127.0.0.1:7643".into(), secret: "s3cr3t".into() };
        let d = format!("{h:?}");
        assert!(d.contains("127.0.0.1"));
        assert!(!d.contains("s3cr3t"));
        assert!(d.contains("<redacted>"));
    }

    #[test]
    fn tool_error_shows_the_first_non_empty_stderr_line() {
        let e = MaterializeError::Tool {
            id: "f/c/a".into(),
            tool: "git".into(),
            subcommand: "clone".into(),
            args: vec![],
            stderr: "\n  fatal: repository not found\nmore".into(),
        };
        assert_eq!(e.to_string(), "f/c/a: git clone: fatal: repository not found");
        assert_eq!(first_line(""), "");
    }
}
```

`crates/balerix-core/src/lib.rs`: add `pub mod ports;` and
```rust
pub use ports::{
    AgentRunner, Clock, HookTarget, Keep, LaunchPlan, MaterializeError, Materializer, ObservedState,
    ProcessState, RunnerError, first_line,
};
```

- [ ] **Step 2: Run, lint, commit**

Run: `mise x -- cargo nextest run -p balerix-core ports && mise run lint`
Expected: 3 PASS.
```bash
git add crates/balerix-core
git commit -m "Define the Materializer, AgentRunner and Clock ports in balerix-core

Materialization is its own port (Phase 2 spec P2-4): files and processes
fail differently and are faked differently.

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---

### Task 4: `balerix-core` — fakes

**Files:**
- Create: `crates/balerix-core/src/fakes.rs`
- Modify: `crates/balerix-core/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct FakeMaterializer { .. }  impl Materializer
  pub struct FakeRunner { .. }        impl AgentRunner
  pub struct FakeClock(Mutex<Timestamp>)  impl Clock;  FakeClock::new(t), set(t), advance(secs)
  impl FakeMaterializer / FakeRunner {
      pub fn calls(&self) -> Vec<String>;                 // "materialize f/c/a", "ensure_agent f/c/a", …
      pub fn fail_next(&self, method: &str, id: &str, stderr: &str); // the next call of that method for that id fails
  }
  impl FakeRunner { pub fn set_state(&self, id: &AgentId, s: ProcessState); pub fn observed(&self) -> ObservedState }
  ```
  Semantics: `FakeRunner::ensure_crew` creates an empty crew map; `ensure_agent` sets `Running { pid: n }` with a fresh `n`; `stop_agent` removes the window; `stop_crew` removes the crew; `observe` returns the map for that fleet (the fake holds a single fleet). `FakeMaterializer::materialize` returns a `LaunchPlan` whose paths are `/fake/<id>/…`.

- [ ] **Step 1: Write the file with its tests**

`crates/balerix-core/src/fakes.rs`:
```rust
//! In-memory ports for tests (Phase 2 spec §3.5). Always compiled: they are
//! small, dependency-free, and Phase 3's tests need them too.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use balerix_api::{CredentialBundle, GitSettings, Timestamp};

use crate::agent::{CrewRef, ResolvedAgent};
use crate::name::{AgentId, FleetName};
use crate::ports::{
    AgentRunner, Clock, HookTarget, Keep, LaunchPlan, MaterializeError, Materializer, ObservedState, ProcessState, RunnerError,
};
use crate::repo::RepoRef;

#[derive(Default)]
struct Recorder {
    calls: Vec<String>,
    fail_next: Vec<(String, String, String)>, // (method, id, stderr)
}

impl Recorder {
    /// Records the call; returns the stderr to fail with, if one was armed.
    fn record(&mut self, method: &str, id: &str) -> Option<String> {
        self.calls.push(format!("{method} {id}"));
        let pos = self.fail_next.iter().position(|(m, i, _)| m == method && i == id)?;
        Some(self.fail_next.remove(pos).2)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
pub struct FakeMaterializer {
    rec: Mutex<Recorder>,
}

impl FakeMaterializer {
    pub fn calls(&self) -> Vec<String> {
        lock(&self.rec).calls.clone()
    }
    pub fn fail_next(&self, method: &str, id: &str, stderr: &str) {
        lock(&self.rec).fail_next.push((method.into(), id.into(), stderr.into()));
    }
    fn check(&self, method: &str, id: &str, tool: &str) -> Result<(), MaterializeError> {
        match lock(&self.rec).record(method, id) {
            Some(stderr) => Err(MaterializeError::Tool { id: id.into(), tool: tool.into(), subcommand: method.into(), args: vec![], stderr }),
            None => Ok(()),
        }
    }
}

impl Materializer for FakeMaterializer {
    fn ensure_crew(&self, crew: &CrewRef, _: &RepoRef, _: &str, _: &GitSettings, _: &CredentialBundle) -> Result<(), MaterializeError> {
        self.check("ensure_crew", &crew.to_string(), "git")
    }
    fn materialize(&self, agent: &ResolvedAgent, _: &CredentialBundle, _: &HookTarget) -> Result<LaunchPlan, MaterializeError> {
        let id = agent.id.to_string();
        self.check("materialize", &id, "mise")?;
        let root = PathBuf::from("/fake").join(&id);
        Ok(LaunchPlan { cwd: root.join("workspace"), env: BTreeMap::new(), argv: vec!["fake".into()], script: root.join("launch.sh") })
    }
    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError> {
        self.check("remove_agent", &agent.to_string(), "rm")
    }
    fn remove_crew(&self, crew: &CrewRef, keep: Keep) -> Result<(), MaterializeError> {
        let tag = format!("{crew} repos={} sessions={}", keep.repos, keep.sessions);
        self.check("remove_crew", &tag, "rm").map_err(|e| match e {
            MaterializeError::Tool { tool, subcommand, args, stderr, .. } => MaterializeError::Tool { id: crew.to_string(), tool, subcommand, args, stderr },
            other => other,
        })
    }
}

#[derive(Default)]
pub struct FakeRunner {
    rec: Mutex<Recorder>,
    state: Mutex<ObservedState>,
    next_pid: Mutex<u32>,
}

impl FakeRunner {
    pub fn calls(&self) -> Vec<String> {
        lock(&self.rec).calls.clone()
    }
    pub fn fail_next(&self, method: &str, id: &str, stderr: &str) {
        lock(&self.rec).fail_next.push((method.into(), id.into(), stderr.into()));
    }
    pub fn set_state(&self, id: &AgentId, s: ProcessState) {
        lock(&self.state).set(id, s);
    }
    pub fn observed(&self) -> ObservedState {
        lock(&self.state).clone()
    }
    fn check(&self, method: &str, id: &str) -> Result<(), RunnerError> {
        match lock(&self.rec).record(method, id) {
            Some(stderr) => Err(RunnerError::Tool { id: id.into(), subcommand: method.into(), args: vec![], stderr }),
            None => Ok(()),
        }
    }
}

impl AgentRunner for FakeRunner {
    fn ensure_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        self.check("ensure_crew", &crew.to_string())?;
        lock(&self.state).crews.entry(crew.crew.clone()).or_default();
        Ok(())
    }
    fn ensure_agent(&self, agent: &AgentId, _: &LaunchPlan) -> Result<(), RunnerError> {
        self.check("ensure_agent", &agent.to_string())?;
        let pid = {
            let mut p = lock(&self.next_pid);
            *p += 1;
            *p
        };
        lock(&self.state).set(agent, ProcessState::Running { pid });
        Ok(())
    }
    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError> {
        self.check("stop_agent", &agent.to_string())?;
        lock(&self.state).remove(agent);
        Ok(())
    }
    fn stop_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        self.check("stop_crew", &crew.to_string())?;
        lock(&self.state).crews.remove(&crew.crew);
        Ok(())
    }
    fn observe(&self, fleet: &FleetName) -> Result<ObservedState, RunnerError> {
        self.check("observe", fleet.as_str())?;
        Ok(self.observed())
    }
    fn send_text(&self, agent: &AgentId, text: &str, submit: bool) -> Result<(), RunnerError> {
        self.check("send_text", &format!("{agent} {text:?} submit={submit}"))
    }
}

pub struct FakeClock(Mutex<Timestamp>);

impl FakeClock {
    pub fn new(t: Timestamp) -> Self {
        Self(Mutex::new(t))
    }
    pub fn set(&self, t: Timestamp) {
        *lock(&self.0) = t;
    }
    pub fn advance(&self, secs: u64) {
        let mut t = lock(&self.0);
        *t = t.plus_secs(secs);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        *lock(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }

    #[test]
    fn runner_tracks_windows_and_records_calls() {
        let r = FakeRunner::default();
        let crew: CrewRef = "f/c".parse().unwrap();
        r.ensure_crew(&crew).unwrap();
        r.ensure_agent(&id("f/c/a"), &LaunchPlan { cwd: "/x".into(), env: BTreeMap::new(), argv: vec![], script: "/x/l".into() }).unwrap();
        assert_eq!(r.observe(&"f".parse().unwrap()).unwrap().get(&id("f/c/a")), Some(&ProcessState::Running { pid: 1 }));
        r.stop_agent(&id("f/c/a")).unwrap();
        assert_eq!(r.observed().get(&id("f/c/a")), None);
        assert_eq!(r.calls(), vec!["ensure_crew f/c", "ensure_agent f/c/a", "observe f", "stop_agent f/c/a"]);
    }

    #[test]
    fn fail_next_fails_exactly_once_for_the_named_call() {
        let m = FakeMaterializer::default();
        m.fail_next("remove_agent", "f/c/a", "boom");
        let e = m.remove_agent(&id("f/c/a")).unwrap_err();
        assert_eq!(e.to_string(), "f/c/a: rm remove_agent: boom");
        assert!(m.remove_agent(&id("f/c/a")).is_ok());
        assert!(m.remove_agent(&id("f/c/b")).is_ok());
    }

    #[test]
    fn clock_advances() {
        let c = FakeClock::new(Timestamp(100));
        c.advance(5);
        assert_eq!(c.now(), Timestamp(105));
        c.set(Timestamp(1));
        assert_eq!(c.now(), Timestamp(1));
    }
}
```

`crates/balerix-core/src/lib.rs`: add `pub mod fakes;`.

- [ ] **Step 2: Run, lint, commit**

Run: `mise x -- cargo nextest run -p balerix-core fakes && mise run lint`
```bash
git add crates/balerix-core
git commit -m "Add in-memory fakes for the ports

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---
### Task 5: `balerix-core` — `reconcile::plan`

**Files:**
- Create: `crates/balerix-core/src/reconcile/mod.rs`
- Modify: `crates/balerix-core/src/lib.rs`

**Interfaces:**
- Consumes: `Fleet`, `ResolvedAgent::{from_fleet, hash}`, `FleetStatus`, `AgentPhase`, `ObservedState`, `ProcessState`, `Keep`, `Timestamp`, `SpecHash`.
- Produces:
  ```rust
  pub struct ReconcilePolicy { pub max_restarts: u32, pub backoff_base_secs: u64, pub backoff_cap_secs: u64 }  // Default 5, 2, 300
  pub enum Step { Stop(AgentId), RemoveAgent(AgentId), RemoveCrew(CrewRef, Keep), EnsureCrew(CrewRef),
                  Materialize(AgentId), Start(AgentId, SpecHash), NoteExit(AgentId, Option<i32>) }
  impl Step { pub fn agent_id(&self) -> Option<&AgentId> }   // Display: "stop f/c/a", "start f/c/a <first 8 hex>", …
  pub type Plan = Vec<Step>;
  pub fn backoff_secs(policy: &ReconcilePolicy, restarts: u32) -> u64;   // min(base·2^(restarts−1), cap); 0 for restarts == 0
  pub fn plan(fleet: &FleetName, desired: Option<&Fleet>, keep: Keep, status: &FleetStatus,
              observed: &ObservedState, policy: &ReconcilePolicy, now: Timestamp) -> Plan;
  ```
  Decision table (Phase 2 spec §3.2, with `MarkDead` folded into `NoteExit`): see the doc comment on `plan` below — it is the normative statement.

- [ ] **Step 1: Write the failing tests**

`crates/balerix-core/src/reconcile/mod.rs` — write the whole module skeleton with the types and a `todo!()`-free stub so tests compile, then the tests:

```rust
//! The reconciler (Phase 2 spec §3): `plan` decides, `execute` acts,
//! `apply` folds each outcome into the status. Nothing here does I/O.

mod execute;
mod status;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use balerix_api::{AgentPhase, FleetStatus, SpecHash, Timestamp};

use crate::agent::{CrewRef, ResolvedAgent};
use crate::fleet::Fleet;
use crate::name::{AgentId, FleetName};
use crate::ports::{Keep, ObservedState, ProcessState};

pub use execute::{ExecuteReport, ReconcileContext, execute, reconcile_pass};
pub use status::{StepOutcome, agent_ready, apply, finish_pass, set_desired};

/// Restart limits (spec §7 "bounded exponential backoff").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcilePolicy {
    pub max_restarts: u32,
    pub backoff_base_secs: u64,
    pub backoff_cap_secs: u64,
}

impl Default for ReconcilePolicy {
    fn default() -> Self {
        Self { max_restarts: 5, backoff_base_secs: 2, backoff_cap_secs: 300 }
    }
}

/// One thing the executor does. Steps carry ids, never data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Stop(AgentId),
    RemoveAgent(AgentId),
    RemoveCrew(CrewRef, Keep),
    EnsureCrew(CrewRef),
    Materialize(AgentId),
    Start(AgentId, SpecHash),
    NoteExit(AgentId, Option<i32>),
}

impl Step {
    pub fn agent_id(&self) -> Option<&AgentId> {
        match self {
            Step::Stop(id) | Step::RemoveAgent(id) | Step::Materialize(id) | Step::Start(id, _) | Step::NoteExit(id, _) => Some(id),
            Step::RemoveCrew(..) | Step::EnsureCrew(_) => None,
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Stop(id) => write!(f, "stop {id}"),
            Step::RemoveAgent(id) => write!(f, "remove-agent {id}"),
            Step::RemoveCrew(c, k) => write!(f, "remove-crew {c} keep-repos={} keep-sessions={}", k.repos, k.sessions),
            Step::EnsureCrew(c) => write!(f, "ensure-crew {c}"),
            Step::Materialize(id) => write!(f, "materialize {id}"),
            Step::Start(id, h) => write!(f, "start {id} {}", &h.as_str()[..8.min(h.as_str().len())]),
            Step::NoteExit(id, code) => write!(f, "note-exit {id} {code:?}"),
        }
    }
}

pub type Plan = Vec<Step>;

/// Seconds to wait before restart number `restarts` (1-based).
pub fn backoff_secs(policy: &ReconcilePolicy, restarts: u32) -> u64 {
    if restarts == 0 {
        return 0;
    }
    let factor = 1u64.checked_shl(restarts - 1).unwrap_or(u64::MAX);
    policy.backoff_base_secs.saturating_mul(factor).min(policy.backoff_cap_secs)
}

/// Decides what to do. Pure. Ordering: stops, agent removals, crew removals,
/// crew ensures, then per agent (sorted by id) either `Materialize`+`Start`
/// or `NoteExit`.
///
/// Per desired agent, with `changed = status.applied_hash != desired hash`:
///
/// | observed | condition | steps |
/// |---|---|---|
/// | `Running` | `!changed` | — |
/// | `Running` | `changed` | `Stop`, `Materialize`, `Start` |
/// | `Exited` | `changed` | `Materialize`, `Start` |
/// | `Exited` | phase `Dead` | — |
/// | `Exited` | `next_restart_at.is_none()` (exit not yet noted) | `NoteExit` |
/// | `Exited` | `next_restart_at <= now` | `Materialize`, `Start` |
/// | `Exited` | otherwise (backoff running) | — |
/// | absent | phase `Dead` and `!changed` | — |
/// | absent | otherwise | `Materialize`, `Start` |
///
/// Known-but-not-desired agents get `Stop` (if observed) and `RemoveAgent`;
/// their crews, if no longer desired, `RemoveCrew`. With `desired == None`
/// (down) every observed agent is stopped and every known crew removed with
/// `keep`; nothing is ensured.
pub fn plan(
    fleet: &FleetName,
    desired: Option<&Fleet>,
    keep: Keep,
    status: &FleetStatus,
    observed: &ObservedState,
    policy: &ReconcilePolicy,
    now: Timestamp,
) -> Plan {
    let _ = (fleet, desired, keep, status, observed, policy, now);
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentSettings, AgentStatus, CrewSpec, FleetSpec, GitSettings};
    use std::collections::BTreeMap;

    fn fleet_name() -> FleetName {
        "f".parse().unwrap()
    }

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }

    /// A fleet "f" with crew "c" and the given agents; `env.V` carries a
    /// per-agent version so the hash can be changed on purpose.
    fn fleet(agents: &[(&str, u32)]) -> Fleet {
        Fleet::try_from(FleetSpec {
            name: "f".into(),
            crews: BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "acme/api".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: agents
                        .iter()
                        .map(|(n, v)| {
                            let mut s = AgentSettings::default();
                            s.env.insert("V".into(), v.to_string());
                            (n.to_string(), s)
                        })
                        .collect(),
                },
            )]),
        })
        .unwrap()
    }

    fn hash_of(f: &Fleet, agent: &str) -> SpecHash {
        ResolvedAgent::from_fleet(f).into_iter().find(|a| a.id.agent.as_str() == agent).unwrap().hash()
    }

    fn render(p: &Plan) -> Vec<String> {
        p.iter().map(ToString::to_string).collect()
    }

    fn short(h: &SpecHash) -> String {
        h.as_str()[..8].to_string()
    }

    fn plan_for(desired: Option<&Fleet>, status: &FleetStatus, observed: &ObservedState, now: u64) -> Vec<String> {
        render(&plan(&fleet_name(), desired, Keep::default(), status, observed, &ReconcilePolicy::default(), Timestamp(now)))
    }

    #[test]
    fn backoff_doubles_from_base_and_caps() {
        let p = ReconcilePolicy::default();
        assert_eq!(backoff_secs(&p, 0), 0);
        assert_eq!(backoff_secs(&p, 1), 2);
        assert_eq!(backoff_secs(&p, 2), 4);
        assert_eq!(backoff_secs(&p, 8), 256);
        assert_eq!(backoff_secs(&p, 9), 300);
        assert_eq!(backoff_secs(&p, 200), 300, "no shift overflow");
    }

    #[test]
    fn fresh_fleet_ensures_crew_then_materializes_and_starts_each_agent() {
        let f = fleet(&[("b", 1), ("a", 1)]);
        let got = plan_for(Some(&f), &FleetStatus::default(), &ObservedState::default(), 0);
        assert_eq!(
            got,
            vec![
                "ensure-crew f/c".to_string(),
                format!("materialize f/c/a"),
                format!("start f/c/a {}", short(&hash_of(&f, "a"))),
                format!("materialize f/c/b"),
                format!("start f/c/b {}", short(&hash_of(&f, "b"))),
            ]
        );
    }

    #[test]
    fn ready_and_unchanged_fleet_plans_only_ensure_crew() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        st.entry("f/c/a").applied_hash = Some(hash_of(&f, "a"));
        st.entry("f/c/a").phase = AgentPhase::Ready;
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Running { pid: 1 });
        assert_eq!(plan_for(Some(&f), &st, &obs, 0), vec!["ensure-crew f/c"]);
    }

    #[test]
    fn changed_hash_on_a_running_agent_stops_then_restarts_it() {
        let old = fleet(&[("a", 1)]);
        let new = fleet(&[("a", 2)]);
        let mut st = FleetStatus::default();
        st.entry("f/c/a").applied_hash = Some(hash_of(&old, "a"));
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Running { pid: 1 });
        assert_eq!(
            plan_for(Some(&new), &st, &obs, 0),
            vec!["stop f/c/a".to_string(), "ensure-crew f/c".into(), "materialize f/c/a".into(), format!("start f/c/a {}", short(&hash_of(&new, "a")))]
        );
    }

    #[test]
    fn undesired_agents_and_crews_are_stopped_and_removed() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        st.entry("f/old/z").phase = AgentPhase::Ready;
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/gone"), ProcessState::Running { pid: 1 });
        let got = plan_for(Some(&f), &st, &obs, 0);
        assert_eq!(got[0], "stop f/c/gone");
        assert_eq!(got[1], "remove-agent f/c/gone");
        assert_eq!(got[2], "remove-agent f/old/z");
        assert_eq!(got[3], "remove-crew f/old keep-repos=false keep-sessions=false");
        assert_eq!(got[4], "ensure-crew f/c");
        assert!(got[5].starts_with("materialize f/c/a"));
    }

    #[test]
    fn down_stops_everything_and_removes_crews_with_keep() {
        let mut st = FleetStatus::default();
        st.entry("f/c/a").phase = AgentPhase::Ready;
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Running { pid: 1 });
        obs.set(&id("f/d/b"), ProcessState::Exited { code: Some(0) });
        let got = render(&plan(&fleet_name(), None, Keep { repos: true, sessions: false }, &st, &obs, &ReconcilePolicy::default(), Timestamp(0)));
        assert_eq!(
            got,
            vec![
                "stop f/c/a",
                "stop f/d/b",
                "remove-crew f/c keep-repos=true keep-sessions=false",
                "remove-crew f/d keep-repos=true keep-sessions=false",
            ]
        );
    }

    #[test]
    fn an_unnoted_exit_is_noted_not_restarted() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        st.entry("f/c/a").applied_hash = Some(hash_of(&f, "a"));
        st.entry("f/c/a").phase = AgentPhase::Ready;
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Exited { code: Some(137) });
        assert_eq!(plan_for(Some(&f), &st, &obs, 0), vec!["ensure-crew f/c", "note-exit f/c/a Some(137)"]);
    }

    #[test]
    fn a_noted_exit_restarts_only_once_due() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        *st.entry("f/c/a") = AgentStatus {
            phase: AgentPhase::Ready,
            applied_hash: Some(hash_of(&f, "a")),
            restarts: 1,
            next_restart_at: Some(Timestamp(10)),
            ..AgentStatus::default()
        };
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Exited { code: None });
        assert_eq!(plan_for(Some(&f), &st, &obs, 9), vec!["ensure-crew f/c"]);
        let due = plan_for(Some(&f), &st, &obs, 10);
        assert_eq!(due.len(), 3);
        assert_eq!(due[1], "materialize f/c/a");
    }

    #[test]
    fn dead_agents_are_left_alone_until_the_hash_changes() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        *st.entry("f/c/a") = AgentStatus { phase: AgentPhase::Dead, applied_hash: Some(hash_of(&f, "a")), restarts: 6, ..AgentStatus::default() };
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Exited { code: Some(1) });
        assert_eq!(plan_for(Some(&f), &st, &obs, 0), vec!["ensure-crew f/c"]);
        // window vanished entirely: still left alone
        assert_eq!(plan_for(Some(&f), &st, &ObservedState::default(), 0), vec!["ensure-crew f/c"]);
        // new spec: restart
        let f2 = fleet(&[("a", 2)]);
        assert_eq!(plan_for(Some(&f2), &st, &obs, 0).len(), 3);
    }

    #[test]
    fn a_vanished_window_is_recreated() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        *st.entry("f/c/a") = AgentStatus { phase: AgentPhase::Ready, applied_hash: Some(hash_of(&f, "a")), ..AgentStatus::default() };
        let got = plan_for(Some(&f), &st, &ObservedState::default(), 0);
        assert_eq!(got.len(), 3);
        assert_eq!(got[1], "materialize f/c/a");
    }
}
```

`crates/balerix-core/src/lib.rs`: add `pub mod reconcile;` and `pub use reconcile::{Plan, ReconcilePolicy, Step};`. Create empty placeholders `crates/balerix-core/src/reconcile/status.rs` and `execute.rs` containing only the items the `pub use` lines need as stubs — simplest: comment out the two `pub use` lines and the `mod` lines until Tasks 6 and 7 add them, and uncomment then.

- [ ] **Step 2: Run to verify failure**

Run: `mise x -- cargo nextest run -p balerix-core reconcile`
Expected: `backoff_doubles_from_base_and_caps` passes; every `plan` test FAILS (empty plan).

- [ ] **Step 3: Implement `plan`**

Replace the stub body:
```rust
    let desired_agents: BTreeMap<AgentId, ResolvedAgent> = desired
        .map(|f| ResolvedAgent::from_fleet(f).into_iter().map(|a| (a.id.clone(), a)).collect())
        .unwrap_or_default();
    let desired_crews: BTreeSet<CrewRef> = desired_agents.keys().map(AgentId::crew_ref).collect();

    let mut known_agents: BTreeSet<AgentId> = status.agents.keys().filter_map(|k| k.parse().ok()).collect();
    known_agents.extend(observed.agent_ids(fleet));
    let mut known_crews: BTreeSet<CrewRef> = known_agents.iter().map(AgentId::crew_ref).collect();
    known_crews.extend(observed.crews.keys().map(|c| CrewRef { fleet: fleet.clone(), crew: c.clone() }));

    let mut stops = Vec::new();
    let mut remove_agents = Vec::new();
    let mut remove_crews = Vec::new();
    let mut ensures = Vec::new();
    let mut agent_steps = Vec::new();

    for id in &known_agents {
        if desired_agents.contains_key(id) {
            continue;
        }
        if observed.get(id).is_some() {
            stops.push(Step::Stop(id.clone()));
        }
        if desired.is_some() {
            remove_agents.push(Step::RemoveAgent(id.clone()));
        }
    }
    for crew in &known_crews {
        if !desired_crews.contains(crew) {
            let k = if desired.is_some() { Keep::default() } else { keep };
            remove_crews.push(Step::RemoveCrew(crew.clone(), k));
        }
    }
    for crew in &desired_crews {
        ensures.push(Step::EnsureCrew(crew.clone()));
    }

    for (id, agent) in &desired_agents {
        let hash = agent.hash();
        let st = status.agents.get(&id.to_string());
        let changed = st.and_then(|s| s.applied_hash.as_ref()) != Some(&hash);
        let dead = st.is_some_and(|s| s.phase == AgentPhase::Dead);
        let restart = |steps: &mut Vec<Step>| {
            steps.push(Step::Materialize(id.clone()));
            steps.push(Step::Start(id.clone(), hash.clone()));
        };
        match observed.get(id) {
            Some(ProcessState::Running { .. }) if !changed => {}
            Some(ProcessState::Running { .. }) => {
                stops.push(Step::Stop(id.clone()));
                restart(&mut agent_steps);
            }
            Some(ProcessState::Exited { code }) => {
                if changed {
                    restart(&mut agent_steps);
                } else if dead {
                } else {
                    match st.and_then(|s| s.next_restart_at) {
                        None => agent_steps.push(Step::NoteExit(id.clone(), *code)),
                        Some(due) if due <= now => restart(&mut agent_steps),
                        Some(_) => {}
                    }
                }
            }
            None => {
                if !(dead && !changed) {
                    restart(&mut agent_steps);
                }
            }
        }
    }

    let mut out = stops;
    out.extend(remove_agents);
    out.extend(remove_crews);
    out.extend(ensures);
    out.extend(agent_steps);
    out
```
`stops` is pushed in agent-id order for undesired agents first, then desired-but-changed agents; the test `changed_hash_on_a_running_agent_stops_then_restarts_it` has only one stop so order within stops is not observable there. Keep it deterministic anyway: after the loops, `stops.sort_by_key(|s| s.agent_id().cloned())` before assembling.

- [ ] **Step 4: Run to verify pass, lint, commit**

Run: `mise x -- cargo nextest run -p balerix-core reconcile && mise run lint`
Expected: all 10 PASS.
```bash
git add crates/balerix-core
git commit -m "Add the reconciler planner

plan() is a total function from desired, status and observed state to
an ordered step list; every branch of the decision table has a test.

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---

### Task 6: `balerix-core` — `apply`, `agent_ready`, `set_desired`, `finish_pass`

**Files:**
- Create: `crates/balerix-core/src/reconcile/status.rs`
- Modify: `crates/balerix-core/src/reconcile/mod.rs` (uncomment `mod status;` and its `pub use`)

**Interfaces:**
- Produces:
  ```rust
  pub type StepOutcome = Result<(), String>;
  pub fn apply(status: &mut FleetStatus, step: &Step, outcome: &StepOutcome, policy: &ReconcilePolicy, now: Timestamp);
  pub fn agent_ready(status: &mut FleetStatus, agent: &AgentId, now: Timestamp);   // ignored for unknown agents
  pub fn set_desired(status: &mut FleetStatus, generation: u64);
  pub fn finish_pass(status: &mut FleetStatus, terminating: bool, all_ok: bool);   // derives fleet phase; bumps observed_generation when all_ok
  ```

- [ ] **Step 1: Write the file with its tests**

`crates/balerix-core/src/reconcile/status.rs`:
```rust
//! Folding step outcomes and external signals into `FleetStatus`
//! (Phase 2 spec §3.4). Pure.

use balerix_api::{AgentPhase, FleetPhase, FleetStatus, Timestamp};

use crate::name::AgentId;
use crate::reconcile::{ReconcilePolicy, Step, backoff_secs};

pub type StepOutcome = Result<(), String>;

pub fn apply(status: &mut FleetStatus, step: &Step, outcome: &StepOutcome, policy: &ReconcilePolicy, now: Timestamp) {
    match (step, outcome) {
        (Step::Stop(id), Ok(())) => {
            if let Some(a) = status.agents.get_mut(&id.to_string()) {
                a.phase = AgentPhase::Stopped;
            }
        }
        (Step::RemoveAgent(id), Ok(())) => {
            status.agents.remove(&id.to_string());
        }
        (Step::RemoveCrew(crew, _), Ok(())) => {
            let prefix = format!("{crew}/");
            status.agents.retain(|k, _| !k.starts_with(&prefix));
        }
        (Step::RemoveCrew(crew, _), Err(e)) => {
            let prefix = format!("{crew}/");
            for (_, a) in status.agents.iter_mut().filter(|(k, _)| k.starts_with(&prefix)) {
                a.message.clone_from(e);
            }
        }
        (Step::EnsureCrew(_), _) => {}
        (Step::Materialize(id), Ok(())) => {
            let a = status.entry(&id.to_string());
            a.phase = AgentPhase::Materializing;
            a.message.clear();
        }
        (Step::Start(id, hash), Ok(())) => {
            let a = status.entry(&id.to_string());
            if a.applied_hash.as_ref() != Some(hash) {
                a.restarts = 0;
            }
            a.phase = AgentPhase::Starting;
            a.applied_hash = Some(hash.clone());
            a.next_restart_at = None;
            a.message.clear();
        }
        (Step::NoteExit(id, code), Ok(())) => {
            let a = status.entry(&id.to_string());
            a.restarts += 1;
            a.message = match code {
                Some(c) => format!("exited with status {c}"),
                None => "exited (killed by signal)".to_string(),
            };
            if a.restarts > policy.max_restarts {
                a.phase = AgentPhase::Dead;
                a.next_restart_at = None;
                a.message.push_str(&format!("; gave up after {} restarts", policy.max_restarts));
            } else {
                a.next_restart_at = Some(now.plus_secs(backoff_secs(policy, a.restarts)));
            }
        }
        (Step::Stop(id) | Step::RemoveAgent(id) | Step::Materialize(id) | Step::Start(id, _) | Step::NoteExit(id, _), Err(e)) => {
            status.entry(&id.to_string()).message.clone_from(e);
        }
    }
}

/// `SessionStart` reached the daemon: the agent is up and accepting input.
pub fn agent_ready(status: &mut FleetStatus, agent: &AgentId, now: Timestamp) {
    let Some(a) = status.agents.get_mut(&agent.to_string()) else {
        return;
    };
    a.phase = AgentPhase::Ready;
    a.restarts = 0;
    a.next_restart_at = None;
    a.last_event_at = Some(now);
    a.message.clear();
    let terminating = status.phase == FleetPhase::Terminating;
    status.phase = derive_fleet_phase(status, terminating, true);
}

pub fn set_desired(status: &mut FleetStatus, generation: u64) {
    status.generation = generation;
}

pub fn finish_pass(status: &mut FleetStatus, terminating: bool, all_ok: bool) {
    status.phase = derive_fleet_phase(status, terminating, all_ok);
    if all_ok {
        status.observed_generation = status.generation;
    }
}

fn derive_fleet_phase(status: &FleetStatus, terminating: bool, all_ok: bool) -> FleetPhase {
    if terminating {
        return FleetPhase::Terminating;
    }
    let phases = || status.agents.values().map(|a| a.phase);
    if !all_ok || phases().any(|p| p == AgentPhase::Dead) {
        FleetPhase::Degraded
    } else if phases().any(|p| matches!(p, AgentPhase::Pending | AgentPhase::Materializing | AgentPhase::Starting)) {
        FleetPhase::Reconciling
    } else if !status.agents.is_empty() && phases().all(|p| p == AgentPhase::Ready) {
        FleetPhase::Ready
    } else if status.agents.is_empty() {
        FleetPhase::Pending
    } else {
        FleetPhase::Reconciling
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::SpecHash;

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }
    fn h(s: &str) -> SpecHash {
        SpecHash::new(s.to_string())
    }
    fn policy() -> ReconcilePolicy {
        ReconcilePolicy { max_restarts: 2, backoff_base_secs: 2, backoff_cap_secs: 300 }
    }

    #[test]
    fn materialize_then_start_moves_through_phases_and_records_the_hash() {
        let mut s = FleetStatus::default();
        apply(&mut s, &Step::Materialize(id("f/c/a")), &Ok(()), &policy(), Timestamp(0));
        assert_eq!(s.agents["f/c/a"].phase, AgentPhase::Materializing);
        apply(&mut s, &Step::Start(id("f/c/a"), h("aaaa")), &Ok(()), &policy(), Timestamp(0));
        let a = &s.agents["f/c/a"];
        assert_eq!(a.phase, AgentPhase::Starting);
        assert_eq!(a.applied_hash, Some(h("aaaa")));
        assert_eq!(a.next_restart_at, None);
    }

    #[test]
    fn failures_set_the_message_and_keep_the_phase() {
        let mut s = FleetStatus::default();
        apply(&mut s, &Step::Materialize(id("f/c/a")), &Err("f/c/a: git worktree: boom".into()), &policy(), Timestamp(0));
        assert_eq!(s.agents["f/c/a"].phase, AgentPhase::Pending);
        assert_eq!(s.agents["f/c/a"].message, "f/c/a: git worktree: boom");
        apply(&mut s, &Step::Materialize(id("f/c/a")), &Ok(()), &policy(), Timestamp(0));
        assert_eq!(s.agents["f/c/a"].message, "", "success clears the message");
    }

    #[test]
    fn exits_back_off_then_give_up() {
        let mut s = FleetStatus::default();
        apply(&mut s, &Step::Start(id("f/c/a"), h("x")), &Ok(()), &policy(), Timestamp(0));
        apply(&mut s, &Step::NoteExit(id("f/c/a"), Some(1)), &Ok(()), &policy(), Timestamp(100));
        let a = &s.agents["f/c/a"];
        assert_eq!((a.restarts, a.next_restart_at, a.phase), (1, Some(Timestamp(102)), AgentPhase::Starting));
        assert_eq!(a.message, "exited with status 1");
        apply(&mut s, &Step::Start(id("f/c/a"), h("x")), &Ok(()), &policy(), Timestamp(102));
        assert_eq!(s.agents["f/c/a"].restarts, 1, "same hash keeps the count");
        apply(&mut s, &Step::NoteExit(id("f/c/a"), None), &Ok(()), &policy(), Timestamp(200));
        assert_eq!(s.agents["f/c/a"].next_restart_at, Some(Timestamp(204)));
        apply(&mut s, &Step::Start(id("f/c/a"), h("x")), &Ok(()), &policy(), Timestamp(204));
        apply(&mut s, &Step::NoteExit(id("f/c/a"), Some(2)), &Ok(()), &policy(), Timestamp(300));
        let a = &s.agents["f/c/a"];
        assert_eq!(a.phase, AgentPhase::Dead);
        assert_eq!(a.next_restart_at, None);
        assert_eq!(a.message, "exited with status 2; gave up after 2 restarts");
    }

    #[test]
    fn a_new_hash_resets_the_restart_count() {
        let mut s = FleetStatus::default();
        apply(&mut s, &Step::Start(id("f/c/a"), h("x")), &Ok(()), &policy(), Timestamp(0));
        apply(&mut s, &Step::NoteExit(id("f/c/a"), Some(1)), &Ok(()), &policy(), Timestamp(1));
        apply(&mut s, &Step::Start(id("f/c/a"), h("y")), &Ok(()), &policy(), Timestamp(3));
        assert_eq!(s.agents["f/c/a"].restarts, 0);
    }

    #[test]
    fn ready_signal_resets_restarts_and_stamps_the_event() {
        let mut s = FleetStatus::default();
        apply(&mut s, &Step::Start(id("f/c/a"), h("x")), &Ok(()), &policy(), Timestamp(0));
        s.agents.get_mut("f/c/a").unwrap().restarts = 2;
        agent_ready(&mut s, &id("f/c/a"), Timestamp(7));
        let a = &s.agents["f/c/a"];
        assert_eq!((a.phase, a.restarts, a.last_event_at), (AgentPhase::Ready, 0, Some(Timestamp(7))));
        assert_eq!(s.phase, FleetPhase::Ready);
        agent_ready(&mut s, &id("f/c/unknown"), Timestamp(8));
        assert!(!s.agents.contains_key("f/c/unknown"));
    }

    #[test]
    fn removals_drop_entries() {
        let mut s = FleetStatus::default();
        s.entry("f/c/a");
        s.entry("f/c/b");
        s.entry("f/d/x");
        apply(&mut s, &Step::Stop(id("f/c/a")), &Ok(()), &policy(), Timestamp(0));
        assert_eq!(s.agents["f/c/a"].phase, AgentPhase::Stopped);
        apply(&mut s, &Step::RemoveAgent(id("f/c/a")), &Ok(()), &policy(), Timestamp(0));
        assert!(!s.agents.contains_key("f/c/a"));
        apply(&mut s, &Step::RemoveCrew("f/c".parse().unwrap(), Default::default()), &Ok(()), &policy(), Timestamp(0));
        assert_eq!(s.agents.keys().collect::<Vec<_>>(), vec!["f/d/x"]);
    }

    #[test]
    fn fleet_phase_is_derived_and_observed_generation_follows_success() {
        let mut s = FleetStatus::default();
        set_desired(&mut s, 3);
        finish_pass(&mut s, false, true);
        assert_eq!((s.phase, s.observed_generation), (FleetPhase::Pending, 3));
        s.entry("f/c/a").phase = AgentPhase::Starting;
        set_desired(&mut s, 4);
        finish_pass(&mut s, false, false);
        assert_eq!((s.phase, s.observed_generation), (FleetPhase::Degraded, 3));
        finish_pass(&mut s, false, true);
        assert_eq!((s.phase, s.observed_generation), (FleetPhase::Reconciling, 4));
        s.entry("f/c/a").phase = AgentPhase::Ready;
        finish_pass(&mut s, false, true);
        assert_eq!(s.phase, FleetPhase::Ready);
        s.entry("f/c/b").phase = AgentPhase::Dead;
        finish_pass(&mut s, false, true);
        assert_eq!(s.phase, FleetPhase::Degraded);
        finish_pass(&mut s, true, true);
        assert_eq!(s.phase, FleetPhase::Terminating);
    }
}
```

- [ ] **Step 2: Run, lint, commit**

Run: `mise x -- cargo nextest run -p balerix-core reconcile::status && mise run lint`
Expected: 7 PASS.
```bash
git add crates/balerix-core
git commit -m "Fold step outcomes and Ready signals into FleetStatus

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---
### Task 7: `balerix-core` — `execute`, `reconcile_pass`, plan golden snapshots

**Files:**
- Create: `crates/balerix-core/src/reconcile/execute.rs`, `crates/balerix-core/tests/plan_golden.rs`
- Modify: `crates/balerix-core/src/reconcile/mod.rs` (uncomment `mod execute;` and its `pub use`), `crates/balerix-core/Cargo.toml` (dev-dep `insta`)

**Interfaces:**
- Produces:
  ```rust
  pub struct ReconcileContext<'a> {
      pub fleet: &'a FleetName, pub desired: Option<&'a Fleet>, pub keep: Keep,
      pub materializer: &'a dyn Materializer, pub runner: &'a dyn AgentRunner,
      pub creds: &'a CredentialBundle, pub hooks: &'a dyn Fn(&AgentId) -> HookTarget,
      pub policy: &'a ReconcilePolicy, pub clock: &'a dyn Clock,
  }
  pub struct ExecuteReport { pub failures: Vec<(Step, String)>, pub skipped: Vec<Step> }
  impl ExecuteReport { pub fn all_ok(&self) -> bool }
  pub fn execute(plan: &Plan, status: &mut FleetStatus, ctx: &ReconcileContext) -> ExecuteReport;  // calls finish_pass at the end
  pub fn reconcile_pass(status: &mut FleetStatus, ctx: &ReconcileContext) -> Result<(Plan, ExecuteReport), RunnerError>; // observe → plan → execute
  ```

- [ ] **Step 1: Write the file with its tests**

`crates/balerix-core/src/reconcile/execute.rs`:
```rust
//! Walks a `Plan` through the ports (Phase 2 spec §3.3). Dumb on purpose:
//! every decision was made by `plan`, every status change is `apply`.

use std::collections::BTreeMap;

use balerix_api::{CredentialBundle, FleetStatus};

use crate::agent::{CrewRef, ResolvedAgent};
use crate::fleet::Fleet;
use crate::name::{AgentId, FleetName};
use crate::ports::{AgentRunner, Clock, HookTarget, Keep, LaunchPlan, Materializer, RunnerError};
use crate::reconcile::{Plan, ReconcilePolicy, Step, apply, finish_pass, plan};

pub struct ReconcileContext<'a> {
    pub fleet: &'a FleetName,
    pub desired: Option<&'a Fleet>,
    pub keep: Keep,
    pub materializer: &'a dyn Materializer,
    pub runner: &'a dyn AgentRunner,
    pub creds: &'a CredentialBundle,
    pub hooks: &'a dyn Fn(&AgentId) -> HookTarget,
    pub policy: &'a ReconcilePolicy,
    pub clock: &'a dyn Clock,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExecuteReport {
    pub failures: Vec<(Step, String)>,
    pub skipped: Vec<Step>,
}

impl ExecuteReport {
    pub fn all_ok(&self) -> bool {
        self.failures.is_empty() && self.skipped.is_empty()
    }
}

/// Runs every step in order. A failed step for an agent skips that agent's
/// later steps; a failed `EnsureCrew` skips the crew's agents and writes the
/// crew error into each of their messages. Ends with `finish_pass`.
pub fn execute(plan: &Plan, status: &mut FleetStatus, ctx: &ReconcileContext) -> ExecuteReport {
    let agents: BTreeMap<AgentId, ResolvedAgent> = ctx
        .desired
        .map(|f| ResolvedAgent::from_fleet(f).into_iter().map(|a| (a.id.clone(), a)).collect())
        .unwrap_or_default();
    let mut report = ExecuteReport::default();
    let mut failed_agents: Vec<AgentId> = Vec::new();
    let mut failed_crews: BTreeMap<CrewRef, String> = BTreeMap::new();
    let mut plans: BTreeMap<AgentId, LaunchPlan> = BTreeMap::new();

    for step in plan {
        if let Some(id) = step.agent_id() {
            if failed_agents.contains(id) {
                report.skipped.push(step.clone());
                continue;
            }
            if let Some(err) = failed_crews.get(&id.crew_ref()) {
                status.entry(&id.to_string()).message.clone_from(err);
                report.skipped.push(step.clone());
                continue;
            }
        }
        let now = ctx.clock.now();
        let outcome: Result<(), String> = match step {
            Step::Stop(id) => ctx.runner.stop_agent(id).map_err(|e| e.to_string()),
            Step::RemoveAgent(id) => ctx.materializer.remove_agent(id).map_err(|e| e.to_string()),
            Step::RemoveCrew(crew, keep) => ctx
                .runner
                .stop_crew(crew)
                .map_err(|e| e.to_string())
                .and_then(|()| ctx.materializer.remove_crew(crew, *keep).map_err(|e| e.to_string())),
            Step::EnsureCrew(crew) => ensure_crew(ctx, crew),
            Step::Materialize(id) => match agents.get(id) {
                Some(agent) => ctx
                    .materializer
                    .materialize(agent, ctx.creds, &(ctx.hooks)(id))
                    .map(|p| {
                        plans.insert(id.clone(), p);
                    })
                    .map_err(|e| e.to_string()),
                None => Err(format!("{id}: not in the desired fleet")),
            },
            Step::Start(id, _) => match plans.get(id) {
                Some(p) => ctx.runner.ensure_agent(id, p).map_err(|e| e.to_string()),
                None => Err(format!("{id}: no launch plan (materialize did not run)")),
            },
            Step::NoteExit(..) => Ok(()),
        };
        apply(status, step, &outcome, ctx.policy, now);
        if let Err(e) = outcome {
            match step {
                Step::EnsureCrew(crew) => {
                    failed_crews.insert(crew.clone(), e.clone());
                }
                _ => {
                    if let Some(id) = step.agent_id() {
                        failed_agents.push(id.clone());
                    }
                }
            }
            report.failures.push((step.clone(), e));
        }
    }
    finish_pass(status, ctx.desired.is_none(), report.all_ok());
    report
}

fn ensure_crew(ctx: &ReconcileContext, crew: &CrewRef) -> Result<(), String> {
    let desired = ctx.desired.and_then(|f| f.crews.get(&crew.crew)).ok_or_else(|| format!("{crew}: not in the desired fleet"))?;
    ctx.materializer
        .ensure_crew(crew, &desired.repo, &desired.git_ref, &desired.git, ctx.creds)
        .map_err(|e| e.to_string())?;
    ctx.runner.ensure_crew(crew).map_err(|e| e.to_string())
}

/// One full pass: observe, plan, execute.
pub fn reconcile_pass(status: &mut FleetStatus, ctx: &ReconcileContext) -> Result<(Plan, ExecuteReport), RunnerError> {
    let observed = ctx.runner.observe(ctx.fleet)?;
    let p = plan(ctx.fleet, ctx.desired, ctx.keep, status, &observed, ctx.policy, ctx.clock.now());
    let report = execute(&p, status, ctx);
    Ok((p, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::{FakeClock, FakeMaterializer, FakeRunner};
    use balerix_api::{AgentPhase, AgentSettings, CrewSpec, FleetPhase, FleetSpec, GitSettings, Timestamp};

    fn fleet(agents: &[&str]) -> Fleet {
        Fleet::try_from(FleetSpec {
            name: "f".into(),
            crews: std::collections::BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "acme/api".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: agents.iter().map(|a| (a.to_string(), AgentSettings::default())).collect(),
                },
            )]),
        })
        .unwrap()
    }

    fn hooks(id: &AgentId) -> HookTarget {
        HookTarget { url: "https://127.0.0.1:7643".into(), secret: format!("s-{id}") }
    }

    struct Harness {
        m: FakeMaterializer,
        r: FakeRunner,
        clock: FakeClock,
        creds: CredentialBundle,
        policy: ReconcilePolicy,
        fleet_name: FleetName,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                m: FakeMaterializer::default(),
                r: FakeRunner::default(),
                clock: FakeClock::new(Timestamp(1000)),
                creds: CredentialBundle::default(),
                policy: ReconcilePolicy::default(),
                fleet_name: "f".parse().unwrap(),
            }
        }
        fn ctx<'a>(&'a self, desired: Option<&'a Fleet>) -> ReconcileContext<'a> {
            ReconcileContext {
                fleet: &self.fleet_name,
                desired,
                keep: Keep::default(),
                materializer: &self.m,
                runner: &self.r,
                creds: &self.creds,
                hooks: &hooks,
                policy: &self.policy,
                clock: &self.clock,
            }
        }
    }

    #[test]
    fn a_pass_brings_a_fresh_fleet_to_starting() {
        let h = Harness::new();
        let f = fleet(&["a", "b"]);
        let mut st = FleetStatus::default();
        let (p, rep) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert!(rep.all_ok(), "{rep:?}");
        assert_eq!(p.len(), 5);
        assert_eq!(st.agents["f/c/a"].phase, AgentPhase::Starting);
        assert_eq!(st.agents["f/c/b"].phase, AgentPhase::Starting);
        assert_eq!(st.phase, FleetPhase::Reconciling);
        assert_eq!(st.observed_generation, st.generation);
        assert_eq!(
            h.m.calls(),
            vec!["ensure_crew f/c", "materialize f/c/a", "materialize f/c/b"]
        );
        assert_eq!(
            h.r.calls(),
            vec!["observe f", "ensure_crew f/c", "ensure_agent f/c/a", "ensure_agent f/c/b"]
        );
        // second pass: nothing but ensure-crew
        let (p2, _) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(p2.iter().map(ToString::to_string).collect::<Vec<_>>(), vec!["ensure-crew f/c"]);
    }

    #[test]
    fn a_failed_materialize_skips_start_and_degrades_the_fleet() {
        let h = Harness::new();
        h.m.fail_next("materialize", "f/c/a", "no space left");
        let f = fleet(&["a", "b"]);
        let mut st = FleetStatus::default();
        let (_, rep) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(rep.failures.len(), 1);
        assert_eq!(rep.skipped.iter().map(ToString::to_string).collect::<Vec<_>>().len(), 1);
        assert_eq!(st.agents["f/c/a"].phase, AgentPhase::Pending);
        assert_eq!(st.agents["f/c/a"].message, "f/c/a: mise materialize: no space left");
        assert_eq!(st.agents["f/c/b"].phase, AgentPhase::Starting, "other agents proceed");
        assert_eq!(st.phase, FleetPhase::Degraded);
        assert_ne!(st.observed_generation, 1);
        assert!(!h.r.calls().contains(&"ensure_agent f/c/a".to_string()));
        // next pass retries and succeeds
        let (_, rep) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert!(rep.all_ok());
        assert_eq!(st.agents["f/c/a"].phase, AgentPhase::Starting);
    }

    #[test]
    fn a_failed_ensure_crew_skips_every_agent_of_that_crew() {
        let h = Harness::new();
        h.m.fail_next("ensure_crew", "f/c", "clone failed");
        let f = fleet(&["a"]);
        let mut st = FleetStatus::default();
        let (_, rep) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(rep.failures.len(), 1);
        assert_eq!(rep.skipped.len(), 2);
        assert_eq!(st.agents["f/c/a"].message, "f/c: git ensure_crew: clone failed");
        assert!(h.m.calls().iter().all(|c| !c.starts_with("materialize")));
    }

    #[test]
    fn down_stops_and_removes_then_terminating() {
        let h = Harness::new();
        let f = fleet(&["a"]);
        let mut st = FleetStatus::default();
        reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        let ctx = ReconcileContext { keep: Keep { repos: true, sessions: true }, ..h.ctx(None) };
        let (p, rep) = reconcile_pass(&mut st, &ctx).unwrap();
        assert!(rep.all_ok());
        assert_eq!(
            p.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["stop f/c/a", "remove-crew f/c keep-repos=true keep-sessions=true"]
        );
        assert!(st.agents.is_empty());
        assert_eq!(st.phase, FleetPhase::Terminating);
        assert!(h.m.calls().contains(&"remove_crew f/c repos=true sessions=true".to_string()));
        assert!(h.r.observed().crews.is_empty());
    }

    #[test]
    fn an_exit_is_noted_then_restarted_after_backoff() {
        let h = Harness::new();
        let f = fleet(&["a"]);
        let mut st = FleetStatus::default();
        reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        h.r.set_state(&"f/c/a".parse().unwrap(), crate::ports::ProcessState::Exited { code: Some(1) });
        reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(st.agents["f/c/a"].restarts, 1);
        assert_eq!(st.agents["f/c/a"].next_restart_at, Some(Timestamp(1002)));
        let (p, _) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(p.len(), 1, "backoff not elapsed: only ensure-crew");
        h.clock.advance(2);
        let (p, _) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(st.agents["f/c/a"].phase, AgentPhase::Starting);
        assert_eq!(h.r.observed().get(&"f/c/a".parse().unwrap()), Some(&crate::ports::ProcessState::Running { pid: 2 }));
    }
}
```

`crates/balerix-core/Cargo.toml` `[dev-dependencies]`: add `insta = { workspace = true }`.

- [ ] **Step 2: Run the unit tests**

Run: `mise x -- cargo nextest run -p balerix-core reconcile::execute`
Expected: 5 PASS.

- [ ] **Step 3: Write the plan golden test**

`crates/balerix-core/tests/plan_golden.rs`:
```rust
//! Plans for four canned situations, rendered one step per line. Review a
//! `.snap.new` against the expected step lists in the Phase 2 plan, Task 7.

use std::collections::BTreeMap;

use balerix_api::{AgentPhase, AgentSettings, AgentStatus, CrewSpec, FleetSpec, FleetStatus, GitSettings, Timestamp};
use balerix_core::reconcile::{Plan, ReconcilePolicy, plan};
use balerix_core::{Fleet, Keep, ObservedState, ProcessState, ResolvedAgent};

fn fleet(agents: &[(&str, &str)]) -> Fleet {
    Fleet::try_from(FleetSpec {
        name: "payments".into(),
        crews: BTreeMap::from([(
            "backend".to_string(),
            CrewSpec {
                repo: "acme/payments-api".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: agents
                    .iter()
                    .map(|(n, model)| {
                        let mut s = AgentSettings::default();
                        s.claude.settings = serde_json::json!({ "model": model });
                        (n.to_string(), s)
                    })
                    .collect(),
            },
        )]),
    })
    .unwrap()
}

fn applied(f: &Fleet, st: &mut FleetStatus, phase: AgentPhase) {
    for a in ResolvedAgent::from_fleet(f) {
        *st.entry(&a.id.to_string()) = AgentStatus { phase, applied_hash: Some(a.hash()), ..AgentStatus::default() };
    }
}

fn running(f: &Fleet) -> ObservedState {
    let mut o = ObservedState::default();
    for a in ResolvedAgent::from_fleet(f) {
        o.set(&a.id, ProcessState::Running { pid: 100 });
    }
    o
}

fn render(p: &Plan) -> String {
    // hashes are stable (they derive from fixed settings), so they may appear
    p.iter().map(|s| format!("{s}\n")).collect()
}

fn go(desired: Option<&Fleet>, keep: Keep, st: &FleetStatus, obs: &ObservedState) -> String {
    render(&plan(&"payments".parse().unwrap(), desired, keep, st, obs, &ReconcilePolicy::default(), Timestamp(1_000)))
}

#[test]
fn fresh_up() {
    let f = fleet(&[("alice", "sonnet"), ("bob", "opus")]);
    insta::assert_snapshot!("fresh_up", go(Some(&f), Keep::default(), &FleetStatus::default(), &ObservedState::default()));
}

#[test]
fn one_agent_hash_changed() {
    let before = fleet(&[("alice", "sonnet"), ("bob", "opus")]);
    let after = fleet(&[("alice", "sonnet"), ("bob", "haiku")]);
    let mut st = FleetStatus::default();
    applied(&before, &mut st, AgentPhase::Ready);
    insta::assert_snapshot!("one_agent_hash_changed", go(Some(&after), Keep::default(), &st, &running(&before)));
}

#[test]
fn one_agent_exited_past_max_restarts() {
    let f = fleet(&[("alice", "sonnet"), ("bob", "opus")]);
    let mut st = FleetStatus::default();
    applied(&f, &mut st, AgentPhase::Ready);
    let mut obs = running(&f);
    obs.set(&"payments/backend/bob".parse().unwrap(), ProcessState::Exited { code: Some(1) });
    // not yet noted
    let noted_next = go(Some(&f), Keep::default(), &st, &obs);
    // noted, dead
    let bob = st.entry("payments/backend/bob");
    bob.phase = AgentPhase::Dead;
    bob.restarts = 6;
    let dead = go(Some(&f), Keep::default(), &st, &obs);
    insta::assert_snapshot!("one_agent_exited", format!("-- unnoted exit --\n{noted_next}-- dead --\n{dead}"));
}

#[test]
fn down_keep_repos() {
    let f = fleet(&[("alice", "sonnet"), ("bob", "opus")]);
    let mut st = FleetStatus::default();
    applied(&f, &mut st, AgentPhase::Ready);
    insta::assert_snapshot!("down_keep_repos", go(None, Keep { repos: true, sessions: false }, &st, &running(&f)));
}
```

- [ ] **Step 4: Run, review snapshots, accept**

Run: `mise x -- cargo nextest run -p balerix-core --test plan_golden`
Expected values to check in each `.snap.new` before accepting:
- `fresh_up`: `ensure-crew payments/backend`, `materialize payments/backend/alice`, `start payments/backend/alice <8 hex>`, `materialize payments/backend/bob`, `start payments/backend/bob <8 hex>` — five lines, that order.
- `one_agent_hash_changed`: `stop payments/backend/bob`, `ensure-crew payments/backend`, `materialize payments/backend/bob`, `start payments/backend/bob <hex>` — alice absent.
- `one_agent_exited`: under `-- unnoted exit --`: `ensure-crew …`, `note-exit payments/backend/bob Some(1)`; under `-- dead --`: `ensure-crew …` only.
- `down_keep_repos`: `stop …/alice`, `stop …/bob`, `remove-crew payments/backend keep-repos=true keep-sessions=false`.

Then: `mise x -- cargo insta accept`.

- [ ] **Step 5: Lint and commit**

Run: `mise run check`
```bash
git add crates/balerix-core
git commit -m "Add the reconcile executor and plan golden snapshots

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---

### Task 8: `balerix-core` — model-based reconciler test

**Files:**
- Create: `crates/balerix-core/tests/reconcile_model.rs`
- Modify: `crates/balerix-core/Cargo.toml` (dev-dep `proptest-state-machine`)

**Interfaces:**
- Consumes: everything from Tasks 4–7.

The reference model tracks, per agent, the phase the reconciler must report, its restart count and pending restart time, using the policy `max_restarts = 2, base = 2, cap = 8` so the sequences hit `Dead` and the cap quickly. Each transition is followed by exactly one `reconcile_pass`; the invariant check then runs a *second* pass and requires an empty plan apart from `EnsureCrew` (idempotence).

- [ ] **Step 1: Add the dev-dependency**

`crates/balerix-core/Cargo.toml` `[dev-dependencies]`: add `proptest-state-machine = { workspace = true }`.

- [ ] **Step 2: Write the test**

`crates/balerix-core/tests/reconcile_model.rs`:
```rust
//! Model-based test (Phase 2 spec §3, §6): the reconciler against a reference
//! model over random up / update / down / exit / ready / tick sequences.

use std::collections::BTreeMap;

use balerix_api::{AgentPhase, AgentSettings, CredentialBundle, CrewSpec, FleetPhase, FleetSpec, FleetStatus, GitSettings, Timestamp};
use balerix_core::fakes::{FakeClock, FakeMaterializer, FakeRunner};
use balerix_core::reconcile::{ReconcileContext, ReconcilePolicy, Step, reconcile_pass};
use balerix_core::{AgentId, Fleet, FleetName, HookTarget, Keep, ProcessState};
use proptest::prelude::*;
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest, prop_state_machine};

const AGENTS: [&str; 3] = ["a", "b", "c"];
const MAX_RESTARTS: u32 = 2;
const BASE: u64 = 2;
const CAP: u64 = 8;

fn policy() -> ReconcilePolicy {
    ReconcilePolicy { max_restarts: MAX_RESTARTS, backoff_base_secs: BASE, backoff_cap_secs: CAP }
}

fn backoff(restarts: u32) -> u64 {
    (BASE << (restarts - 1)).min(CAP)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RefAgent {
    phase: AgentPhase,
    restarts: u32,
    next_restart_at: Option<u64>,
    version: u32,
}

#[derive(Clone, Debug, Default)]
struct RefState {
    desired: Option<BTreeMap<String, u32>>,
    agents: BTreeMap<String, RefAgent>,
    now: u64,
}

#[derive(Clone, Debug)]
enum Transition {
    Up(BTreeMap<String, u32>),
    Update(String, u32),
    Down,
    AgentExits(String),
    AgentReady(String),
    Tick(u64),
}

struct Model;

impl ReferenceStateMachine for Model {
    type State = RefState;
    type Transition = Transition;

    fn init_state() -> BoxedStrategy<Self::State> {
        Just(RefState { now: 1_000, ..RefState::default() }).boxed()
    }

    fn transitions(state: &Self::State) -> BoxedStrategy<Self::Transition> {
        let up = proptest::collection::btree_map(proptest::sample::select(AGENTS.to_vec()).prop_map(String::from), 0..3u32, 1..=3)
            .prop_map(Transition::Up);
        let tick = (1..12u64).prop_map(Transition::Tick);
        let names: Vec<String> = state.agents.keys().cloned().collect();
        if names.is_empty() {
            return prop_oneof![up, Just(Transition::Down), tick].boxed();
        }
        let pick = proptest::sample::select(names);
        prop_oneof![
            2 => up,
            2 => (pick.clone(), 0..3u32).prop_map(|(a, v)| Transition::Update(a, v)),
            1 => Just(Transition::Down),
            3 => pick.clone().prop_map(Transition::AgentExits),
            3 => pick.prop_map(Transition::AgentReady),
            3 => tick,
        ]
        .boxed()
    }

    fn preconditions(state: &Self::State, t: &Self::Transition) -> bool {
        match t {
            Transition::Update(a, _) => state.desired.as_ref().is_some_and(|d| d.contains_key(a)),
            // only while the window is actually running: a pending restart
            // (next_restart_at set) means the window is already Exited
            Transition::AgentExits(a) | Transition::AgentReady(a) => state
                .agents
                .get(a)
                .is_some_and(|x| matches!(x.phase, AgentPhase::Starting | AgentPhase::Ready) && x.next_restart_at.is_none()),
            _ => true,
        }
    }

    /// The state after the transition AND one reconcile pass.
    fn apply(mut s: Self::State, t: &Self::Transition) -> Self::State {
        match t {
            Transition::Up(map) => {
                s.desired = Some(map.clone());
                s.agents.retain(|k, _| map.contains_key(k));
                for (name, v) in map {
                    let changed = s.agents.get(name).is_none_or(|a| a.version != *v);
                    if changed {
                        s.agents.insert(name.clone(), RefAgent { phase: AgentPhase::Starting, restarts: 0, next_restart_at: None, version: *v });
                    }
                }
            }
            Transition::Update(name, v) => {
                if let Some(d) = s.desired.as_mut() {
                    d.insert(name.clone(), *v);
                }
                if s.agents.get(name).is_some_and(|a| a.version != *v) {
                    s.agents.insert(name.clone(), RefAgent { phase: AgentPhase::Starting, restarts: 0, next_restart_at: None, version: *v });
                }
            }
            Transition::Down => {
                s.desired = None;
                s.agents.clear();
            }
            Transition::AgentExits(name) => {
                if let Some(a) = s.agents.get_mut(name) {
                    a.restarts += 1;
                    if a.restarts > MAX_RESTARTS {
                        a.phase = AgentPhase::Dead;
                        a.next_restart_at = None;
                    } else {
                        a.next_restart_at = Some(s.now + backoff(a.restarts));
                    }
                }
            }
            Transition::AgentReady(name) => {
                if let Some(a) = s.agents.get_mut(name) {
                    a.phase = AgentPhase::Ready;
                    a.restarts = 0;
                    a.next_restart_at = None;
                }
            }
            Transition::Tick(secs) => {
                s.now += secs;
            }
        }
        // the pass after the transition restarts every due agent
        for a in s.agents.values_mut() {
            if a.next_restart_at.is_some_and(|t| t <= s.now) {
                a.phase = AgentPhase::Starting;
                a.next_restart_at = None;
            }
        }
        s
    }
}

struct Sut {
    status: FleetStatus,
    m: FakeMaterializer,
    r: FakeRunner,
    clock: FakeClock,
    desired: Option<Fleet>,
    fleet: FleetName,
    creds: CredentialBundle,
}

fn fleet_of(map: &BTreeMap<String, u32>) -> Fleet {
    Fleet::try_from(FleetSpec {
        name: "f".into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/api".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: map
                    .iter()
                    .map(|(n, v)| {
                        let mut s = AgentSettings::default();
                        s.env.insert("V".into(), v.to_string());
                        (n.clone(), s)
                    })
                    .collect(),
            },
        )]),
    })
    .unwrap()
}

fn hooks(id: &AgentId) -> HookTarget {
    HookTarget { url: "https://127.0.0.1:7643".into(), secret: id.to_string() }
}

fn id(name: &str) -> AgentId {
    format!("f/c/{name}").parse().unwrap()
}

impl Sut {
    fn pass(&mut self) -> Vec<Step> {
        let ctx = ReconcileContext {
            fleet: &self.fleet,
            desired: self.desired.as_ref(),
            keep: Keep::default(),
            materializer: &self.m,
            runner: &self.r,
            creds: &self.creds,
            hooks: &hooks,
            policy: &policy(),
            clock: &self.clock,
        };
        let (plan, report) = reconcile_pass(&mut self.status, &ctx).unwrap();
        assert!(report.all_ok(), "no failures are injected in this model: {report:?}");
        plan
    }
}

impl StateMachineTest for Sut {
    type SystemUnderTest = Sut;
    type Reference = Model;

    fn init_test(_: &RefState) -> Self::SystemUnderTest {
        Sut {
            status: FleetStatus::default(),
            m: FakeMaterializer::default(),
            r: FakeRunner::default(),
            clock: FakeClock::new(Timestamp(1_000)),
            desired: None,
            fleet: "f".parse().unwrap(),
            creds: CredentialBundle::default(),
        }
    }

    fn apply(mut sut: Self::SystemUnderTest, _: &RefState, t: Transition) -> Self::SystemUnderTest {
        match t {
            Transition::Up(map) => {
                sut.desired = Some(fleet_of(&map));
                sut.status.generation += 1;
            }
            Transition::Update(name, v) => {
                if let Some(f) = sut.desired.as_mut() {
                    let mut s = f.crews.get_mut(&"c".parse().unwrap()).unwrap().agents.get(&name.parse().unwrap()).cloned().unwrap();
                    s.env.insert("V".into(), v.to_string());
                    f.crews.get_mut(&"c".parse().unwrap()).unwrap().agents.insert(name.parse().unwrap(), s);
                }
                sut.status.generation += 1;
            }
            Transition::Down => sut.desired = None,
            Transition::AgentExits(name) => sut.r.set_state(&id(&name), ProcessState::Exited { code: Some(1) }),
            Transition::AgentReady(name) => balerix_core::reconcile::agent_ready(&mut sut.status, &id(&name), sut.clock.now()),
            Transition::Tick(secs) => sut.clock.advance(secs),
        }
        sut.pass();
        sut
    }

    fn check_invariants(sut: &Self::SystemUnderTest, r: &RefState) {
        let got: BTreeMap<String, RefAgent> = sut
            .status
            .agents
            .iter()
            .map(|(k, a)| {
                let name = k.rsplit('/').next().unwrap().to_string();
                let version = r.agents.get(&name).map_or(u32::MAX, |x| x.version);
                (name, RefAgent { phase: a.phase, restarts: a.restarts, next_restart_at: a.next_restart_at.map(|t| t.0), version })
            })
            .collect();
        assert_eq!(got, r.agents, "status vs model\nstatus: {:#?}", sut.status);

        let observed = sut.r.observed();
        for (name, a) in &r.agents {
            let running = matches!(observed.get(&id(name)), Some(ProcessState::Running { .. }));
            let should_run = matches!(a.phase, AgentPhase::Starting | AgentPhase::Ready) && a.next_restart_at.is_none();
            assert_eq!(running, should_run, "{name}: process state vs phase {a:?}");
        }

        let expected_fleet = if r.desired.is_none() {
            FleetPhase::Terminating
        } else if r.agents.values().any(|a| a.phase == AgentPhase::Dead) {
            FleetPhase::Degraded
        } else if r.agents.values().any(|a| a.phase == AgentPhase::Starting) {
            FleetPhase::Reconciling
        } else if r.agents.is_empty() {
            FleetPhase::Pending
        } else {
            FleetPhase::Ready
        };
        assert_eq!(sut.status.phase, expected_fleet);
        assert_eq!(sut.status.observed_generation, sut.status.generation);

        // idempotence: a second pass with the same inputs only re-ensures crews
        let mut again = Sut {
            status: sut.status.clone(),
            m: FakeMaterializer::default(),
            r: FakeRunner::default(),
            clock: FakeClock::new(sut.clock.now()),
            desired: sut.desired.clone(),
            fleet: sut.fleet.clone(),
            creds: CredentialBundle::default(),
        };
        for (crew, agents) in &observed.crews {
            for (agent, s) in agents {
                again.r.set_state(&AgentId { fleet: sut.fleet.clone(), crew: crew.clone(), agent: agent.clone() }, *s);
            }
        }
        let plan = again.pass();
        assert!(plan.iter().all(|s| matches!(s, Step::EnsureCrew(_))), "second pass not idempotent: {plan:?}");
    }
}

prop_state_machine! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]
    #[test]
    fn reconciler_matches_the_reference_model(sequential 1..40 => Sut);
}
```

Two details the executor must keep straight when this first runs: `FleetStatus.generation` in the SUT is bumped on `Up`/`Update` so `observed_generation == generation` after a clean pass is a real check; and `FakeRunner` keeps the process map across `Down`/`Up`, which is exactly why the model treats a re-`Up` of an unchanged version as "already running" only when the reconciler's status still holds the hash — after `Down` the status is cleared, so every agent restarts, matching the model's `s.agents.clear()`.

- [ ] **Step 3: Run**

Run: `mise x -- cargo nextest run -p balerix-core --test reconcile_model`
Expected: PASS, 256 cases. If a counterexample appears, it is a real disagreement between §3 of the spec and the code (or the model): reduce it with the printed transition sequence, fix the code or the model, and record which in the commit body.

- [ ] **Step 4: Commit**

Run: `mise run check`
```bash
git add crates/balerix-core
git commit -m "Add the model-based reconciler test

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---
### Task 9: `balerix-runtime` scaffold — layout, tools, fsutil, `sh_quote`

**Files:**
- Modify: `Cargo.toml` (workspace members already glob `crates/*`; nothing to add)
- Create: `crates/balerix-runtime/Cargo.toml`, `src/lib.rs`, `src/layout.rs`, `src/tools.rs`, `src/fsutil.rs`, `src/quote.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct StateLayout { pub state_root: PathBuf, pub data_root: PathBuf, pub config_root: PathBuf }
  impl StateLayout {
      pub fn from_env(home: &Path, env: impl Fn(&str) -> Option<OsString>) -> Self;  // XDG_*_HOME or ~/.local/state, ~/.local/share, ~/.config, each + "/balerix"
      pub fn mise_data_dir(&self) -> PathBuf;            // data_root/mise
      pub fn system_mise_toml(&self) -> PathBuf;         // config_root/mise.toml
      pub fn fleet_dir(&self, f: &FleetName) -> PathBuf; // state_root/fleets/<f>
      pub fn fleet_gh_dir(&self, f: &FleetName) -> PathBuf;   // …/gh
      pub fn crew(&self, c: &CrewRef) -> CrewPaths;
      pub fn agent(&self, id: &AgentId) -> AgentPaths;
  }
  pub struct CrewPaths { pub root: PathBuf, pub repo: PathBuf }
  pub struct AgentPaths { pub root, home, workspace, nono_home, mise_toml, profile, launch, logs: PathBuf }
  impl AgentPaths { claude_dir(), gh_dir(), xdg_config(), xdg_data(), xdg_state(), xdg_cache(),
                    mise_config_dir(), mise_state_dir(), mise_cache_dir(), claude_projects() -> PathBuf }
  pub struct ToolPaths { pub git, pub gh, pub mise, pub nono, pub tmux: PathBuf }
  impl ToolPaths { pub fn discover_in(path: &OsStr) -> Result<Self, MissingTool> }
  pub struct MissingTool(pub String);   // Display: "required tool not found on PATH: <name>"
  pub(crate) struct Cmd { .. }          // builder: Cmd::new(&tool).arg().args().env().cwd().log(path)
  pub(crate) struct CmdOutput { pub stdout: String }
  impl Cmd { pub(crate) fn run(&self) -> Result<CmdOutput, CmdFailure>;   // Ok only on exit 0
             pub(crate) fn subcommand(&self) -> String }
  pub(crate) struct CmdFailure { pub tool: String, pub subcommand: String, pub args: Vec<String>, pub stderr: String }
  pub(crate) fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> io::Result<()>;
  pub(crate) fn ensure_dir(path: &Path) -> io::Result<()>;
  pub fn sh_quote(s: &str) -> String;
  ```

- [ ] **Step 1: Create the crate**

`crates/balerix-runtime/Cargo.toml`:
```toml
[package]
name = "balerix-runtime"
description = "Driven adapters: materialize agents on disk and run them in tmux"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
balerix-api = { workspace = true }
balerix-core = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
serde_norway = { workspace = true }
thiserror = { workspace = true }
toml = { workspace = true }

[dev-dependencies]
insta = { workspace = true }
proptest = { workspace = true }
tempfile = { workspace = true }
pretty_assertions = { workspace = true }

[lints]
workspace = true
```

`crates/balerix-runtime/src/lib.rs`:
```rust
//! Driven adapters (Phase 2 spec §4): everything that turns a `ResolvedAgent`
//! into files and a tmux window. Every path comes from `StateLayout`, every
//! binary from `ToolPaths`; nothing here reads the process environment.

pub mod fsutil;
pub mod layout;
pub mod quote;
pub mod tools;

pub use layout::{AgentPaths, CrewPaths, StateLayout};
pub use quote::sh_quote;
pub use tools::{MissingTool, ToolPaths};
```

- [ ] **Step 2: Write `layout.rs` with tests**

```rust
//! Where everything lives (Phase 2 spec §4.1).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use balerix_core::{AgentId, CrewRef, FleetName};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateLayout {
    pub state_root: PathBuf,
    pub data_root: PathBuf,
    pub config_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewPaths {
    pub root: PathBuf,
    pub repo: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaths {
    pub root: PathBuf,
    pub home: PathBuf,
    pub workspace: PathBuf,
    /// nono's own `$HOME` — its state root must not overlap `home/` (P2-5).
    pub nono_home: PathBuf,
    pub mise_toml: PathBuf,
    pub profile: PathBuf,
    pub launch: PathBuf,
    pub logs: PathBuf,
}

impl StateLayout {
    /// XDG resolution: `$XDG_{STATE,DATA,CONFIG}_HOME/balerix`, defaulting to
    /// `~/.local/state`, `~/.local/share`, `~/.config`.
    pub fn from_env(home: &Path, env: impl Fn(&str) -> Option<OsString>) -> Self {
        let pick = |var: &str, default: PathBuf| env(var).map(PathBuf::from).unwrap_or(default).join("balerix");
        Self {
            state_root: pick("XDG_STATE_HOME", home.join(".local").join("state")),
            data_root: pick("XDG_DATA_HOME", home.join(".local").join("share")),
            config_root: pick("XDG_CONFIG_HOME", home.join(".config")),
        }
    }

    pub fn mise_data_dir(&self) -> PathBuf {
        self.data_root.join("mise")
    }
    pub fn system_mise_toml(&self) -> PathBuf {
        self.config_root.join("mise.toml")
    }
    pub fn fleet_dir(&self, f: &FleetName) -> PathBuf {
        self.state_root.join("fleets").join(f.as_str())
    }
    pub fn fleet_gh_dir(&self, f: &FleetName) -> PathBuf {
        self.fleet_dir(f).join("gh")
    }
    pub fn crew(&self, c: &CrewRef) -> CrewPaths {
        let root = self.fleet_dir(&c.fleet).join("crews").join(c.crew.as_str());
        CrewPaths { repo: root.join("repo"), root }
    }
    pub fn agent(&self, id: &AgentId) -> AgentPaths {
        let root = self.crew(&id.crew_ref()).root.join("agents").join(id.agent.as_str());
        AgentPaths {
            home: root.join("home"),
            workspace: root.join("workspace"),
            nono_home: root.join("nono"),
            mise_toml: root.join("mise.toml"),
            profile: root.join("nono-profile.json"),
            launch: root.join("launch.sh"),
            logs: root.join("logs"),
            root,
        }
    }
}

impl AgentPaths {
    pub fn claude_dir(&self) -> PathBuf {
        self.home.join(".claude")
    }
    pub fn claude_projects(&self) -> PathBuf {
        self.claude_dir().join("projects")
    }
    pub fn xdg_config(&self) -> PathBuf {
        self.home.join(".config")
    }
    pub fn xdg_data(&self) -> PathBuf {
        self.home.join(".local").join("share")
    }
    pub fn xdg_state(&self) -> PathBuf {
        self.home.join(".local").join("state")
    }
    pub fn xdg_cache(&self) -> PathBuf {
        self.home.join(".cache")
    }
    pub fn gh_dir(&self) -> PathBuf {
        self.xdg_config().join("gh")
    }
    pub fn mise_config_dir(&self) -> PathBuf {
        self.xdg_config().join("mise")
    }
    pub fn mise_state_dir(&self) -> PathBuf {
        self.xdg_state().join("mise")
    }
    pub fn mise_cache_dir(&self) -> PathBuf {
        self.xdg_cache().join("mise")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn defaults_to_xdg_dirs_under_home() {
        let l = StateLayout::from_env(Path::new("/h"), no_env);
        assert_eq!(l.state_root, PathBuf::from("/h/.local/state/balerix"));
        assert_eq!(l.data_root, PathBuf::from("/h/.local/share/balerix"));
        assert_eq!(l.config_root, PathBuf::from("/h/.config/balerix"));
    }

    #[test]
    fn honours_xdg_variables() {
        let l = StateLayout::from_env(Path::new("/h"), |k| (k == "XDG_STATE_HOME").then(|| OsString::from("/xs")));
        assert_eq!(l.state_root, PathBuf::from("/xs/balerix"));
        assert_eq!(l.data_root, PathBuf::from("/h/.local/share/balerix"));
    }

    #[test]
    fn agent_paths_follow_the_spec_layout() {
        let l = StateLayout::from_env(Path::new("/h"), no_env);
        let a = l.agent(&"payments/backend/alice".parse().unwrap());
        let base = "/h/.local/state/balerix/fleets/payments/crews/backend/agents/alice";
        assert_eq!(a.root, PathBuf::from(base));
        assert_eq!(a.home, PathBuf::from(format!("{base}/home")));
        assert_eq!(a.nono_home, PathBuf::from(format!("{base}/nono")));
        assert_eq!(a.profile, PathBuf::from(format!("{base}/nono-profile.json")));
        assert_eq!(a.claude_dir(), PathBuf::from(format!("{base}/home/.claude")));
        assert_eq!(a.mise_cache_dir(), PathBuf::from(format!("{base}/home/.cache/mise")));
        assert_eq!(l.crew(&"payments/backend".parse().unwrap()).repo, PathBuf::from("/h/.local/state/balerix/fleets/payments/crews/backend/repo"));
        assert_eq!(l.fleet_gh_dir(&"payments".parse().unwrap()), PathBuf::from("/h/.local/state/balerix/fleets/payments/gh"));
        assert_eq!(l.mise_data_dir(), PathBuf::from("/h/.local/share/balerix/mise"));
    }
}
```

- [ ] **Step 3: Write `tools.rs` with tests**

```rust
//! Tool discovery and a logged subprocess runner. Argv arrays only.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPaths {
    pub git: PathBuf,
    pub gh: PathBuf,
    pub mise: PathBuf,
    pub nono: PathBuf,
    pub tmux: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("required tool not found on PATH: {0}")]
pub struct MissingTool(pub String);

impl ToolPaths {
    /// Finds each tool as the first executable file on `path`.
    pub fn discover_in(path: &OsStr) -> Result<Self, MissingTool> {
        let find = |name: &str| -> Result<PathBuf, MissingTool> {
            std::env::split_paths(path)
                .map(|d| d.join(name))
                .find(|p| p.is_file())
                .ok_or_else(|| MissingTool(name.to_string()))
        };
        Ok(Self { git: find("git")?, gh: find("gh")?, mise: find("mise")?, nono: find("nono")?, tmux: find("tmux")? })
    }
}

/// One subprocess invocation.
pub(crate) struct Cmd {
    program: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
    log: Option<PathBuf>,
}

pub(crate) struct CmdOutput {
    pub stdout: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CmdFailure {
    pub tool: String,
    pub subcommand: String,
    pub args: Vec<String>,
    pub stderr: String,
}

impl fmt::Display for CmdFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.tool, self.subcommand, balerix_core::first_line(&self.stderr))
    }
}

impl Cmd {
    pub(crate) fn new(program: &Path) -> Self {
        Self { program: program.to_path_buf(), args: Vec::new(), env: BTreeMap::new(), cwd: None, log: None }
    }
    pub(crate) fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
    pub(crate) fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, a: I) -> Self {
        self.args.extend(a.into_iter().map(Into::into));
        self
    }
    pub(crate) fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }
    pub(crate) fn envs(mut self, vars: &BTreeMap<String, String>) -> Self {
        self.env.extend(vars.iter().map(|(k, v)| (k.clone(), v.clone())));
        self
    }
    pub(crate) fn cwd(mut self, d: &Path) -> Self {
        self.cwd = Some(d.to_path_buf());
        self
    }
    /// Append `$ argv`, stdout and stderr to this file after the run.
    pub(crate) fn log(mut self, file: &Path) -> Self {
        self.log = Some(file.to_path_buf());
        self
    }
    pub(crate) fn tool(&self) -> String {
        self.program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
    /// First argument that does not start with `-`, e.g. `clone` in
    /// `git -C x clone`.
    pub(crate) fn subcommand(&self) -> String {
        let mut skip_value = false;
        for a in &self.args {
            if skip_value {
                skip_value = false;
                continue;
            }
            if a == "-C" || a == "-c" || a == "-L" || a == "-t" {
                skip_value = true;
                continue;
            }
            if !a.starts_with('-') {
                return a.clone();
            }
        }
        String::new()
    }

    pub(crate) fn run(&self) -> Result<CmdOutput, CmdFailure> {
        let mut c = Command::new(&self.program);
        c.args(&self.args).envs(&self.env);
        if let Some(d) = &self.cwd {
            c.current_dir(d);
        }
        let failure = |stderr: String| CmdFailure { tool: self.tool(), subcommand: self.subcommand(), args: self.args.clone(), stderr };
        let out = c.output().map_err(|e| failure(format!("cannot execute {}: {e}", self.program.display())))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if let Some(log) = &self.log {
            if let Some(dir) = log.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log) {
                let _ = writeln!(f, "$ {} {}\n{stdout}{stderr}[exit {}]", self.tool(), self.args.join(" "), out.status);
            }
        }
        if !out.status.success() {
            return Err(failure(if stderr.trim().is_empty() { format!("exit status {}", out.status) } else { stderr }));
        }
        Ok(CmdOutput { stdout })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_finds_tools_on_path_and_names_the_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        for t in ["git", "gh", "mise", "nono"] {
            std::fs::write(dir.path().join(t), "").unwrap();
        }
        let err = ToolPaths::discover_in(dir.path().as_os_str()).unwrap_err();
        assert_eq!(err.to_string(), "required tool not found on PATH: tmux");
        std::fs::write(dir.path().join("tmux"), "").unwrap();
        let t = ToolPaths::discover_in(dir.path().as_os_str()).unwrap();
        assert_eq!(t.tmux, dir.path().join("tmux"));
    }

    #[test]
    fn subcommand_skips_flags_with_values() {
        let c = Cmd::new(Path::new("/usr/bin/git")).args(["-C", "/x", "-c", "k=v", "worktree", "add"]);
        assert_eq!(c.subcommand(), "worktree");
        assert_eq!(Cmd::new(Path::new("/t/tmux")).args(["-L", "s", "new-window"]).subcommand(), "new-window");
        assert_eq!(c.tool(), "git");
    }

    #[test]
    fn run_captures_output_logs_and_fails_on_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("sh.log");
        let ok = Cmd::new(Path::new("/bin/sh")).args(["-c", "echo out; echo err >&2"]).log(&log).run().unwrap();
        assert_eq!(ok.stdout, "out\n");
        let err = Cmd::new(Path::new("/bin/sh")).args(["-c", "echo bad >&2; exit 3"]).log(&log).run().unwrap_err();
        assert_eq!(err.stderr, "bad\n");
        assert_eq!(err.to_string(), "sh -c: bad");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("$ sh -c echo out"));
        assert!(logged.contains("err\n"), "stderr is logged even on success");
        assert!(logged.contains("[exit exit status: 3]"));
    }

    #[test]
    fn run_reports_a_missing_program() {
        let err = Cmd::new(Path::new("/nonexistent/tool")).arg("x").run().unwrap_err();
        assert!(err.stderr.starts_with("cannot execute /nonexistent/tool"));
    }
}
```

- [ ] **Step 4: Write `fsutil.rs` and `quote.rs` with tests**

`crates/balerix-runtime/src/fsutil.rs`:
```rust
//! Atomic file writes with explicit modes.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub(crate) fn ensure_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)
}

/// Writes `contents` to `path` via a sibling temp file and rename, with
/// `mode` (e.g. `0o600`) applied before the rename.
pub(crate) fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("path has no parent"))?;
    ensure_dir(dir)?;
    let name = path.file_name().ok_or_else(|| io::Error::other("path has no file name"))?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    fs::write(&tmp, contents)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_with_mode_and_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("f");
        write_atomic(&p, b"one", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "one");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        write_atomic(&p, b"two", 0o644).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o644);
        assert_eq!(fs::read_dir(p.parent().unwrap()).unwrap().count(), 1, "no temp file left");
    }
}
```

`crates/balerix-runtime/src/quote.rs`:
```rust
//! POSIX `sh` single-quoting. The only way a value reaches `launch.sh`.

/// Wraps `s` in single quotes, escaping embedded single quotes as `'\''`.
/// Always quotes, even safe strings, so the output shape is uniform.
pub fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn quotes_plain_and_awkward_strings() {
        assert_eq!(sh_quote("abc"), "'abc'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote("$HOME `x` \"y\" \\"), "'$HOME `x` \"y\" \\'");
    }

    proptest! {
        /// Round-trips through a real shell: `sh -c "printf %s <quoted>"`
        /// must print the original bytes.
        #[test]
        fn round_trips_through_sh(s in "[^\\x00]{0,40}") {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s {}", sh_quote(&s)))
                .output()
                .unwrap();
            prop_assert!(out.status.success());
            prop_assert_eq!(String::from_utf8_lossy(&out.stdout), s);
        }
    }
}
```

- [ ] **Step 5: Run, lint, commit**

Run: `mise x -- cargo nextest run -p balerix-runtime && mise run lint`
Expected: all PASS (proptest runs 256 shell invocations; a few seconds).
```bash
git add Cargo.lock crates/balerix-runtime
git commit -m "Scaffold balerix-runtime: layout, tool discovery, logged subprocess runner, sh quoting

toml parses the system tool table (Task 11). All paths derive from an
injected StateLayout so tests run in a temp root; every subprocess is an
argv array and appends to a per-agent log.

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---

### Task 10: `balerix-runtime` — agent environment and `AgentHome`

**Files:**
- Create: `crates/balerix-runtime/src/env.rs`, `crates/balerix-runtime/src/home.rs`
- Modify: `crates/balerix-runtime/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub fn agent_env(id: &AgentId, paths: &AgentPaths, layout: &StateLayout, api_url: &str, user_env: &BTreeMap<String,String>) -> BTreeMap<String,String>;
  pub const HOOK_EVENTS: &[&str];
  pub fn render_settings(user: &Value, id: &AgentId, hooks: &HookTarget) -> Value;   // user settings with balerix's `hooks` block
  pub fn render_hosts_yml(token: &str) -> String;
  pub fn render_claude_json(account: Option<&Value>) -> Value;
  pub struct HomeInputs<'a> { pub settings: &'a Value, pub creds: &'a CredentialBundle, pub hooks: &'a HookTarget, pub with_gh: bool, pub redact_credentials: bool }
  pub fn write_home(id: &AgentId, paths: &AgentPaths, inputs: &HomeInputs) -> Result<(), MaterializeError>;
  ```

- [ ] **Step 1: Write `env.rs` with tests**

```rust
//! The agent's environment (spec §4 table). These become the nono profile's
//! `environment.set_vars`; `PATH` is set by nono and `mise exec`.

use std::collections::BTreeMap;

use balerix_core::AgentId;

use crate::layout::{AgentPaths, StateLayout};

pub fn agent_env(id: &AgentId, paths: &AgentPaths, layout: &StateLayout, api_url: &str, user_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let s = |p: &std::path::Path| p.display().to_string();
    let mut env: BTreeMap<String, String> = BTreeMap::from([
        ("HOME".to_string(), s(&paths.home)),
        ("XDG_CONFIG_HOME".to_string(), s(&paths.xdg_config())),
        ("XDG_DATA_HOME".to_string(), s(&paths.xdg_data())),
        ("XDG_STATE_HOME".to_string(), s(&paths.xdg_state())),
        ("XDG_CACHE_HOME".to_string(), s(&paths.xdg_cache())),
        ("CLAUDE_CONFIG_DIR".to_string(), s(&paths.claude_dir())),
        ("GH_CONFIG_DIR".to_string(), s(&paths.gh_dir())),
        ("MISE_GLOBAL_CONFIG_FILE".to_string(), s(&paths.mise_toml)),
        ("MISE_DATA_DIR".to_string(), s(&layout.mise_data_dir())),
        ("MISE_CONFIG_DIR".to_string(), s(&paths.mise_config_dir())),
        ("MISE_STATE_DIR".to_string(), s(&paths.mise_state_dir())),
        ("MISE_CACHE_DIR".to_string(), s(&paths.mise_cache_dir())),
        ("BALERIX_FLEET".to_string(), id.fleet.to_string()),
        ("BALERIX_CREW".to_string(), id.crew.to_string()),
        ("BALERIX_AGENT".to_string(), id.agent.to_string()),
        ("BALERIX_AGENT_ID".to_string(), id.to_string()),
        ("BALERIX_API_URL".to_string(), api_url.to_string()),
    ]);
    // Reserved keys were rejected at config validation; `entry().or_insert`
    // keeps the isolation rows authoritative even so.
    for (k, v) in user_env {
        env.entry(k.clone()).or_insert_with(|| v.clone());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn isolation_rows_come_first_and_user_env_cannot_override_them() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let id: AgentId = "payments/backend/alice".parse().unwrap();
        let paths = layout.agent(&id);
        let user = BTreeMap::from([("RUST_LOG".to_string(), "info".to_string()), ("HOME".to_string(), "/evil".to_string())]);
        let env = agent_env(&id, &paths, &layout, "https://127.0.0.1:7643", &user);
        assert_eq!(env["HOME"], "/h/.local/state/balerix/fleets/payments/crews/backend/agents/alice/home");
        assert_eq!(env["RUST_LOG"], "info");
        assert_eq!(env["BALERIX_AGENT_ID"], "payments/backend/alice");
        assert_eq!(env["MISE_DATA_DIR"], "/h/.local/share/balerix/mise");
        assert!(!env.contains_key("PATH"), "PATH is nono's");
        assert_eq!(env.len(), 18);
    }
}
```

- [ ] **Step 2: Write `home.rs` with tests**

```rust
//! The agent's `$HOME` (Phase 2 spec §4.2 step 2): Claude settings with
//! balerix's hooks, credentials, a seeded `.claude.json`, gh hosts.

use std::path::Path;

use balerix_api::CredentialBundle;
use balerix_core::{AgentId, HookTarget, MaterializeError};
use serde_json::{Map, Value, json};

use crate::fsutil::{ensure_dir, write_atomic};
use crate::layout::AgentPaths;

/// Every Claude Code hook event balerix listens to. Kept as one list so the
/// daemon and the settings writer agree.
pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "SubagentStop",
    "PreCompact",
];

pub const REDACTED: &str = "<redacted>";

/// User settings plus the balerix-owned `hooks` block. Any user `hooks` key
/// was rejected at validation; this overwrites unconditionally anyway.
pub fn render_settings(user: &Value, id: &AgentId, hooks: &HookTarget) -> Value {
    let mut settings = match user {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    let url = format!("{}/v1/agents/{}/{}/{}/events", hooks.url.trim_end_matches('/'), id.fleet, id.crew, id.agent);
    let entry = json!([{ "hooks": [{ "type": "http", "url": url, "headers": { "Authorization": format!("Bearer {}", hooks.secret) } }] }]);
    let block: Map<String, Value> = HOOK_EVENTS.iter().map(|e| (e.to_string(), entry.clone())).collect();
    settings.insert("hooks".to_string(), Value::Object(block));
    Value::Object(settings)
}

pub fn render_hosts_yml(token: &str) -> String {
    format!("github.com:\n    oauth_token: {token}\n    git_protocol: https\n")
}

/// Seed that suppresses first-run prompts, overlaid with the account fields
/// the bundle carried (`oauthAccount`, `hasCompletedOnboarding`).
pub fn render_claude_json(account: Option<&Value>) -> Value {
    let mut m = Map::new();
    m.insert("hasCompletedOnboarding".to_string(), json!(true));
    if let Some(Value::Object(a)) = account {
        for (k, v) in a {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

pub struct HomeInputs<'a> {
    pub settings: &'a Value,
    pub creds: &'a CredentialBundle,
    pub hooks: &'a HookTarget,
    pub with_gh: bool,
    pub redact_credentials: bool,
}

fn io_err(id: &AgentId, path: &Path, e: std::io::Error) -> MaterializeError {
    MaterializeError::Io { id: id.to_string(), path: path.to_path_buf(), message: e.to_string() }
}

pub fn write_home(id: &AgentId, paths: &AgentPaths, inputs: &HomeInputs) -> Result<(), MaterializeError> {
    for d in [paths.home.clone(), paths.claude_dir(), paths.gh_dir(), paths.xdg_data(), paths.xdg_state(), paths.xdg_cache(), paths.nono_home.clone(), paths.logs.clone()] {
        ensure_dir(&d).map_err(|e| io_err(id, &d, e))?;
    }
    let pretty = |v: &Value| serde_json::to_vec_pretty(v).unwrap_or_default();

    let settings_path = paths.claude_dir().join("settings.json");
    write_atomic(&settings_path, &pretty(&render_settings(inputs.settings, id, inputs.hooks)), 0o644).map_err(|e| io_err(id, &settings_path, e))?;

    let creds_path = paths.claude_dir().join(".credentials.json");
    match (&inputs.creds.claude_credentials, inputs.redact_credentials) {
        (Some(_), true) => write_atomic(&creds_path, &pretty(&json!(REDACTED)), 0o600),
        (Some(c), false) => write_atomic(&creds_path, &pretty(c), 0o600),
        (None, _) => Ok(()),
    }
    .map_err(|e| io_err(id, &creds_path, e))?;

    let claude_json = paths.home.join(".claude.json");
    write_atomic(&claude_json, &pretty(&render_claude_json(inputs.creds.claude_account.as_ref())), 0o600).map_err(|e| io_err(id, &claude_json, e))?;

    if inputs.with_gh {
        if let Some(token) = &inputs.creds.gh_token {
            let hosts = paths.gh_dir().join("hosts.yml");
            let token = if inputs.redact_credentials { REDACTED } else { token.as_str() };
            write_atomic(&hosts, render_hosts_yml(token).as_bytes(), 0o600).map_err(|e| io_err(id, &hosts, e))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::StateLayout;
    use std::os::unix::fs::PermissionsExt;

    fn id() -> AgentId {
        "payments/backend/alice".parse().unwrap()
    }
    fn hooks() -> HookTarget {
        HookTarget { url: "https://127.0.0.1:7643/".into(), secret: "s3".into() }
    }

    #[test]
    fn settings_get_a_hooks_block_for_every_event() {
        let v = render_settings(&json!({ "model": "opus", "hooks": { "Stop": [] } }), &id(), &hooks());
        assert_eq!(v["model"], "opus");
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), HOOK_EVENTS.len());
        let h = &hooks["PreToolUse"][0]["hooks"][0];
        assert_eq!(h["type"], "http");
        assert_eq!(h["url"], "https://127.0.0.1:7643/v1/agents/payments/backend/alice/events");
        assert_eq!(h["headers"]["Authorization"], "Bearer s3");
        assert_eq!(hooks["Stop"], hooks["PreToolUse"], "user hooks are replaced");
    }

    #[test]
    fn hosts_yml_and_claude_json_render() {
        assert_eq!(render_hosts_yml("gho_x"), "github.com:\n    oauth_token: gho_x\n    git_protocol: https\n");
        let v = render_claude_json(Some(&json!({ "oauthAccount": { "emailAddress": "a@b.c" }, "hasCompletedOnboarding": true })));
        assert_eq!(v["hasCompletedOnboarding"], true);
        assert_eq!(v["oauthAccount"]["emailAddress"], "a@b.c");
        assert_eq!(render_claude_json(None), json!({ "hasCompletedOnboarding": true }));
    }

    fn creds() -> CredentialBundle {
        CredentialBundle {
            claude_credentials: Some(json!({ "claudeAiOauth": { "accessToken": "sk-SECRET" } })),
            claude_account: Some(json!({ "oauthAccount": { "emailAddress": "a@b.c" } })),
            gh_token: Some("gho_SECRET".into()),
        }
    }

    #[test]
    fn writes_every_file_with_the_right_mode() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::from_env(dir.path(), |_| None);
        let paths = layout.agent(&id());
        let c = creds();
        write_home(&id(), &paths, &HomeInputs { settings: &json!({ "model": "opus" }), creds: &c, hooks: &hooks(), with_gh: true, redact_credentials: false }).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&paths.claude_dir().join("settings.json")), 0o644);
        assert_eq!(mode(&paths.claude_dir().join(".credentials.json")), 0o600);
        assert_eq!(mode(&paths.gh_dir().join("hosts.yml")), 0o600);
        assert!(std::fs::read_to_string(paths.claude_dir().join(".credentials.json")).unwrap().contains("sk-SECRET"));
        assert!(std::fs::read_to_string(paths.gh_dir().join("hosts.yml")).unwrap().contains("gho_SECRET"));
        assert!(paths.nono_home.is_dir());
        assert!(paths.logs.is_dir());
    }

    #[test]
    fn redaction_replaces_secrets_and_no_gh_means_no_hosts_file() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::from_env(dir.path(), |_| None);
        let paths = layout.agent(&id());
        let c = creds();
        write_home(&id(), &paths, &HomeInputs { settings: &json!({}), creds: &c, hooks: &hooks(), with_gh: false, redact_credentials: true }).unwrap();
        let creds_file = std::fs::read_to_string(paths.claude_dir().join(".credentials.json")).unwrap();
        assert!(!creds_file.contains("SECRET"));
        assert!(creds_file.contains(REDACTED));
        assert!(!paths.gh_dir().join("hosts.yml").exists());
    }
}
```

`lib.rs`: add `pub mod env; pub mod home;` and `pub use env::agent_env; pub use home::{HOOK_EVENTS, HomeInputs, write_home};`.

- [ ] **Step 3: Run, lint, commit**

Run: `mise x -- cargo nextest run -p balerix-runtime && mise run lint`
```bash
git add crates/balerix-runtime
git commit -m "Render the agent environment and write the agent home

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---
### Task 11: `balerix-runtime` — `Toolchain` (mise) with an offline integration test

**Files:**
- Create: `crates/balerix-runtime/src/toolchain.rs`, `crates/balerix-runtime/tests/support/mod.rs`, `crates/balerix-runtime/tests/toolchain_it.rs`
- Modify: `crates/balerix-runtime/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub const EMBEDDED_MISE_TOML: &str = include_str!("../../../mise.toml");   // this repo's pins
  pub fn embedded_system_tools() -> BTreeMap<String, String>;               // { claude: <pin>, gh: <pin> } from the embedded file
  pub fn system_tools(layout: &StateLayout, id: &AgentId) -> Result<BTreeMap<String,String>, MaterializeError>; // config_root/mise.toml [tools] if present, else embedded
  pub fn render_mise_toml(id: &AgentId, system: &BTreeMap<String,String>, user: &BTreeMap<String,String>, with_gh: bool) -> String;
  pub fn mise_env(paths: &AgentPaths, layout: &StateLayout) -> BTreeMap<String,String>;  // MISE_* for install and exec
  pub struct Toolchain<'a> { pub tools: &'a ToolPaths, pub layout: &'a StateLayout }
  impl Toolchain<'_> { pub fn write(&self, id, paths, system, user, with_gh) -> Result<bool /*changed*/, MaterializeError>;
                       pub fn install(&self, id, paths) -> Result<(), MaterializeError> }   // mise trust + mise install
  ```
  Test support (used by every later `_it.rs`):
  ```rust
  pub fn tools() -> Option<ToolPaths>;                 // discover on PATH; None → skip
  pub fn require_or_skip(name: &str, present: bool) -> bool; // prints "skip: <name>" and returns false, or panics under BALERIX_REQUIRE_TOOLS=1
  pub fn temp_root(test: &str) -> PathBuf;             // fresh dir under CARGO_TARGET_TMPDIR (NOT /tmp — nono grants /tmp by default)
  pub fn layout(root: &Path) -> StateLayout;           // state/data/config under root
  ```

- [ ] **Step 1: Write `toolchain.rs` with unit tests**

```rust
//! The agent's tool table (Phase 2 spec §4.2 step 3, P2-8): system pins ⊕
//! user `tools`, installed by the daemon into the shared data dir.

use std::collections::BTreeMap;
use std::path::Path;

use balerix_core::{AgentId, MaterializeError};

use crate::fsutil::write_atomic;
use crate::layout::{AgentPaths, StateLayout};
use crate::tools::{Cmd, ToolPaths};

/// This repository's own pins, embedded at build time so a fresh install can
/// launch `claude` without the user writing a system tool table first.
pub const EMBEDDED_MISE_TOML: &str = include_str!("../../../mise.toml");

/// Tools an agent inherits from the embedded table. `gh` is only emitted
/// when the crew authenticates through it.
const INHERITED: &[&str] = &["claude", "gh"];

fn tools_table(toml_text: &str) -> Result<BTreeMap<String, String>, String> {
    let doc: toml::Table = toml_text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let mut out = BTreeMap::new();
    if let Some(toml::Value::Table(tools)) = doc.get("tools") {
        for (k, v) in tools {
            let version = match v {
                toml::Value::String(s) => s.clone(),
                toml::Value::Table(t) => match t.get("version") {
                    Some(toml::Value::String(s)) => s.clone(),
                    _ => return Err(format!("tools.{k}: expected a version string")),
                },
                _ => return Err(format!("tools.{k}: expected a version string")),
            };
            out.insert(k.clone(), version);
        }
    }
    Ok(out)
}

pub fn embedded_system_tools() -> BTreeMap<String, String> {
    tools_table(EMBEDDED_MISE_TOML)
        .unwrap_or_default()
        .into_iter()
        .filter(|(k, _)| INHERITED.contains(&k.as_str()))
        .collect()
}

/// `$XDG_CONFIG_HOME/balerix/mise.toml` `[tools]` if the file exists, else
/// the embedded default.
pub fn system_tools(layout: &StateLayout, id: &AgentId) -> Result<BTreeMap<String, String>, MaterializeError> {
    let path = layout.system_mise_toml();
    match std::fs::read_to_string(&path) {
        Ok(text) => tools_table(&text).map_err(|message| MaterializeError::Io { id: id.to_string(), path, message }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(embedded_system_tools()),
        Err(e) => Err(MaterializeError::Io { id: id.to_string(), path, message: e.to_string() }),
    }
}

pub fn render_mise_toml(id: &AgentId, system: &BTreeMap<String, String>, user: &BTreeMap<String, String>, with_gh: bool) -> String {
    let mut table: BTreeMap<&str, &str> = system.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    if !with_gh {
        table.remove("gh");
    }
    for (k, v) in user {
        table.insert(k, v);
    }
    let mut out = format!("# generated by balerix for {id}; edit the fleet file, not this\n[tools]\n");
    for (k, v) in table {
        out.push_str(&format!("{} = {:?}\n", toml_key(k), v));
    }
    out
}

fn toml_key(k: &str) -> String {
    if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') { k.to_string() } else { format!("{k:?}") }
}

pub fn mise_env(paths: &AgentPaths, layout: &StateLayout) -> BTreeMap<String, String> {
    let s = |p: std::path::PathBuf| p.display().to_string();
    BTreeMap::from([
        ("MISE_GLOBAL_CONFIG_FILE".to_string(), s(paths.mise_toml.clone())),
        ("MISE_DATA_DIR".to_string(), s(layout.mise_data_dir())),
        ("MISE_CONFIG_DIR".to_string(), s(paths.mise_config_dir())),
        ("MISE_STATE_DIR".to_string(), s(paths.mise_state_dir())),
        ("MISE_CACHE_DIR".to_string(), s(paths.mise_cache_dir())),
        ("MISE_YES".to_string(), "1".to_string()),
        ("MISE_QUIET".to_string(), "1".to_string()),
        // never install from inside the sandbox; the daemon installs beforehand
        ("MISE_AUTO_INSTALL".to_string(), "false".to_string()),
    ])
}

pub struct Toolchain<'a> {
    pub tools: &'a ToolPaths,
    pub layout: &'a StateLayout,
}

impl Toolchain<'_> {
    /// Writes `mise.toml`; returns whether its content changed.
    pub fn write(&self, id: &AgentId, paths: &AgentPaths, system: &BTreeMap<String, String>, user: &BTreeMap<String, String>, with_gh: bool) -> Result<bool, MaterializeError> {
        let text = render_mise_toml(id, system, user, with_gh);
        if std::fs::read_to_string(&paths.mise_toml).ok().as_deref() == Some(text.as_str()) {
            return Ok(false);
        }
        write_atomic(&paths.mise_toml, text.as_bytes(), 0o644)
            .map_err(|e| MaterializeError::Io { id: id.to_string(), path: paths.mise_toml.clone(), message: e.to_string() })?;
        Ok(true)
    }

    /// `mise trust` then `mise install`, unsandboxed, into the shared data dir.
    /// Runs with `cwd=/` so no project `mise.toml` on the way up (the repo
    /// balerix itself sits in, during tests) is discovered: only the agent's
    /// global file counts.
    pub fn install(&self, id: &AgentId, paths: &AgentPaths) -> Result<(), MaterializeError> {
        let env = mise_env(paths, self.layout);
        let log = paths.logs.join("mise.toolchain.log");
        let run = |args: &[&str]| {
            Cmd::new(&self.tools.mise).args(args.iter().copied()).envs(&env).cwd(Path::new("/")).log(&log).run().map(|_| ()).map_err(|f| {
                MaterializeError::Tool { id: id.to_string(), tool: f.tool, subcommand: f.subcommand, args: f.args, stderr: f.stderr }
            })
        };
        run(&["trust", &paths.mise_toml.display().to_string()])?;
        run(&["install"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> AgentId {
        "payments/backend/alice".parse().unwrap()
    }

    #[test]
    fn embedded_table_carries_only_claude_and_gh_pins() {
        let t = embedded_system_tools();
        assert_eq!(t.keys().collect::<Vec<_>>(), vec!["claude", "gh"]);
        assert!(t["claude"].chars().next().unwrap().is_ascii_digit());
    }

    #[test]
    fn tools_table_accepts_string_and_table_forms() {
        let t = tools_table("[tools]\nrust = { version = \"1.98.1\", components = [\"clippy\"] }\n\"cargo:x\" = \"1.0.0\"\n").unwrap();
        assert_eq!(t["rust"], "1.98.1");
        assert_eq!(t["cargo:x"], "1.0.0");
        assert!(tools_table("[tools]\nx = 3\n").unwrap_err().starts_with("tools.x"));
    }

    #[test]
    fn render_merges_user_over_system_and_drops_gh_without_auth() {
        let system = BTreeMap::from([("claude".to_string(), "2.0.0".to_string()), ("gh".to_string(), "2.100.0".to_string())]);
        let user = BTreeMap::from([("node".to_string(), "22.11.0".to_string()), ("claude".to_string(), "2.1.0".to_string())]);
        let with = render_mise_toml(&id(), &system, &user, true);
        assert_eq!(with, "# generated by balerix for payments/backend/alice; edit the fleet file, not this\n[tools]\nclaude = \"2.1.0\"\ngh = \"2.100.0\"\nnode = \"22.11.0\"\n");
        let without = render_mise_toml(&id(), &system, &user, false);
        assert!(!without.contains("gh ="));
        let quoted = render_mise_toml(&id(), &BTreeMap::new(), &BTreeMap::from([("cargo:x".to_string(), "1.0.0".to_string())]), false);
        assert!(quoted.contains("\"cargo:x\" = \"1.0.0\""));
    }

    #[test]
    fn system_tools_prefers_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::from_env(dir.path(), |_| None);
        assert_eq!(system_tools(&layout, &id()).unwrap(), embedded_system_tools());
        std::fs::create_dir_all(&layout.config_root).unwrap();
        std::fs::write(layout.system_mise_toml(), "[tools]\nclaude = \"9.9.9\"\n").unwrap();
        assert_eq!(system_tools(&layout, &id()).unwrap()["claude"], "9.9.9");
    }

    #[test]
    fn write_reports_changes() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::from_env(dir.path(), |_| None);
        let tools = ToolPaths { git: "/g".into(), gh: "/h".into(), mise: "/m".into(), nono: "/n".into(), tmux: "/t".into() };
        let tc = Toolchain { tools: &tools, layout: &layout };
        let paths = layout.agent(&id());
        let sys = embedded_system_tools();
        assert!(tc.write(&id(), &paths, &sys, &BTreeMap::new(), false).unwrap());
        assert!(!tc.write(&id(), &paths, &sys, &BTreeMap::new(), false).unwrap());
        assert!(tc.write(&id(), &paths, &sys, &BTreeMap::new(), true).unwrap());
    }
}
```

`lib.rs`: add `pub mod toolchain;` and `pub use toolchain::{Toolchain, embedded_system_tools, mise_env, render_mise_toml, system_tools};`.

- [ ] **Step 2: Write the shared test support**

`crates/balerix-runtime/tests/support/mod.rs`:
```rust
//! Shared helpers for the integration tests that drive real tools.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use balerix_runtime::{StateLayout, ToolPaths};

/// Tools from the test process's PATH (mise puts the pinned ones there).
pub fn tools() -> Option<ToolPaths> {
    ToolPaths::discover_in(&std::env::var_os("PATH").unwrap_or_default()).ok()
}

/// Returns `true` if the test may run. Otherwise prints a skip reason, or
/// panics when `BALERIX_REQUIRE_TOOLS=1` (CI never skips).
pub fn require_or_skip(name: &str, present: bool) -> bool {
    if present {
        return true;
    }
    if std::env::var_os("BALERIX_REQUIRE_TOOLS").is_some_and(|v| v == "1") {
        panic!("{name} is required (BALERIX_REQUIRE_TOOLS=1) but not available");
    }
    eprintln!("skip: {name} not available");
    false
}

/// A fresh directory under `target/tmp`. Deliberately not `/tmp`: nono's
/// built-in groups grant `/tmp`, so a sandbox-escape assertion there would
/// pass vacuously.
pub fn temp_root(test: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

pub fn layout(root: &Path) -> StateLayout {
    StateLayout { state_root: root.join("state"), data_root: root.join("data"), config_root: root.join("config") }
}

/// True when Landlock is usable: `nono run` of `true` succeeds.
pub fn landlock_works(tools: &ToolPaths, root: &Path) -> bool {
    let home = root.join("nono-probe-home");
    std::fs::create_dir_all(&home).unwrap();
    std::process::Command::new(&tools.nono)
        .args(["-s", "run", "--", "/bin/true"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .current_dir(root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
```

- [ ] **Step 3: Write the toolchain integration test**

The shared data dir is pre-seeded by copying the host's installed `gh` (whatever version the repo pins), so `mise install` is a no-op and `mise exec` must resolve it read-only. This also settles Phase 2 spec §4.4 row 2.

`crates/balerix-runtime/tests/toolchain_it.rs`:
```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::process::Command;

use balerix_core::AgentId;
use balerix_runtime::{Toolchain, embedded_system_tools, mise_env};

/// Copies the host's `gh@<pin>` install into the shared data dir. Returns
/// false if the host does not have it.
fn seed_gh(tools: &balerix_runtime::ToolPaths, layout: &balerix_runtime::StateLayout, version: &str) -> bool {
    let out = Command::new(&tools.mise).args(["where", &format!("gh@{version}")]).output().unwrap();
    if !out.status.success() {
        return false;
    }
    let src = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dst = layout.mise_data_dir().join("installs").join("gh").join(version);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    let status = Command::new("cp").args(["-r", &src, &dst.display().to_string()]).status().unwrap();
    status.success()
}

#[test]
fn installs_nothing_when_seeded_and_exec_resolves_read_only() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise+gh", false));
        return;
    };
    let root = support::temp_root("toolchain");
    let layout = support::layout(&root);
    let system = embedded_system_tools();
    let gh_version = system["gh"].clone();
    if !support::require_or_skip("gh install to seed from", seed_gh(&tools, &layout, &gh_version)) {
        return;
    }
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    std::fs::create_dir_all(&paths.root).unwrap();
    let tc = Toolchain { tools: &tools, layout: &layout };
    // only gh: claude is not seeded and must not be attempted
    let only_gh: BTreeMap<String, String> = BTreeMap::from([("gh".to_string(), gh_version.clone())]);
    tc.write(&id, &paths, &only_gh, &BTreeMap::new(), true).unwrap();
    tc.install(&id, &paths).unwrap();

    // read-only shared dir: does `mise exec` still resolve? (spec §4.4 row 2)
    let ro = |on: bool| {
        let mode = if on { "a-w" } else { "u+w" };
        assert!(Command::new("chmod").args(["-R", mode, &layout.mise_data_dir().display().to_string()]).status().unwrap().success());
    };
    ro(true);
    let out = Command::new(&tools.mise)
        .args(["exec", "--", "gh", "--version"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &paths.home)
        .envs(&mise_env(&paths, &layout))
        .current_dir("/")
        .output()
        .unwrap();
    ro(false);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "mise exec failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains(&gh_version), "resolved {stdout}");
    assert!(paths.logs.join("mise.toolchain.log").exists());
}
```

- [ ] **Step 4: Run, record the §4.4 verdict, commit**

Run: `mise x -- cargo nextest run -p balerix-runtime`
Expected: PASS. If `mise exec` fails on the read-only data dir, the fallback in Phase 2 spec §4.4 applies: find what mise wants to write (`strace -f -e trace=openat` or the stderr) and either point it at `home/` via another `MISE_*` variable or grant that subdir; then note the outcome.

Record the verdict now: in `docs/superpowers/specs/2026-09-06-balerix-a2-runtime-design.md` §4.4, append to the `mise exec` row a final sentence *Verdict (Task 11): <holds | fallback used: …>*.

```bash
git add crates/balerix-runtime docs/superpowers/specs/2026-09-06-balerix-a2-runtime-design.md
git commit -m "Render and install the agent tool table with mise

Embeds this repo's claude/gh pins as the default system table (P2-8).
The integration test seeds the shared data dir from the host's gh
install so it runs offline and proves read-only resolution.

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---

### Task 12: `balerix-runtime` — `SandboxProfile` (nono)

**Files:**
- Create: `crates/balerix-runtime/src/sandbox.rs`, `crates/balerix-runtime/tests/sandbox_it.rs`
- Modify: `crates/balerix-runtime/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct Grants { pub read: Vec<PathBuf>, pub allow: Vec<PathBuf> }           // balerix's required paths
  pub fn balerix_grants(paths: &AgentPaths, crew: &CrewPaths, layout: &StateLayout) -> Grants;
  pub fn render_profile(id: &AgentId, grants: &Grants, daemon_port: u16, env: &BTreeMap<String,String>, user: &Value) -> Result<Value, MaterializeError>;
  pub fn merge_profile(base: Value, user: &Value) -> Value;     // nono extends semantics
  pub fn check_conflicts(id: &AgentId, user: &Value, grants: &Grants) -> Result<(), MaterializeError>;
  pub fn write_profile(id: &AgentId, paths: &AgentPaths, profile: &Value) -> Result<(), MaterializeError>;
  pub fn validate_profile(tools: &ToolPaths, id: &AgentId, paths: &AgentPaths) -> Result<(), MaterializeError>;  // nono profile validate
  ```

- [ ] **Step 1: Write `sandbox.rs` with unit tests**

```rust
//! The generated nono profile (Phase 2 spec §4.2 step 4, P2-5, P2-7).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use balerix_core::{AgentId, MaterializeError};
use serde_json::{Value, json};

use crate::fsutil::write_atomic;
use crate::layout::{AgentPaths, CrewPaths, StateLayout};
use crate::tools::{Cmd, ToolPaths};

const SYSTEM_READ: &[&str] = &["/usr", "/lib", "/lib64", "/bin", "/etc"];

/// Paths balerix grants; user grants that overlap these at a different
/// level are rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grants {
    pub read: Vec<PathBuf>,
    pub allow: Vec<PathBuf>,
}

pub fn balerix_grants(paths: &AgentPaths, crew: &CrewPaths, layout: &StateLayout) -> Grants {
    let mut read: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
    read.push(layout.mise_data_dir());
    Grants { read, allow: vec![paths.home.clone(), paths.workspace.clone(), crew.repo.join(".git")] }
}

fn strs(paths: &[PathBuf]) -> Value {
    Value::Array(paths.iter().map(|p| Value::String(p.display().to_string())).collect())
}

/// Base profile ⊕ user block, conflicts rejected first.
pub fn render_profile(id: &AgentId, grants: &Grants, daemon_port: u16, env: &BTreeMap<String, String>, user: &Value) -> Result<Value, MaterializeError> {
    check_conflicts(id, user, grants)?;
    let base = json!({
        "meta": { "name": format!("balerix-{}-{}-{}", id.fleet, id.crew, id.agent), "description": format!("generated by balerix for {id}") },
        "filesystem": { "read": strs(&grants.read), "allow": strs(&grants.allow) },
        "workdir": { "access": "none" },
        "network": { "connect_port": [daemon_port] },
        "environment": { "deny_vars": ["*"], "set_vars": env },
    });
    Ok(merge_profile(base, user))
}

/// nono's `extends` rules: arrays append and dedupe, maps recurse, scalars
/// user-wins.
pub fn merge_profile(base: Value, user: &Value) -> Value {
    match (base, user) {
        (Value::Object(mut b), Value::Object(u)) => {
            for (k, v) in u {
                let merged = match b.remove(k) {
                    Some(existing) => merge_profile(existing, v),
                    None => v.clone(),
                };
                b.insert(k.clone(), merged);
            }
            Value::Object(b)
        }
        (Value::Array(mut b), Value::Array(u)) => {
            for v in u {
                if !b.contains(v) {
                    b.push(v.clone());
                }
            }
            Value::Array(b)
        }
        (_, u) => u.clone(),
    }
}

/// Access level of a grant list key.
fn level(key: &str) -> Option<&'static str> {
    match key {
        "read" | "read_file" => Some("read"),
        "allow" | "allow_file" => Some("allow"),
        "write" | "write_file" => Some("write"),
        _ => None,
    }
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

fn entry_path(v: &Value) -> Option<PathBuf> {
    match v {
        Value::String(s) => Some(PathBuf::from(s)),
        Value::Object(m) => m.get("path").and_then(Value::as_str).map(PathBuf::from),
        _ => None,
    }
}

pub fn check_conflicts(id: &AgentId, user: &Value, grants: &Grants) -> Result<(), MaterializeError> {
    let conflict = |path: String, message: String| MaterializeError::SandboxConflict { id: id.to_string(), path, message };
    let Value::Object(u) = user else {
        return Ok(());
    };
    if u.contains_key("environment") {
        return Err(conflict("environment".into(), "balerix-owned; set variables through `env` instead".into()));
    }
    if u.contains_key("meta") {
        return Err(conflict("meta".into(), "balerix-owned".into()));
    }
    let Some(Value::Object(fs)) = u.get("filesystem") else {
        return Ok(());
    };
    for (key, entries) in fs {
        let Some(user_level) = level(key) else {
            continue;
        };
        let Value::Array(items) = entries else {
            continue;
        };
        for (i, item) in items.iter().enumerate() {
            let Some(p) = entry_path(item) else {
                continue;
            };
            let hits = grants.read.iter().map(|g| (g, "read")).chain(grants.allow.iter().map(|g| (g, "allow")));
            for (g, balerix_level) in hits {
                if overlaps(&p, g) && balerix_level != user_level {
                    return Err(conflict(
                        format!("filesystem.{key}[{i}]"),
                        format!("{} overlaps balerix {balerix_level} path {}", p.display(), g.display()),
                    ));
                }
            }
        }
    }
    Ok(())
}

pub fn write_profile(id: &AgentId, paths: &AgentPaths, profile: &Value) -> Result<(), MaterializeError> {
    let bytes = serde_json::to_vec_pretty(profile).unwrap_or_default();
    write_atomic(&paths.profile, &bytes, 0o644).map_err(|e| MaterializeError::Io { id: id.to_string(), path: paths.profile.clone(), message: e.to_string() })
}

pub fn validate_profile(tools: &ToolPaths, id: &AgentId, paths: &AgentPaths) -> Result<(), MaterializeError> {
    Cmd::new(&tools.nono)
        .args(["-s", "profile", "validate", &paths.profile.display().to_string()])
        .env("HOME", paths.nono_home.display().to_string())
        .log(&paths.logs.join("nono.validate.log"))
        .run()
        .map(|_| ())
        .map_err(|f| MaterializeError::Tool { id: id.to_string(), tool: f.tool, subcommand: "profile validate".into(), args: f.args, stderr: if f.stderr.trim().is_empty() { "profile invalid".into() } else { f.stderr } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (AgentId, Grants, BTreeMap<String, String>) {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let id: AgentId = "f/c/a".parse().unwrap();
        let paths = layout.agent(&id);
        let crew = layout.crew(&id.crew_ref());
        let grants = balerix_grants(&paths, &crew, &layout);
        (id, grants, BTreeMap::from([("HOME".to_string(), paths.home.display().to_string())]))
    }

    #[test]
    fn base_profile_has_the_required_grants_env_and_port() {
        let (id, grants, env) = fixture();
        let p = render_profile(&id, &grants, 7643, &env, &json!({})).unwrap();
        assert_eq!(p["meta"]["name"], "balerix-f-c-a");
        assert_eq!(p["filesystem"]["read"][0], "/usr");
        assert_eq!(p["filesystem"]["read"][5], "/h/.local/share/balerix/mise");
        assert_eq!(p["filesystem"]["allow"][2], "/h/.local/state/balerix/fleets/f/crews/c/repo/.git");
        assert_eq!(p["workdir"]["access"], "none");
        assert_eq!(p["network"]["connect_port"], json!([7643]));
        assert_eq!(p["environment"]["deny_vars"], json!(["*"]));
        assert_eq!(p["environment"]["set_vars"]["HOME"], "/h/.local/state/balerix/fleets/f/crews/c/agents/a/home");
    }

    #[test]
    fn user_block_merges_with_extends_semantics() {
        let (id, grants, env) = fixture();
        let user = json!({ "network": { "block": true, "connect_port": [7643, 8080] }, "filesystem": { "read": ["/opt/data"] }, "extends": "default" });
        let p = render_profile(&id, &grants, 7643, &env, &user).unwrap();
        assert_eq!(p["network"]["block"], true);
        assert_eq!(p["network"]["connect_port"], json!([7643, 8080]), "append + dedupe");
        assert_eq!(p["filesystem"]["read"].as_array().unwrap().last().unwrap(), "/opt/data");
        assert_eq!(p["extends"], "default");
    }

    #[test]
    fn owned_keys_and_overlapping_grants_are_rejected_with_paths() {
        let (id, grants, env) = fixture();
        let e = render_profile(&id, &grants, 1, &env, &json!({ "environment": { "set_vars": { "X": "1" } } })).unwrap_err();
        assert_eq!(e.to_string(), "f/c/a: sandbox.environment: balerix-owned; set variables through `env` instead");
        let e = render_profile(&id, &grants, 1, &env, &json!({ "filesystem": { "read": ["/x", "/h/.local/state/balerix/fleets/f/crews/c/agents/a/home/.claude"] } })).unwrap_err();
        assert_eq!(
            e.to_string(),
            "f/c/a: sandbox.filesystem.read[1]: /h/.local/state/balerix/fleets/f/crews/c/agents/a/home/.claude overlaps balerix allow path /h/.local/state/balerix/fleets/f/crews/c/agents/a/home"
        );
        let e = render_profile(&id, &grants, 1, &env, &json!({ "filesystem": { "allow": [{ "path": "/usr/local" }] } })).unwrap_err();
        assert!(e.to_string().contains("overlaps balerix read path /usr"));
        // same level is fine
        assert!(render_profile(&id, &grants, 1, &env, &json!({ "filesystem": { "read": ["/usr/share"] } })).is_ok());
    }

    #[test]
    fn merge_is_idempotent() {
        let base = json!({ "a": [1, 2], "b": { "c": 1 } });
        let user = json!({ "a": [2, 3], "b": { "d": 2 } });
        let once = merge_profile(base.clone(), &user);
        let twice = merge_profile(once.clone(), &user);
        assert_eq!(once, twice);
        assert_eq!(once, json!({ "a": [1, 2, 3], "b": { "c": 1, "d": 2 } }));
    }
}
```
`lib.rs`: add `pub mod sandbox;` and `pub use sandbox::{Grants, check_conflicts, balerix_grants, merge_profile, render_profile, validate_profile, write_profile};`.

- [ ] **Step 2: Write the nono integration test**

`crates/balerix-runtime/tests/sandbox_it.rs`:
```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::process::Command;

use balerix_core::AgentId;
use balerix_runtime::{agent_env, balerix_grants, render_profile, validate_profile, write_profile};

#[test]
fn generated_profile_validates_and_enforces_isolation() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("nono", false));
        return;
    };
    let root = support::temp_root("sandbox");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    let crew = layout.crew(&id.crew_ref());
    for d in [&paths.home, &paths.workspace, &paths.nono_home, &paths.logs, &crew.repo.join(".git"), &layout.mise_data_dir()] {
        std::fs::create_dir_all(d).unwrap();
    }
    let env = agent_env(&id, &paths, &layout, "https://127.0.0.1:7643", &Default::default());
    let profile = render_profile(&id, &balerix_grants(&paths, &crew, &layout), 7643, &env, &serde_json::json!({})).unwrap();
    write_profile(&id, &paths, &profile).unwrap();
    validate_profile(&tools, &id, &paths).unwrap();

    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let script = format!(
        "echo in > \"$HOME/ok\" && echo HOME=$HOME && echo FOO=$FOO && (echo x > {}/nope 2>/dev/null && echo ESCAPED || echo denied)",
        outside.display()
    );
    let out = Command::new(&tools.nono)
        .args(["-s", "run", "--profile", &paths.profile.display().to_string(), "--", "/bin/sh", "-c", &script])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &paths.nono_home)
        .env("FOO", "leak")
        .current_dir(&paths.workspace)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "nono run failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains(&format!("HOME={}", paths.home.display())), "HOME relocated via set_vars: {stdout}");
    assert!(stdout.contains("FOO=\n"), "outer env stripped by deny_vars: {stdout}");
    assert!(stdout.contains("denied"), "write outside grants must fail: {stdout}");
    assert!(paths.home.join("ok").exists(), "write inside home succeeds");
    assert!(!outside.join("nope").exists());
}
```

- [ ] **Step 3: Run, lint, commit**

Run: `mise x -- cargo nextest run -p balerix-runtime && mise run lint`
Expected: PASS, including `sandbox_it` here (Landlock is available in the devcontainer).
```bash
git add crates/balerix-runtime
git commit -m "Generate, merge and validate the nono profile

The agent environment travels in environment.set_vars with deny_vars
[\"*\"] (P2-5); user sandbox blocks merge with nono's extends rules and
conflicts with balerix's grants are rejected with a path.

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---
### Task 13: `balerix-runtime` — `launch.sh`, `Runtime::render_agent`, generated-files golden test

**Files:**
- Create: `crates/balerix-runtime/src/launch.rs`, `crates/balerix-runtime/src/materializer.rs`, `crates/balerix-runtime/tests/generated_golden.rs`
- Modify: `crates/balerix-runtime/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub fn outer_path(tools: &ToolPaths) -> String;   // "/usr/local/bin:/usr/bin:/bin:<dir of mise>"
  pub fn wants_continue(settings: &ClaudeSettings, paths: &AgentPaths) -> bool;  // resume && projects dir non-empty
  pub fn render_launch(paths: &AgentPaths, tools: &ToolPaths, binary: &str, args: &[String], resume: bool) -> (String, LaunchPlan);
  pub fn write_launch(id: &AgentId, paths: &AgentPaths, script: &str) -> Result<(), MaterializeError>;  // 0755
  pub fn hooks_port(url: &str) -> u16;   // port in the URL, else 443 for https, 80 for http
  pub struct Runtime { pub layout: StateLayout, pub tools: ToolPaths }
  pub struct RenderOptions { pub redact_credentials: bool }
  impl Runtime {
      pub fn new(layout: StateLayout, tools: ToolPaths) -> Self;
      /// Steps 2, 3 (write only), 4 (render only), 5. No subprocess.
      pub fn render_agent(&self, agent: &ResolvedAgent, creds: &CredentialBundle, hooks: &HookTarget, opts: &RenderOptions) -> Result<LaunchPlan, MaterializeError>;
      /// `mise install` + `nono profile validate`.
      pub fn install_and_validate(&self, agent: &ResolvedAgent) -> Result<(), MaterializeError>;
  }
  ```
  `impl Materializer for Runtime` comes in Task 15 (it needs Task 14's git).

- [ ] **Step 1: Write `launch.rs` with unit tests**

```rust
//! `launch.sh` and the `LaunchPlan` (Phase 2 spec §4.2 step 5). No
//! credential is ever an input here.

use balerix_api::ClaudeSettings;
use balerix_core::{AgentId, LaunchPlan, MaterializeError};
use std::collections::BTreeMap;

use crate::fsutil::write_atomic;
use crate::layout::AgentPaths;
use crate::quote::sh_quote;
use crate::tools::ToolPaths;

/// The outer `PATH`: system dirs plus wherever `mise` lives, so `mise exec`
/// inside the sandbox can find itself.
pub fn outer_path(tools: &ToolPaths) -> String {
    let mut p = String::from("/usr/local/bin:/usr/bin:/bin");
    if let Some(dir) = tools.mise.parent() {
        let d = dir.display().to_string();
        if !p.split(':').any(|x| x == d) {
            p.push(':');
            p.push_str(&d);
        }
    }
    p
}

/// `claude.resume` and a kept session to continue.
pub fn wants_continue(settings: &ClaudeSettings, paths: &AgentPaths) -> bool {
    settings.resume && std::fs::read_dir(paths.claude_projects()).map(|mut d| d.next().is_some()).unwrap_or(false)
}

pub fn render_launch(paths: &AgentPaths, tools: &ToolPaths, binary: &str, args: &[String], resume: bool) -> (String, LaunchPlan) {
    let env = BTreeMap::from([
        ("PATH".to_string(), outer_path(tools)),
        ("HOME".to_string(), paths.nono_home.display().to_string()),
    ]);
    let mut argv: Vec<String> = vec![
        tools.nono.display().to_string(),
        "-s".into(),
        "--log-file".into(),
        paths.logs.join("nono.log").display().to_string(),
        "run".into(),
        "--profile".into(),
        paths.profile.display().to_string(),
        "--".into(),
        tools.mise.display().to_string(),
        "exec".into(),
        "--".into(),
        binary.to_string(),
    ];
    argv.extend(args.iter().cloned());
    if resume {
        argv.push("--continue".into());
    }
    let mut script = String::from("#!/bin/sh\n# generated by balerix — safe to run by hand\nexec env -i");
    for (k, v) in &env {
        script.push_str(&format!(" {k}={}", sh_quote(v)));
    }
    script.push_str(" \\\n ");
    for a in &argv {
        script.push(' ');
        script.push_str(&sh_quote(a));
    }
    script.push('\n');
    (script, LaunchPlan { cwd: paths.workspace.clone(), env, argv, script: paths.launch.clone() })
}

pub fn write_launch(id: &AgentId, paths: &AgentPaths, script: &str) -> Result<(), MaterializeError> {
    write_atomic(&paths.launch, script.as_bytes(), 0o755).map_err(|e| MaterializeError::Io { id: id.to_string(), path: paths.launch.clone(), message: e.to_string() })
}

/// Port the agent must be allowed to connect to for hooks.
pub fn hooks_port(url: &str) -> u16 {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host_port = rest.split('/').next().unwrap_or(rest);
    host_port.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(if url.starts_with("http://") { 80 } else { 443 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::StateLayout;
    use std::path::Path;

    fn tools() -> ToolPaths {
        ToolPaths { git: "/usr/bin/git".into(), gh: "/opt/gh".into(), mise: "/opt/mise/bin/mise".into(), nono: "/opt/nono".into(), tmux: "/opt/tmux".into() }
    }

    #[test]
    fn script_quotes_everything_and_carries_no_secret_inputs() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let paths = layout.agent(&"f/c/a".parse().unwrap());
        let (script, plan) = render_launch(&paths, &tools(), "claude", &["--verbose".into(), "it's".into()], true);
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("exec env -i HOME='/h/.local/state/balerix/fleets/f/crews/c/agents/a/nono' PATH='/usr/local/bin:/usr/bin:/bin:/opt/mise/bin'"));
        assert!(script.contains("'/opt/nono' '-s' '--log-file'"));
        assert!(script.contains("'run' '--profile' '/h/.local/state/balerix/fleets/f/crews/c/agents/a/nono-profile.json' '--' '/opt/mise/bin/mise' 'exec' '--' 'claude' '--verbose' 'it'\\''s' '--continue'"));
        assert_eq!(plan.cwd, paths.workspace);
        assert_eq!(plan.script, paths.launch);
        assert_eq!(plan.argv.last().unwrap(), "--continue");
        assert_eq!(plan.env.len(), 2);
    }

    #[test]
    fn wants_continue_needs_resume_and_a_kept_session() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::from_env(dir.path(), |_| None);
        let paths = layout.agent(&"f/c/a".parse().unwrap());
        let mut s = ClaudeSettings::default();
        assert!(!wants_continue(&s, &paths));
        s.resume = true;
        assert!(!wants_continue(&s, &paths), "no projects dir yet");
        std::fs::create_dir_all(paths.claude_projects().join("x")).unwrap();
        assert!(wants_continue(&s, &paths));
    }

    #[test]
    fn hooks_port_parses_urls() {
        assert_eq!(hooks_port("https://127.0.0.1:7643"), 7643);
        assert_eq!(hooks_port("https://127.0.0.1:7643/"), 7643);
        assert_eq!(hooks_port("https://example.com"), 443);
        assert_eq!(hooks_port("http://localhost"), 80);
    }
}
```

- [ ] **Step 2: Write `materializer.rs` (render half only)**

```rust
//! `Runtime`: the `Materializer` over real tools. `render_agent` is the
//! pure-ish half (files only) shared with `balerix dev materialize`.

use balerix_api::{CredentialBundle, GitAuth};
use balerix_core::{HookTarget, LaunchPlan, MaterializeError, ResolvedAgent};

use crate::env::agent_env;
use crate::home::{HomeInputs, write_home};
use crate::launch::{hooks_port, render_launch, wants_continue, write_launch};
use crate::layout::StateLayout;
use crate::sandbox::{balerix_grants, render_profile, validate_profile, write_profile};
use crate::toolchain::{Toolchain, system_tools};
use crate::tools::ToolPaths;

pub struct Runtime {
    pub layout: StateLayout,
    pub tools: ToolPaths,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions {
    pub redact_credentials: bool,
}

impl Runtime {
    pub fn new(layout: StateLayout, tools: ToolPaths) -> Self {
        Self { layout, tools }
    }

    /// Steps 2–5 of the pipeline without subprocesses: home, `mise.toml`,
    /// `nono-profile.json`, `launch.sh`. Idempotent.
    pub fn render_agent(&self, agent: &ResolvedAgent, creds: &CredentialBundle, hooks: &HookTarget, opts: &RenderOptions) -> Result<LaunchPlan, MaterializeError> {
        let id = &agent.id;
        let paths = self.layout.agent(id);
        let crew = self.layout.crew(&id.crew_ref());
        let with_gh = agent.git.auth == GitAuth::Gh;

        write_home(id, &paths, &HomeInputs { settings: &agent.settings.claude.settings, creds, hooks, with_gh, redact_credentials: opts.redact_credentials })?;

        let system = system_tools(&self.layout, id)?;
        Toolchain { tools: &self.tools, layout: &self.layout }.write(id, &paths, &system, &agent.settings.tools, with_gh)?;

        let env = agent_env(id, &paths, &self.layout, &hooks.url, &agent.settings.env);
        let profile = render_profile(id, &balerix_grants(&paths, &crew, &self.layout), hooks_port(&hooks.url), &env, &agent.settings.sandbox)?;
        write_profile(id, &paths, &profile)?;

        let resume = wants_continue(&agent.settings.claude, &paths);
        let (script, plan) = render_launch(&paths, &self.tools, &agent.settings.claude.binary, &agent.settings.claude.args, resume);
        write_launch(id, &paths, &script)?;
        Ok(plan)
    }

    /// The subprocess half of steps 3 and 4.
    pub fn install_and_validate(&self, agent: &ResolvedAgent) -> Result<(), MaterializeError> {
        let paths = self.layout.agent(&agent.id);
        Toolchain { tools: &self.tools, layout: &self.layout }.install(&agent.id, &paths)?;
        validate_profile(&self.tools, &agent.id, &paths)
    }
}
```

`lib.rs`: add `pub mod launch; pub mod materializer;` and `pub use launch::{hooks_port, render_launch, wants_continue}; pub use materializer::{RenderOptions, Runtime};`.

- [ ] **Step 3: Write the golden test**

`crates/balerix-runtime/tests/generated_golden.rs`:
```rust
//! The four generated files for the `payments` example agents. Paths under
//! the temp root are replaced by `<root>`; tool paths by `<tools>/name`.
//! Review `.snap.new` against the expected values in the Phase 2 plan, Task 13.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use balerix_api::{AgentSettings, CredentialBundle, CrewSpec, FleetSpec, GitSettings};
use balerix_core::{Fleet, HookTarget, ResolvedAgent};
use balerix_runtime::{RenderOptions, Runtime, StateLayout, ToolPaths};
use serde_json::json;

fn fleet() -> Fleet {
    let mut alice = AgentSettings::default();
    alice.claude.settings = json!({ "model": "sonnet", "permissions": { "allow": ["Bash(git *)"] } });
    alice.claude.args = vec!["--verbose".into()];
    alice.claude.resume = true;
    alice.sandbox = json!({ "network": { "mode": "allow" } });
    alice.tools = BTreeMap::from([("node".to_string(), "22.11.0".to_string()), ("python".to_string(), "3.12.8".to_string())]);
    alice.env = BTreeMap::from([("RUST_LOG".to_string(), "info".to_string())]);
    let mut bob = alice.clone();
    bob.claude.settings["model"] = json!("opus");
    Fleet::try_from(FleetSpec {
        name: "payments".into(),
        crews: BTreeMap::from([(
            "backend".to_string(),
            CrewSpec { repo: "acme/payments-api".into(), git_ref: "main".into(), git: GitSettings::default(), agents: BTreeMap::from([("alice".to_string(), alice), ("bob".to_string(), bob)]) },
        )]),
    })
    .unwrap()
}

fn normalize(text: &str, root: &std::path::Path) -> String {
    text.replace(&root.display().to_string(), "<root>")
}

#[test]
fn payments_agents_generate_known_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let layout = StateLayout { state_root: root.join("state"), data_root: root.join("data"), config_root: root.join("config") };
    // fixed tool paths so launch.sh is deterministic; nothing is executed
    let tools = ToolPaths { git: "/tools/git".into(), gh: "/tools/gh".into(), mise: "/tools/mise".into(), nono: "/tools/nono".into(), tmux: "/tools/tmux".into() };
    // fixed system table so the snapshot does not move with the repo's claude pin
    std::fs::create_dir_all(&layout.config_root).unwrap();
    std::fs::write(layout.system_mise_toml(), "[tools]\nclaude = \"2.1.0\"\ngh = \"2.100.0\"\n").unwrap();
    let rt = Runtime::new(layout.clone(), tools);
    let creds = CredentialBundle { claude_credentials: Some(json!({ "claudeAiOauth": { "accessToken": "sk-SECRET" } })), claude_account: Some(json!({ "oauthAccount": { "emailAddress": "a@b.c" } })), gh_token: Some("gho_SECRET".into()) };
    let mut out = String::new();
    for agent in ResolvedAgent::from_fleet(&fleet()) {
        let hooks = HookTarget { url: "https://127.0.0.1:7643".into(), secret: format!("secret-{}", agent.id.agent) };
        let plan = rt.render_agent(&agent, &creds, &hooks, &RenderOptions { redact_credentials: true }).unwrap();
        let paths = layout.agent(&agent.id);
        assert_eq!(plan.script, paths.launch);
        for (label, path) in [
            ("settings.json", paths.claude_dir().join("settings.json")),
            ("mise.toml", paths.mise_toml.clone()),
            ("nono-profile.json", paths.profile.clone()),
            ("launch.sh", paths.launch.clone()),
        ] {
            out.push_str(&format!("==== {} {label} ====\n{}\n", agent.id, normalize(&std::fs::read_to_string(path).unwrap(), root)));
        }
        let creds_file = std::fs::read_to_string(paths.claude_dir().join(".credentials.json")).unwrap();
        assert!(!creds_file.contains("SECRET"));
        assert!(!std::fs::read_to_string(&paths.launch).unwrap().contains("SECRET"));
    }
    insta::assert_snapshot!("payments_generated", out);
}
```

- [ ] **Step 4: Run, review, accept**

Run: `mise x -- cargo nextest run -p balerix-runtime --test generated_golden`
Check in `payments_generated.snap.new` before accepting:
- `settings.json` for alice: `"model": "sonnet"`, `permissions.allow` intact, `hooks` with nine events each pointing at `https://127.0.0.1:7643/v1/agents/payments/backend/alice/events` and header `Bearer secret-alice`; bob has `"model": "opus"` and `secret-bob`.
- `mise.toml`: `claude = "2.1.0"`, `gh = "2.100.0"`, `node = "22.11.0"`, `python = "3.12.8"` in that order.
- `nono-profile.json`: `filesystem.read` ends with `<root>/data/mise`; `filesystem.allow` = home, workspace, `<root>/state/fleets/payments/crews/backend/repo/.git`; `network.connect_port: [7643]` and `network.mode: "allow"` (user block merged); `environment.set_vars` has 18 keys including `RUST_LOG: info` and `BALERIX_API_URL`.
- `launch.sh`: `exec env -i HOME='<root>/state/…/alice/nono' PATH='/usr/local/bin:/usr/bin:/bin:/tools'` then `'/tools/nono' '-s' '--log-file' … 'run' '--profile' … '--' '/tools/mise' 'exec' '--' 'claude' '--verbose'` and **no** `--continue` (no kept session).
- Nothing containing `SECRET` anywhere in the snapshot.

Then `mise x -- cargo insta accept`.

- [ ] **Step 5: Lint and commit**

Run: `mise run check`
```bash
git add crates/balerix-runtime
git commit -m "Render launch.sh and the full agent directory; snapshot the generated files

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---

### Task 14: `balerix-runtime` — `Workspace` (git) with a local-repo integration test

**Files:**
- Create: `crates/balerix-runtime/src/workspace.rs`, `crates/balerix-runtime/tests/workspace_it.rs`
- Modify: `crates/balerix-runtime/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct Workspace<'a> { pub tools: &'a ToolPaths, pub gh_config_dir: Option<PathBuf> /* Some when git.auth == gh */ }
  impl Workspace<'_> {
      pub fn write_fleet_gh_config(dir: &Path, token: &str, id: &str) -> Result<(), MaterializeError>;  // hosts.yml 0600
      pub fn ensure_repo(&self, id: &str, crew: &CrewPaths, repo: &RepoRef, git_ref: &str) -> Result<(), MaterializeError>;
      pub fn ensure_worktree(&self, id: &str, crew: &CrewPaths, workspace: &Path, branch: &str, git_ref: &str) -> Result<(), MaterializeError>;
      pub fn remove_worktree(&self, id: &str, crew: &CrewPaths, workspace: &Path) -> Result<(), MaterializeError>;
  }
  ```

- [ ] **Step 1: Write `workspace.rs`**

```rust
//! Crew clone and per-agent worktree (Phase 2 spec §4.2 step 1, P2-6).

use std::path::{Path, PathBuf};

use balerix_core::{MaterializeError, RepoRef};

use crate::fsutil::write_atomic;
use crate::home::render_hosts_yml;
use crate::layout::CrewPaths;
use crate::tools::{Cmd, ToolPaths};

pub struct Workspace<'a> {
    pub tools: &'a ToolPaths,
    /// `GH_CONFIG_DIR` for the daemon's git calls when `git.auth: gh`.
    pub gh_config_dir: Option<PathBuf>,
}

impl Workspace<'_> {
    pub fn write_fleet_gh_config(dir: &Path, token: &str, id: &str) -> Result<(), MaterializeError> {
        let hosts = dir.join("hosts.yml");
        write_atomic(&hosts, render_hosts_yml(token).as_bytes(), 0o600).map_err(|e| MaterializeError::Io { id: id.to_string(), path: hosts, message: e.to_string() })
    }

    fn git(&self, id: &str, crew: &CrewPaths, args: &[&str]) -> Result<String, MaterializeError> {
        let mut cmd = Cmd::new(&self.tools.git).log(&crew.root.join("logs").join("git.log"));
        if let Some(dir) = &self.gh_config_dir {
            cmd = cmd
                .env("GH_CONFIG_DIR", dir.display().to_string())
                .args(["-c", "credential.helper=", "-c", &format!("credential.helper=!{} auth git-credential", self.tools.gh.display())]);
        }
        cmd = cmd.env("GIT_TERMINAL_PROMPT", "0").args(args.iter().copied());
        cmd.run().map(|o| o.stdout).map_err(|f| MaterializeError::Tool { id: id.to_string(), tool: f.tool, subcommand: f.subcommand, args: f.args, stderr: f.stderr })
    }

    /// Clone without a checkout if absent, else fetch. Idempotent.
    pub fn ensure_repo(&self, id: &str, crew: &CrewPaths, repo: &RepoRef, git_ref: &str) -> Result<(), MaterializeError> {
        let _ = git_ref;
        if crew.repo.join(".git").is_dir() {
            self.git(id, crew, &["-C", &crew.repo.display().to_string(), "fetch", "--quiet", "origin"])?;
        } else {
            std::fs::create_dir_all(&crew.root).map_err(|e| MaterializeError::Io { id: id.to_string(), path: crew.root.clone(), message: e.to_string() })?;
            self.git(id, crew, &["clone", "--quiet", "--no-checkout", &repo.clone_url(), &crew.repo.display().to_string()])?;
        }
        Ok(())
    }

    /// Reuses a registered worktree; reuses an existing branch; otherwise
    /// creates the branch from `origin/<git_ref>`.
    pub fn ensure_worktree(&self, id: &str, crew: &CrewPaths, workspace: &Path, branch: &str, git_ref: &str) -> Result<(), MaterializeError> {
        let repo = crew.repo.display().to_string();
        let list = self.git(id, crew, &["-C", &repo, "worktree", "list", "--porcelain"])?;
        let registered = list.lines().any(|l| l.strip_prefix("worktree ").map(Path::new) == Some(workspace));
        if registered && workspace.join(".git").exists() {
            return Ok(());
        }
        self.git(id, crew, &["-C", &repo, "worktree", "prune"])?;
        let ws = workspace.display().to_string();
        let branch_exists = self.git(id, crew, &["-C", &repo, "rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
        if branch_exists {
            self.git(id, crew, &["-C", &repo, "worktree", "add", "--quiet", &ws, branch])?;
        } else {
            self.git(id, crew, &["-C", &repo, "worktree", "add", "--quiet", "-b", branch, &ws, &format!("origin/{git_ref}")])?;
        }
        Ok(())
    }

    pub fn remove_worktree(&self, id: &str, crew: &CrewPaths, workspace: &Path) -> Result<(), MaterializeError> {
        if !crew.repo.join(".git").is_dir() {
            return Ok(());
        }
        let repo = crew.repo.display().to_string();
        if workspace.exists() {
            self.git(id, crew, &["-C", &repo, "worktree", "remove", "--force", &workspace.display().to_string()])?;
        }
        self.git(id, crew, &["-C", &repo, "worktree", "prune"]).map(|_| ())
    }
}
```

`lib.rs`: add `pub mod workspace;` and `pub use workspace::Workspace;`.

- [ ] **Step 2: Write the git integration test**

`crates/balerix-runtime/tests/workspace_it.rs`:
```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::path::Path;
use std::process::Command;

use balerix_core::RepoRef;
use balerix_runtime::Workspace;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A bare repo with one commit on `main`, served over file://.
fn bare_repo(root: &Path) -> RepoRef {
    let work = root.join("upstream-work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("upstream.git");
    git(root, &["clone", "-q", "--bare", &work.display().to_string(), &bare.display().to_string()]);
    RepoRef::parse(&format!("file://{}", bare.display())).unwrap()
}

#[test]
fn clone_worktree_reuse_and_remove() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace { tools: &tools, gh_config_dir: None };

    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    assert!(crew.repo.join(".git").is_dir());
    assert!(!crew.repo.join("README").exists(), "--no-checkout");
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap(); // fetch path

    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "balerix/f/c/a", "main").unwrap();
    assert_eq!(std::fs::read_to_string(paths.workspace.join("README")).unwrap(), "hi\n");
    assert_eq!(git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "balerix/f/c/a");
    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "balerix/f/c/a", "main").unwrap(); // idempotent

    // agent commits; remove the worktree; re-adding must keep the commit (P2-6)
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    ws.remove_worktree("f/c/a", &crew, &paths.workspace).unwrap();
    assert!(!paths.workspace.exists());
    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "balerix/f/c/a", "main").unwrap();
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), sha, "branch reused, commit preserved");
    assert!(paths.workspace.join("work.txt").exists());

    // a second agent gets its own branch from origin/main, not from alice's
    let b = layout.agent(&"f/c/b".parse().unwrap());
    ws.ensure_worktree("f/c/b", &crew, &b.workspace, "balerix/f/c/b", "main").unwrap();
    assert!(!b.workspace.join("work.txt").exists());
    assert!(crew.root.join("logs").join("git.log").exists());
}

#[test]
fn errors_name_the_id_tool_and_first_stderr_line() {
    let Some(tools) = support::tools() else {
        return;
    };
    let root = support::temp_root("workspace-err");
    let layout = support::layout(&root);
    let crew = layout.crew(&"f/c".parse().unwrap());
    let ws = Workspace { tools: &tools, gh_config_dir: None };
    let err = ws.ensure_repo("f/c", &crew, &RepoRef::parse("file:///nonexistent/repo.git").unwrap(), "main").unwrap_err();
    let msg = err.to_string();
    assert!(msg.starts_with("f/c: git clone: "), "{msg}");
    assert!(msg.contains("fatal") || msg.contains("does not exist") || msg.contains("not found"), "{msg}");
}
```

- [ ] **Step 3: Verify the gh credential helper assumption (spec §4.4 row 3) and record it**

This needs a real token, so it is a manual probe, not a test:
```bash
D=$(mktemp -d); printf 'github.com:\n    oauth_token: %s\n    git_protocol: https\n' "$(mise x -- gh auth token)" > "$D/hosts.yml"; chmod 600 "$D/hosts.yml"
GH_CONFIG_DIR="$D" printf 'protocol=https\nhost=github.com\n\n' | GH_CONFIG_DIR="$D" mise x -- gh auth git-credential get | sed 's/password=.*/password=<redacted>/'
rm -rf "$D"
```
Expected: prints `username=…` and `password=<redacted>`. Record in Phase 2 spec §4.4 row 3: *Verdict (Task 14): holds — `oauth_token` + `git_protocol` suffice* (or the fallback you needed).

- [ ] **Step 4: Run, lint, commit**

Run: `mise x -- cargo nextest run -p balerix-runtime && mise run lint`
```bash
git add crates/balerix-runtime docs/superpowers/specs/2026-09-06-balerix-a2-runtime-design.md
git commit -m "Clone crew repos and manage per-agent worktrees

Branches are reused, never reset (P2-6): re-adding a worktree after
removal keeps unpushed commits.

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---
### Task 15: `balerix-runtime` — `impl Materializer for Runtime`

**Files:**
- Modify: `crates/balerix-runtime/src/materializer.rs`
- Create: `crates/balerix-runtime/tests/materialize_it.rs`

**Interfaces:**
- Consumes: Tasks 10–14.
- Produces: `impl Materializer for Runtime` — `ensure_crew` (fleet gh config + clone/fetch), `materialize` (worktree → `render_agent` → `install_and_validate`), `remove_agent`, `remove_crew(keep)`.

- [ ] **Step 1: Add the impl**

Append to `crates/balerix-runtime/src/materializer.rs`:
```rust
use balerix_api::GitSettings;
use balerix_core::{AgentId, CrewRef, Keep, Materializer, RepoRef};

use crate::workspace::Workspace;

impl Runtime {
    fn workspace(&self, fleet: &balerix_core::FleetName, git: &GitSettings) -> Workspace<'_> {
        Workspace { tools: &self.tools, gh_config_dir: (git.auth == GitAuth::Gh).then(|| self.layout.fleet_gh_dir(fleet)) }
    }

    fn rm_rf(id: &str, path: &std::path::Path) -> Result<(), MaterializeError> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(MaterializeError::Io { id: id.to_string(), path: path.to_path_buf(), message: e.to_string() }),
        }
    }
}

impl Materializer for Runtime {
    fn ensure_crew(&self, crew: &CrewRef, repo: &RepoRef, git_ref: &str, git: &GitSettings, creds: &CredentialBundle) -> Result<(), MaterializeError> {
        let id = crew.to_string();
        if git.auth == GitAuth::Gh {
            let token = creds.gh_token.as_deref().ok_or_else(|| MaterializeError::Invalid { id: id.clone(), message: "git.auth is gh but no gh token was provided (run `gh auth login` on the client)".into() })?;
            Workspace::write_fleet_gh_config(&self.layout.fleet_gh_dir(&crew.fleet), token, &id)?;
        }
        self.workspace(&crew.fleet, git).ensure_repo(&id, &self.layout.crew(crew), repo, git_ref)
    }

    fn materialize(&self, agent: &ResolvedAgent, creds: &CredentialBundle, hooks: &HookTarget) -> Result<LaunchPlan, MaterializeError> {
        let id = agent.id.to_string();
        let crew = self.layout.crew(&agent.id.crew_ref());
        let paths = self.layout.agent(&agent.id);
        self.workspace(&agent.id.fleet, &agent.git).ensure_worktree(&id, &crew, &paths.workspace, &agent.branch(), &agent.git_ref)?;
        let plan = self.render_agent(agent, creds, hooks, &RenderOptions::default())?;
        self.install_and_validate(agent)?;
        Ok(plan)
    }

    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError> {
        let id = agent.to_string();
        let crew = self.layout.crew(&agent.crew_ref());
        let paths = self.layout.agent(agent);
        Workspace { tools: &self.tools, gh_config_dir: None }.remove_worktree(&id, &crew, &paths.workspace)?;
        Self::rm_rf(&id, &paths.root)
    }

    fn remove_crew(&self, crew: &CrewRef, keep: Keep) -> Result<(), MaterializeError> {
        let id = crew.to_string();
        let paths = self.layout.crew(crew);
        if !keep.sessions {
            let agents_dir = paths.root.join("agents");
            if let Ok(entries) = std::fs::read_dir(&agents_dir) {
                for e in entries.flatten() {
                    Workspace { tools: &self.tools, gh_config_dir: None }.remove_worktree(&id, &paths, &e.path().join("workspace"))?;
                }
            }
            Self::rm_rf(&id, &agents_dir)?;
        }
        if !keep.repos {
            Self::rm_rf(&id, &paths.repo)?;
        }
        if !keep.repos && !keep.sessions {
            Self::rm_rf(&id, &paths.root)?;
        }
        Ok(())
    }
}
```

- [ ] **Step 2: Write the end-to-end materialize test**

`crates/balerix-runtime/tests/materialize_it.rs`:
```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use balerix_api::{AgentSettings, CredentialBundle, CrewSpec, FleetSpec, GitAuth, GitSettings};
use balerix_core::{Fleet, HookTarget, Keep, Materializer, ResolvedAgent};
use balerix_runtime::{Runtime, embedded_system_tools};

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git").args(args).current_dir(dir).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").status().unwrap();
    assert!(st.success());
}

fn fleet(repo_url: &str) -> Fleet {
    let mut s = AgentSettings::default();
    s.claude.binary = "/bin/sh".into();
    s.tools = BTreeMap::new();
    Fleet::try_from(FleetSpec {
        name: "f".into(),
        crews: BTreeMap::from([("c".to_string(), CrewSpec { repo: repo_url.into(), git_ref: "main".into(), git: GitSettings { push: false, auth: GitAuth::None }, agents: BTreeMap::from([("a".to_string(), s)]) })]),
    })
    .unwrap()
}

#[test]
fn materialize_then_remove_round_trip() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("materialize");
    let layout = support::layout(&root);
    // system table: only gh, seeded from the host so mise install is offline
    let gh_version = embedded_system_tools()["gh"].clone();
    let out = Command::new(&tools.mise).args(["where", &format!("gh@{gh_version}")]).output().unwrap();
    if !support::require_or_skip("gh install to seed from", out.status.success()) {
        return;
    }
    let src = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dst = layout.mise_data_dir().join("installs").join("gh").join(&gh_version);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    assert!(Command::new("cp").args(["-r", &src, &dst.display().to_string()]).status().unwrap().success());
    std::fs::create_dir_all(&layout.config_root).unwrap();
    std::fs::write(layout.system_mise_toml(), format!("[tools]\ngh = \"{gh_version}\"\n")).unwrap();

    let work = root.join("up");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "x").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("up.git");
    git(&root, &["clone", "-q", "--bare", &work.display().to_string(), &bare.display().to_string()]);

    let f = fleet(&format!("file://{}", bare.display()));
    let rt = Runtime::new(layout.clone(), tools);
    let agent = ResolvedAgent::from_fleet(&f).remove(0);
    let crew = agent.id.crew_ref();
    let creds = CredentialBundle::default();
    let hooks = HookTarget { url: "https://127.0.0.1:7643".into(), secret: "s".into() };

    rt.ensure_crew(&crew, &agent.repo, &agent.git_ref, &agent.git, &creds).unwrap();
    let plan = rt.materialize(&agent, &creds, &hooks).unwrap();
    let paths = layout.agent(&agent.id);
    assert!(paths.workspace.join("README").exists());
    assert!(paths.launch.exists() && paths.profile.exists() && paths.mise_toml.exists());
    assert!(paths.claude_dir().join("settings.json").exists());
    assert!(!paths.claude_dir().join(".credentials.json").exists(), "no creds in the bundle → no file");
    assert_eq!(plan.cwd, paths.workspace);
    // idempotent
    rt.ensure_crew(&crew, &agent.repo, &agent.git_ref, &agent.git, &creds).unwrap();
    rt.materialize(&agent, &creds, &hooks).unwrap();

    rt.remove_agent(&agent.id).unwrap();
    assert!(!paths.root.exists());
    rt.remove_crew(&crew, Keep { repos: true, sessions: false }).unwrap();
    assert!(layout.crew(&crew).repo.exists());
    rt.remove_crew(&crew, Keep::default()).unwrap();
    assert!(!layout.crew(&crew).root.exists());
}

#[test]
fn gh_auth_without_a_token_is_a_clear_error() {
    let Some(tools) = support::tools() else {
        return;
    };
    let root = support::temp_root("materialize-noauth");
    let rt = Runtime::new(support::layout(&root), tools);
    let f = fleet("file:///nowhere.git");
    let agent = ResolvedAgent::from_fleet(&f).remove(0);
    let err = rt.ensure_crew(&agent.id.crew_ref(), &agent.repo, "main", &GitSettings::default(), &CredentialBundle::default()).unwrap_err();
    assert_eq!(err.to_string(), "f/c: git.auth is gh but no gh token was provided (run `gh auth login` on the client)");
}
```

- [ ] **Step 3: Run, lint, commit**

Run: `mise x -- cargo nextest run -p balerix-runtime && mise run lint`
Expected: PASS. `nono profile validate` runs for real here; if it rejects the generated profile, its stderr is the error message — fix the profile shape in `sandbox.rs`, never by skipping validation.
```bash
git add crates/balerix-runtime
git commit -m "Implement Materializer for Runtime

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---

### Task 16: `balerix-runtime` — `TmuxRunner`

**Files:**
- Create: `crates/balerix-runtime/src/tmux.rs`, `crates/balerix-runtime/tests/tmux_it.rs`
- Modify: `crates/balerix-runtime/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct TmuxRunner { pub tmux: PathBuf, pub socket: String }   // socket: "balerix" in production
  impl TmuxRunner { pub fn new(tmux: PathBuf, socket: impl Into<String>) -> Self }
  impl AgentRunner for TmuxRunner
  pub(crate) fn parse_windows(fleet: &FleetName, crew: &CrewName, text: &str) -> Result<BTreeMap<AgentName, ProcessState>, RunnerError>;
  pub const ANCHOR_WINDOW: &str = "balerix";
  ```

- [ ] **Step 1: Write `tmux.rs` with unit tests**

```rust
//! `AgentRunner` over a dedicated tmux server (Phase 2 spec §4.3).

use std::collections::BTreeMap;
use std::path::PathBuf;

use balerix_core::{AgentId, AgentName, AgentRunner, CrewName, CrewRef, FleetName, LaunchPlan, ObservedState, ProcessState, RunnerError};

use crate::tools::Cmd;

/// The window that keeps a session alive when every agent window is gone.
pub const ANCHOR_WINDOW: &str = "balerix";
const WINDOW_FORMAT: &str = "#{window_name}\t#{pane_dead}\t#{pane_pid}\t#{pane_dead_status}";

pub struct TmuxRunner {
    pub tmux: PathBuf,
    pub socket: String,
}

impl TmuxRunner {
    pub fn new(tmux: PathBuf, socket: impl Into<String>) -> Self {
        Self { tmux, socket: socket.into() }
    }

    fn cmd(&self) -> Cmd {
        Cmd::new(&self.tmux).args(["-L".to_string(), self.socket.clone()])
    }

    fn run(&self, id: &str, args: &[&str]) -> Result<String, RunnerError> {
        self.cmd().args(args.iter().copied()).run().map(|o| o.stdout).map_err(|f| RunnerError::Tool { id: id.to_string(), subcommand: f.subcommand, args: f.args, stderr: f.stderr })
    }

    /// tmux exits non-zero with "no server running" / "can't find" when
    /// nothing exists; those are "absent", not errors.
    fn run_optional(&self, id: &str, args: &[&str]) -> Result<Option<String>, RunnerError> {
        match self.cmd().args(args.iter().copied()).run() {
            Ok(o) => Ok(Some(o.stdout)),
            Err(f) if f.stderr.contains("no server running") || f.stderr.contains("can't find") || f.stderr.contains("no such") || f.stderr.contains("error connecting") => Ok(None),
            Err(f) => Err(RunnerError::Tool { id: id.to_string(), subcommand: f.subcommand, args: f.args, stderr: f.stderr }),
        }
    }

    fn session_target(crew: &CrewRef) -> String {
        format!("={crew}")
    }

    fn window_target(agent: &AgentId) -> String {
        format!("={}:={}", agent.crew_ref(), agent.agent)
    }

    fn windows(&self, crew: &CrewRef) -> Result<Option<BTreeMap<AgentName, ProcessState>>, RunnerError> {
        let Some(text) = self.run_optional(&crew.to_string(), &["list-windows", "-t", &Self::session_target(crew), "-F", WINDOW_FORMAT])? else {
            return Ok(None);
        };
        parse_windows(&crew.fleet, &crew.crew, &text).map(Some)
    }
}

pub(crate) fn parse_windows(fleet: &FleetName, crew: &CrewName, text: &str) -> Result<BTreeMap<AgentName, ProcessState>, RunnerError> {
    let id = format!("{fleet}/{crew}");
    let mut out = BTreeMap::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        let cols: Vec<&str> = line.split('\t').collect();
        let [name, dead, pid, status] = cols[..] else {
            return Err(RunnerError::Parse { id, message: format!("expected 4 columns in {line:?}") });
        };
        if name == ANCHOR_WINDOW {
            continue;
        }
        let Ok(agent) = name.parse::<AgentName>() else {
            continue; // a window we did not create; ignore
        };
        let state = if dead == "1" {
            ProcessState::Exited { code: status.parse().ok() }
        } else {
            ProcessState::Running { pid: pid.parse().map_err(|_| RunnerError::Parse { id: id.clone(), message: format!("bad pid in {line:?}") })? }
        };
        out.insert(agent, state);
    }
    Ok(out)
}

impl AgentRunner for TmuxRunner {
    fn ensure_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        if self.run_optional(&crew.to_string(), &["has-session", "-t", &Self::session_target(crew)])?.is_some() {
            return Ok(());
        }
        self.run(&crew.to_string(), &["new-session", "-d", "-s", &crew.to_string(), "-n", ANCHOR_WINDOW, "--", "/bin/sh", "-c", "while :; do sleep 3600; done"]).map(|_| ())
    }

    fn ensure_agent(&self, agent: &AgentId, plan: &LaunchPlan) -> Result<(), RunnerError> {
        let id = agent.to_string();
        let crew = agent.crew_ref();
        let exists = self.windows(&crew)?.is_some_and(|w| w.contains_key(&agent.agent));
        let cwd = plan.cwd.display().to_string();
        let script = plan.script.display().to_string();
        let target = Self::window_target(agent);
        if exists {
            self.run(&id, &["respawn-window", "-k", "-t", &target, "-c", &cwd, &script])?;
        } else {
            self.run(&id, &["new-window", "-d", "-t", &Self::session_target(&crew), "-n", agent.agent.as_str(), "-c", &cwd, "--", &script])?;
            self.run(&id, &["set-option", "-w", "-t", &target, "remain-on-exit", "on"])?;
        }
        let log = plan.script.parent().map(|p| p.join("logs").join("tmux.log")).unwrap_or_else(|| PathBuf::from("tmux.log"));
        self.run(&id, &["pipe-pane", "-o", "-t", &target, &format!("cat >> {}", crate::quote::sh_quote(&log.display().to_string()))]).map(|_| ())
    }

    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError> {
        self.run_optional(&agent.to_string(), &["kill-window", "-t", &Self::window_target(agent)]).map(|_| ())
    }

    fn stop_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        self.run_optional(&crew.to_string(), &["kill-session", "-t", &Self::session_target(crew)]).map(|_| ())
    }

    fn observe(&self, fleet: &FleetName) -> Result<ObservedState, RunnerError> {
        let mut out = ObservedState::default();
        let Some(sessions) = self.run_optional(fleet.as_str(), &["list-sessions", "-F", "#{session_name}"])? else {
            return Ok(out);
        };
        let prefix = format!("{fleet}/");
        for name in sessions.lines() {
            let Some(crew_name) = name.strip_prefix(&prefix) else {
                continue;
            };
            let Ok(crew) = crew_name.parse::<CrewName>() else {
                continue;
            };
            let crew_ref = CrewRef { fleet: fleet.clone(), crew: crew.clone() };
            let windows = self.windows(&crew_ref)?.unwrap_or_default();
            out.crews.insert(crew, windows);
        }
        Ok(out)
    }

    fn send_text(&self, agent: &AgentId, text: &str, submit: bool) -> Result<(), RunnerError> {
        let id = agent.to_string();
        let target = Self::window_target(agent);
        self.run(&id, &["send-keys", "-t", &target, "-l", text])?;
        if submit {
            self.run(&id, &["send-keys", "-t", &target, "Enter"])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_running_dead_and_skips_anchor_and_foreign_windows() {
        let f: FleetName = "f".parse().unwrap();
        let c: CrewName = "c".parse().unwrap();
        let text = "balerix\t0\t10\t\nalice\t0\t42\t\nbob\t1\t43\t137\nNot_An_Agent\t0\t1\t\n";
        let w = parse_windows(&f, &c, text).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(w[&"alice".parse::<AgentName>().unwrap()], ProcessState::Running { pid: 42 });
        assert_eq!(w[&"bob".parse::<AgentName>().unwrap()], ProcessState::Exited { code: Some(137) });
        assert!(parse_windows(&f, &c, "garbage").unwrap_err().to_string().contains("expected 4 columns"));
    }

    #[test]
    fn targets_use_exact_match_prefixes() {
        let a: AgentId = "f/c/a".parse().unwrap();
        assert_eq!(TmuxRunner::session_target(&a.crew_ref()), "=f/c");
        assert_eq!(TmuxRunner::window_target(&a), "=f/c:=a");
    }
}
```

`lib.rs`: add `pub mod tmux;` and `pub use tmux::{ANCHOR_WINDOW, TmuxRunner};`.

- [ ] **Step 2: Write the tmux integration test**

`crates/balerix-runtime/tests/tmux_it.rs`:
```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use balerix_core::{AgentId, AgentRunner, LaunchPlan, ProcessState};
use balerix_runtime::TmuxRunner;

fn wait_for(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn session_window_observe_exit_respawn_and_teardown() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return;
    };
    let root = support::temp_root("tmux");
    let socket = format!("balerix-test-{}", std::process::id());
    let r = TmuxRunner::new(tools.tmux.clone(), socket.clone());
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = id.crew_ref();
    let fleet = id.fleet.clone();

    let agent_dir = root.join("a");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let script = agent_dir.join("launch.sh");
    std::fs::write(&script, "#!/bin/sh\necho hello-from-agent\nexec sleep 300\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan { cwd: agent_dir.clone(), env: BTreeMap::new(), argv: vec![], script: script.clone() };

    assert!(r.observe(&fleet).unwrap().crews.is_empty(), "no server yet is not an error");
    r.ensure_crew(&crew).unwrap();
    r.ensure_crew(&crew).unwrap();
    assert!(r.observe(&fleet).unwrap().crews[&crew.crew].is_empty(), "anchor window is not an agent");

    r.ensure_agent(&id, &plan).unwrap();
    let pid = match r.observe(&fleet).unwrap().get(&id) {
        Some(ProcessState::Running { pid }) => *pid,
        other => panic!("expected running, got {other:?}"),
    };
    wait_for(|| std::fs::read_to_string(agent_dir.join("logs").join("tmux.log")).is_ok_and(|s| s.contains("hello-from-agent")));

    // kill the script's process group leader → pane dead, remain-on-exit keeps the window
    assert!(std::process::Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap().success());
    wait_for(|| matches!(r.observe(&fleet).unwrap().get(&id), Some(ProcessState::Exited { .. })));

    r.ensure_agent(&id, &plan).unwrap(); // respawn
    wait_for(|| matches!(r.observe(&fleet).unwrap().get(&id), Some(ProcessState::Running { pid: p }) if *p != pid));

    r.send_text(&id, "ignored", true).unwrap();
    r.stop_agent(&id).unwrap();
    r.stop_agent(&id).unwrap(); // absent → ok
    assert_eq!(r.observe(&fleet).unwrap().get(&id), None);
    assert!(r.observe(&fleet).unwrap().crews.contains_key(&crew.crew), "session survives via the anchor");
    r.stop_crew(&crew).unwrap();
    r.stop_crew(&crew).unwrap();
    assert!(r.observe(&fleet).unwrap().crews.is_empty());
    let _ = std::process::Command::new(&tools.tmux).args(["-L", &socket, "kill-server"]).status();
}
```

- [ ] **Step 3: Run, lint, commit**

Run: `mise x -- cargo nextest run -p balerix-runtime && mise run lint`
Expected: PASS. If `respawn-window`'s pid check is flaky, compare on `Running` only and drop the pid inequality; note it in the commit.
```bash
git add crates/balerix-runtime
git commit -m "Add TmuxRunner over a dedicated tmux server

Sessions carry an anchor window so they outlive their agent windows;
windows keep remain-on-exit so a crashed agent's output stays visible.

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---
### Task 17: `balerix` binary — wiring and `dev materialize`

**Files:**
- Modify: `crates/balerix/Cargo.toml`, `crates/balerix/src/cli.rs`, `crates/balerix/src/main.rs`, `crates/balerix/src/commands/mod.rs`
- Create: `crates/balerix/src/wiring.rs`, `crates/balerix/src/commands/dev.rs`, `crates/balerix/tests/cli_dev_materialize.rs`

**Interfaces:**
- Consumes: `balerix_config::{read, resolve, ResolveOptions, host, HostPaths}`, `balerix_core::{Fleet, ResolvedAgent, HookTarget}`, `balerix_runtime::{Runtime, RenderOptions, StateLayout, ToolPaths}`.
- Produces:
  ```rust
  // wiring.rs
  pub fn layout_from_env() -> anyhow::Result<StateLayout>;   // home dir + XDG vars
  pub fn tool_paths() -> anyhow::Result<ToolPaths>;           // from PATH
  // cli.rs
  Command::Dev { command: DevCommand }  (hidden);  DevCommand::Materialize(MaterializeArgs)
  MaterializeArgs { file: PathBuf, agent: String /* crew/agent */, out: Option<PathBuf>, name: Option<String>,
                    no_host_defaults: bool, hooks_url: String /* default https://127.0.0.1:7643 */, install: bool, with_credentials: bool }
  // commands/dev.rs
  pub fn materialize_command(args: &MaterializeArgs) -> anyhow::Result<String>;
  ```

- [ ] **Step 1: Dependencies and CLI**

`crates/balerix/Cargo.toml` `[dependencies]`: add `balerix-core = { workspace = true }`, `balerix-runtime = { workspace = true }`, `tempfile = { workspace = true }`, `sha2 = { workspace = true }`, `hex = { workspace = true }`.

`crates/balerix/src/cli.rs`: add to `Command`:
```rust
    /// Developer tools; not part of the supported surface.
    #[command(hide = true)]
    Dev {
        #[command(subcommand)]
        command: DevCommand,
    },
```
and:
```rust
#[derive(Debug, Subcommand)]
pub enum DevCommand {
    /// Render one agent's generated files (settings.json, mise.toml,
    /// nono-profile.json, launch.sh) without launching anything.
    Materialize(MaterializeArgs),
}

#[derive(Debug, Args)]
pub struct MaterializeArgs {
    /// Path to the fleet YAML file.
    pub file: PathBuf,
    /// Agent to render, as crew/agent.
    pub agent: String,
    /// Output root (default: a fresh temp dir, printed).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Fleet name; overrides `name` in the file.
    #[arg(long)]
    pub name: Option<String>,
    /// Do not layer the host's ~/.claude/settings.json beneath the file.
    #[arg(long)]
    pub no_host_defaults: bool,
    /// Where the generated hooks post to.
    #[arg(long, default_value = "https://127.0.0.1:7643")]
    pub hooks_url: String,
    /// Also run `mise install` and `nono profile validate`.
    #[arg(long)]
    pub install: bool,
    /// Write real credentials instead of "<redacted>" placeholders.
    #[arg(long)]
    pub with_credentials: bool,
}
```

`crates/balerix/src/main.rs`: add `mod wiring;`, import `DevCommand`, and the arm:
```rust
        Command::Dev { command: DevCommand::Materialize(args) } => commands::dev::materialize_command(&args),
```
`crates/balerix/src/commands/mod.rs`: add `pub mod dev;`.

- [ ] **Step 2: Write `wiring.rs`**

```rust
//! The only place adapters meet the process environment.

use anyhow::{Context, Result};
use balerix_runtime::{StateLayout, ToolPaths};


pub fn layout_from_env() -> Result<StateLayout> {
    let home = std::env::home_dir().context("cannot determine the home directory")?;
    Ok(StateLayout::from_env(&home, |k| std::env::var_os(k)))
}

pub fn tool_paths() -> Result<ToolPaths> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    ToolPaths::discover_in(&path).map_err(|e| anyhow::anyhow!("{e} (balerix needs git, gh, mise, nono and tmux on PATH; see mise.toml)"))
}
```

- [ ] **Step 3: Write `commands/dev.rs`**

```rust
//! `balerix dev …` (Phase 2 spec §5).

use anyhow::{Context, Result, bail};
use balerix_config::{HostPaths, ResolveOptions, host, read, resolve};
use balerix_core::{Fleet, HookTarget, ResolvedAgent};
use balerix_runtime::{RenderOptions, Runtime, StateLayout};
use sha2::{Digest, Sha256};

use crate::cli::MaterializeArgs;
use crate::wiring::{layout_from_env, tool_paths};

pub fn materialize_command(args: &MaterializeArgs) -> Result<String> {
    if args.agent.split('/').count() != 2 {
        bail!("agent must be crew/agent");
    }
    let file = read(&args.file)?;
    let defaults = if args.no_host_defaults { host::HostDefaults::default() } else { host::load(&HostPaths::discover()?)? };
    let spec = resolve(&file, &ResolveOptions { name_override: args.name.clone(), host_claude_settings: defaults.claude_settings })?;
    let fleet = Fleet::try_from(spec)?;
    let wanted = format!("{}/{}", fleet.name, args.agent);
    let agent = ResolvedAgent::from_fleet(&fleet)
        .into_iter()
        .find(|a| a.id.to_string() == wanted)
        .with_context(|| format!("no agent {wanted:?} in the fleet (expected crew/agent)"))?;

    let real = layout_from_env()?;
    let out_root = match &args.out {
        Some(p) => p.clone(),
        None => tempfile::Builder::new().prefix("balerix-materialize-").tempdir()?.keep(),
    };
    let layout = StateLayout { state_root: out_root.join("state"), data_root: out_root.join("data"), config_root: real.config_root };
    let rt = Runtime::new(layout.clone(), tool_paths()?);

    let paths = layout.agent(&agent.id);
    std::fs::create_dir_all(&paths.workspace)?;
    std::fs::create_dir_all(layout.crew(&agent.id.crew_ref()).repo.join(".git"))?;
    let hooks = HookTarget { url: args.hooks_url.clone(), secret: throwaway_secret() };
    rt.render_agent(&agent, &defaults.credentials, &hooks, &RenderOptions { redact_credentials: !args.with_credentials })?;
    if args.install {
        rt.install_and_validate(&agent)?;
    }

    let mut out = format!("agent dir: {}\n", paths.root.display());
    for (label, p) in [
        ("settings.json", paths.claude_dir().join("settings.json")),
        (".credentials.json", paths.claude_dir().join(".credentials.json")),
        ("hosts.yml", paths.gh_dir().join("hosts.yml")),
        ("mise.toml", paths.mise_toml.clone()),
        ("nono-profile.json", paths.profile.clone()),
        ("launch.sh", paths.launch.clone()),
    ] {
        if p.exists() {
            out.push_str(&format!("  {label:<19} {}\n", p.display()));
        }
    }
    out.push_str(if args.with_credentials { "credentials: written (real)\n" } else { "credentials: <redacted> placeholders (pass --with-credentials to write them)\n" });
    if !args.install {
        out.push_str("not run: mise install, nono profile validate (pass --install)\n");
    }
    Ok(out)
}

/// Not a real secret: only so the rendered settings.json has the right shape.
fn throwaway_secret() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    hex::encode(Sha256::digest(format!("{}-{nanos}", std::process::id())))[..32].to_string()
}
```
- [ ] **Step 4: Write the CLI test**

`crates/balerix/tests/cli_dev_materialize.rs`:
```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

const PAYMENTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/payments.yaml");

/// Fake tools on PATH so discovery succeeds without the real ones.
fn fake_tools(dir: &Path) {
    for t in ["git", "gh", "mise", "nono", "tmux"] {
        fs::write(dir.join(t), "#!/bin/sh\nexit 0\n").unwrap();
    }
}

fn balerix(home: &Path, tools: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_balerix"));
    cmd.env("HOME", home).env("PATH", tools).env_remove("CLAUDE_CONFIG_DIR").env_remove("GH_CONFIG_DIR").env_remove("XDG_CONFIG_HOME").env_remove("XDG_STATE_HOME").env_remove("XDG_DATA_HOME");
    cmd
}

#[test]
fn renders_the_four_files_with_redacted_credentials_by_default() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    // host credentials exist → must be redacted
    fs::create_dir_all(home.path().join(".claude")).unwrap();
    fs::write(home.path().join(".claude").join(".credentials.json"), r#"{"claudeAiOauth":{"accessToken":"sk-SECRET"}}"#).unwrap();
    fs::create_dir_all(home.path().join(".config").join("gh")).unwrap();
    fs::write(home.path().join(".config").join("gh").join("hosts.yml"), "github.com:\n    oauth_token: gho_SECRET\n").unwrap();
    let out = tempfile::tempdir().unwrap();
    let assert = balerix(home.path(), tools.path())
        .args(["dev", "materialize", PAYMENTS, "backend/bob", "--out", &out.path().display().to_string()])
        .assert()
        .success()
        .stdout(predicate::str::contains("agent dir:"))
        .stdout(predicate::str::contains("launch.sh"))
        .stdout(predicate::str::contains("<redacted> placeholders"));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let agent_dir = stdout.lines().next().unwrap().trim_start_matches("agent dir: ").to_string();
    let settings = fs::read_to_string(Path::new(&agent_dir).join("home/.claude/settings.json")).unwrap();
    assert!(settings.contains("\"model\": \"opus\""));
    assert!(settings.contains("/v1/agents/payments/backend/bob/events"));
    let creds = fs::read_to_string(Path::new(&agent_dir).join("home/.claude/.credentials.json")).unwrap();
    assert!(!creds.contains("SECRET") && creds.contains("<redacted>"));
    let hosts = fs::read_to_string(Path::new(&agent_dir).join("home/.config/gh/hosts.yml")).unwrap();
    assert!(!hosts.contains("SECRET"));
    assert!(fs::read_to_string(Path::new(&agent_dir).join("mise.toml")).unwrap().contains("python = \"3.12.8\""));
    assert!(fs::read_to_string(Path::new(&agent_dir).join("nono-profile.json")).unwrap().contains("\"connect_port\""));
    let launch = fs::read_to_string(Path::new(&agent_dir).join("launch.sh")).unwrap();
    assert!(launch.starts_with("#!/bin/sh"));
    assert!(launch.contains("'--verbose'"));
}

#[test]
fn with_credentials_writes_real_values() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    fs::create_dir_all(home.path().join(".claude")).unwrap();
    fs::write(home.path().join(".claude").join(".credentials.json"), r#"{"claudeAiOauth":{"accessToken":"sk-REAL"}}"#).unwrap();
    let out = tempfile::tempdir().unwrap();
    balerix(home.path(), tools.path())
        .args(["dev", "materialize", PAYMENTS, "backend/alice", "--out", &out.path().display().to_string(), "--with-credentials"])
        .assert()
        .success()
        .stdout(predicate::str::contains("credentials: written (real)"));
    let creds = fs::read_to_string(out.path().join("state/fleets/payments/crews/backend/agents/alice/home/.claude/.credentials.json")).unwrap();
    assert!(creds.contains("sk-REAL"));
}

#[test]
fn unknown_agent_and_missing_tools_fail_clearly() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    balerix(home.path(), tools.path())
        .args(["dev", "materialize", PAYMENTS, "backend/nobody", "--no-host-defaults"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no agent \"payments/backend/nobody\""));
    let empty = tempfile::tempdir().unwrap();
    balerix(home.path(), empty.path())
        .args(["dev", "materialize", PAYMENTS, "backend/bob", "--no-host-defaults"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("required tool not found on PATH"));
}

#[test]
fn dev_is_hidden_from_help() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    balerix(home.path(), tools.path()).arg("--help").assert().success().stdout(predicate::str::contains("dev").not());
}
```

- [ ] **Step 5: Run, lint, commit**

Run: `mise x -- cargo nextest run -p balerix && mise run lint`
Expected: 4 new tests PASS plus the existing 6.
```bash
git add crates/balerix Cargo.lock
git commit -m "Add hidden balerix dev materialize

Renders one agent's generated files without launching anything; the
runtime counterpart of config resolve. Credentials are redacted unless
--with-credentials is passed.

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

---

### Task 18: CI, tasks, docs, threat model, spec addendum

**Files:**
- Modify: `mise.toml`, `.github/workflows/ci.yml`, `ARCHITECTURE.md`, `AGENTS.md`, `README.md`, `docs/THREAT-MODEL.md`, both specs

- [ ] **Step 1: mise tasks**

Append to `mise.toml`:
```toml
[tasks.mutants]
description = "Nightly tier: mutation-test the reconciler"
run = "cargo mutants -p balerix-core --in-place"

[tasks.test-it]
description = "Integration tests against real git/mise/nono/tmux, failing (not skipping) when a tool is missing"
env = { BALERIX_REQUIRE_TOOLS = "1" }
run = "cargo nextest run -p balerix-runtime"
```

- [ ] **Step 2: CI**

Replace `.github/workflows/ci.yml` `check` job's install and run lines, and add the nightly mutants job:
```yaml
  check:
    runs-on: ubuntu-latest
    env:
      BALERIX_REQUIRE_TOOLS: "1"
    steps:
      - uses: actions/checkout@v4
      - uses: jdx/mise-action@v2
        with: { install: false, cache: true }
      - run: mise install rust cargo:cargo-nextest cargo-insta tmux nono gh
      - run: mise run check
  mutants:
    if: github.event_name == 'schedule'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: jdx/mise-action@v2
        with: { install: false, cache: true }
      - run: mise install rust github:sourcefrog/cargo-mutants
      - run: mise run mutants
```
Push the branch and confirm the `check` job is green and its wall-clock stays under five minutes. If the tool installs push it over, split `cargo nextest run -p balerix-runtime` into its own blocking job that installs only `rust cargo:cargo-nextest tmux nono gh`.

- [ ] **Step 3: `ARCHITECTURE.md`**

Replace the "How it flows (Phase 1)" section with:
```markdown
## How it flows
**Config (Phase 1):** `read` (file.rs) → `resolve` (resolve.rs): for each agent fold
`host claude.settings → defaults → crew.defaults → agent` with `merge_layers`
(merge.rs), deserialize into `AgentSettings`, `validate_agent` (validate.rs),
then `Fleet::try_from` for names and repos.

**Runtime (Phase 2):** `reconcile::plan` (core) turns desired `Fleet` + last
`FleetStatus` + `ObservedState` into an ordered step list; `execute` walks it
through two ports. `Materializer` (`balerix-runtime::Runtime`) makes files:
clone/worktree → `home/` (settings.json with balerix's hooks, credentials,
hosts.yml) → `mise.toml` + `mise install` → `nono-profile.json` + validate →
`launch.sh`. `AgentRunner` (`TmuxRunner`) makes processes: session per crew,
window per agent, `remain-on-exit`, `respawn-window`. `balerix dev
materialize` runs the file half alone.
```
Add to "The pieces": `- balerix-runtime — driven adapters over git, gh, mise, nono, tmux. One module per materialization step; every path from StateLayout, every binary from ToolPaths; never reads the process environment.`
Add to "Non-obvious decisions":
```markdown
- **The agent environment lives in the nono profile, not in `env -i`.** nono
  refuses a read-write grant on any directory holding its own state root, and
  it derives that root from its own `$HOME`. So nono runs with `HOME=agents/<a>/nono`
  and sets the agent's `HOME`, `XDG_*`, `BALERIX_*` through `environment.set_vars`
  with `deny_vars: ["*"]`. `PATH` is the one variable that crosses from outside.
- **Worktree branches are reused, never reset.** `-B … origin/<ref>` would drop
  unpushed agent commits on every re-`up` after `down --keep-repos`.
- **The planner is pure; the executor is dumb.** Every decision is in
  `reconcile::plan` (a total function) so the model-based test compares plans
  structurally and `cargo mutants` has something to bite.
```

- [ ] **Step 4: `AGENTS.md`, `README.md`**

`AGENTS.md` Conventions: change the first bullet to `- Ports (\`Materializer\`, \`AgentRunner\`, \`Clock\`; later \`FleetStore\`, \`EventHandler\`) live in \`balerix-core\`; adapter crates implement them and never depend on each other. Only the \`balerix\` binary wires adapters to ports.` Add gotchas:
```markdown
- `balerix-runtime` integration tests skip with a printed reason when a tool or
  Landlock is missing; `mise run test-it` (and CI) sets `BALERIX_REQUIRE_TOOLS=1`
  so they fail instead. Their temp roots live under `target/tmp`, never `/tmp`
  (nono grants `/tmp` by default, which would make escape assertions vacuous).
- The embedded default tool table is `include_str!("../../../mise.toml")` in
  `balerix-runtime/src/toolchain.rs`; bumping `claude` or `gh` in `mise.toml`
  changes what agents get.
- nono's state root follows nono's own `$HOME`; never point that at the agent's
  `home/` (see ARCHITECTURE.md).
```
`README.md`: add quickstart line 5: `\`mise x -- cargo run -q -p balerix -- dev materialize examples/payments.yaml backend/bob --no-host-defaults\` — renders bob's settings.json, mise.toml, nono-profile.json and launch.sh into a temp dir.` Replace the Status section: `Phases 1 (configuration) and 2 (runtime: materialization, tmux runner, reconciler) are complete. The daemon, hook ingress and \`up\`/\`down\` are Phase 3; see the spec's §11.`

- [ ] **Step 5: Threat model**

In `docs/THREAT-MODEL.md` Mitigations, update rows:
- "User `env` clobbering isolation variables": append `; the agent environment is set through the nono profile's set_vars (deny_vars ["*"]), so nothing but PATH crosses from the outer process` and add location `crates/balerix-runtime/src/env.rs`, `sandbox.rs`.
- "Sandbox mis-grants": replace *(planned §4, §6)* with `crates/balerix-runtime/src/sandbox.rs` (conflicts rejected with a path; `nono profile validate` before launch).
- "Secrets in argv / env / `launch.sh`": replace the planned marker with `crates/balerix-runtime/src/launch.rs` (no credential is an input), `home.rs`, `workspace.rs` (gh token only in two 0600 `hosts.yml` files).
Add to "Out of scope / accepted risks": `- **The per-agent hook secret is readable by its own agent** — it sits in the agent's settings.json; it authenticates only that agent's events.`

- [ ] **Step 6: Spec addenda**

Architecture spec: append after §12:
```markdown
## Addendum 2026-09-06 (Phase 2)
`docs/superpowers/specs/2026-09-06-balerix-a2-runtime-design.md` §8 lists the
corrections Phase 2 made to §3 (ports), §4 (agent environment via the nono
profile; nono's own `$HOME`), and §6 (worktree branch reuse, pipeline split
into `Materializer`). Where they differ, the Phase 2 spec wins.
```
Phase 2 spec: (a) §2.1 note that `Timestamp` and `SpecHash` are defined in `balerix-api::status` and re-exported; (b) §3.2 replace the `MarkDead` row with `NoteExit(agent, code)` — "`Exited`, not `Dead`, `next_restart_at` is `None`" — and state that `apply` moves the agent to `Dead` when `restarts > max_restarts`; (c) §2.2/§3.2 `remove_crew(crew, keep: Keep)` and `RemoveCrew(crew, Keep)`; (d) fill every remaining *Verdict* in §4.4 (rows 1 and 4 are exercised by `sandbox_it` and `home.rs`: record what `CLAUDE_CONFIG_DIR`/`GH_CONFIG_DIR` did to nono's home in the integration run — `ls agents/<a>/nono` after `materialize_it` — and which `.claude.json` keys were needed).

- [ ] **Step 7: Verify onboarding, commit**

Run, from a clean checkout in a temp dir (`git clone /workspace /tmp/…` is fine here), the README quickstart lines 1, 3 and 5. Then:
```bash
mise run check && mise run test-it
git add mise.toml .github ARCHITECTURE.md AGENTS.md README.md docs
git commit -m "Document Phase 2: architecture map, gotchas, threat model, CI tiers, spec addenda

Claude-Session: https://claude.ai/code/session_01Ub1PnbZbwk8qhVwmPMTWpU"
```

## Done when

- `mise run check` passes on a fresh clone here and in CI with `BALERIX_REQUIRE_TOOLS=1` (no integration test skipped).
- `balerix dev materialize examples/payments.yaml backend/bob --no-host-defaults` writes the four files; `generated_golden` matches them.
- `reconcile_model` runs 256 cases clean; `mise run mutants` reports no surviving mutants in `balerix-core/src/reconcile/`.
- Every §4.4 row of the Phase 2 spec carries a *Verdict*.
- Test count: `balerix-api` +4, `balerix-core` +~30 unit + 4 golden + 1 model, `balerix-runtime` ~30 unit + 1 golden + 6 integration, `balerix` +4.

## Deliberately deferred

- `FleetStore`, `EventHandler`, vault, API, `serve`/`up`/`update`/`down`, e2e with `fake-claude` — Phase 3.
- Verifying that Claude Code's HTTP hooks send custom headers to a self-signed HTTPS endpoint — Phase 3 (architecture spec §12 row 1).
- A repository's own `mise.toml` inside the sandbox: `mise exec` in the worktree will discover it. `MISE_AUTO_INSTALL=false` stops installs, but whether mise then errors or ignores the missing tools is verified in Phase 3's e2e with a real checkout.
- Materialize-failure backoff: a failed pipeline step is retried every pass (30 s resync in Phase 3), not with exponential backoff; revisit if a flapping clone ever hammers a remote.
