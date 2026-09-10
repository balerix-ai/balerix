# Balerix Spec A / Phase 3 — Control Plane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the `balerix-server` daemon (per-fleet actors over the Phase 2 reconciler, file store with an encrypted secrets vault, plain-HTTP API on loopback, hook ingress, Prometheus metrics), the CLI commands `serve`, `up`, `update`, `down`, `status`, `list` and `hook-relay`, three runtime carry-overs, and an end-to-end journey driven by a fake Claude. Spec A is complete when this plan is done.

**Architecture:** `balerix-core` gains the `FleetStore` and `EventHandler` ports plus the persisted `FleetRecord`/`FleetSecrets`. `balerix-server` (depends on `core` + `api` only) runs one tokio task per fleet that owns the fleet's record and status, runs `reconcile_pass` inside `spawn_blocking`, persists after every pass, and publishes snapshots through a `watch` channel that the axum handlers read. Hook ingress authenticates a per-agent secret, rate-limits, and forwards `Event` messages to the actor; `SessionStart` arrives through `balerix hook-relay` (a `command` hook, because Claude refuses HTTP hooks for that event) while the other events stay HTTP hooks. The binary wires `Runtime`, `TmuxRunner`, `FileFleetStore`, `SystemClock` and `PassThrough` into the daemon and talks to it with `ureq`.

**Tech Stack:** Rust 1.98.1 (edition 2024); tokio 1.53.1, axum 0.8.9, ureq 3.4.1 (no TLS), prometheus 0.14.0, chacha20poly1305 0.11.0, rand 0.10.2, tracing 0.1.44 + tracing-subscriber 0.3.23; serde/serde_json/toml; thiserror; clap 4; insta, proptest; tempfile, assert_cmd, predicates; real `git`, `mise`, `nono 0.75.0`, `tmux 3.7c` for the e2e.

**Spec:** `docs/superpowers/specs/2026-09-06-balerix-a3-control-plane-design.md` (the *Phase 3 spec*), on top of `docs/superpowers/specs/2026-09-06-balerix-a2-runtime-design.md` (the *Phase 2 spec*) and `docs/superpowers/specs/2026-09-05-balerix-architecture-design.md` (the *architecture spec*). Read Phase 3 spec §2–§5 before any server or CLI task, §6 before Task 3–4, §7–§8 before Task 11–12. Where this plan refines the spec (recorded again in Task 13): `FleetStore` is `load_all`/`put`/`purge` — `purge` removes the whole `fleets/<name>/` directory (the spec put that removal on the actor without naming who touches the filesystem) and the spec's `delete` is dropped because nothing would call it; `FleetPhase::Down` is set when a terminating pass succeeded **and** the status holds no agents (`agent_ready` cannot flip a mid-termination fleet to `Down`); the `DownQuery` flags travel as `key=true` because axum's `Query` rejects bare keys; `serve` accepts a hidden `--tmux-socket` so the e2e never touches a real daemon's tmux server.

## Global Constraints

Copied from the specs; every task's requirements include these.

- Rust **1.98.1**, `edition = "2024"`, `rust-version = "1.98"`; every tool in `mise.toml` is an exact version. Run cargo as `mise x -- cargo …` or through `mise run <task>`.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass at every commit. `unsafe_code = "forbid"`. No `unwrap`/`expect` outside tests (clippy warns; `-D warnings` makes it an error; test modules and `tests/*.rs` carry `#![allow(clippy::unwrap_used, clippy::expect_used)]`). `std::env::set_var` is unsafe in edition 2024 — inject environment through parameters.
- Library crates return `thiserror` errors whose `Display` starts with the fleet/crew/agent id (runtime, server) or config path (config, core); only the `balerix` binary uses `anyhow`.
- Dependency direction: `api` leaf → `core` → `config` / `runtime` / `server` (adapters) → binary. `server` never depends on `runtime` or `config`; no adapter depends on another adapter.
- New Cargo dependencies go in `[workspace.dependencies]` with an exact version and a reason in the commit message. This plan adds exactly: `tokio = "1.53.1"`, `axum = "0.8.9"`, `ureq = "3.4.1"` (`default-features = false, features = ["json"]` — no TLS stack, P3-1/P3-9), `prometheus = "0.14.0"`, `chacha20poly1305 = "0.11.0"`, `rand = "0.10.2"`, `tracing = "0.1.44"`, `tracing-subscriber = "0.3.23"`.
- Plain HTTP on `127.0.0.1` only (P3-1). No `rustls`, `rcgen`, `reqwest`.
- Secrets never appear in `Debug` output, logs, argv, the outer environment, or `launch.sh`. `FleetSecrets`, `Vault`, `HookTarget`, `CredentialBundle` hand-implement `Debug` with `<redacted>`. Hook payloads are logged at `debug` only.
- Subprocesses are argv arrays via `std::process::Command`; never a shell string. `launch.sh` and the hook command pass through `sh_quote`.
- Names are already validated (`FleetName` etc.); the runtime and server never build a path from an unvalidated string. API path segments are parsed into `FleetName`/`AgentId` before use.
- Integration and e2e tests skip with a printed reason when a tool or Landlock is missing; if `BALERIX_REQUIRE_TOOLS=1` is set (CI), a would-be skip panics instead. Temp roots live under `target/tmp` (`CARGO_TARGET_TMPDIR`), never `/tmp`.
- Commit messages: imperative subject, body explains why, and end with the trailer line `Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT`.
- insta: read every `.snap.new`, compare against the expected values listed in the task, then `mise x -- cargo insta accept`. Never blind-accept.

## File structure

```
Cargo.toml                                        + balerix-server member/dep; tokio, axum, ureq, prometheus, chacha20poly1305, rand, tracing, tracing-subscriber
mise.toml                                         + tasks: serve, e2e
.github/workflows/ci.yml                          e2e runs inside `check` (no new installs)
crates/balerix-api/src/fleet.rs                   + GitIdentity; GitSettings.identity
crates/balerix-api/src/status.rs                  + FleetPhase::Down, FleetSummary
crates/balerix-api/src/hook.rs                    HookEvent
crates/balerix-api/src/request.rs                 + DownQuery, ErrorBody
crates/balerix-core/src/fleet.rs                  + identity validation (FleetError::EmptyIdentity)
crates/balerix-core/src/ports.rs                  Keep: serde
crates/balerix-core/src/store.rs                  FleetRecord, Desired, FleetSecrets, StoreError, FleetStore
crates/balerix-core/src/events.rs                 Outcome, EventHandler, PassThrough
crates/balerix-core/src/reconcile/status.rs       Terminating → Down
crates/balerix-runtime/src/tools.rs               ToolPaths.balerix
crates/balerix-runtime/src/layout.rs              + server_dir, fleets_dir
crates/balerix-runtime/src/env.rs                 + BALERIX_HOOK_SECRET
crates/balerix-runtime/src/home.rs                SessionStart command hook; .gitconfig
crates/balerix-runtime/src/sandbox.rs             + balerix binary read grant
crates/balerix-runtime/src/materializer.rs        RenderOutcome, install marker
crates/balerix-runtime/src/workspace.rs           clone-only ensure_repo; fetch before -b
crates/balerix-runtime/tests/generated_golden.rs  snapshot update (+ .gitconfig)
crates/balerix-server/Cargo.toml
crates/balerix-server/src/lib.rs                  re-exports
crates/balerix-server/src/vault.rs                Vault (XChaCha20-Poly1305), random_hex
crates/balerix-server/src/store.rs                FileFleetStore
crates/balerix-server/src/metrics.rs              Metrics (8 series), encode, set_gauges
crates/balerix-server/src/auth.rs                 constant_time_eq, bearer, RateLimiter
crates/balerix-server/src/hooks.rs                parse_event, events handler
crates/balerix-server/src/actor.rs                Msg, FleetHandle, Ports, Shared, spawn
crates/balerix-server/src/daemon.rs               Daemon registry, DaemonError
crates/balerix-server/src/api.rs                  router, handlers, ApiError, serve
crates/balerix-server/src/lifecycle.rs            ServerPaths, token, endpoint, pid
crates/balerix-server/tests/api_it.rs             in-process router over the fakes
crates/balerix/src/cli.rs                         + serve/up/update/down/status/list/hook-relay/dev fake-claude
crates/balerix/src/wiring.rs                      + balerix path, server paths, SystemClock
crates/balerix/src/client.rs                      Endpoint resolution, Client (ureq)
crates/balerix/src/commands/serve.rs              serve, config.toml, detach
crates/balerix/src/commands/fleet.rs              up/update/down/status/list, renderers, wait loop
crates/balerix/src/commands/relay.rs              hook-relay
crates/balerix/src/commands/dev.rs                + fake-claude
crates/balerix/tests/cli_fleet.rs                 renderer/endpoint tests via the binary (--help, offline errors)
crates/balerix/tests/e2e.rs                       the journey
ARCHITECTURE.md AGENTS.md README.md docs/THREAT-MODEL.md   Phase 3 updates
docs/superpowers/specs/2026-09-05-balerix-architecture-design.md   dated addendum
docs/superpowers/specs/2026-09-06-balerix-a3-control-plane-design.md   §8.1 verdicts, refinements
```

---

### Task 1: Workspace dependencies and `balerix-api` additions

**Files:**
- Modify: `Cargo.toml` (workspace)
- Modify: `crates/balerix-api/src/fleet.rs`
- Modify: `crates/balerix-api/src/status.rs`
- Create: `crates/balerix-api/src/hook.rs`
- Modify: `crates/balerix-api/src/request.rs`
- Modify: `crates/balerix-api/src/lib.rs`
- Modify: `crates/balerix-runtime/tests/materialize_it.rs` (struct literal gains `..GitSettings::default()`)

**Interfaces:**
- Produces:
  ```rust
  pub struct GitIdentity { pub name: String, pub email: String }
  pub struct GitSettings { pub push: bool, pub auth: GitAuth, pub identity: Option<GitIdentity> }
  pub enum FleetPhase { Pending, Reconciling, Ready, Degraded, Terminating, Down }
  pub struct FleetSummary { pub name: String, pub phase: FleetPhase, pub generation: u64, pub observed_generation: u64, pub agents: usize }
  pub struct HookEvent { pub agent: String, pub name: String, pub session_id: Option<String>, pub received_at: Timestamp, pub payload: Value }
  pub struct DownQuery { pub keep_repos: bool, pub keep_sessions: bool, pub purge: bool }
  impl DownQuery { pub fn to_query_string(&self) -> String }   // "keep_repos=true&keep_sessions=false&purge=false"
  pub struct ErrorBody { pub error: String }
  ```

- [ ] **Step 1: Add the workspace dependencies**

In `Cargo.toml` `[workspace.dependencies]`, after `balerix-runtime`:
```toml
balerix-server = { path = "crates/balerix-server" }
```
after `toml = "1.1.5"`:
```toml
tokio = { version = "1.53.1", features = ["rt-multi-thread", "macros", "sync", "time", "signal", "net"] }
axum = "0.8.9"
# no TLS stack on purpose (Phase 3 spec P3-1, P3-9)
ureq = { version = "3.4.1", default-features = false, features = ["json"] }
prometheus = "0.14.0"
chacha20poly1305 = "0.11.0"
rand = "0.10.2"
tracing = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }
```

- [ ] **Step 2: Write the failing api tests**

Append to the `tests` module of `crates/balerix-api/src/fleet.rs`:
```rust
    #[test]
    fn identity_is_optional_and_round_trips() {
        let g: GitSettings = serde_json::from_value(json!({ "push": true, "auth": "none" })).unwrap();
        assert_eq!(g.identity, None);
        let back = serde_json::to_value(&g).unwrap();
        assert!(back.get("identity").is_none(), "absent identity is not serialized");
        let g: GitSettings = serde_json::from_value(
            json!({ "identity": { "name": "Alice Bot", "email": "alice@example.com" } }),
        )
        .unwrap();
        assert_eq!(
            g.identity,
            Some(GitIdentity { name: "Alice Bot".into(), email: "alice@example.com".into() })
        );
        assert!(
            serde_json::from_value::<GitSettings>(json!({ "identity": { "name": "x", "nope": 1 } }))
                .is_err()
        );
    }
```
Append to the `tests` module of `crates/balerix-api/src/status.rs`:
```rust
    #[test]
    fn down_is_a_phase_and_summaries_round_trip() {
        assert_eq!(serde_json::to_value(FleetPhase::Down).unwrap(), json!("down"));
        let s = FleetSummary {
            name: "payments".into(),
            phase: FleetPhase::Ready,
            generation: 3,
            observed_generation: 3,
            agents: 2,
        };
        let back: FleetSummary = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
```
Create `crates/balerix-api/src/hook.rs`:
```rust
//! Hook ingress wire type (architecture spec §8; Phase 3 spec §2). The
//! payload stays raw JSON on purpose: Spec B matches on JSON-pointer paths.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Timestamp;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookEvent {
    /// `fleet/crew/agent`.
    pub agent: String,
    /// Claude's `hook_event_name`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub received_at: Timestamp,
    pub payload: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_with_and_without_a_session() {
        let e = HookEvent {
            agent: "f/c/a".into(),
            name: "PreToolUse".into(),
            session_id: None,
            received_at: Timestamp(5),
            payload: json!({ "tool_input": { "command": "ls" } }),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert!(v.get("session_id").is_none());
        let back: HookEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, e);
    }
}
```
Append to the `tests` module of `crates/balerix-api/src/request.rs`:
```rust
    #[test]
    fn down_query_defaults_to_false_and_renders_every_flag() {
        let q: DownQuery = serde_json::from_value(serde_json::json!({ "purge": true })).unwrap();
        assert!(q.purge && !q.keep_repos && !q.keep_sessions);
        assert_eq!(
            DownQuery { keep_repos: true, keep_sessions: false, purge: false }.to_query_string(),
            "keep_repos=true&keep_sessions=false&purge=false"
        );
        assert!(serde_json::from_value::<DownQuery>(serde_json::json!({ "x": 1 })).is_err());
        let e: ErrorBody = serde_json::from_str(r#"{"error":"fleet exists"}"#).unwrap();
        assert_eq!(e.error, "fleet exists");
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-api`
Expected: compile errors (`GitIdentity`, `FleetPhase::Down`, `FleetSummary`, `DownQuery`, `ErrorBody`, `hook` module not found).

- [ ] **Step 4: Implement**

`crates/balerix-api/src/fleet.rs` — replace the `GitSettings` struct and its `Default`:
```rust
/// Git permissions for a crew.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSettings {
    #[serde(default = "default_true")]
    pub push: bool,
    #[serde(default)]
    pub auth: GitAuth,
    /// Commit identity written to the agent's `.gitconfig` (Phase 3 spec
    /// §6.3). Absent → derived from the agent id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<GitIdentity>,
}

impl Default for GitSettings {
    fn default() -> Self {
        Self {
            push: true,
            auth: GitAuth::default(),
            identity: None,
        }
    }
}

/// `user.name` / `user.email` for commits made inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
}
```
`crates/balerix-api/src/status.rs` — add `Down,` after `Terminating,` in `FleetPhase` and append:
```rust
/// One row of `GET /v1/fleets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSummary {
    pub name: String,
    pub phase: FleetPhase,
    pub generation: u64,
    pub observed_generation: u64,
    pub agents: usize,
}
```
`crates/balerix-api/src/request.rs` — append after `FleetRequest`:
```rust
/// Query flags of `DELETE /v1/fleets/{name}` (spec D6). Every flag is sent
/// as `key=true|false`: axum's `Query` rejects a bare key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DownQuery {
    pub keep_repos: bool,
    pub keep_sessions: bool,
    pub purge: bool,
}

impl DownQuery {
    pub fn to_query_string(&self) -> String {
        format!(
            "keep_repos={}&keep_sessions={}&purge={}",
            self.keep_repos, self.keep_sessions, self.purge
        )
    }
}

/// Body of every non-2xx API response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}
```
`crates/balerix-api/src/lib.rs` — add `pub mod hook;` after `pub mod fleet;` and extend the re-exports:
```rust
pub use fleet::{CrewSpec, FleetSpec, GitAuth, GitIdentity, GitSettings};
pub use hook::HookEvent;
pub use request::{DownQuery, ErrorBody, FleetRequest};
pub use status::{
    AgentPhase, AgentStatus, FleetPhase, FleetStatus, FleetSummary, SpecHash, Timestamp,
};
```
`crates/balerix-runtime/tests/materialize_it.rs` — the `GitSettings { push: false, auth: GitAuth::None }` literal becomes:
```rust
                git: GitSettings {
                    push: false,
                    auth: GitAuth::None,
                    ..GitSettings::default()
                },
```

- [ ] **Step 5: Run the workspace tests**

Run: `mise run check`
Expected: PASS (`balerix-api` +4 tests; every other crate unchanged). If any golden snapshot mentions `git` (the `resolve_golden` snapshots serialize `GitSettings`), it is unchanged because `identity` is skipped when `None`.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/balerix-api crates/balerix-runtime/tests/materialize_it.rs
git commit -m "Add the Phase 3 wire types and pin the control-plane dependencies

GitIdentity (spec §6.3), FleetPhase::Down and FleetSummary (P3-5, P3-7),
HookEvent (§2), DownQuery and ErrorBody (§3.4). Dependencies: tokio and
axum for the daemon, ureq without TLS for the CLI client and relay
(P3-1, P3-9), prometheus for /metrics, chacha20poly1305 + rand for the
vault and secrets, tracing + tracing-subscriber for the daemon log.

Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT"
```

---

### Task 2: `balerix-core` — store and event ports, `Down`, identity validation

**Files:**
- Modify: `crates/balerix-core/src/ports.rs` (`Keep` serde)
- Create: `crates/balerix-core/src/store.rs`
- Create: `crates/balerix-core/src/events.rs`
- Modify: `crates/balerix-core/src/reconcile/status.rs`
- Modify: `crates/balerix-core/src/reconcile/execute.rs` (one test)
- Modify: `crates/balerix-core/tests/reconcile_model.rs` (phase oracle)
- Modify: `crates/balerix-core/src/fleet.rs`
- Modify: `crates/balerix-core/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct FleetRecord { pub spec: FleetSpec, pub generation: u64, pub desired: Desired, pub status: FleetStatus }
  impl FleetRecord { pub fn new(spec: FleetSpec) -> Self; pub fn name(&self) -> &str; pub fn summary(&self) -> FleetSummary; pub fn is_down(&self) -> bool }
  pub enum Desired { Up, Down { keep: Keep, purge: bool } }
  pub struct FleetSecrets { pub credentials: CredentialBundle, pub hook_secrets: BTreeMap<String, String> }
  pub enum StoreError { Io { path: PathBuf, message: String }, Corrupt { path: PathBuf, message: String } }
  pub trait FleetStore: Send + Sync {
      fn load_all(&self) -> Result<Vec<(FleetRecord, FleetSecrets)>, StoreError>;
      fn put(&self, record: &FleetRecord, secrets: &FleetSecrets) -> Result<(), StoreError>;
      fn purge(&self, name: &FleetName) -> Result<(), StoreError>;   // removes fleets/<name>/ entirely
  }
  pub struct Outcome { pub response: serde_json::Value }
  impl Outcome { pub fn allow() -> Self }
  pub trait EventHandler: Send + Sync { fn handle(&self, event: &HookEvent) -> Outcome; }
  pub struct PassThrough;
  // finish_pass(status, terminating: true, all_ok: true) with no agents left ⇒ FleetPhase::Down
  // FleetError::EmptyIdentity { path }  for `crews.<c>.git.identity.name|email` == ""
  ```

- [ ] **Step 1: Write the failing tests**

Create `crates/balerix-core/src/store.rs` with only the test module for now (the types come in Step 3):
```rust
//! What the daemon persists per fleet, and the port it persists through
//! (Phase 3 spec §2). The registry in memory is authoritative while the
//! daemon runs; the store is how it survives a restart.

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{FleetPhase, FleetSpec};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn spec() -> FleetSpec {
        FleetSpec {
            name: "payments".into(),
            crews: BTreeMap::new(),
        }
    }

    #[test]
    fn a_new_record_is_up_at_generation_zero() {
        let r = FleetRecord::new(spec());
        assert_eq!(r.generation, 0);
        assert_eq!(r.desired, Desired::Up);
        assert_eq!(r.status.phase, FleetPhase::Pending);
        assert_eq!(r.name(), "payments");
        assert!(!r.is_down());
        let s = r.summary();
        assert_eq!((s.name.as_str(), s.generation, s.agents), ("payments", 0, 0));
    }

    #[test]
    fn desired_serializes_with_a_state_tag() {
        assert_eq!(serde_json::to_value(Desired::Up).unwrap(), json!({ "state": "up" }));
        let d = Desired::Down {
            keep: Keep {
                repos: true,
                sessions: false,
            },
            purge: false,
        };
        let v = serde_json::to_value(d).unwrap();
        assert_eq!(v["state"], "down");
        assert_eq!(v["keep"]["repos"], true);
        let back: Desired = serde_json::from_value(v).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn is_down_needs_both_the_desire_and_the_phase() {
        let mut r = FleetRecord::new(spec());
        r.desired = Desired::Down {
            keep: Keep::default(),
            purge: false,
        };
        assert!(!r.is_down(), "still terminating");
        r.status.phase = FleetPhase::Down;
        assert!(r.is_down());
    }

    #[test]
    fn secrets_debug_is_redacted_and_round_trips() {
        let s = FleetSecrets {
            credentials: CredentialBundle {
                gh_token: Some("gho_SECRET".into()),
                ..CredentialBundle::default()
            },
            hook_secrets: BTreeMap::from([("f/c/a".to_string(), "hook-SECRET".to_string())]),
        };
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("SECRET"), "{dbg}");
        assert!(dbg.contains("hook_secrets: 1 <redacted>"));
        let back: FleetSecrets = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let empty: FleetSecrets = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, FleetSecrets::default());
    }

    #[test]
    fn store_errors_start_with_the_path() {
        let e = StoreError::Corrupt {
            path: "/x/fleet.json".into(),
            message: "expected value at line 1".into(),
        };
        assert_eq!(e.to_string(), "/x/fleet.json: expected value at line 1");
    }
}
```
Create `crates/balerix-core/src/events.rs` with its test module:
```rust
//! The hook-event port (architecture spec §8; Phase 3 spec P3-6). Handlers
//! are fast and pure; `Outcome` carries only the response until Spec B.

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::Timestamp;
    use serde_json::json;

    #[test]
    fn pass_through_allows_everything() {
        let e = HookEvent {
            agent: "f/c/a".into(),
            name: "PreToolUse".into(),
            session_id: None,
            received_at: Timestamp(1),
            payload: json!({ "tool_name": "Bash" }),
        };
        let h: &dyn EventHandler = &PassThrough;
        assert_eq!(h.handle(&e), Outcome::allow());
        assert_eq!(Outcome::allow().response, json!({}));
    }
}
```
In `crates/balerix-core/src/reconcile/status.rs` tests, replace the last two lines of `fleet_phase_is_derived_and_observed_generation_follows_success` (`finish_pass(&mut s, true, true); assert_eq!(s.phase, FleetPhase::Terminating);`) with:
```rust
        finish_pass(&mut s, true, true);
        assert_eq!(
            s.phase,
            FleetPhase::Terminating,
            "agents still recorded: not done"
        );
        s.agents.clear();
        finish_pass(&mut s, true, false);
        assert_eq!(s.phase, FleetPhase::Terminating, "a failed step keeps it");
        finish_pass(&mut s, true, true);
        assert_eq!(s.phase, FleetPhase::Down);
```
In `crates/balerix-core/src/reconcile/execute.rs`, rename the test `down_stops_and_removes_then_terminating` to `down_stops_and_removes_then_down` and change its assertion to `assert_eq!(st.phase, FleetPhase::Down);`.
In `crates/balerix-core/tests/reconcile_model.rs` `check_invariants`, the oracle's first branch becomes:
```rust
        let expected_fleet = if r.desired.is_none() {
            // every pass with `desired: None` stops and removes everything,
            // so a settled down fleet is `Down`, never left `Terminating`
            FleetPhase::Down
```
and the comment in `init_test` changes its parenthetical to `(spec §3.4 and Phase 3 spec P3-5: \`Down\` once a down pass has run clean, \`Pending\` for no agents)`.
Append to the `tests` module of `crates/balerix-core/src/fleet.rs`:
```rust
    #[test]
    fn an_empty_identity_field_reports_its_path() {
        let mut s = spec("f", "c", "acme/x", "main", &["a"]);
        s.crews.get_mut("c").unwrap().git.identity = Some(balerix_api::GitIdentity {
            name: "".into(),
            email: "a@b.c".into(),
        });
        assert_eq!(
            Fleet::try_from(s.clone()).unwrap_err().to_string(),
            "crews.c.git.identity.name: must not be empty"
        );
        s.crews.get_mut("c").unwrap().git.identity = Some(balerix_api::GitIdentity {
            name: "A".into(),
            email: " ".into(),
        });
        assert_eq!(
            Fleet::try_from(s).unwrap_err().to_string(),
            "crews.c.git.identity.email: must not be empty"
        );
    }
```
(`spec(...)` is the existing helper in that module.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-core`
Expected: compile errors in the new modules (no items yet) and in `fleet.rs` tests (`EmptyIdentity` unknown); `finish_pass` assertions fail once it compiles.

- [ ] **Step 3: Implement**

`crates/balerix-core/src/ports.rs` — derive serde on `Keep`:
```rust
/// What `down` leaves behind (spec D6).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
#[serde(default)]
pub struct Keep {
    pub repos: bool,
    pub sessions: bool,
}
```
`crates/balerix-core/src/store.rs` — above the test module:
```rust
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use balerix_api::{CredentialBundle, FleetPhase, FleetSpec, FleetStatus, FleetSummary};
use serde::{Deserialize, Serialize};

use crate::name::FleetName;
use crate::ports::Keep;

/// One fleet as the daemon stores and returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetRecord {
    pub spec: FleetSpec,
    pub generation: u64,
    pub desired: Desired,
    pub status: FleetStatus,
}

/// Whether the fleet should be running (P3-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Desired {
    Up,
    Down { keep: Keep, purge: bool },
}

impl FleetRecord {
    pub fn new(spec: FleetSpec) -> Self {
        Self {
            spec,
            generation: 0,
            desired: Desired::Up,
            status: FleetStatus::default(),
        }
    }
    pub fn name(&self) -> &str {
        &self.spec.name
    }
    /// Downed and settled: `up` may re-apply in place, `POST` is not a 409.
    pub fn is_down(&self) -> bool {
        matches!(self.desired, Desired::Down { .. }) && self.status.phase == FleetPhase::Down
    }
    pub fn summary(&self) -> FleetSummary {
        FleetSummary {
            name: self.spec.name.clone(),
            phase: self.status.phase,
            generation: self.generation,
            observed_generation: self.status.observed_generation,
            agents: self.status.agents.len(),
        }
    }
}

/// Everything secret about a fleet: the credential bundle and one hook
/// secret per agent id. Encrypted at rest by the store.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FleetSecrets {
    #[serde(default)]
    pub credentials: CredentialBundle,
    /// `fleet/crew/agent` → bearer secret its hooks present.
    #[serde(default)]
    pub hook_secrets: BTreeMap<String, String>,
}

impl fmt::Debug for FleetSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FleetSecrets {{ credentials: {:?}, hook_secrets: {} <redacted> }}",
            self.credentials,
            self.hook_secrets.len()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Corrupt { path: PathBuf, message: String },
}

/// Persists fleets. `put` is atomic per fleet; `purge` removes the fleet's
/// whole directory, kept repos and homes included (`down --purge`). There is
/// no narrower delete: a downed fleet keeps its record until purged (P3-5).
pub trait FleetStore: Send + Sync {
    fn load_all(&self) -> Result<Vec<(FleetRecord, FleetSecrets)>, StoreError>;
    fn put(&self, record: &FleetRecord, secrets: &FleetSecrets) -> Result<(), StoreError>;
    fn purge(&self, name: &FleetName) -> Result<(), StoreError>;
}
```
`crates/balerix-core/src/events.rs` — above the test module:
```rust
use balerix_api::HookEvent;
use serde_json::{Value, json};

/// What the daemon answers Claude with. `{}` means allow / no-op.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub response: Value,
}

impl Outcome {
    pub fn allow() -> Self {
        Self { response: json!({}) }
    }
}

pub trait EventHandler: Send + Sync {
    fn handle(&self, event: &HookEvent) -> Outcome;
}

/// Phase 3's only handler: allow everything, do nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassThrough;

impl EventHandler for PassThrough {
    fn handle(&self, _: &HookEvent) -> Outcome {
        Outcome::allow()
    }
}
```
`crates/balerix-core/src/reconcile/status.rs` — `derive_fleet_phase` starts with:
```rust
    if terminating {
        // Down only once a terminating pass ran clean and nothing is left
        // recorded; `agent_ready` (all_ok = true, agents non-empty) cannot
        // get here.
        return if all_ok && status.agents.is_empty() {
            FleetPhase::Down
        } else {
            FleetPhase::Terminating
        };
    }
```
`agent_ready` computes `let terminating = matches!(status.phase, FleetPhase::Terminating | FleetPhase::Down);`.

`crates/balerix-core/src/fleet.rs` — add a variant and the check:
```rust
    #[error("{path}: must not be empty")]
    EmptyIdentity { path: String },
```
in `convert_crew`, after the `git_ref` check:
```rust
    if let Some(identity) = &crew.git.identity {
        for (field, value) in [("name", &identity.name), ("email", &identity.email)] {
            if value.trim().is_empty() {
                return Err(FleetError::EmptyIdentity {
                    path: format!("{path}.git.identity.{field}"),
                });
            }
        }
    }
```
`crates/balerix-core/src/lib.rs` — add `pub mod events;` and `pub mod store;` (alphabetical) and:
```rust
pub use events::{EventHandler, Outcome, PassThrough};
pub use store::{Desired, FleetRecord, FleetSecrets, FleetStore, StoreError};
```

- [ ] **Step 4: Run the tests**

Run: `mise run check`
Expected: PASS. The model test's oracle now expects `Down` for a downed fleet and the `execute` test asserts `Down`. `plan_golden` snapshots are plans only and do not move.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-core
git commit -m "Add the FleetStore and EventHandler ports, the Down phase and identity validation

FleetRecord/Desired/FleetSecrets are what the daemon persists (Phase 3
spec §2); FleetStore is load_all/put/purge (purge backs down --purge). Outcome carries only
the response until Spec B (P3-6). A terminating pass that ran clean with
nothing left recorded settles in Down (P3-5). git.identity fields must
be non-empty, reported with their config path.

Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT"
```

---

### Task 3: `balerix-runtime` — relay hook, hook secret, binary grant, `.gitconfig`

**Files:**
- Modify: `crates/balerix-runtime/src/tools.rs` (`ToolPaths.balerix`)
- Modify: `crates/balerix-runtime/src/layout.rs` (`server_dir`, `fleets_dir`)
- Modify: `crates/balerix-runtime/src/env.rs` (`BALERIX_HOOK_SECRET`)
- Modify: `crates/balerix-runtime/src/home.rs` (command hook for `SessionStart`, `.gitconfig`)
- Modify: `crates/balerix-runtime/src/sandbox.rs` (binary read grant)
- Modify: `crates/balerix-runtime/src/materializer.rs`, `src/launch.rs` (test fixture), `src/toolchain.rs` (test fixture)
- Modify: `crates/balerix-runtime/tests/support/mod.rs`, `tests/sandbox_it.rs`, `tests/generated_golden.rs`, `tests/materialize_it.rs`
- Modify: `crates/balerix/src/wiring.rs`, `src/cli.rs` (`--hooks-url` default), `tests/cli_dev_materialize.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct ToolPaths { pub git, pub gh, pub mise, pub nono, pub tmux, pub balerix: PathBuf }
  impl ToolPaths { pub fn discover_in(path: &OsStr, balerix: &Path) -> Result<Self, MissingTool> }
  impl StateLayout { pub fn server_dir(&self) -> PathBuf /* state_root/server */; pub fn fleets_dir(&self) -> PathBuf /* state_root/fleets */ }
  pub fn agent_env(id, paths, layout, api_url: &str, hook_secret: &str, user_env) -> BTreeMap<String, String>   // + BALERIX_HOOK_SECRET
  pub const RELAY_EVENTS: &[&str] = &["SessionStart"];
  pub fn render_settings(user: &Value, id: &AgentId, hooks: &HookTarget, relay: &Path) -> Value
  pub fn render_gitconfig(id: &AgentId, git: &GitSettings) -> String
  pub struct HomeInputs<'a> { settings, creds, hooks, git: &'a GitSettings, relay: &'a Path, with_gh, redact_credentials }
  pub fn balerix_grants(paths: &AgentPaths, crew: &CrewPaths, layout: &StateLayout, balerix: &Path) -> Grants
  ```

- [ ] **Step 1: Write the failing unit tests**

`crates/balerix-runtime/src/tools.rs` test `discover_finds_tools_on_path_and_names_the_missing_one`: both `discover_in` calls gain a second argument `Path::new("/opt/balerix")`, and add `assert_eq!(t.balerix, PathBuf::from("/opt/balerix"));` after the `tmux` assertion.

`crates/balerix-runtime/src/layout.rs` — append to `agent_paths_follow_the_spec_layout`:
```rust
        assert_eq!(
            l.server_dir(),
            PathBuf::from("/h/.local/state/balerix/server")
        );
        assert_eq!(
            l.fleets_dir(),
            PathBuf::from("/h/.local/state/balerix/fleets")
        );
```

`crates/balerix-runtime/src/env.rs` test: the call becomes `agent_env(&id, &paths, &layout, "http://127.0.0.1:7643", "hook-s3", &user)`; add `assert_eq!(env["BALERIX_HOOK_SECRET"], "hook-s3");` and change the length assertion to `assert_eq!(env.len(), 19);`.

`crates/balerix-runtime/src/home.rs` tests — replace `settings_get_a_hooks_block_for_every_event` and add a `.gitconfig` test:
```rust
    #[test]
    fn settings_get_http_hooks_except_session_start_which_relays() {
        let v = render_settings(
            &json!({ "model": "opus", "hooks": { "Stop": [] } }),
            &id(),
            &hooks(),
            Path::new("/opt/it's/balerix"),
        );
        assert_eq!(v["model"], "opus");
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), HOOK_EVENTS.len());
        let h = &hooks["PreToolUse"][0]["hooks"][0];
        assert_eq!(h["type"], "http");
        assert_eq!(
            h["url"],
            "http://127.0.0.1:7643/v1/agents/payments/backend/alice/events"
        );
        assert_eq!(h["headers"]["Authorization"], "Bearer s3");
        assert_eq!(hooks["Stop"], hooks["PreToolUse"], "user hooks are replaced");
        let s = &hooks["SessionStart"][0]["hooks"][0];
        assert_eq!(s["type"], "command");
        assert_eq!(s["command"], "'/opt/it'\\''s/balerix' hook-relay");
        assert_eq!(s["timeout"], 10);
        assert!(s.get("url").is_none(), "no secret in the relay entry");
    }

    #[test]
    fn gitconfig_renders_identity_and_helper_only_for_gh_push() {
        let by_default = render_gitconfig(&id(), &GitSettings::default());
        assert_eq!(
            by_default,
            "# generated by balerix for payments/backend/alice\n[user]\n\tname = payments/backend/alice\n\temail = alice@backend.payments.balerix.invalid\n[credential]\n\thelper = \n\thelper = !gh auth git-credential\n"
        );
        let named = render_gitconfig(
            &id(),
            &GitSettings {
                identity: Some(GitIdentity {
                    name: "Alice Bot".into(),
                    email: "alice@example.com".into(),
                }),
                ..GitSettings::default()
            },
        );
        assert!(named.contains("\tname = Alice Bot\n\temail = alice@example.com\n"));
        let no_push = render_gitconfig(
            &id(),
            &GitSettings {
                push: false,
                ..GitSettings::default()
            },
        );
        assert!(!no_push.contains("[credential]"));
        let no_auth = render_gitconfig(
            &id(),
            &GitSettings {
                auth: GitAuth::None,
                ..GitSettings::default()
            },
        );
        assert!(!no_auth.contains("[credential]"));
    }
```
The `hooks()` fixture URL becomes `"http://127.0.0.1:7643/"`. In `writes_every_file_with_the_right_mode` and `redaction_replaces_secrets_and_no_gh_means_no_hosts_file`, the `HomeInputs` literals gain `git: &GitSettings::default(), relay: Path::new("/opt/balerix"),`; in the first, add:
```rust
        assert_eq!(mode(&paths.home.join(".gitconfig")), 0o600);
        assert!(
            std::fs::read_to_string(paths.home.join(".gitconfig"))
                .unwrap()
                .contains("gh auth git-credential")
        );
```
Imports for the test module: `use balerix_api::{GitAuth, GitIdentity, GitSettings};` and `use std::path::Path;` (already imported at module level).

`crates/balerix-runtime/src/sandbox.rs` tests — `fixture()` calls `balerix_grants(&paths, &crew, &layout, Path::new("/opt/balerix"))`; in `base_profile_has_the_required_grants_env_and_port` add `assert_eq!(p["filesystem"]["read"][6], "/opt/balerix");`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-runtime --lib`
Expected: compile errors (`balerix` field, `render_gitconfig`, arity mismatches).

- [ ] **Step 3: Implement**

`crates/balerix-runtime/src/tools.rs`:
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPaths {
    pub git: PathBuf,
    pub gh: PathBuf,
    pub mise: PathBuf,
    pub nono: PathBuf,
    pub tmux: PathBuf,
    /// This binary: the `SessionStart` relay hook runs it inside the
    /// sandbox (Phase 3 spec P3-3), so the profile grants it read-only.
    pub balerix: PathBuf,
}
```
and `discover_in(path: &OsStr, balerix: &Path)` sets `balerix: balerix.to_path_buf()`.

`crates/balerix-runtime/src/layout.rs`:
```rust
    /// `server/`: token, vault key, endpoint, pid, log (Phase 3 spec §3.3, §5).
    pub fn server_dir(&self) -> PathBuf {
        self.state_root.join("server")
    }
    pub fn fleets_dir(&self) -> PathBuf {
        self.state_root.join("fleets")
    }
    pub fn fleet_dir(&self, f: &FleetName) -> PathBuf {
        self.fleets_dir().join(f.as_str())
    }
```

`crates/balerix-runtime/src/env.rs` — new parameter `hook_secret: &str` after `api_url`, and a row after `BALERIX_API_URL`:
```rust
        ("BALERIX_HOOK_SECRET".to_string(), hook_secret.to_string()),
```
Update the doc comment: "`BALERIX_HOOK_SECRET` is what `balerix hook-relay` presents; the reserved `BALERIX_` prefix keeps user `env` away from it."

`crates/balerix-runtime/src/home.rs`:
```rust
use std::path::Path;

use balerix_api::{CredentialBundle, GitAuth, GitSettings};
use balerix_core::{AgentId, HookTarget, MaterializeError};
use serde_json::{Map, Value, json};

use crate::fsutil::{ensure_dir, ensure_private_dir, write_atomic};
use crate::layout::AgentPaths;
use crate::quote::sh_quote;

/// Events Claude refuses to deliver over HTTP (verified against claude
/// 2.1.263: "HTTP hooks are not supported for SessionStart/Setup"); these
/// run `balerix hook-relay` as a `command` hook instead (P3-2).
pub const RELAY_EVENTS: &[&str] = &["SessionStart"];

/// User settings plus the balerix-owned `hooks` block. Any user `hooks` key
/// was rejected at validation; this overwrites unconditionally anyway.
pub fn render_settings(user: &Value, id: &AgentId, hooks: &HookTarget, relay: &Path) -> Value {
    let mut settings = match user {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    let url = format!(
        "{}/v1/agents/{}/{}/{}/events",
        hooks.url.trim_end_matches('/'),
        id.fleet,
        id.crew,
        id.agent
    );
    let http = json!([{ "hooks": [{ "type": "http", "url": url, "headers": { "Authorization": format!("Bearer {}", hooks.secret) } }] }]);
    // The relay reads the URL and secret from its environment (set through
    // the nono profile), so this entry carries neither.
    let command = json!([{ "hooks": [{ "type": "command", "command": format!("{} hook-relay", sh_quote(&relay.display().to_string())), "timeout": 10 }] }]);
    let block: Map<String, Value> = HOOK_EVENTS
        .iter()
        .map(|e| {
            let entry = if RELAY_EVENTS.contains(e) {
                command.clone()
            } else {
                http.clone()
            };
            (e.to_string(), entry)
        })
        .collect();
    settings.insert("hooks".to_string(), Value::Object(block));
    Value::Object(settings)
}

/// `home/.gitconfig` (Phase 3 spec §6.3): identity always; the gh credential
/// helper only when the crew may push through gh. `gh` resolves through the
/// agent's mise shims inside the sandbox.
pub fn render_gitconfig(id: &AgentId, git: &GitSettings) -> String {
    let (name, email) = match &git.identity {
        Some(i) => (i.name.clone(), i.email.clone()),
        None => (
            id.to_string(),
            format!("{}@{}.{}.balerix.invalid", id.agent, id.crew, id.fleet),
        ),
    };
    let mut out = format!("# generated by balerix for {id}\n[user]\n\tname = {name}\n\temail = {email}\n");
    if git.auth == GitAuth::Gh && git.push {
        out.push_str("[credential]\n\thelper = \n\thelper = !gh auth git-credential\n");
    }
    out
}
```
`HomeInputs` gains `pub git: &'a GitSettings, pub relay: &'a Path,`. In `write_home`, the settings call becomes `render_settings(inputs.settings, id, inputs.hooks, inputs.relay)`, and after the `.claude.json` loop:
```rust
    // 0600 like the rest of home/: it names the credential helper.
    let gitconfig = paths.home.join(".gitconfig");
    write_atomic(
        &gitconfig,
        render_gitconfig(id, inputs.git).as_bytes(),
        0o600,
    )
    .map_err(|e| io_err(id, &gitconfig, e))?;
```

`crates/balerix-runtime/src/sandbox.rs` — `balerix_grants(paths, crew, layout, balerix: &Path)` pushes `read.push(balerix.to_path_buf());` after the mise data dir (verified 2026-09-06: nono 0.75.0 validates and enforces a single-file `read` entry; `nono run` inside such a profile executes the granted binary).

`crates/balerix-runtime/src/materializer.rs` — in `render_agent`:
```rust
        write_home(
            id,
            &paths,
            &HomeInputs {
                settings: &agent.settings.claude.settings,
                creds,
                hooks,
                git: &agent.git,
                relay: &self.tools.balerix,
                with_gh,
                redact_credentials: opts.redact_credentials,
            },
        )?;
        …
        let env = agent_env(
            id,
            &paths,
            &self.layout,
            &hooks.url,
            &hooks.secret,
            &agent.settings.env,
        );
        let profile = render_profile(
            id,
            &balerix_grants(&paths, &crew, &self.layout, &self.tools.balerix),
            hooks_port(&hooks.url),
            &env,
            &agent.settings.sandbox,
        )?;
```
Fixtures: `launch.rs` `tools()` and `toolchain.rs` `write_reports_changes` gain `balerix: "/opt/balerix".into()` / `balerix: "/x".into()`; `tests/generated_golden.rs` gains `balerix: "/tools/balerix".into()` and its `HookTarget.url` becomes `"http://127.0.0.1:7643"`; `tests/materialize_it.rs` URL likewise. `tests/support/mod.rs`:
```rust
/// Tools from the test process's PATH (mise puts the pinned ones there); the
/// relay binary slot is filled with this test executable — it only has to
/// exist for the profile to validate.
pub fn tools() -> Option<ToolPaths> {
    let exe = std::env::current_exe().ok()?;
    ToolPaths::discover_in(&std::env::var_os("PATH").unwrap_or_default(), &exe).ok()
}
```
`tests/sandbox_it.rs`: `agent_env(&id, &paths, &layout, "http://127.0.0.1:7643", "s3", &Default::default())` and `balerix_grants(&paths, &crew, &layout, &tools.balerix)`.

`crates/balerix/src/wiring.rs`:
```rust
pub fn tool_paths() -> Result<ToolPaths> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let me = std::env::current_exe().context("cannot determine balerix's own path")?;
    ToolPaths::discover_in(&path, &me).map_err(|e| {
        anyhow::anyhow!("{e} (balerix needs git, gh, mise, nono and tmux on PATH; see mise.toml)")
    })
}
```
`crates/balerix/src/cli.rs`: `--hooks-url` default `"http://127.0.0.1:7643"`. `crates/balerix/tests/cli_dev_materialize.rs` `renders_the_four_files_with_redacted_credentials_by_default`: add `assert!(settings.contains("hook-relay"));` after the events-URL assertion, and assert `Path::new(&agent_dir).join("home/.gitconfig")` exists.

- [ ] **Step 4: Run the unit tests, then the golden**

Run: `mise x -- cargo test -p balerix-runtime --lib` → PASS.
Run: `mise x -- cargo test -p balerix-runtime --test generated_golden` → FAIL with a `.snap.new`. In `tests/generated_golden.rs`, extend the per-agent file list with `(".gitconfig", paths.home.join(".gitconfig"))` after `settings.json`. Rerun and review `crates/balerix-runtime/tests/snapshots/generated_golden__payments_generated.snap.new`. Expected differences from the old snapshot, for both agents:
  - `settings.json`: `"SessionStart"` is `[{"hooks":[{"command":"'/tools/balerix' hook-relay","timeout":10,"type":"command"}]}]`; every other event unchanged except the URL scheme `http://`.
  - new `.gitconfig` section: `[user]` name `payments/backend/<agent>`, email `<agent>@backend.payments.balerix.invalid`, then the `[credential]` block (the fixture's `GitSettings::default()` is gh + push).
  - `nono-profile.json`: `filesystem.read` has a seventh entry `/tools/balerix`; `set_vars` gains `"BALERIX_HOOK_SECRET": "secret-<agent>"` and `BALERIX_API_URL` is `http://127.0.0.1:7643`.
  - `mise.toml`, `launch.sh`: unchanged.
Then `mise x -- cargo insta accept`.

- [ ] **Step 5: Run everything**

Run: `mise run check && mise run test-it`
Expected: PASS (the `cli_dev_materialize` tests see `hook-relay` and `.gitconfig`; `sandbox_it` validates the profile with the extra grant).

- [ ] **Step 6: Commit**

```bash
git add crates/balerix-runtime crates/balerix
git commit -m "Relay SessionStart through balerix, grant the binary, write .gitconfig

Claude 2.1.263 refuses HTTP hooks for SessionStart, so that event runs
\`balerix hook-relay\` as a command hook and the nono profile grants the
binary read-only and passes BALERIX_HOOK_SECRET (P3-2, P3-3). Every
agent gets a .gitconfig with an identity and, for gh crews that may
push, the gh credential helper (spec §6.3). Hook URLs are plain http.

Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT"
```

---

### Task 4: `balerix-runtime` — cheap steady state and the install marker

**Files:**
- Modify: `crates/balerix-runtime/src/workspace.rs`
- Modify: `crates/balerix-runtime/src/layout.rs` (`installed_marker`)
- Modify: `crates/balerix-runtime/src/sandbox.rs` (`write_profile` returns `bool`)
- Modify: `crates/balerix-runtime/src/materializer.rs`
- Modify: `crates/balerix-runtime/src/lib.rs` (export `RenderOutcome`)
- Modify: `crates/balerix-runtime/tests/workspace_it.rs`, `tests/materialize_it.rs`, `tests/generated_golden.rs`
- Modify: `crates/balerix/src/commands/dev.rs`

**Interfaces:**
- Produces:
  ```rust
  impl AgentPaths { pub fn installed_marker(&self) -> PathBuf }   // agents/<a>/.installed
  pub fn write_profile(id, paths, profile) -> Result<bool, MaterializeError>   // true if bytes changed
  pub struct RenderOutcome { pub plan: LaunchPlan, pub toolchain_changed: bool }
  impl Runtime { pub fn render_agent(..) -> Result<RenderOutcome, MaterializeError>; pub fn install_and_validate(&self, agent) -> Result<(), MaterializeError> /* skips when the marker exists */ }
  // Workspace::ensure_repo: clone if absent, else nothing. ensure_worktree fetches only before `worktree add -b`.
  ```

- [ ] **Step 1: Write the failing tests**

In `crates/balerix-runtime/tests/workspace_it.rs` `clone_worktree_reuse_and_remove`, replace the `// fetch path` line and add a log check right after the first `ensure_worktree`:
```rust
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap(); // present → no git call
    let git_log = || std::fs::read_to_string(crew.root.join("logs").join("git.log")).unwrap_or_default();
    let fetches = |log: &str| log.lines().filter(|l| l.starts_with("$ git") && l.contains(" fetch ")).count();
    assert_eq!(fetches(&git_log()), 0, "a second ensure_repo must not fetch");

    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(fetches(&git_log()), 1, "creating a branch fetches first");
```
and after the idempotent second `ensure_worktree`: `assert_eq!(fetches(&git_log()), 1, "a registered worktree costs no fetch");`.

In `crates/balerix-runtime/tests/materialize_it.rs` `materialize_then_remove_round_trip`, after the second (idempotent) `materialize`:
```rust
    assert!(paths.installed_marker().exists());
    let mise_log = std::fs::read_to_string(paths.logs.join("mise.toolchain.log")).unwrap();
    assert_eq!(
        mise_log.lines().filter(|l| l.starts_with("$ mise install")).count(),
        1,
        "unchanged table → install skipped on the second pass"
    );
```

New unit tests in `crates/balerix-runtime/src/materializer.rs` (a `tests` module at the bottom; it renders into a tempdir with fake tool paths and never spawns anything):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentSettings, CrewSpec, FleetSpec};
    use balerix_core::Fleet;
    use std::collections::BTreeMap;

    fn runtime(root: &std::path::Path) -> Runtime {
        let layout = StateLayout {
            state_root: root.join("state"),
            data_root: root.join("data"),
            config_root: root.join("config"),
        };
        std::fs::create_dir_all(&layout.config_root).unwrap();
        std::fs::write(layout.system_mise_toml(), "[tools]\n").unwrap();
        let tools = ToolPaths {
            git: "/nonexistent/git".into(),
            gh: "/nonexistent/gh".into(),
            mise: "/nonexistent/mise".into(),
            nono: "/nonexistent/nono".into(),
            tmux: "/nonexistent/tmux".into(),
            balerix: "/nonexistent/balerix".into(),
        };
        Runtime::new(layout, tools)
    }

    fn agent(tools: &[(&str, &str)]) -> ResolvedAgent {
        let mut s = AgentSettings::default();
        s.tools = tools
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let fleet = Fleet::try_from(FleetSpec {
            name: "f".into(),
            crews: BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "acme/x".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: BTreeMap::from([("a".to_string(), s)]),
                },
            )]),
        })
        .unwrap();
        ResolvedAgent::from_fleet(&fleet).remove(0)
    }

    fn hooks() -> HookTarget {
        HookTarget {
            url: "http://127.0.0.1:7643".into(),
            secret: "s".into(),
        }
    }

    #[test]
    fn rendering_reports_toolchain_changes_and_clears_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path());
        let creds = CredentialBundle::default();
        let a = agent(&[("node", "22.11.0")]);
        let paths = rt.layout.agent(&a.id);
        let first = rt.render_agent(&a, &creds, &hooks(), &RenderOptions::default()).unwrap();
        assert!(first.toolchain_changed);
        let again = rt.render_agent(&a, &creds, &hooks(), &RenderOptions::default()).unwrap();
        assert!(!again.toolchain_changed, "same inputs, same files");
        assert_eq!(again.plan, first.plan);

        std::fs::write(paths.installed_marker(), "").unwrap();
        rt.render_agent(&a, &creds, &hooks(), &RenderOptions::default()).unwrap();
        assert!(paths.installed_marker().exists(), "unchanged render keeps the marker");
        let b = agent(&[("node", "22.12.0")]);
        let changed = rt.render_agent(&b, &creds, &hooks(), &RenderOptions::default()).unwrap();
        assert!(changed.toolchain_changed);
        assert!(!paths.installed_marker().exists(), "a new table invalidates the install");
    }

    #[test]
    fn install_is_skipped_when_the_marker_exists() {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path());
        let a = agent(&[]);
        let paths = rt.layout.agent(&a.id);
        rt.render_agent(&a, &CredentialBundle::default(), &hooks(), &RenderOptions::default())
            .unwrap();
        // tools point nowhere: running mise would fail with "cannot execute"
        assert!(rt.install_and_validate(&a).is_err());
        std::fs::write(paths.installed_marker(), "").unwrap();
        rt.install_and_validate(&a).unwrap();
    }
}
```
Add `tempfile` to `balerix-runtime`'s `[dev-dependencies]` if not already present (it is).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-runtime --lib materializer`
Expected: compile errors (`RenderOutcome`, `installed_marker`).

- [ ] **Step 3: Implement**

`crates/balerix-runtime/src/layout.rs` — in `impl AgentPaths`:
```rust
    /// Written after `mise install` and `nono profile validate` succeed for
    /// the current `mise.toml` + `nono-profile.json`; removed when either
    /// rendered file changes (Phase 3 spec §6.2).
    pub fn installed_marker(&self) -> PathBuf {
        self.root.join(".installed")
    }
```

`crates/balerix-runtime/src/sandbox.rs` — `write_profile` returns `Result<bool, MaterializeError>`:
```rust
/// Writes the profile; returns whether its bytes changed.
pub fn write_profile(
    id: &AgentId,
    paths: &AgentPaths,
    profile: &Value,
) -> Result<bool, MaterializeError> {
    let bytes = serde_json::to_vec_pretty(profile).unwrap_or_default();
    if std::fs::read(&paths.profile).ok().as_deref() == Some(bytes.as_slice()) {
        return Ok(false);
    }
    // 0600: the profile is the sandbox's boundary, and a writable or
    // widely readable one is a map of every path the agent may reach.
    write_atomic(&paths.profile, &bytes, 0o600).map_err(|e| MaterializeError::Io {
        id: id.to_string(),
        path: paths.profile.clone(),
        message: e.to_string(),
    })?;
    Ok(true)
}
```
(`tests/sandbox_it.rs` calls `write_profile(...).unwrap();` — the unused `bool` is fine.)

`crates/balerix-runtime/src/workspace.rs`:
```rust
    /// Clone without a checkout if absent; otherwise nothing (Phase 3 spec
    /// §6.1: a steady-state pass costs no git call). `ensure_worktree`
    /// fetches when it actually needs `origin/<ref>`.
    pub fn ensure_repo(
        &self,
        id: &str,
        crew: &CrewPaths,
        repo: &RepoRef,
        git_ref: &str,
    ) -> Result<(), MaterializeError> {
        let _ = git_ref;
        if crew.repo.join(".git").is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(&crew.root).map_err(|e| MaterializeError::Io {
            id: id.to_string(),
            path: crew.root.clone(),
            message: e.to_string(),
        })?;
        self.git(
            id,
            crew,
            &[
                "clone",
                "--quiet",
                "--no-checkout",
                &repo.clone_url(),
                &crew.repo.display().to_string(),
            ],
        )?;
        Ok(())
    }
```
In `ensure_worktree`, the `else` branch (branch does not exist) first runs:
```rust
            // the only moment `origin/<ref>` must be current
            self.git(id, crew, &["-C", &repo, "fetch", "--quiet", "origin"])?;
```
before `worktree add … -b …`.

`crates/balerix-runtime/src/materializer.rs`:
```rust
/// What `render_agent` produced and whether the installable inputs moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOutcome {
    pub plan: LaunchPlan,
    /// `mise.toml` or `nono-profile.json` changed: the install marker was
    /// removed and `install_and_validate` will run the tools again.
    pub toolchain_changed: bool,
}
```
`render_agent` returns `Result<RenderOutcome, MaterializeError>`: capture `let tools_changed = Toolchain {..}.write(..)?;` and `let profile_changed = write_profile(id, &paths, &profile)?;`, then before `Ok`:
```rust
        let toolchain_changed = tools_changed || profile_changed;
        if toolchain_changed
            && let Err(e) = std::fs::remove_file(paths.installed_marker())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(MaterializeError::Io {
                id: id.to_string(),
                path: paths.installed_marker(),
                message: e.to_string(),
            });
        }
        Ok(RenderOutcome {
            plan,
            toolchain_changed,
        })
```

```rust
    /// The subprocess half of steps 3 and 4. Skipped when the marker from a
    /// previous success exists (Phase 3 spec §6.2); written on success.
    pub fn install_and_validate(&self, agent: &ResolvedAgent) -> Result<(), MaterializeError> {
        let paths = self.layout.agent(&agent.id);
        if paths.installed_marker().exists() {
            return Ok(());
        }
        Toolchain {
            tools: &self.tools,
            layout: &self.layout,
        }
        .install(&agent.id, &paths)?;
        validate_profile(&self.tools, &agent.id, &paths)?;
        std::fs::write(paths.installed_marker(), b"").map_err(|e| MaterializeError::Io {
            id: agent.id.to_string(),
            path: paths.installed_marker(),
            message: e.to_string(),
        })
    }
```
`Materializer::materialize` becomes `let out = self.render_agent(agent, creds, hooks, &RenderOptions::default())?; self.install_and_validate(agent)?; Ok(out.plan)`. `lib.rs`: `pub use materializer::{RenderOptions, RenderOutcome, Runtime};`. `tests/generated_golden.rs`: `let plan = rt.render_agent(..).unwrap().plan;`. `crates/balerix/src/commands/dev.rs`: the `rt.render_agent(...)?;` statement is unchanged in shape (its `RenderOutcome` is discarded).

- [ ] **Step 4: Run the tests**

Run: `mise run check && mise run test-it`
Expected: PASS, including the new fetch-count and install-count assertions.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-runtime crates/balerix
git commit -m "Make the steady-state pass cheap and skip repeated installs

ensure_repo only clones; the worktree step fetches right before it
creates a branch (Phase 3 spec §6.1). mise install and nono profile
validate run only when the rendered mise.toml or profile changed since
the last success, tracked by agents/<a>/.installed (§6.2).

Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT"
```

---

### Task 5: `balerix-server` scaffold — vault and file store

**Files:**
- Modify: `Cargo.toml` (workspace member is `crates/*`, nothing to add; the dep alias was added in Task 1)
- Create: `crates/balerix-server/Cargo.toml`
- Create: `crates/balerix-server/src/lib.rs`
- Create: `crates/balerix-server/src/fsutil.rs`
- Create: `crates/balerix-server/src/vault.rs`
- Create: `crates/balerix-server/src/store.rs`

**Interfaces:**
- Produces:
  ```rust
  pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()>   // tmp + rename, 0600 from creation, parent created
  pub struct Vault { /* key: [u8; 32] */ }                                    // Debug: Vault(<redacted>)
  impl Vault { pub fn from_key(key: [u8; 32]) -> Self; pub fn load_or_create(path: &Path) -> Result<Self, VaultError>;
               pub fn seal(&self, aad: &str, plain: &[u8]) -> Result<Vec<u8>, VaultError>; pub fn open(&self, aad: &str, blob: &[u8]) -> Result<Vec<u8>, VaultError> }
  pub enum VaultError { Io { path: PathBuf, message: String }, BadKey { path: PathBuf, len: usize }, Seal, Tampered }
  pub fn random_hex(bytes: usize) -> String                                   // 2*bytes lowercase hex chars
  pub struct FileFleetStore { /* root: PathBuf (the fleets dir), vault: Vault */ }
  impl FileFleetStore { pub fn new(fleets_dir: PathBuf, vault: Vault) -> Self; pub fn fleet_dir(&self, name: &FleetName) -> PathBuf }
  impl FleetStore for FileFleetStore { load_all, put, purge }
  ```

- [ ] **Step 1: Create the crate**

`crates/balerix-server/Cargo.toml`:
```toml
[package]
name = "balerix-server"
description = "The balerix daemon: fleet registry, reconcile tasks, HTTP API, hook ingress"
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
thiserror = { workspace = true }
tokio = { workspace = true }
axum = { workspace = true }
prometheus = { workspace = true }
chacha20poly1305 = { workspace = true }
rand = { workspace = true }
hex = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
proptest = { workspace = true }
ureq = { workspace = true }

[lints]
workspace = true
```
`crates/balerix-server/src/lib.rs`:
```rust
//! The daemon (Phase 3 spec §3): a registry of per-fleet actors over the
//! Phase 2 reconciler, a file store with an encrypted secrets vault, the
//! HTTP API, hook ingress and metrics. Depends on `balerix-core` and
//! `balerix-api` only; the binary wires the runtime adapters in.

pub mod fsutil;
pub mod store;
pub mod vault;

pub use store::FileFleetStore;
pub use vault::{Vault, VaultError, random_hex};
```

- [ ] **Step 2: Write the failing tests**

`crates/balerix-server/src/fsutil.rs`:
```rust
//! Private file writes. A copy of `balerix-runtime`'s `write_atomic`
//! narrowed to 0600: `server` must not depend on `runtime`.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Writes `bytes` to `path` via a sibling temp file and rename, created
/// 0600 from the start, parent directory created if missing.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("path has no file name"))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_0600_creates_parents_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a").join("b.txt");
        write_private(&p, b"one").unwrap();
        write_private(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_dir(dir.path().join("a")).unwrap().count(), 1, "no temp file left");
    }
}
```
`crates/balerix-server/src/vault.rs` test module (types in Step 4):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::os::unix::fs::PermissionsExt;

    fn vault() -> Vault {
        Vault::from_key([7u8; 32])
    }

    #[test]
    fn seals_and_opens_with_the_same_key_and_aad() {
        let blob = vault().seal("payments", b"hello").unwrap();
        assert_ne!(&blob[24..], b"hello");
        assert_eq!(vault().open("payments", &blob).unwrap(), b"hello");
        assert_eq!(vault().open("other", &blob), Err(VaultError::Tampered));
        assert_eq!(Vault::from_key([8u8; 32]).open("payments", &blob), Err(VaultError::Tampered));
        assert_eq!(vault().open("payments", &blob[..10]), Err(VaultError::Tampered));
    }

    #[test]
    fn two_seals_of_the_same_plaintext_differ() {
        let a = vault().seal("f", b"x").unwrap();
        let b = vault().seal("f", b"x").unwrap();
        assert_ne!(a, b, "fresh nonce every time");
    }

    #[test]
    fn load_or_create_makes_a_0600_key_and_reloads_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server").join("vault.key");
        let v1 = Vault::load_or_create(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&path).unwrap().len(), 32);
        let v2 = Vault::load_or_create(&path).unwrap();
        let blob = v1.seal("f", b"same key").unwrap();
        assert_eq!(v2.open("f", &blob).unwrap(), b"same key");
        std::fs::write(&path, b"short").unwrap();
        assert_eq!(
            Vault::load_or_create(&path).unwrap_err(),
            VaultError::BadKey { path: path.clone(), len: 5 }
        );
        assert!(format!("{v1:?}").contains("<redacted>"));
        assert!(!format!("{v1:?}").contains('7'));
    }

    #[test]
    fn random_hex_has_the_requested_width_and_alphabet() {
        let s = random_hex(32);
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(random_hex(16), random_hex(16));
    }

    proptest! {
        #[test]
        fn open_inverts_seal(plain in proptest::collection::vec(any::<u8>(), 0..512), aad in "[a-z0-9-]{1,20}") {
            let v = vault();
            let blob = v.seal(&aad, &plain).unwrap();
            prop_assert_eq!(v.open(&aad, &blob).unwrap(), plain);
        }

        #[test]
        fn any_flipped_byte_is_rejected(plain in proptest::collection::vec(any::<u8>(), 1..128), idx in any::<prop::sample::Index>()) {
            let v = vault();
            let mut blob = v.seal("f", &plain).unwrap();
            let i = idx.index(blob.len());
            blob[i] ^= 0x01;
            prop_assert_eq!(v.open("f", &blob), Err(VaultError::Tampered));
        }
    }
}
```
`crates/balerix-server/src/store.rs` test module:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{CredentialBundle, FleetSpec};
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    fn store(dir: &Path) -> FileFleetStore {
        FileFleetStore::new(dir.join("fleets"), Vault::from_key([1u8; 32]))
    }
    fn record(name: &str) -> FleetRecord {
        FleetRecord::new(FleetSpec {
            name: name.into(),
            crews: BTreeMap::new(),
        })
    }
    fn secrets() -> FleetSecrets {
        FleetSecrets {
            credentials: CredentialBundle {
                gh_token: Some("gho_SECRET".into()),
                ..CredentialBundle::default()
            },
            hook_secrets: BTreeMap::from([("f/c/a".to_string(), "hook-SECRET".to_string())]),
        }
    }
    fn name(s: &str) -> FleetName {
        s.parse().unwrap()
    }

    #[test]
    fn put_then_load_all_round_trips_records_and_secrets_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        assert!(s.load_all().unwrap().is_empty(), "no fleets dir yet is not an error");
        let mut b = record("b");
        b.generation = 4;
        s.put(&b, &secrets()).unwrap();
        s.put(&record("a"), &FleetSecrets::default()).unwrap();
        let all = s.load_all().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0.name(), "a");
        assert_eq!(all[1].0.generation, 4);
        assert_eq!(all[1].1, secrets());
        let dir_b = s.fleet_dir(&name("b"));
        for f in ["fleet.json", "secrets.enc"] {
            assert_eq!(
                std::fs::metadata(dir_b.join(f)).unwrap().permissions().mode() & 0o777,
                0o600,
                "{f}"
            );
        }
        let enc = std::fs::read(dir_b.join("secrets.enc")).unwrap();
        assert!(!String::from_utf8_lossy(&enc).contains("SECRET"));
        // put replaces
        s.put(&b, &FleetSecrets::default()).unwrap();
        assert_eq!(s.load_all().unwrap()[1].1, FleetSecrets::default());
    }

    #[test]
    fn purge_removes_the_whole_fleet_directory() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.put(&record("a"), &FleetSecrets::default()).unwrap();
        std::fs::create_dir_all(s.fleet_dir(&name("a")).join("crews").join("c")).unwrap();
        s.purge(&name("a")).unwrap();
        assert!(!s.fleet_dir(&name("a")).exists());
        s.purge(&name("a")).unwrap(); // idempotent
    }

    #[test]
    fn corrupt_files_and_foreign_keys_are_errors_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.put(&record("a"), &secrets()).unwrap();
        let rec = s.fleet_dir(&name("a")).join("fleet.json");
        std::fs::write(&rec, "{ not json").unwrap();
        let e = s.load_all().unwrap_err();
        assert!(matches!(e, StoreError::Corrupt { .. }));
        assert!(e.to_string().starts_with(&rec.display().to_string()), "{e}");

        s.put(&record("a"), &secrets()).unwrap();
        let other = FileFleetStore::new(dir.path().join("fleets"), Vault::from_key([2u8; 32]));
        let e = other.load_all().unwrap_err();
        assert!(e.to_string().ends_with("ciphertext rejected (wrong key, wrong fleet, or tampered)"), "{e}");
        assert!(e.to_string().contains("secrets.enc"));
    }

    #[test]
    fn a_directory_without_a_record_is_skipped_and_missing_secrets_default() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        std::fs::create_dir_all(s.fleet_dir(&name("stray"))).unwrap();
        s.put(&record("a"), &secrets()).unwrap();
        std::fs::remove_file(s.fleet_dir(&name("a")).join("secrets.enc")).unwrap();
        let all = s.load_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1, FleetSecrets::default());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-server`
Expected: compile errors (`Vault`, `FileFleetStore`, … undefined).

- [ ] **Step 4: Implement**

`crates/balerix-server/src/vault.rs` above the tests:
```rust
//! Encryption at rest for a fleet's secrets (architecture spec D4, disk
//! half; Phase 3 spec §3.3). XChaCha20-Poly1305 with a random 24-byte nonce
//! per write and the fleet name as associated data.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::Rng;

use crate::fsutil::write_private;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

#[derive(Clone)]
pub struct Vault {
    key: [u8; KEY_LEN],
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Vault(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VaultError {
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("{path}: expected a {KEY_LEN}-byte key, found {len} bytes")]
    BadKey { path: PathBuf, len: usize },
    #[error("encryption failed")]
    Seal,
    #[error("ciphertext rejected (wrong key, wrong fleet, or tampered)")]
    Tampered,
}

impl Vault {
    pub fn from_key(key: [u8; KEY_LEN]) -> Self {
        Self { key }
    }

    /// Reads the key at `path`, or creates it (0600) from 32 random bytes.
    pub fn load_or_create(path: &Path) -> Result<Self, VaultError> {
        let io = |e: std::io::Error| VaultError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        };
        match fs::read(path) {
            Ok(bytes) => {
                let key: [u8; KEY_LEN] =
                    bytes.as_slice().try_into().map_err(|_| VaultError::BadKey {
                        path: path.to_path_buf(),
                        len: bytes.len(),
                    })?;
                Ok(Self { key })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut key = [0u8; KEY_LEN];
                rand::rng().fill_bytes(&mut key);
                write_private(path, &key).map_err(io)?;
                Ok(Self { key })
            }
            Err(e) => Err(io(e)),
        }
    }

    /// `nonce ‖ ciphertext`.
    pub fn seal(&self, aad: &str, plain: &[u8]) -> Result<Vec<u8>, VaultError> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let ct = XChaCha20Poly1305::new((&self.key).into())
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plain,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| VaultError::Seal)?;
        let mut out = nonce.to_vec();
        out.extend(ct);
        Ok(out)
    }

    pub fn open(&self, aad: &str, blob: &[u8]) -> Result<Vec<u8>, VaultError> {
        if blob.len() < NONCE_LEN {
            return Err(VaultError::Tampered);
        }
        let (nonce, ct) = blob.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| VaultError::Tampered)?;
        XChaCha20Poly1305::new((&self.key).into())
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ct,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| VaultError::Tampered)
    }
}

/// `bytes` random bytes as lowercase hex: the admin token and the per-agent
/// hook secrets.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}
```
`crates/balerix-server/src/store.rs` above the tests:
```rust
//! `FleetStore` over `fleets/<name>/{fleet.json,secrets.enc}` (Phase 3
//! spec §3.3). Secrets are written first so a crash between the two writes
//! leaves a readable pair.

use std::fs;
use std::path::{Path, PathBuf};

use balerix_core::{FleetName, FleetRecord, FleetSecrets, FleetStore, StoreError};

use crate::fsutil::write_private;
use crate::vault::Vault;

const RECORD: &str = "fleet.json";
const SECRETS: &str = "secrets.enc";

pub struct FileFleetStore {
    root: PathBuf,
    vault: Vault,
}

impl FileFleetStore {
    /// `fleets_dir` is `$XDG_STATE_HOME/balerix/fleets`.
    pub fn new(fleets_dir: PathBuf, vault: Vault) -> Self {
        Self {
            root: fleets_dir,
            vault,
        }
    }

    pub fn fleet_dir(&self, name: &FleetName) -> PathBuf {
        self.root.join(name.as_str())
    }

    fn io(path: &Path, e: impl std::fmt::Display) -> StoreError {
        StoreError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }

    fn corrupt(path: &Path, e: impl std::fmt::Display) -> StoreError {
        StoreError::Corrupt {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }

    fn load_one(&self, dir: &Path) -> Result<Option<(FleetRecord, FleetSecrets)>, StoreError> {
        let record_path = dir.join(RECORD);
        let text = match fs::read_to_string(&record_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(dir = %dir.display(), "fleet directory without a record; skipped");
                return Ok(None);
            }
            Err(e) => return Err(Self::io(&record_path, e)),
        };
        let record: FleetRecord =
            serde_json::from_str(&text).map_err(|e| Self::corrupt(&record_path, e))?;
        let secrets_path = dir.join(SECRETS);
        let secrets = match fs::read(&secrets_path) {
            Ok(blob) => {
                let plain = self
                    .vault
                    .open(record.name(), &blob)
                    .map_err(|e| Self::corrupt(&secrets_path, e))?;
                serde_json::from_slice(&plain).map_err(|e| Self::corrupt(&secrets_path, e))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FleetSecrets::default(),
            Err(e) => return Err(Self::io(&secrets_path, e)),
        };
        Ok(Some((record, secrets)))
    }
}

impl FleetStore for FileFleetStore {
    fn load_all(&self) -> Result<Vec<(FleetRecord, FleetSecrets)>, StoreError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Self::io(&self.root, e)),
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        let mut out = Vec::new();
        for dir in dirs {
            if let Some(pair) = self.load_one(&dir)? {
                out.push(pair);
            }
        }
        Ok(out)
    }

    fn put(&self, record: &FleetRecord, secrets: &FleetSecrets) -> Result<(), StoreError> {
        let dir = self.root.join(record.name());
        fs::create_dir_all(&dir).map_err(|e| Self::io(&dir, e))?;
        let secrets_path = dir.join(SECRETS);
        let plain = serde_json::to_vec(secrets).map_err(|e| Self::io(&secrets_path, e))?;
        let blob = self
            .vault
            .seal(record.name(), &plain)
            .map_err(|e| Self::io(&secrets_path, e))?;
        write_private(&secrets_path, &blob).map_err(|e| Self::io(&secrets_path, e))?;
        let record_path = dir.join(RECORD);
        let text = serde_json::to_vec_pretty(record).map_err(|e| Self::io(&record_path, e))?;
        write_private(&record_path, &text).map_err(|e| Self::io(&record_path, e))
    }

    fn purge(&self, name: &FleetName) -> Result<(), StoreError> {
        let dir = self.fleet_dir(name);
        match fs::remove_dir_all(&dir) {
            Ok(()) | Err(_) if !dir.exists() => Ok(()),
            Err(e) => Err(Self::io(&dir, e)),
        }
    }
}
```
Write `purge` without the guard-in-pattern: `match fs::remove_dir_all(&dir) { Ok(()) => Ok(()), Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()), Err(e) => Err(Self::io(&dir, e)) }`.

- [ ] **Step 5: Run the tests**

Run: `mise run check`
Expected: PASS (`balerix-server` 1 + 4 + 2 property + 4 tests). `cargo deny` is nightly; run `mise run audit` once here anyway to confirm the new crates' licenses are allowed by `deny.toml` (they are all MIT/Apache-2.0/BSD; `subtle`/`hybrid-array` are BSD-3/MIT). If `audit` reports a license outside the allow list, add it to `deny.toml` in this commit and say so in the message.

- [ ] **Step 6: Commit**

```bash
git add crates/balerix-server deny.toml Cargo.lock
git commit -m "Scaffold balerix-server with the secrets vault and the file fleet store

XChaCha20-Poly1305 with a per-write nonce and the fleet name as
associated data; the key is created 0600 on first use. FileFleetStore
writes secrets.enc before fleet.json so a crash leaves a readable pair,
and purge removes the fleet directory for down --purge (Phase 3 spec §3.3).

Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT"
```

---

### Task 6: `balerix-server` — metrics, auth primitives, event parsing

**Files:**
- Create: `crates/balerix-server/src/metrics.rs`
- Create: `crates/balerix-server/src/auth.rs`
- Create: `crates/balerix-server/src/hooks.rs` (parsing half; the handler arrives in Task 8)
- Modify: `crates/balerix-server/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone)] pub struct Metrics { /* Arc<Inner> */ }
  impl Metrics {
      pub fn new() -> Result<Metrics, prometheus::Error>;
      pub fn encode(&self) -> String;                                   // text exposition
      pub fn set_gauges(&self, records: &[FleetRecord]);                // balerix_fleets{phase}, balerix_agents{fleet,crew,phase}
      pub fn reconcile(&self, fleet: &str, secs: f64, ok: bool);        // duration histogram + errors counter
      pub fn restart(&self, id: &AgentId);                              // balerix_agent_restarts_total
      pub fn hook_event(&self, id: &AgentId, event: &str, secs: f64);   // events counter + handle duration
  }
  pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool
  pub fn bearer(headers: &HeaderMap) -> Option<&str>
  pub struct RateLimiter;  impl RateLimiter { pub fn new(rate_per_sec: f64, burst: f64) -> Self; pub fn allow(&self, key: &str) -> bool; pub fn allow_at(&self, key: &str, now: Instant) -> bool }
  pub struct ParsedEvent { pub name: String, pub session_id: Option<String>, pub payload: Value }
  pub fn parse_event(body: &[u8]) -> Result<ParsedEvent, String>
  ```

- [ ] **Step 1: Write the failing tests**

`crates/balerix-server/src/metrics.rs` test module:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentPhase, FleetPhase, FleetSpec};
    use balerix_core::FleetRecord;
    use std::collections::BTreeMap;

    fn record(name: &str, phase: FleetPhase, agents: &[(&str, AgentPhase)]) -> FleetRecord {
        let mut r = FleetRecord::new(FleetSpec {
            name: name.into(),
            crews: BTreeMap::new(),
        });
        r.status.phase = phase;
        for (id, p) in agents {
            r.status.entry(id).phase = *p;
        }
        r
    }

    #[test]
    fn gauges_follow_the_snapshots_and_counters_accumulate() {
        let m = Metrics::new().unwrap();
        m.set_gauges(&[
            record("f", FleetPhase::Ready, &[("f/c/a", AgentPhase::Ready), ("f/c/b", AgentPhase::Ready)]),
            record("g", FleetPhase::Degraded, &[("g/d/x", AgentPhase::Dead)]),
        ]);
        let text = m.encode();
        assert!(text.contains("balerix_fleets{phase=\"ready\"} 1"), "{text}");
        assert!(text.contains("balerix_fleets{phase=\"degraded\"} 1"));
        assert!(text.contains("balerix_agents{crew=\"c\",fleet=\"f\",phase=\"ready\"} 2"));
        assert!(text.contains("balerix_agents{crew=\"d\",fleet=\"g\",phase=\"dead\"} 1"));
        // a fleet that disappears takes its gauges with it
        m.set_gauges(&[record("f", FleetPhase::Ready, &[("f/c/a", AgentPhase::Ready)])]);
        let text = m.encode();
        assert!(!text.contains("fleet=\"g\""));
        assert!(text.contains("balerix_agents{crew=\"c\",fleet=\"f\",phase=\"ready\"} 1"));

        let id: AgentId = "f/c/a".parse().unwrap();
        m.reconcile("f", 0.25, true);
        m.reconcile("f", 0.5, false);
        m.restart(&id);
        m.hook_event(&id, "Notification", 0.001);
        m.hook_event(&id, "Notification", 0.002);
        let text = m.encode();
        assert!(text.contains("balerix_reconcile_errors_total{fleet=\"f\"} 1"));
        assert!(text.contains("balerix_reconcile_duration_seconds_count{fleet=\"f\"} 2"));
        assert!(text.contains("balerix_agent_restarts_total{agent=\"a\",crew=\"c\",fleet=\"f\"} 1"));
        assert!(text.contains("balerix_hook_events_total{agent=\"a\",crew=\"c\",event=\"Notification\",fleet=\"f\"} 2"));
        assert!(text.contains("balerix_hook_handle_duration_seconds_count{event=\"Notification\"} 2"));
        for name in [
            "balerix_fleets",
            "balerix_agents",
            "balerix_reconcile_duration_seconds",
            "balerix_reconcile_errors_total",
            "balerix_agent_restarts_total",
            "balerix_hook_events_total",
            "balerix_hook_handle_duration_seconds",
            "balerix_hook_actions_total",
        ] {
            assert!(text.contains(&format!("# TYPE {name} ")), "{name} missing");
        }
    }
}
```
`crates/balerix-server/src/auth.rs` test module:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use proptest::prelude::*;
    use std::time::Duration;

    #[test]
    fn constant_time_eq_compares_whole_slices() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn bearer_extracts_the_token_or_nothing() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert("authorization", HeaderValue::from_static("Bearer  tok-1 "));
        assert_eq!(bearer(&h), Some("tok-1"));
        h.insert("authorization", HeaderValue::from_static("Basic xyz"));
        assert_eq!(bearer(&h), None);
        h.insert("authorization", HeaderValue::from_static("bearer lower"));
        assert_eq!(bearer(&h), Some("lower"), "scheme is case-insensitive");
    }

    #[test]
    fn bucket_allows_a_burst_then_refills_at_rate() {
        let l = RateLimiter::new(2.0, 3.0);
        let t0 = Instant::now();
        assert!(l.allow_at("a", t0) && l.allow_at("a", t0) && l.allow_at("a", t0));
        assert!(!l.allow_at("a", t0), "burst spent");
        assert!(l.allow_at("b", t0), "keys are independent");
        assert!(!l.allow_at("a", t0 + Duration::from_millis(400)), "0.8 tokens: not yet");
        assert!(l.allow_at("a", t0 + Duration::from_millis(600)), "1.2 tokens");
        assert!(!l.allow_at("a", t0 + Duration::from_millis(600)));
        assert!(l.allow_at("a", t0 + Duration::from_secs(60)), "long idle refills…");
        assert!(l.allow_at("a", t0 + Duration::from_secs(60)));
        assert!(l.allow_at("a", t0 + Duration::from_secs(60)));
        assert!(!l.allow_at("a", t0 + Duration::from_secs(60)), "…but only to the burst");
    }

    proptest! {
        #[test]
        fn immediate_calls_never_exceed_the_burst(n in 0usize..50, burst in 1u32..10) {
            let l = RateLimiter::new(1.0, f64::from(burst));
            let t = Instant::now();
            let allowed = (0..n).filter(|_| l.allow_at("k", t)).count();
            prop_assert_eq!(allowed, n.min(burst as usize));
        }
    }
}
```
`crates/balerix-server/src/hooks.rs` (parsing half) test module:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_name_session_and_keeps_the_payload_raw() {
        let body = json!({ "hook_event_name": "PreToolUse", "session_id": "s1", "tool_input": { "command": "ls" } });
        let e = parse_event(body.to_string().as_bytes()).unwrap();
        assert_eq!(e.name, "PreToolUse");
        assert_eq!(e.session_id.as_deref(), Some("s1"));
        assert_eq!(e.payload, body);
        let e = parse_event(br#"{"hook_event_name":"Whatever"}"#).unwrap();
        assert_eq!((e.name.as_str(), e.session_id), ("Whatever", None));
    }

    #[test]
    fn rejects_non_json_non_objects_and_missing_names() {
        assert!(parse_event(b"nope").unwrap_err().starts_with("body is not JSON"));
        assert_eq!(parse_event(b"[1]").unwrap_err(), "body must be a JSON object");
        assert_eq!(
            parse_event(br#"{"x":1}"#).unwrap_err(),
            "hook_event_name must be a string"
        );
        assert_eq!(
            parse_event(br#"{"hook_event_name":7}"#).unwrap_err(),
            "hook_event_name must be a string"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-server`
Expected: compile errors for the three new modules.

- [ ] **Step 3: Implement**

`crates/balerix-server/src/metrics.rs`:
```rust
//! Prometheus series (architecture spec §8 table). Gauges are recomputed
//! from snapshots on every `/metrics` scrape; counters and histograms are
//! bumped where the event happens.

use std::sync::Arc;

use balerix_core::{AgentId, FleetRecord};
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry,
    TextEncoder,
};

#[derive(Clone)]
pub struct Metrics {
    inner: Arc<Inner>,
}

struct Inner {
    registry: Registry,
    fleets: IntGaugeVec,
    agents: IntGaugeVec,
    reconcile_duration: HistogramVec,
    reconcile_errors: IntCounterVec,
    agent_restarts: IntCounterVec,
    hook_events: IntCounterVec,
    hook_handle_duration: HistogramVec,
    /// Always zero until Spec B ships actions; registered so dashboards
    /// can be built now.
    hook_actions: IntCounterVec,
}

/// Lowercase phase label, the same spelling as the wire form.
fn label<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

impl Metrics {
    pub fn new() -> Result<Self, prometheus::Error> {
        let registry = Registry::new();
        let fleets = IntGaugeVec::new(Opts::new("balerix_fleets", "Fleets by phase"), &["phase"])?;
        let agents = IntGaugeVec::new(
            Opts::new("balerix_agents", "Agents by fleet, crew and phase"),
            &["fleet", "crew", "phase"],
        )?;
        let reconcile_duration = HistogramVec::new(
            HistogramOpts::new("balerix_reconcile_duration_seconds", "Reconcile pass duration"),
            &["fleet"],
        )?;
        let reconcile_errors = IntCounterVec::new(
            Opts::new("balerix_reconcile_errors_total", "Passes with a failed step or observe"),
            &["fleet"],
        )?;
        let agent_restarts = IntCounterVec::new(
            Opts::new("balerix_agent_restarts_total", "Agent exits noted by the reconciler"),
            &["fleet", "crew", "agent"],
        )?;
        let hook_events = IntCounterVec::new(
            Opts::new("balerix_hook_events_total", "Hook events accepted"),
            &["fleet", "crew", "agent", "event"],
        )?;
        let hook_handle_duration = HistogramVec::new(
            HistogramOpts::new("balerix_hook_handle_duration_seconds", "Handler latency"),
            &["event"],
        )?;
        let hook_actions = IntCounterVec::new(
            Opts::new("balerix_hook_actions_total", "Actions executed for hook events"),
            &["fleet", "crew", "agent", "action"],
        )?;
        for c in [
            Box::new(fleets.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(agents.clone()),
            Box::new(reconcile_duration.clone()),
            Box::new(reconcile_errors.clone()),
            Box::new(agent_restarts.clone()),
            Box::new(hook_events.clone()),
            Box::new(hook_handle_duration.clone()),
            Box::new(hook_actions.clone()),
        ] {
            registry.register(c)?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                registry,
                fleets,
                agents,
                reconcile_duration,
                reconcile_errors,
                agent_restarts,
                hook_events,
                hook_handle_duration,
                hook_actions,
            }),
        })
    }

    pub fn encode(&self) -> String {
        let mut buf = Vec::new();
        let _ = TextEncoder::new().encode(&self.inner.registry.gather(), &mut buf);
        String::from_utf8(buf).unwrap_or_default()
    }

    pub fn set_gauges(&self, records: &[FleetRecord]) {
        self.inner.fleets.reset();
        self.inner.agents.reset();
        for r in records {
            self.inner
                .fleets
                .with_label_values(&[&label(r.status.phase)])
                .inc();
            for (id, a) in &r.status.agents {
                let Ok(id) = id.parse::<AgentId>() else {
                    continue;
                };
                self.inner
                    .agents
                    .with_label_values(&[id.fleet.as_str(), id.crew.as_str(), &label(a.phase)])
                    .inc();
            }
        }
    }

    pub fn reconcile(&self, fleet: &str, secs: f64, ok: bool) {
        self.inner
            .reconcile_duration
            .with_label_values(&[fleet])
            .observe(secs);
        if !ok {
            self.inner.reconcile_errors.with_label_values(&[fleet]).inc();
        }
    }

    pub fn restart(&self, id: &AgentId) {
        self.inner
            .agent_restarts
            .with_label_values(&[id.fleet.as_str(), id.crew.as_str(), id.agent.as_str()])
            .inc();
    }

    pub fn hook_event(&self, id: &AgentId, event: &str, secs: f64) {
        self.inner
            .hook_events
            .with_label_values(&[id.fleet.as_str(), id.crew.as_str(), id.agent.as_str(), event])
            .inc();
        self.inner
            .hook_handle_duration
            .with_label_values(&[event])
            .observe(secs);
    }
}
```
(`AgentName`/`CrewName`/`FleetName` expose `as_str()`; confirm in `balerix-core/src/name.rs` and use `.to_string()` if not.) The `hook_actions` field is read by nothing yet; silence the dead-code lint with `#[allow(dead_code)]` on the field and a comment pointing at Spec B.

`crates/balerix-server/src/auth.rs`:
```rust
//! Authentication primitives: constant-time comparison, bearer extraction,
//! a per-key token bucket (Phase 3 spec §3.5).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use axum::http::HeaderMap;

/// Length-then-bytes comparison with no early exit on the bytes.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b) {
        acc |= x ^ y;
    }
    acc == 0
}

/// The token of an `Authorization: Bearer <token>` header, trimmed.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get("authorization")?.to_str().ok()?;
    let (scheme, rest) = v.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let tok = rest.trim();
    (!tok.is_empty()).then_some(tok)
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Token bucket per key: `rate_per_sec` refills up to `burst`.
pub struct RateLimiter {
    rate: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new(rate_per_sec: f64, burst: f64) -> Self {
        Self {
            rate: rate_per_sec,
            burst,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn allow(&self, key: &str) -> bool {
        self.allow_at(key, Instant::now())
    }

    pub fn allow_at(&self, key: &str, now: Instant) -> bool {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let b = buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: self.burst,
            last: now,
        });
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.rate).min(self.burst);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}
```
`crates/balerix-server/src/hooks.rs` (parsing half):
```rust
//! Hook ingress (Phase 3 spec §3.5): body validation here; the axum
//! handler joins in `api.rs`'s router (Task 8).

use serde_json::Value;

/// The three things the daemon needs from a hook body.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    pub name: String,
    pub session_id: Option<String>,
    pub payload: Value,
}

/// Validates at the edge: JSON object with a string `hook_event_name`.
/// Unknown names are fine — Claude's list grows. The error is the 400 body.
pub fn parse_event(body: &[u8]) -> Result<ParsedEvent, String> {
    let payload: Value =
        serde_json::from_slice(body).map_err(|e| format!("body is not JSON: {e}"))?;
    let Value::Object(map) = &payload else {
        return Err("body must be a JSON object".to_string());
    };
    let name = match map.get("hook_event_name") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err("hook_event_name must be a string".to_string()),
    };
    let session_id = map
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(ParsedEvent {
        name,
        session_id,
        payload,
    })
}
```
`lib.rs`: add `pub mod auth; pub mod hooks; pub mod metrics;` and `pub use auth::{RateLimiter, bearer, constant_time_eq}; pub use hooks::{ParsedEvent, parse_event}; pub use metrics::Metrics;`.

- [ ] **Step 4: Run the tests**

Run: `mise run check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-server
git commit -m "Add the daemon's metrics registry, auth primitives and hook body validation

The eight series from the architecture spec, gauges recomputed from
snapshots on scrape; constant-time bearer comparison and a per-agent
token bucket for ingress; hook bodies validated at the edge as a JSON
object with a string hook_event_name (Phase 3 spec §3.5, §3.6).

Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT"
```

---

### Task 7: `balerix-server` — the per-fleet actor

**Files:**
- Create: `crates/balerix-server/src/testing.rs`
- Create: `crates/balerix-server/src/actor.rs`
- Modify: `crates/balerix-server/src/lib.rs`

**Interfaces:**
- Consumes: `reconcile_pass`, `ReconcileContext`, `agent_ready`, `set_desired` (core); `Metrics` (Task 6); `random_hex` (Task 5); `FleetStore` (Task 2).
- Produces:
  ```rust
  pub const READY_EVENT: &str = "SessionStart";
  pub enum Msg {
      Apply { spec: FleetSpec, credentials: CredentialBundle, reply: oneshot::Sender<FleetRecord> },
      Down { keep: Keep, purge: bool, reply: oneshot::Sender<FleetRecord> },
      Event { agent: AgentId, name: String, at: Timestamp },
  }
  #[derive(Clone)] pub struct FleetHandle { pub tx: mpsc::Sender<Msg>, pub status: watch::Receiver<FleetRecord> }
  pub struct Ports { pub materializer: Arc<dyn Materializer>, pub runner: Arc<dyn AgentRunner>, pub clock: Arc<dyn Clock>,
                     pub store: Arc<dyn FleetStore>, pub policy: ReconcilePolicy, pub hook_url: String, pub resync: Duration }
  pub type SecretIndex = Arc<tokio::sync::RwLock<HashMap<AgentId, String>>>;
  #[derive(Clone)] pub struct Shared { pub hook_secrets: SecretIndex, pub metrics: Metrics, pub purged: mpsc::Sender<FleetName> }
  pub fn shared(metrics: Metrics) -> (Shared, mpsc::Receiver<FleetName>)
  pub fn spawn(name: FleetName, record: FleetRecord, secrets: FleetSecrets, ports: Arc<Ports>, shared: Shared, initial_pass: bool) -> FleetHandle
  // testing:
  pub struct MemoryStore;  impl MemoryStore { pub fn new() -> Self; pub fn names(&self) -> Vec<String>; pub fn get(&self, name: &str) -> Option<(FleetRecord, FleetSecrets)> }  impl FleetStore for MemoryStore
  pub struct Harness { pub materializer: Arc<FakeMaterializer>, pub runner: Arc<FakeRunner>, pub clock: Arc<FakeClock>, pub store: Arc<MemoryStore>, pub ports: Arc<Ports> }
  impl Harness { pub fn new(resync: Duration) -> Self; pub fn with_policy(resync: Duration, policy: ReconcilePolicy) -> Self }
  ```

- [ ] **Step 1: Write the test support module**

`crates/balerix-server/src/testing.rs` (always compiled, like `balerix_core::fakes`; `tests/api_it.rs` and the binary's tests use it):
```rust
//! Test doubles for the daemon: an in-memory `FleetStore` and a `Ports`
//! bundle over the `balerix-core` fakes.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use balerix_api::Timestamp;
use balerix_core::fakes::{FakeClock, FakeMaterializer, FakeRunner};
use balerix_core::{FleetName, FleetRecord, FleetSecrets, FleetStore, ReconcilePolicy, StoreError};

use crate::actor::Ports;

#[derive(Default)]
pub struct MemoryStore {
    fleets: Mutex<BTreeMap<String, (FleetRecord, FleetSecrets)>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, (FleetRecord, FleetSecrets)>> {
        self.fleets.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn names(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }
    pub fn get(&self, name: &str) -> Option<(FleetRecord, FleetSecrets)> {
        self.lock().get(name).cloned()
    }
}

impl FleetStore for MemoryStore {
    fn load_all(&self) -> Result<Vec<(FleetRecord, FleetSecrets)>, StoreError> {
        Ok(self.lock().values().cloned().collect())
    }
    fn put(&self, record: &FleetRecord, secrets: &FleetSecrets) -> Result<(), StoreError> {
        self.lock()
            .insert(record.name().to_string(), (record.clone(), secrets.clone()));
        Ok(())
    }
    fn purge(&self, name: &FleetName) -> Result<(), StoreError> {
        self.lock().remove(name.as_str());
        Ok(())
    }
}

/// Fakes plus the `Ports` the actor and daemon take.
pub struct Harness {
    pub materializer: Arc<FakeMaterializer>,
    pub runner: Arc<FakeRunner>,
    pub clock: Arc<FakeClock>,
    pub store: Arc<MemoryStore>,
    pub ports: Arc<Ports>,
}

impl Harness {
    pub fn new(resync: Duration) -> Self {
        Self::with_policy(resync, ReconcilePolicy::default())
    }

    pub fn with_policy(resync: Duration, policy: ReconcilePolicy) -> Self {
        let materializer = Arc::new(FakeMaterializer::default());
        let runner = Arc::new(FakeRunner::default());
        let clock = Arc::new(FakeClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new());
        let ports = Arc::new(Ports {
            materializer: materializer.clone(),
            runner: runner.clone(),
            clock: clock.clone(),
            store: store.clone(),
            policy,
            hook_url: "http://127.0.0.1:1".to_string(),
            resync,
        });
        Self {
            materializer,
            runner,
            clock,
            store,
            ports,
        }
    }
}
```

- [ ] **Step 2: Write the failing actor tests**

`crates/balerix-server/src/actor.rs` test module (the actor itself comes in Step 4):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Harness;
    use balerix_api::{AgentPhase, AgentSettings, CrewSpec, FleetPhase, GitSettings};
    use balerix_core::ProcessState;
    use std::collections::BTreeMap;

    fn spec(agents: &[&str]) -> FleetSpec {
        FleetSpec {
            name: "f".into(),
            crews: BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "acme/x".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: agents
                        .iter()
                        .map(|a| (a.to_string(), AgentSettings::default()))
                        .collect(),
                },
            )]),
        }
    }

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }

    async fn wait(rx: &mut watch::Receiver<FleetRecord>, pred: impl Fn(&FleetRecord) -> bool) -> FleetRecord {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if pred(&rx.borrow()) {
                    return rx.borrow().clone();
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| panic!("condition not reached; last: {:#?}", rx.borrow().status))
    }

    async fn apply(h: &FleetHandle, spec: FleetSpec) -> FleetRecord {
        let (tx, rx) = oneshot::channel();
        h.tx.send(Msg::Apply {
            spec,
            credentials: CredentialBundle::default(),
            reply: tx,
        })
        .await
        .unwrap();
        rx.await.unwrap()
    }

    async fn down(h: &FleetHandle, purge: bool) -> FleetRecord {
        let (tx, rx) = oneshot::channel();
        h.tx.send(Msg::Down {
            keep: Keep::default(),
            purge,
            reply: tx,
        })
        .await
        .unwrap();
        rx.await.unwrap()
    }

    fn start(h: &Harness) -> (FleetHandle, Shared, mpsc::Receiver<FleetName>) {
        let (shared, purged) = shared(Metrics::new().unwrap());
        let handle = spawn(
            "f".parse().unwrap(),
            FleetRecord::new(spec(&[])),
            FleetSecrets::default(),
            h.ports.clone(),
            shared.clone(),
            false,
        );
        (handle, shared, purged)
    }

    #[tokio::test]
    async fn apply_reconciles_mints_secrets_and_persists() {
        let h = Harness::new(Duration::from_secs(3600));
        let (handle, shared, _purged) = start(&h);
        let reply = apply(&handle, spec(&["a", "b"])).await;
        assert_eq!(reply.generation, 1);
        assert_eq!(reply.desired, Desired::Up);
        let mut rx = handle.status.clone();
        let rec = wait(&mut rx, |r| r.status.observed_generation == 1).await;
        assert_eq!(rec.status.agents["f/c/a"].phase, AgentPhase::Starting);
        assert_eq!(rec.status.phase, FleetPhase::Reconciling);
        assert!(h.runner.calls().contains(&"ensure_agent f/c/b".to_string()));
        let idx = shared.hook_secrets.read().await;
        assert_eq!(idx.len(), 2);
        assert_eq!(idx[&id("f/c/a")].len(), 64, "32 random bytes as hex");
        let (stored, secrets) = h.store.get("f").unwrap();
        assert_eq!(stored.generation, 1);
        assert_eq!(secrets.hook_secrets.len(), 2);
        assert_eq!(secrets.hook_secrets["f/c/a"], idx[&id("f/c/a")]);
    }

    #[tokio::test]
    async fn session_start_readies_an_agent_and_a_new_apply_keeps_its_secret() {
        let h = Harness::new(Duration::from_secs(3600));
        let (handle, shared, _purged) = start(&h);
        apply(&handle, spec(&["a", "b"])).await;
        let mut rx = handle.status.clone();
        wait(&mut rx, |r| r.status.observed_generation == 1).await;

        handle
            .tx
            .send(Msg::Event {
                agent: id("f/c/a"),
                name: READY_EVENT.into(),
                at: Timestamp(5),
            })
            .await
            .unwrap();
        let rec = wait(&mut rx, |r| r.status.agents["f/c/a"].phase == AgentPhase::Ready).await;
        assert_eq!(rec.status.agents["f/c/a"].last_event_at, Some(Timestamp(5)));
        assert_eq!(h.store.get("f").unwrap().0.status.agents["f/c/a"].phase, AgentPhase::Ready, "a phase change is persisted");

        handle
            .tx
            .send(Msg::Event {
                agent: id("f/c/b"),
                name: "PreToolUse".into(),
                at: Timestamp(9),
            })
            .await
            .unwrap();
        let rec = wait(&mut rx, |r| r.status.agents["f/c/b"].last_event_at == Some(Timestamp(9))).await;
        assert_eq!(rec.status.agents["f/c/b"].phase, AgentPhase::Starting, "only SessionStart readies");

        let before = shared.hook_secrets.read().await[&id("f/c/a")].clone();
        let reply = apply(&handle, spec(&["a"])).await;
        assert_eq!(reply.generation, 2);
        wait(&mut rx, |r| r.status.observed_generation == 2).await;
        let idx = shared.hook_secrets.read().await;
        assert_eq!(idx.len(), 1);
        assert_eq!(idx[&id("f/c/a")], before, "a surviving agent keeps its secret");
        assert!(h.runner.calls().contains(&"stop_agent f/c/b".to_string()));
        assert_eq!(h.store.get("f").unwrap().1.hook_secrets.len(), 1);
    }

    #[tokio::test]
    async fn down_settles_then_purge_removes_everything_and_ends_the_task() {
        let h = Harness::new(Duration::from_secs(3600));
        let (handle, shared, mut purged) = start(&h);
        apply(&handle, spec(&["a"])).await;
        let mut rx = handle.status.clone();
        wait(&mut rx, |r| r.status.observed_generation == 1).await;

        let reply = down(&handle, false).await;
        assert!(matches!(reply.desired, Desired::Down { purge: false, .. }));
        let rec = wait(&mut rx, |r| r.status.phase == FleetPhase::Down).await;
        assert!(rec.is_down());
        assert!(h.runner.observed().crews.is_empty());
        assert_eq!(h.store.names(), vec!["f"], "record kept until purge");

        down(&handle, true).await;
        let name = tokio::time::timeout(Duration::from_secs(5), purged.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(name.as_str(), "f");
        assert!(h.store.names().is_empty());
        assert!(shared.hook_secrets.read().await.is_empty());
        tokio::time::timeout(Duration::from_secs(5), handle.tx.closed())
            .await
            .expect("actor task ends after purge");
    }

    #[tokio::test]
    async fn an_exit_is_noted_then_restarted_on_the_timer() {
        let policy = ReconcilePolicy {
            max_restarts: 5,
            backoff_base_secs: 0,
            backoff_cap_secs: 0,
        };
        let h = Harness::with_policy(Duration::from_millis(50), policy);
        let (handle, shared, _purged) = start(&h);
        apply(&handle, spec(&["a"])).await;
        let mut rx = handle.status.clone();
        wait(&mut rx, |r| r.status.observed_generation == 1).await;

        h.runner.set_state(&id("f/c/a"), ProcessState::Exited { code: Some(1) });
        let rec = wait(&mut rx, |r| r.status.agents["f/c/a"].restarts == 1).await;
        assert!(rec.status.agents["f/c/a"].message.starts_with("exited with status 1"));
        wait(&mut rx, |r| {
            r.status.agents["f/c/a"].next_restart_at.is_none()
                && h.runner.calls().iter().filter(|c| *c == "ensure_agent f/c/a").count() == 2
        })
        .await;
        assert!(shared.metrics.encode().contains(
            "balerix_agent_restarts_total{agent=\"a\",crew=\"c\",fleet=\"f\"} 1"
        ));
    }

    #[tokio::test]
    async fn a_loaded_record_seeds_the_secret_index_and_reconciles_once() {
        let h = Harness::new(Duration::from_secs(3600));
        let (shared, _purged) = shared(Metrics::new().unwrap());
        let mut record = FleetRecord::new(spec(&["a"]));
        record.generation = 3;
        record.status.generation = 3;
        let secrets = FleetSecrets {
            hook_secrets: BTreeMap::from([("f/c/a".to_string(), "kept".to_string())]),
            ..FleetSecrets::default()
        };
        let handle = spawn("f".parse().unwrap(), record, secrets, h.ports.clone(), shared.clone(), true);
        let mut rx = handle.status.clone();
        let rec = wait(&mut rx, |r| r.status.observed_generation == 3).await;
        assert_eq!(rec.status.agents["f/c/a"].phase, AgentPhase::Starting);
        assert_eq!(shared.hook_secrets.read().await[&id("f/c/a")], "kept");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-server actor`
Expected: compile errors (`actor` module has no items).

- [ ] **Step 4: Implement**

`crates/balerix-server/src/actor.rs` above the tests:
```rust
//! One task per fleet (Phase 3 spec §3.2, P3-4): the single writer of that
//! fleet's record, secrets and status. Passes run in `spawn_blocking`;
//! snapshots go out on a `watch` channel; the inbox queues while a pass
//! runs, so a Ready arriving mid-pass lands when the pass ends.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::{CredentialBundle, FleetSpec, Timestamp};
use balerix_core::reconcile::{ReconcileContext, agent_ready, reconcile_pass, set_desired};
use balerix_core::{
    AgentId, AgentRunner, Clock, Desired, Fleet, FleetName, FleetRecord, FleetSecrets,
    FleetStore, HookTarget, Keep, Materializer, ReconcilePolicy, ResolvedAgent,
};
use tokio::sync::{RwLock, mpsc, oneshot, watch};

use crate::metrics::Metrics;
use crate::vault::random_hex;

/// The hook event that means "Claude is up and accepting input".
pub const READY_EVENT: &str = "SessionStart";

pub enum Msg {
    Apply {
        spec: FleetSpec,
        credentials: CredentialBundle,
        reply: oneshot::Sender<FleetRecord>,
    },
    Down {
        keep: Keep,
        purge: bool,
        reply: oneshot::Sender<FleetRecord>,
    },
    Event {
        agent: AgentId,
        name: String,
        at: Timestamp,
    },
}

#[derive(Clone)]
pub struct FleetHandle {
    pub tx: mpsc::Sender<Msg>,
    pub status: watch::Receiver<FleetRecord>,
}

/// Everything every actor shares read-only.
pub struct Ports {
    pub materializer: Arc<dyn Materializer>,
    pub runner: Arc<dyn AgentRunner>,
    pub clock: Arc<dyn Clock>,
    pub store: Arc<dyn FleetStore>,
    pub policy: ReconcilePolicy,
    /// `http://127.0.0.1:<port>`; every agent's hooks post here.
    pub hook_url: String,
    pub resync: Duration,
}

/// Agent id → the bearer secret its hooks present. Ingress authenticates
/// against this; actors keep it current.
pub type SecretIndex = Arc<RwLock<HashMap<AgentId, String>>>;

#[derive(Clone)]
pub struct Shared {
    pub hook_secrets: SecretIndex,
    pub metrics: Metrics,
    /// An actor announces its own name here after a purge; the registry
    /// drops the handle.
    pub purged: mpsc::Sender<FleetName>,
}

pub fn shared(metrics: Metrics) -> (Shared, mpsc::Receiver<FleetName>) {
    let (purged, rx) = mpsc::channel(16);
    (
        Shared {
            hook_secrets: Arc::default(),
            metrics,
            purged,
        },
        rx,
    )
}

/// Starts the actor. `initial_pass` is true for records loaded at startup
/// (reconcile what tmux still has) and false for a fresh `POST`, whose
/// `Apply` triggers the first pass.
pub fn spawn(
    name: FleetName,
    record: FleetRecord,
    secrets: FleetSecrets,
    ports: Arc<Ports>,
    shared: Shared,
    initial_pass: bool,
) -> FleetHandle {
    let (tx, rx) = mpsc::channel(1024);
    let (publish, status) = watch::channel(record.clone());
    let actor = Actor {
        name,
        record,
        secrets,
        ports,
        shared,
        publish,
        rx,
        last_pass_clean: true,
    };
    tokio::spawn(actor.run(initial_pass));
    FleetHandle { tx, status }
}

struct Actor {
    name: FleetName,
    record: FleetRecord,
    secrets: FleetSecrets,
    ports: Arc<Ports>,
    shared: Shared,
    publish: watch::Sender<FleetRecord>,
    rx: mpsc::Receiver<Msg>,
    /// A pass with a failed step retries at the resync cadence, not at
    /// `next_restart_at`: a failing clone must not spin.
    last_pass_clean: bool,
}

impl Actor {
    async fn run(mut self, initial_pass: bool) {
        self.seed_index().await;
        if initial_pass && !self.record.is_down() {
            self.pass().await;
        }
        loop {
            let deadline = self.deadline();
            let msg = tokio::select! {
                m = self.rx.recv() => match m {
                    Some(m) => Some(m),
                    None => return,
                },
                () = tokio::time::sleep_until(deadline) => None,
            };
            match msg {
                Some(Msg::Apply {
                    spec,
                    credentials,
                    reply,
                }) => {
                    self.apply(spec, credentials).await;
                    let _ = reply.send(self.record.clone());
                    self.pass().await;
                }
                Some(Msg::Down { keep, purge, reply }) => {
                    self.record.desired = Desired::Down { keep, purge };
                    self.publish();
                    let _ = reply.send(self.record.clone());
                    self.pass().await;
                }
                Some(Msg::Event { agent, name, at }) => self.event(agent, name, at).await,
                None => self.pass().await,
            }
            if matches!(self.record.desired, Desired::Down { purge: true, .. }) && self.record.is_down()
            {
                self.purge().await;
                return;
            }
        }
    }

    fn deadline(&self) -> tokio::time::Instant {
        let now = tokio::time::Instant::now();
        let resync = now + self.ports.resync;
        if !self.last_pass_clean {
            return resync;
        }
        let now_ts = self.ports.clock.now();
        match self
            .record
            .status
            .agents
            .values()
            .filter_map(|a| a.next_restart_at)
            .min()
        {
            Some(due) => (now + Duration::from_secs(due.0.saturating_sub(now_ts.0))).min(resync),
            None => resync,
        }
    }

    /// Ids of the agents the current spec wants, or none if the spec does
    /// not convert (the API validated it; a stored record may not).
    fn wanted_agents(&self) -> Vec<AgentId> {
        Fleet::try_from(self.record.spec.clone())
            .map(|f| ResolvedAgent::from_fleet(&f).into_iter().map(|a| a.id).collect())
            .unwrap_or_default()
    }

    async fn seed_index(&self) {
        let mut idx = self.shared.hook_secrets.write().await;
        for (id, secret) in &self.secrets.hook_secrets {
            if let Ok(id) = id.parse::<AgentId>() {
                idx.insert(id, secret.clone());
            }
        }
    }

    async fn apply(&mut self, spec: FleetSpec, credentials: CredentialBundle) {
        self.record.generation += 1;
        self.record.spec = spec;
        self.record.desired = Desired::Up;
        set_desired(&mut self.record.status, self.record.generation);
        self.secrets.credentials = credentials;
        let wanted = self.wanted_agents();
        let mut next = BTreeMap::new();
        for id in &wanted {
            let key = id.to_string();
            let secret = self
                .secrets
                .hook_secrets
                .get(&key)
                .cloned()
                .unwrap_or_else(|| random_hex(32));
            next.insert(key, secret);
        }
        self.secrets.hook_secrets = next;
        {
            let mut idx = self.shared.hook_secrets.write().await;
            idx.retain(|id, _| id.fleet != self.name);
            for id in &wanted {
                if let Some(s) = self.secrets.hook_secrets.get(&id.to_string()) {
                    idx.insert(id.clone(), s.clone());
                }
            }
        }
        self.publish();
    }

    async fn event(&mut self, agent: AgentId, name: String, at: Timestamp) {
        if name == READY_EVENT {
            agent_ready(&mut self.record.status, &agent, at);
            self.persist().await;
        } else if let Some(a) = self.record.status.agents.get_mut(&agent.to_string()) {
            a.last_event_at = Some(at);
        }
        self.publish();
    }

    async fn pass(&mut self) {
        let ports = self.ports.clone();
        let name = self.name.clone();
        let desired = match self.record.desired {
            Desired::Up => Some(Fleet::try_from(self.record.spec.clone())),
            Desired::Down { .. } => None,
        };
        let keep = match self.record.desired {
            Desired::Down { keep, .. } => keep,
            Desired::Up => Keep::default(),
        };
        let before = self.record.status.clone();
        let mut status = before.clone();
        let creds = self.secrets.credentials.clone();
        let hook_secrets = self.secrets.hook_secrets.clone();
        let started = Instant::now();
        let joined = tokio::task::spawn_blocking(move || {
            let fleet = match desired {
                Some(Ok(f)) => Some(f),
                Some(Err(e)) => return (status, Err(format!("{name}: invalid stored spec: {e}"))),
                None => None,
            };
            let hooks = |id: &AgentId| HookTarget {
                url: ports.hook_url.clone(),
                secret: hook_secrets.get(&id.to_string()).cloned().unwrap_or_default(),
            };
            let ctx = ReconcileContext {
                fleet: &name,
                desired: fleet.as_ref(),
                keep,
                materializer: ports.materializer.as_ref(),
                runner: ports.runner.as_ref(),
                creds: &creds,
                hooks: &hooks,
                policy: &ports.policy,
                clock: ports.clock.as_ref(),
            };
            let outcome = reconcile_pass(&mut status, &ctx).map_err(|e| e.to_string());
            (status, outcome)
        })
        .await;
        let secs = started.elapsed().as_secs_f64();
        let clean = match joined {
            Ok((status, Ok((plan, report)))) => {
                self.record.status = status;
                for (step, err) in &report.failures {
                    tracing::warn!(fleet = %self.name, step = %step, "step failed: {err}");
                }
                tracing::info!(
                    fleet = %self.name,
                    steps = plan.len(),
                    failed = report.failures.len(),
                    skipped = report.skipped.len(),
                    phase = ?self.record.status.phase,
                    "reconciled"
                );
                report.all_ok()
            }
            Ok((status, Err(e))) => {
                self.record.status = status;
                tracing::error!(fleet = %self.name, "pass failed: {e}");
                false
            }
            Err(e) => {
                tracing::error!(fleet = %self.name, "reconcile task panicked: {e}");
                false
            }
        };
        self.last_pass_clean = clean;
        self.shared.metrics.reconcile(self.name.as_str(), secs, clean);
        for (id, a) in &self.record.status.agents {
            let prev = before.agents.get(id).map_or(0, |p| p.restarts);
            if a.restarts > prev && let Ok(aid) = id.parse::<AgentId>() {
                self.shared.metrics.restart(&aid);
            }
        }
        self.persist().await;
        self.publish();
    }

    async fn persist(&self) {
        let store = self.ports.store.clone();
        let record = self.record.clone();
        let secrets = self.secrets.clone();
        match tokio::task::spawn_blocking(move || store.put(&record, &secrets)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(fleet = %self.name, "persist failed: {e}"),
            Err(e) => tracing::error!(fleet = %self.name, "persist task panicked: {e}"),
        }
    }

    fn publish(&self) {
        let _ = self.publish.send(self.record.clone());
    }

    async fn purge(&mut self) {
        let store = self.ports.store.clone();
        let name = self.name.clone();
        match tokio::task::spawn_blocking(move || store.purge(&name)).await {
            Ok(Ok(())) => tracing::info!(fleet = %self.name, "purged"),
            Ok(Err(e)) => tracing::error!(fleet = %self.name, "purge failed: {e}"),
            Err(e) => tracing::error!(fleet = %self.name, "purge task panicked: {e}"),
        }
        self.shared
            .hook_secrets
            .write()
            .await
            .retain(|id, _| id.fleet != self.name);
        let _ = self.shared.purged.send(self.name.clone()).await;
    }
}
```
`lib.rs`: add `pub mod actor; pub mod testing;` and `pub use actor::{FleetHandle, Msg, Ports, READY_EVENT, SecretIndex, Shared};`.

- [ ] **Step 5: Run the tests**

Run: `mise run check`
Expected: PASS (5 actor tests). If `an_exit_is_noted_then_restarted_on_the_timer` is flaky, the resync sleep is the only timing involved: raise the `wait` timeout, never the resync.

- [ ] **Step 6: Commit**

```bash
git add crates/balerix-server
git commit -m "Add the per-fleet actor: apply, down, events, timed passes, purge

One tokio task owns each fleet's record, secrets and status; passes run
reconcile_pass in spawn_blocking and persist afterwards; SessionStart
readies an agent and is persisted as a phase change; a failing pass
retries at the resync cadence instead of hammering next_restart_at.
Hook secrets are minted per new agent and kept across updates (Phase 3
spec §3.2, P3-4).

Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT"
```

---

### Task 8: `balerix-server` — registry, API, hook handler, lifecycle files

**Files:**
- Create: `crates/balerix-server/src/daemon.rs`
- Create: `crates/balerix-server/src/api.rs`
- Modify: `crates/balerix-server/src/hooks.rs` (the handler)
- Create: `crates/balerix-server/src/lifecycle.rs`
- Modify: `crates/balerix-server/src/lib.rs`
- Create: `crates/balerix-server/tests/api_it.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct Daemon;  // registry
  impl Daemon {
      pub fn start(ports: Ports, handler: Arc<dyn EventHandler>, metrics: Metrics, token: String, existing: Vec<(FleetRecord, FleetSecrets)>) -> Arc<Daemon>;  // needs a tokio runtime
      pub fn token(&self) -> &str;  pub fn metrics(&self) -> &Metrics;
      pub async fn apply(&self, name: &FleetName, spec: FleetSpec, credentials: CredentialBundle, replace: bool) -> Result<FleetRecord, DaemonError>;
      pub async fn down(&self, name: &FleetName, keep: Keep, purge: bool) -> Result<FleetRecord, DaemonError>;
      pub async fn get(&self, name: &FleetName) -> Option<FleetRecord>;
      pub async fn snapshots(&self) -> Vec<FleetRecord>;  pub async fn list(&self) -> Vec<FleetSummary>;
      pub async fn event(&self, agent: &AgentId, secret: &str, event: ParsedEvent) -> Result<Outcome, DaemonError>;
      pub async fn hook_secret(&self, agent: &AgentId) -> Option<String>;
  }
  pub enum DaemonError { NotFound, Conflict, Invalid(String), Unauthorized, Internal(String) }
  pub struct ApiError { /* status, message */ }  impl ApiError { pub fn new(status: StatusCode, message: impl Into<String>) -> Self }
  pub fn router(daemon: Arc<Daemon>) -> axum::Router
  pub async fn serve(listener: tokio::net::TcpListener, router: Router, shutdown: impl Future<Output = ()> + Send + 'static) -> std::io::Result<()>
  pub struct ServerPaths { pub dir: PathBuf }  impl ServerPaths { pub fn new(dir: PathBuf) -> Self; pub fn token(&self) -> PathBuf; pub fn vault_key(&self) -> PathBuf; pub fn endpoint(&self) -> PathBuf; pub fn pid(&self) -> PathBuf; pub fn log(&self) -> PathBuf }
  pub fn load_or_create_token(path: &Path) -> Result<String, LifecycleError>
  pub fn write_endpoint(path: &Path, url: &str) -> Result<(), LifecycleError>;  pub fn read_endpoint(path: &Path) -> Result<Option<String>, LifecycleError>
  pub fn write_pid(path: &Path, pid: u32) -> Result<(), LifecycleError>;  pub fn read_pid(path: &Path) -> Result<Option<u32>, LifecycleError>;  pub fn remove_if_exists(path: &Path) -> Result<(), LifecycleError>
  pub enum LifecycleError { Io { path: PathBuf, message: String } }
  ```

- [ ] **Step 1: Write the failing lifecycle unit tests**

`crates/balerix-server/src/lifecycle.rs` test module:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn token_is_created_0600_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ServerPaths::new(dir.path().join("server"));
        let t1 = load_or_create_token(&paths.token()).unwrap();
        assert_eq!(t1.len(), 64);
        assert_eq!(std::fs::metadata(paths.token()).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(paths.token(), format!("{t1}\n")).unwrap();
        assert_eq!(load_or_create_token(&paths.token()).unwrap(), t1, "trailing newline tolerated");
        std::fs::write(paths.token(), "").unwrap();
        assert_ne!(load_or_create_token(&paths.token()).unwrap(), "", "an empty file is regenerated");
    }

    #[test]
    fn endpoint_and_pid_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ServerPaths::new(dir.path().join("server"));
        assert_eq!(read_endpoint(&paths.endpoint()).unwrap(), None);
        write_endpoint(&paths.endpoint(), "http://127.0.0.1:4242").unwrap();
        assert_eq!(read_endpoint(&paths.endpoint()).unwrap().as_deref(), Some("http://127.0.0.1:4242"));
        assert_eq!(read_pid(&paths.pid()).unwrap(), None);
        write_pid(&paths.pid(), 4321).unwrap();
        assert_eq!(read_pid(&paths.pid()).unwrap(), Some(4321));
        remove_if_exists(&paths.pid()).unwrap();
        remove_if_exists(&paths.pid()).unwrap();
        assert_eq!(read_pid(&paths.pid()).unwrap(), None);
        assert_eq!(paths.log(), dir.path().join("server").join("server.log"));
        assert_eq!(paths.vault_key(), dir.path().join("server").join("vault.key"));
    }
}
```

- [ ] **Step 2: Write the failing API integration test**

`crates/balerix-server/tests/api_it.rs`:
```rust
//! The router over the fakes, through a real listener and ureq (Phase 3
//! spec §8 "API integration").
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use balerix_api::{AgentPhase, AgentSettings, CrewSpec, ErrorBody, FleetPhase, FleetRequest, FleetSpec, FleetSummary, GitSettings};
use balerix_core::{AgentId, FleetName, FleetRecord, PassThrough};
use balerix_server::testing::Harness;
use balerix_server::{Daemon, Metrics, router, serve};
use serde_json::{Value, json};

fn spec(agents: &[&str]) -> FleetSpec {
    FleetSpec {
        name: "f".into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/x".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: agents.iter().map(|a| (a.to_string(), AgentSettings::default())).collect(),
            },
        )]),
    }
}

struct Api {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl Api {
    fn call(&self, method: &str, path: &str, token: Option<&str>, body: Option<&Value>) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let mut req = match method {
            "GET" => self.agent.get(&url).force_send_body(),
            "POST" => self.agent.post(&url),
            "PUT" => self.agent.put(&url),
            "DELETE" => self.agent.delete(&url).force_send_body(),
            _ => unreachable!(),
        };
        if let Some(t) = token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        let mut resp = match body {
            Some(b) => req.send_json(b).unwrap(),
            None => req.send_empty().unwrap(),
        };
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap();
        let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, v)
    }
    fn admin(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        self.call(method, path, Some(&self.token.clone()), body)
    }
}

async fn wait_for(daemon: &Daemon, pred: impl Fn(Option<&FleetRecord>) -> bool) {
    let name: FleetName = "f".parse().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if pred(daemon.get(&name).await.as_ref()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not reached");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fleet_api_and_hook_ingress_end_to_end() {
    let h = Harness::new(Duration::from_secs(3600));
    // `Harness` keeps its own `Arc<Ports>`; the daemon wants an owned
    // `Ports`, so build one over the same fakes.
    let ports = balerix_server::Ports {
        materializer: h.materializer.clone(),
        runner: h.runner.clone(),
        clock: h.clock.clone(),
        store: h.store.clone(),
        policy: Default::default(),
        hook_url: "http://127.0.0.1:1".into(),
        resync: Duration::from_secs(3600),
    };
    let daemon = Daemon::start(ports, Arc::new(PassThrough), Metrics::new().unwrap(), "admin-tok".into(), Vec::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, router(daemon.clone()), async {
        let _ = stop_rx.await;
    }));
    let api = Arc::new(Api {
        base: format!("http://127.0.0.1:{port}"),
        token: "admin-tok".into(),
        agent: ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .http_status_as_error(false)
            .build()
            .into(),
    });
    let call = |api: Arc<Api>, m: &'static str, p: String, tok: Option<String>, body: Option<Value>| async move {
        tokio::task::spawn_blocking(move || api.call(m, &p, tok.as_deref(), body.as_ref())).await.unwrap()
    };
    let admin = |api: Arc<Api>, m: &'static str, p: String, body: Option<Value>| async move {
        tokio::task::spawn_blocking(move || api.admin(m, &p, body.as_ref())).await.unwrap()
    };

    // health and auth
    let (st, v) = call(api.clone(), "GET", "/healthz".into(), None, None).await;
    assert_eq!((st, v.as_str()), (200, Some("ok")));
    let (st, v) = call(api.clone(), "GET", "/v1/fleets".into(), None, None).await;
    assert_eq!(st, 401);
    let e: ErrorBody = serde_json::from_value(v).unwrap();
    assert!(e.error.contains("admin token"));
    let (st, _) = call(api.clone(), "GET", "/v1/fleets".into(), Some("wrong".into()), None).await;
    assert_eq!(st, 401);

    // create, conflict, get, list
    let req = json!(FleetRequest { spec: spec(&["a", "b"]), credentials: Default::default() });
    let (st, v) = admin(api.clone(), "POST", "/v1/fleets".into(), Some(req.clone())).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["generation"], 1);
    let (st, _) = admin(api.clone(), "POST", "/v1/fleets".into(), Some(req.clone())).await;
    assert_eq!(st, 409);
    wait_for(&daemon, |r| r.is_some_and(|r| r.status.observed_generation == 1)).await;
    let (st, v) = admin(api.clone(), "GET", "/v1/fleets/f".into(), None).await;
    let rec: FleetRecord = serde_json::from_value(v).unwrap();
    assert_eq!((st, rec.status.agents["f/c/a"].phase), (200, AgentPhase::Starting));
    let (_, v) = admin(api.clone(), "GET", "/v1/fleets".into(), None).await;
    let rows: Vec<FleetSummary> = serde_json::from_value(v).unwrap();
    assert_eq!((rows.len(), rows[0].agents), (1, 2));
    let (st, _) = admin(api.clone(), "GET", "/v1/fleets/nope".into(), None).await;
    assert_eq!(st, 404);
    let (st, _) = admin(api.clone(), "GET", "/v1/fleets/Not-Valid!".into(), None).await;
    assert_eq!(st, 400);

    // update: name mismatch, then a real change
    let (st, v) = admin(api.clone(), "PUT", "/v1/fleets/g".into(), Some(req.clone())).await;
    assert_eq!(st, 400, "{v}");
    let mut changed = spec(&["a", "b"]);
    changed.crews.get_mut("c").unwrap().agents.get_mut("a").unwrap().env.insert("V".into(), "1".into());
    let req2 = json!(FleetRequest { spec: changed, credentials: Default::default() });
    let (st, v) = admin(api.clone(), "PUT", "/v1/fleets/f".into(), Some(req2)).await;
    assert_eq!((st, v["generation"].as_u64()), (200, Some(2)));
    let (st, v) = admin(api.clone(), "POST", "/v1/fleets".into(), Some(json!({ "spec": { "name": "f", "bogus": 1 } }))).await;
    assert_eq!(st, 400, "{v}");

    // hooks
    let a: AgentId = "f/c/a".parse().unwrap();
    let secret = daemon.hook_secret(&a).await.unwrap();
    let ev = json!({ "hook_event_name": "SessionStart", "session_id": "s1" });
    let events = "/v1/agents/f/c/a/events".to_string();
    let (st, _) = call(api.clone(), "POST", events.clone(), Some("wrong".into()), Some(ev.clone())).await;
    assert_eq!(st, 401);
    let (st, _) = call(api.clone(), "POST", "/v1/agents/f/c/zzz/events".into(), Some(secret.clone()), Some(ev.clone())).await;
    assert_eq!(st, 401, "unknown agent looks like a bad secret");
    let (st, _) = call(api.clone(), "POST", events.clone(), None, Some(ev.clone())).await;
    assert_eq!(st, 401);
    let (st, v) = call(api.clone(), "POST", events.clone(), Some(secret.clone()), Some(ev.clone())).await;
    assert_eq!((st, v), (200, json!({})));
    wait_for(&daemon, |r| r.is_some_and(|r| r.status.agents["f/c/a"].phase == AgentPhase::Ready)).await;
    let (st, v) = call(api.clone(), "POST", events.clone(), Some(secret.clone()), Some(json!([1]))).await;
    assert_eq!(st, 400);
    assert_eq!(v["error"], "body must be a JSON object");
    let big = json!({ "hook_event_name": "PreToolUse", "blob": "x".repeat(2 << 20) });
    let (st, _) = call(api.clone(), "POST", events.clone(), Some(secret.clone()), Some(big)).await;
    assert_eq!(st, 413);
    let mut saw_429 = false;
    for _ in 0..80 {
        let (st, _) = call(api.clone(), "POST", events.clone(), Some(secret.clone()), Some(json!({ "hook_event_name": "PreToolUse" }))).await;
        if st == 429 {
            saw_429 = true;
            break;
        }
    }
    assert!(saw_429, "burst of 50 must trip the limiter");

    // metrics
    let (st, v) = call(api.clone(), "GET", "/metrics".into(), None, None).await;
    let text = v.as_str().unwrap();
    assert_eq!(st, 200);
    assert!(text.contains("balerix_hook_events_total{agent=\"a\",crew=\"c\",event=\"SessionStart\",fleet=\"f\"} 1"), "{text}");
    assert!(text.contains("balerix_agents{crew=\"c\",fleet=\"f\",phase=\"ready\"} 1"));
    assert!(text.contains("balerix_fleets{phase=\"reconciling\"} 1"));

    // down: bad flags, keep, re-up, purge
    let (st, v) = admin(api.clone(), "DELETE", "/v1/fleets/f?purge=true&keep_repos=true".into(), None).await;
    assert_eq!(st, 400, "{v}");
    let (st, v) = admin(api.clone(), "DELETE", "/v1/fleets/f?keep_repos=true&keep_sessions=true&purge=false".into(), None).await;
    assert_eq!(st, 200, "{v}");
    wait_for(&daemon, |r| r.is_some_and(|r| r.status.phase == FleetPhase::Down)).await;
    assert!(h.materializer.calls().contains(&"remove_crew f/c repos=true sessions=true".to_string()));
    let (st, v) = admin(api.clone(), "POST", "/v1/fleets".into(), Some(req.clone())).await;
    assert_eq!((st, v["generation"].as_u64()), (200, Some(3)), "a downed fleet re-applies in place");
    wait_for(&daemon, |r| r.is_some_and(|r| r.status.observed_generation == 3)).await;
    let (st, _) = admin(api.clone(), "DELETE", "/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=true".into(), None).await;
    assert_eq!(st, 200);
    wait_for(&daemon, |r| r.is_none()).await;
    let (st, _) = admin(api.clone(), "GET", "/v1/fleets/f".into(), None).await;
    assert_eq!(st, 404);
    let (_, v) = admin(api.clone(), "GET", "/v1/fleets".into(), None).await;
    assert_eq!(v, json!([]));
    assert!(h.store.names().is_empty());
    let (st, _) = admin(api.clone(), "DELETE", "/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=false".into(), None).await;
    assert_eq!(st, 404);

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
}
```
`Harness` exposes the fakes so the test can build its own `Ports`; `daemon.hook_secret` and `daemon.get` are the two test-facing accessors.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-server`
Expected: compile errors (`Daemon`, `router`, `serve`, lifecycle items).

- [ ] **Step 4: Implement `daemon.rs`**

```rust
//! The registry (Phase 3 spec §3.1): fleet name → actor handle, the shared
//! secret index, and the request-side logic the API calls into.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use balerix_api::{CredentialBundle, FleetSpec, FleetSummary, HookEvent};
use balerix_core::{
    AgentId, EventHandler, Fleet, FleetName, FleetRecord, FleetSecrets, Keep, Outcome,
};
use tokio::sync::{RwLock, mpsc, oneshot};

use crate::actor::{self, FleetHandle, Msg, Ports, Shared};
use crate::auth::constant_time_eq;
use crate::hooks::ParsedEvent;
use crate::metrics::Metrics;

pub struct Daemon {
    fleets: RwLock<BTreeMap<FleetName, FleetHandle>>,
    ports: Arc<Ports>,
    shared: Shared,
    handler: Arc<dyn EventHandler>,
    token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DaemonError {
    #[error("fleet not found")]
    NotFound,
    #[error("fleet exists; use `balerix update`, or `balerix down` first")]
    Conflict,
    #[error("{0}")]
    Invalid(String),
    #[error("unknown agent or bad secret")]
    Unauthorized,
    #[error("{0}")]
    Internal(String),
}

impl Daemon {
    /// Spawns one actor per stored fleet (each reconciles once) and the
    /// listener that drops purged fleets. Needs a tokio runtime.
    pub fn start(
        ports: Ports,
        handler: Arc<dyn EventHandler>,
        metrics: Metrics,
        token: String,
        existing: Vec<(FleetRecord, FleetSecrets)>,
    ) -> Arc<Self> {
        let (shared, purged) = actor::shared(metrics);
        let ports = Arc::new(ports);
        let mut fleets = BTreeMap::new();
        for (record, secrets) in existing {
            match FleetName::try_from(record.spec.name.clone()) {
                Ok(name) => {
                    let h = actor::spawn(name.clone(), record, secrets, ports.clone(), shared.clone(), true);
                    fleets.insert(name, h);
                }
                Err(e) => tracing::error!("skipping a stored fleet with an invalid name: {e}"),
            }
        }
        let daemon = Arc::new(Self {
            fleets: RwLock::new(fleets),
            ports,
            shared,
            handler,
            token,
        });
        tokio::spawn(Self::forget_purged(Arc::downgrade(&daemon), purged));
        daemon
    }

    async fn forget_purged(daemon: std::sync::Weak<Self>, mut purged: mpsc::Receiver<FleetName>) {
        while let Some(name) = purged.recv().await {
            let Some(d) = daemon.upgrade() else {
                return;
            };
            d.fleets.write().await.remove(&name);
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn metrics(&self) -> &Metrics {
        &self.shared.metrics
    }

    /// `POST` (`replace == false`): 409 unless the fleet is absent or
    /// settled `Down`. `PUT` (`replace == true`): 404 when absent.
    pub async fn apply(
        &self,
        name: &FleetName,
        spec: FleetSpec,
        credentials: CredentialBundle,
        replace: bool,
    ) -> Result<FleetRecord, DaemonError> {
        if spec.name != name.as_str() {
            return Err(DaemonError::Invalid(format!(
                "spec.name {:?} does not match the fleet {name}",
                spec.name
            )));
        }
        Fleet::try_from(spec.clone()).map_err(|e| DaemonError::Invalid(e.to_string()))?;
        let handle = {
            let mut fleets = self.fleets.write().await;
            match fleets.get(name) {
                Some(h) => {
                    if !replace && !h.status.borrow().is_down() {
                        return Err(DaemonError::Conflict);
                    }
                    h.clone()
                }
                None => {
                    if replace {
                        return Err(DaemonError::NotFound);
                    }
                    let h = actor::spawn(
                        name.clone(),
                        FleetRecord::new(spec.clone()),
                        FleetSecrets::default(),
                        self.ports.clone(),
                        self.shared.clone(),
                        false,
                    );
                    fleets.insert(name.clone(), h.clone());
                    h
                }
            }
        };
        let (reply, rx) = oneshot::channel();
        handle
            .tx
            .send(Msg::Apply {
                spec,
                credentials,
                reply,
            })
            .await
            .map_err(|_| DaemonError::Internal("fleet task is gone".into()))?;
        rx.await
            .map_err(|_| DaemonError::Internal("fleet task dropped the request".into()))
    }

    pub async fn down(&self, name: &FleetName, keep: Keep, purge: bool) -> Result<FleetRecord, DaemonError> {
        let handle = self
            .fleets
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or(DaemonError::NotFound)?;
        let (reply, rx) = oneshot::channel();
        handle
            .tx
            .send(Msg::Down { keep, purge, reply })
            .await
            .map_err(|_| DaemonError::Internal("fleet task is gone".into()))?;
        rx.await
            .map_err(|_| DaemonError::Internal("fleet task dropped the request".into()))
    }

    pub async fn get(&self, name: &FleetName) -> Option<FleetRecord> {
        self.fleets
            .read()
            .await
            .get(name)
            .map(|h| h.status.borrow().clone())
    }

    pub async fn snapshots(&self) -> Vec<FleetRecord> {
        self.fleets
            .read()
            .await
            .values()
            .map(|h| h.status.borrow().clone())
            .collect()
    }

    pub async fn list(&self) -> Vec<FleetSummary> {
        self.snapshots().await.iter().map(FleetRecord::summary).collect()
    }

    pub async fn hook_secret(&self, agent: &AgentId) -> Option<String> {
        self.shared.hook_secrets.read().await.get(agent).cloned()
    }

    /// Authenticates, forwards the event to the fleet, runs the handler.
    /// Unknown agent and bad secret are the same error on purpose.
    pub async fn event(
        &self,
        agent: &AgentId,
        secret: &str,
        event: ParsedEvent,
    ) -> Result<Outcome, DaemonError> {
        let expected = self.hook_secret(agent).await;
        match expected {
            Some(s) if constant_time_eq(s.as_bytes(), secret.as_bytes()) => {}
            _ => return Err(DaemonError::Unauthorized),
        }
        let handle = self
            .fleets
            .read()
            .await
            .get(&agent.fleet)
            .cloned()
            .ok_or(DaemonError::Unauthorized)?;
        let started = Instant::now();
        let at = self.ports.clock.now();
        handle
            .tx
            .send(Msg::Event {
                agent: agent.clone(),
                name: event.name.clone(),
                at,
            })
            .await
            .map_err(|_| DaemonError::Internal("fleet task is gone".into()))?;
        let hook_event = HookEvent {
            agent: agent.to_string(),
            name: event.name,
            session_id: event.session_id,
            received_at: at,
            payload: event.payload,
        };
        tracing::debug!(agent = %agent, event = %hook_event.name, payload = %hook_event.payload, "hook event");
        let outcome = self.handler.handle(&hook_event);
        self.shared
            .metrics
            .hook_event(agent, &hook_event.name, started.elapsed().as_secs_f64());
        Ok(outcome)
    }
}
```

- [ ] **Step 5: Implement `api.rs` and the hook handler**

`crates/balerix-server/src/api.rs`:
```rust
//! The HTTP surface (Phase 3 spec §3.4): plain HTTP on loopback, admin
//! bearer on `/v1/fleets*`, per-agent secret on the events route.

use std::future::Future;
use std::sync::Arc;

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{StatusCode, header::CONTENT_TYPE};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use balerix_api::{DownQuery, ErrorBody, FleetRequest, FleetSummary};
use balerix_core::{FleetName, FleetRecord, Keep};

use crate::auth::{RateLimiter, bearer, constant_time_eq};
use crate::daemon::{Daemon, DaemonError};
use crate::hooks;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) daemon: Arc<Daemon>,
    pub(crate) limiter: Arc<RateLimiter>,
}

/// Every error leaves as `{ "error": "<message>" }`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

impl From<DaemonError> for ApiError {
    fn from(e: DaemonError) -> Self {
        let status = match e {
            DaemonError::NotFound => StatusCode::NOT_FOUND,
            DaemonError::Conflict => StatusCode::CONFLICT,
            DaemonError::Invalid(_) => StatusCode::BAD_REQUEST,
            DaemonError::Unauthorized => StatusCode::UNAUTHORIZED,
            DaemonError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, e.to_string())
    }
}

/// 20 events/s with a burst of 50 per agent (spec §3.5).
const HOOK_RATE: f64 = 20.0;
const HOOK_BURST: f64 = 50.0;

pub fn router(daemon: Arc<Daemon>) -> Router {
    let state = AppState {
        daemon,
        limiter: Arc::new(RateLimiter::new(HOOK_RATE, HOOK_BURST)),
    };
    let admin = Router::new()
        .route("/v1/fleets", get(list_fleets).post(create_fleet))
        .route(
            "/v1/fleets/{name}",
            get(get_fleet).put(update_fleet).delete(delete_fleet),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin))
        .layer(DefaultBodyLimit::max(4 << 20));
    let agents = Router::new()
        .route("/v1/agents/{fleet}/{crew}/{agent}/events", post(hooks::events))
        .layer(DefaultBodyLimit::max(1 << 20));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .merge(admin)
        .merge(agents)
        .with_state(state)
}

/// Runs until `shutdown` resolves; in-flight requests finish.
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

async fn require_admin(State(state): State<AppState>, req: Request, next: Next) -> Response {
    match bearer(req.headers()) {
        Some(t) if constant_time_eq(t.as_bytes(), state.daemon.token().as_bytes()) => {
            next.run(req).await
        }
        _ => ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid admin token")
            .into_response(),
    }
}

async fn metrics(State(state): State<AppState>) -> Response {
    let snapshots = state.daemon.snapshots().await;
    state.daemon.metrics().set_gauges(&snapshots);
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.daemon.metrics().encode(),
    )
        .into_response()
}

fn fleet_name(s: &str) -> Result<FleetName, ApiError> {
    s.parse()
        .map_err(|e: balerix_core::NameError| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))
}

fn body(b: Result<Json<FleetRequest>, JsonRejection>) -> Result<FleetRequest, ApiError> {
    b.map(|Json(r)| r)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))
}

async fn create_fleet(
    State(state): State<AppState>,
    b: Result<Json<FleetRequest>, JsonRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let req = body(b)?;
    let name = fleet_name(&req.spec.name)?;
    Ok(Json(
        state
            .daemon
            .apply(&name, req.spec, req.credentials, false)
            .await?,
    ))
}

async fn update_fleet(
    State(state): State<AppState>,
    Path(name): Path<String>,
    b: Result<Json<FleetRequest>, JsonRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let req = body(b)?;
    let name = fleet_name(&name)?;
    Ok(Json(
        state
            .daemon
            .apply(&name, req.spec, req.credentials, true)
            .await?,
    ))
}

async fn get_fleet(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<FleetRecord>, ApiError> {
    let name = fleet_name(&name)?;
    state
        .daemon
        .get(&name)
        .await
        .map(Json)
        .ok_or_else(|| DaemonError::NotFound.into())
}

async fn list_fleets(State(state): State<AppState>) -> Json<Vec<FleetSummary>> {
    Json(state.daemon.list().await)
}

async fn delete_fleet(
    State(state): State<AppState>,
    Path(name): Path<String>,
    q: Result<Query<DownQuery>, QueryRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let Query(q) = q.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    if q.purge && (q.keep_repos || q.keep_sessions) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "purge cannot be combined with keep flags",
        ));
    }
    let name = fleet_name(&name)?;
    let keep = Keep {
        repos: q.keep_repos,
        sessions: q.keep_sessions,
    };
    Ok(Json(state.daemon.down(&name, keep, q.purge).await?))
}
```
`crates/balerix-server/src/hooks.rs` — add above the tests:
```rust
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use balerix_core::AgentId;

use crate::api::{ApiError, AppState};
use crate::auth::bearer;

/// Claude blocks on the response; the handler is pure, so 2 s is generous.
const HANDLE_TIMEOUT: Duration = Duration::from_secs(2);

/// `POST /v1/agents/{fleet}/{crew}/{agent}/events`. Order: secret (401 for
/// a bad one or an unknown agent alike), rate limit (429), body (400),
/// then the fleet's actor and the handler under a timeout (503).
pub(crate) async fn events(
    State(state): State<AppState>,
    Path((fleet, crew, agent)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let unauthorized =
        || ApiError::new(StatusCode::UNAUTHORIZED, "unknown agent or bad secret").into_response();
    let Ok(id) = format!("{fleet}/{crew}/{agent}").parse::<AgentId>() else {
        return unauthorized();
    };
    let Some(secret) = bearer(&headers) else {
        return unauthorized();
    };
    if !state.limiter.allow(&id.to_string()) {
        return ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
    }
    let parsed = match parse_event(&body) {
        Ok(p) => p,
        Err(e) => return ApiError::new(StatusCode::BAD_REQUEST, e).into_response(),
    };
    match tokio::time::timeout(HANDLE_TIMEOUT, state.daemon.event(&id, secret, parsed)).await {
        Ok(Ok(outcome)) => Json(outcome.response).into_response(),
        Ok(Err(e)) => ApiError::from(e).into_response(),
        Err(_) => ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "hook handling timed out")
            .into_response(),
    }
}
```
Update the module doc: "Hook ingress (Phase 3 spec §3.5): body validation and the axum handler."

- [ ] **Step 6: Implement `lifecycle.rs`**

```rust
//! Files under `$XDG_STATE_HOME/balerix/server/` (Phase 3 spec §3.3, §5):
//! admin token, vault key, the bound endpoint, the pid, the log.

use std::fs;
use std::path::{Path, PathBuf};

use crate::fsutil::write_private;
use crate::vault::random_hex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerPaths {
    pub dir: PathBuf,
}

impl ServerPaths {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
    pub fn token(&self) -> PathBuf {
        self.dir.join("token")
    }
    pub fn vault_key(&self) -> PathBuf {
        self.dir.join("vault.key")
    }
    /// `http://127.0.0.1:<port>` of the running daemon; clients read it.
    pub fn endpoint(&self) -> PathBuf {
        self.dir.join("endpoint")
    }
    pub fn pid(&self) -> PathBuf {
        self.dir.join("balerix.pid")
    }
    pub fn log(&self) -> PathBuf {
        self.dir.join("server.log")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LifecycleError {
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
}

fn io(path: &Path, e: std::io::Error) -> LifecycleError {
    LifecycleError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

fn read_trimmed(path: &Path) -> Result<Option<String>, LifecycleError> {
    match fs::read_to_string(path) {
        Ok(s) => {
            let s = s.trim().to_string();
            Ok((!s.is_empty()).then_some(s))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path, e)),
    }
}

/// The admin bearer token: 32 random bytes as hex, 0600, created on first
/// `serve`; an empty file is regenerated.
pub fn load_or_create_token(path: &Path) -> Result<String, LifecycleError> {
    if let Some(t) = read_trimmed(path)? {
        return Ok(t);
    }
    let token = random_hex(32);
    write_private(path, token.as_bytes()).map_err(|e| io(path, e))?;
    Ok(token)
}

pub fn write_endpoint(path: &Path, url: &str) -> Result<(), LifecycleError> {
    write_private(path, url.as_bytes()).map_err(|e| io(path, e))
}

pub fn read_endpoint(path: &Path) -> Result<Option<String>, LifecycleError> {
    read_trimmed(path)
}

pub fn write_pid(path: &Path, pid: u32) -> Result<(), LifecycleError> {
    write_private(path, pid.to_string().as_bytes()).map_err(|e| io(path, e))
}

pub fn read_pid(path: &Path) -> Result<Option<u32>, LifecycleError> {
    Ok(read_trimmed(path)?.and_then(|s| s.parse().ok()))
}

pub fn remove_if_exists(path: &Path) -> Result<(), LifecycleError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(path, e)),
    }
}
```
`lib.rs` final shape:
```rust
pub mod actor;
pub mod api;
pub mod auth;
pub mod daemon;
pub mod fsutil;
pub mod hooks;
pub mod lifecycle;
pub mod metrics;
pub mod store;
pub mod testing;
pub mod vault;

pub use actor::{FleetHandle, Msg, Ports, READY_EVENT, SecretIndex, Shared};
pub use api::{ApiError, router, serve};
pub use auth::{RateLimiter, bearer, constant_time_eq};
pub use daemon::{Daemon, DaemonError};
pub use hooks::{ParsedEvent, parse_event};
pub use lifecycle::{
    LifecycleError, ServerPaths, load_or_create_token, read_endpoint, read_pid,
    remove_if_exists, write_endpoint, write_pid,
};
pub use metrics::Metrics;
pub use store::FileFleetStore;
pub use vault::{Vault, VaultError, random_hex};
```

- [ ] **Step 7: Run the tests**

Run: `mise run check`
Expected: PASS, including `api_it`. If the 413 assertion fails because axum answers the body-limit rejection before the route (it does not — `DefaultBodyLimit` applies inside the handler's `Bytes` extractor), read the response text in the failure message and adjust nothing else.

- [ ] **Step 8: Commit**

```bash
git add crates/balerix-server
git commit -m "Add the fleet registry, the HTTP API, hook ingress and the server files

Daemon maps fleet names to actor handles and owns the request-side
rules: 409 unless absent or Down on POST, 404 on PUT, one 401 for a bad
hook secret and an unknown agent. The axum router serves plain HTTP on
loopback with an admin bearer on /v1/fleets*, a per-agent bucket, 1 MiB
hook bodies and a 2 s handler timeout; /metrics recomputes gauges from
snapshots. lifecycle.rs owns token, vault key, endpoint and pid files
(Phase 3 spec §3.1, §3.4, §3.5).

Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT"
```

---

### Task 9: `balerix` binary — `serve`

**Files:**
- Modify: `crates/balerix/Cargo.toml`
- Modify: `crates/balerix/src/cli.rs`
- Modify: `crates/balerix/src/wiring.rs` (`SystemClock`, `server_paths`)
- Create: `crates/balerix/src/commands/serve.rs`
- Modify: `crates/balerix/src/commands/mod.rs`, `src/main.rs`
- Create: `crates/balerix/tests/cli_serve.rs`

**Interfaces:**
- Consumes: `Daemon::start`, `router`, `serve`, `FileFleetStore`, `Vault`, `ServerPaths`, lifecycle helpers (Task 8); `Runtime`, `TmuxRunner`, `StateLayout::server_dir/fleets_dir` (runtime).
- Produces:
  ```rust
  pub struct ServeArgs { pub bind: Option<String>, pub detach: bool, pub tmux_socket: String /* hidden, default "balerix" */, pub detached_child: bool /* hidden */ }
  pub struct SystemClock;  impl Clock for SystemClock
  pub fn server_paths(layout: &StateLayout) -> ServerPaths
  pub struct ServerConfig { pub bind: String, pub log: String }   // config.toml [server]; defaults "127.0.0.1:7643", "info"
  impl ServerConfig { pub fn load(path: &Path) -> anyhow::Result<Self> }
  pub fn serve_command(args: &ServeArgs) -> anyhow::Result<String>
  // Files: server/token, server/vault.key (first run); server/endpoint + server/balerix.pid while running; server/server.log when detached.
  ```

- [ ] **Step 1: Dependencies and CLI surface**

`crates/balerix/Cargo.toml` `[dependencies]` add:
```toml
balerix-server = { workspace = true }
tokio = { workspace = true }
ureq = { workspace = true }
toml = { workspace = true }
serde = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
```
`crates/balerix/src/cli.rs` — the `Command` enum becomes (keep `Config` and `Dev` as they are; the fleet commands' arg structs arrive in Task 10, so add only `Serve` and its struct now):
```rust
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon (foreground, or detached with -d).
    Serve(ServeArgs),
    /// Inspect fleet configuration without talking to the daemon.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Developer tools; not part of the supported surface.
    #[command(hide = true)]
    Dev {
        #[command(subcommand)]
        command: DevCommand,
    },
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Address to bind (default: config.toml `[server] bind`, else 127.0.0.1:7643).
    #[arg(long)]
    pub bind: Option<String>,
    /// Re-exec detached; log to server/server.log; print the endpoint.
    #[arg(short = 'd', long)]
    pub detach: bool,
    /// tmux server socket name (tests use a private one).
    #[arg(long, hide = true, default_value = "balerix")]
    pub tmux_socket: String,
    /// Set by `-d` on the child it spawns.
    #[arg(long, hide = true)]
    pub detached_child: bool,
}
```
`crates/balerix/src/main.rs` `run()` gains `Command::Serve(args) => commands::serve::serve_command(&args),`; `commands/mod.rs` gains `pub mod serve;`.

`crates/balerix/src/wiring.rs` additions:
```rust
use std::time::{SystemTime, UNIX_EPOCH};

use balerix_api::Timestamp;
use balerix_core::Clock;
use balerix_server::ServerPaths;

/// Wall clock in whole seconds.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        )
    }
}

pub fn server_paths(layout: &StateLayout) -> ServerPaths {
    ServerPaths::new(layout.server_dir())
}
```

- [ ] **Step 2: Write the failing tests**

`crates/balerix/src/commands/serve.rs` unit tests (module skeleton with tests only; the body follows in Step 4):
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_when_missing_and_parses_the_server_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let c = ServerConfig::load(&path).unwrap();
        assert_eq!((c.bind.as_str(), c.log.as_str()), ("127.0.0.1:7643", "info"));
        std::fs::write(&path, "[server]\nbind = \"127.0.0.1:9000\"\n").unwrap();
        let c = ServerConfig::load(&path).unwrap();
        assert_eq!((c.bind.as_str(), c.log.as_str()), ("127.0.0.1:9000", "info"));
        std::fs::write(&path, "[server]\nport = 1\n").unwrap();
        let e = ServerConfig::load(&path).unwrap_err().to_string();
        assert!(e.contains("config.toml") && e.contains("port"), "{e}");
    }
}
```
`crates/balerix/tests/cli_serve.rs`:
```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Fake tools on PATH: `serve` discovers them but calls none without a fleet.
fn fake_tools(dir: &Path) {
    for t in ["git", "gh", "mise", "nono", "tmux"] {
        fs::write(dir.join(t), "#!/bin/sh\nexit 0\n").unwrap();
    }
}

fn balerix(home: &Path, tools: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_balerix"));
    cmd.env("HOME", home)
        .env("PATH", tools)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("BALERIX_API_URL");
    cmd
}

fn wait_for_file(path: &Path) -> String {
    let start = Instant::now();
    loop {
        if let Ok(s) = fs::read_to_string(path)
            && !s.trim().is_empty()
        {
            return s.trim().to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(10), "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn get(url: &str, token: Option<&str>) -> (u16, String) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut req = agent.get(url);
    if let Some(t) = token {
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    let mut resp = req.call().unwrap();
    (resp.status().as_u16(), resp.body_mut().read_to_string().unwrap())
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn foreground_serve_writes_endpoint_and_answers_with_the_token() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    let server_dir = home.path().join(".local/state/balerix/server");
    let child = balerix(home.path(), tools.path())
        .args(["serve", "--bind", "127.0.0.1:0", "--tmux-socket", &format!("balerix-test-{}", std::process::id())])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _kill = Kill(child);
    let url = wait_for_file(&server_dir.join("endpoint"));
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    let token = fs::read_to_string(server_dir.join("token")).unwrap().trim().to_string();
    assert_eq!(token.len(), 64);
    assert!(server_dir.join("vault.key").exists());
    assert!(server_dir.join("balerix.pid").exists());
    assert_eq!(get(&format!("{url}/healthz"), None), (200, "ok".to_string()));
    assert_eq!(get(&format!("{url}/v1/fleets"), None).0, 401);
    assert_eq!(get(&format!("{url}/v1/fleets"), Some(&token)), (200, "[]".to_string()));
    assert_eq!(get(&format!("{url}/metrics"), None).0, 200);
}

#[test]
fn detached_serve_prints_the_endpoint_logs_to_a_file_and_stops_on_term() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    let server_dir = home.path().join(".local/state/balerix/server");
    let out = balerix(home.path(), tools.path())
        .args(["serve", "-d", "--bind", "127.0.0.1:0", "--tmux-socket", &format!("balerix-test-{}-d", std::process::id())])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("http://127.0.0.1:"), "{stdout}");
    let url = fs::read_to_string(server_dir.join("endpoint")).unwrap().trim().to_string();
    let pid: u32 = fs::read_to_string(server_dir.join("balerix.pid")).unwrap().trim().parse().unwrap();
    assert_eq!(get(&format!("{url}/healthz"), None).0, 200);
    let log = fs::read_to_string(server_dir.join("server.log")).unwrap();
    assert!(log.contains("listening"), "{log}");

    // a second daemon refuses to start while the first answers
    let again = balerix(home.path(), tools.path())
        .args(["serve", "--bind", "127.0.0.1:0"])
        .output()
        .unwrap();
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already running"));

    assert!(Command::new("kill").args(["-TERM", &pid.to_string()]).status().unwrap().success());
    let start = Instant::now();
    while server_dir.join("endpoint").exists() {
        assert!(start.elapsed() < Duration::from_secs(10), "endpoint file not removed on SIGTERM");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!server_dir.join("balerix.pid").exists());
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix --test cli_serve`
Expected: the binary rejects `serve` (clap error) once it compiles; the `ServerConfig` unit test does not compile yet.

- [ ] **Step 4: Implement `commands/serve.rs`**

```rust
//! `balerix serve` (Phase 3 spec §5): wire the runtime adapters into the
//! daemon, bind, publish the endpoint, run until SIGINT/SIGTERM.

use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use balerix_core::{PassThrough, ReconcilePolicy};
use balerix_runtime::{Runtime, StateLayout, TmuxRunner};
use balerix_server::{
    Daemon, FileFleetStore, Metrics, Ports, ServerPaths, Vault, load_or_create_token,
    read_endpoint, remove_if_exists, router, serve, write_endpoint, write_pid,
};
use balerix_core::FleetStore;
use serde::Deserialize;

use crate::cli::ServeArgs;
use crate::wiring::{SystemClock, layout_from_env, server_paths, tool_paths};

const RESYNC: Duration = Duration::from_secs(30);
const DETACH_WAIT: Duration = Duration::from_secs(10);

/// `$XDG_CONFIG_HOME/balerix/config.toml`, all optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub bind: String,
    pub log: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    server: ServerTable,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerTable {
    bind: Option<String>,
    log: Option<String>,
}

impl ServerConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let file: ConfigFile = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("{}: invalid config", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
            Err(e) => return Err(e).with_context(|| path.display().to_string()),
        };
        Ok(Self {
            bind: file.server.bind.unwrap_or_else(|| "127.0.0.1:7643".to_string()),
            log: file.server.log.unwrap_or_else(|| "info".to_string()),
        })
    }
}

pub fn serve_command(args: &ServeArgs) -> Result<String> {
    let layout = layout_from_env()?;
    let paths = server_paths(&layout);
    let config = ServerConfig::load(&layout.config_root.join("config.toml"))?;
    let bind = args.bind.clone().unwrap_or(config.bind);
    if args.detach {
        return detach(&paths, &bind, &args.tmux_socket);
    }
    run(&layout, &paths, &bind, &config.log, &args.tmux_socket, args.detached_child)
}

fn already_running(paths: &ServerPaths) -> Result<Option<String>> {
    let Some(url) = read_endpoint(&paths.endpoint())? else {
        return Ok(None);
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .http_status_as_error(false)
        .build()
        .into();
    Ok(agent
        .get(&format!("{url}/healthz"))
        .call()
        .is_ok()
        .then_some(url))
}

fn init_tracing(paths: &ServerPaths, level: &str, to_file: bool) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .with_context(|| format!("config.toml: invalid log level {level:?}"))?;
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false);
    if to_file {
        std::fs::create_dir_all(&paths.dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.log())?;
        builder.with_writer(std::sync::Mutex::new(file)).init();
    } else {
        builder.with_writer(std::io::stderr).init();
    }
    Ok(())
}

fn run(
    layout: &StateLayout,
    paths: &ServerPaths,
    bind: &str,
    log: &str,
    tmux_socket: &str,
    detached_child: bool,
) -> Result<String> {
    if let Some(url) = already_running(paths)? {
        bail!("a balerix daemon is already running at {url}");
    }
    init_tracing(paths, log, detached_child)?;
    let tools = tool_paths()?;
    let token = load_or_create_token(&paths.token())?;
    let vault = Vault::load_or_create(&paths.vault_key())?;
    let store = FileFleetStore::new(layout.fleets_dir(), vault);
    let existing = store.load_all()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .with_context(|| format!("cannot bind {bind}"))?;
        let url = format!("http://{}", listener.local_addr()?);
        let ports = Ports {
            materializer: Arc::new(Runtime::new(layout.clone(), tools.clone())),
            runner: Arc::new(TmuxRunner::new(tools.tmux.clone(), tmux_socket)),
            clock: Arc::new(SystemClock),
            store: Arc::new(store),
            policy: ReconcilePolicy::default(),
            hook_url: url.clone(),
            resync: RESYNC,
        };
        let fleets = existing.len();
        let daemon = Daemon::start(ports, Arc::new(PassThrough), Metrics::new()?, token, existing);
        write_endpoint(&paths.endpoint(), &url)?;
        write_pid(&paths.pid(), std::process::id())?;
        tracing::info!(%url, fleets, tmux_socket, "balerix daemon listening");
        if !detached_child {
            eprintln!("listening on {url}");
        }
        serve(listener, router(daemon), shutdown_signal()).await?;
        tracing::info!("shutting down; agents keep running in tmux");
        remove_if_exists(&paths.endpoint())?;
        remove_if_exists(&paths.pid())?;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(String::new())
}

/// SIGINT or SIGTERM ends the daemon; SIGHUP is ignored so a closed
/// terminal does not take a detached daemon with it.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("cannot listen for SIGTERM: {e}");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    let _hup = signal(SignalKind::hangup()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

fn detach(paths: &ServerPaths, bind: &str, tmux_socket: &str) -> Result<String> {
    if let Some(url) = already_running(paths)? {
        bail!("a balerix daemon is already running at {url}");
    }
    remove_if_exists(&paths.endpoint())?;
    std::fs::create_dir_all(&paths.dir)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.log())?;
    let exe = std::env::current_exe().context("cannot determine balerix's own path")?;
    let mut child = Command::new(exe)
        .args([
            "serve",
            "--bind",
            bind,
            "--tmux-socket",
            tmux_socket,
            "--detached-child",
        ])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("cannot start the daemon")?;
    let start = Instant::now();
    loop {
        if let Some(url) = read_endpoint(&paths.endpoint())? {
            return Ok(format!(
                "balerix daemon started (pid {}) at {url}\nlog: {}\n",
                child.id(),
                paths.log().display()
            ));
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "daemon exited early ({status}); see {}",
                paths.log().display()
            );
        }
        if start.elapsed() > DETACH_WAIT {
            bail!(
                "daemon did not publish an endpoint within {}s; see {}",
                DETACH_WAIT.as_secs(),
                paths.log().display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
```
Add `tempfile` to the binary's `[dev-dependencies]` is unnecessary (it is a normal dependency already).

- [ ] **Step 5: Run the tests**

Run: `mise run check`
Expected: PASS. The `cli_serve` tests bind port 0 and use private tmux socket names, so they run alongside the runtime's tmux test.

- [ ] **Step 6: Commit**

```bash
git add crates/balerix
git commit -m "Add balerix serve: wire the runtime into the daemon, publish the endpoint, detach

First run creates server/token and server/vault.key; every start binds,
then writes server/endpoint (so tests can bind port 0) and the pid file;
-d re-execs the same argv in its own process group with stdio in
server/server.log and waits for the endpoint. SIGINT/SIGTERM stop the
daemon and leave agents running; a second daemon refuses to start while
the first answers /healthz (Phase 3 spec §5).

Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT"
```

---

### Task 10: `balerix` binary — client and `up`/`update`/`down`/`status`/`list`

**Files:**
- Create: `crates/balerix/src/client.rs`
- Create: `crates/balerix/src/commands/fleet.rs`
- Modify: `crates/balerix/src/cli.rs`, `src/main.rs`, `src/commands/mod.rs`
- Create: `crates/balerix/tests/cli_fleet.rs`

**Interfaces:**
- Consumes: `FleetRequest`, `FleetRecord`, `FleetSummary`, `DownQuery`, `ErrorBody` (api/core); `read`/`resolve`/`host` (config); `Daemon`/`router`/`serve`/`testing::Harness` (server, in tests).
- Produces:
  ```rust
  pub fn resolve_endpoint(flag: Option<&str>, env: Option<&str>, endpoint_file: &Path) -> anyhow::Result<String>
  pub struct Client;  impl Client {
      pub fn connect(api_url: Option<&str>) -> anyhow::Result<Client>;   // flag > $BALERIX_API_URL > server/endpoint; token from server/token
      pub fn new(base: String, token: String) -> Client;
      pub fn create(&self, req: &FleetRequest) -> anyhow::Result<FleetRecord>;  pub fn update(&self, req: &FleetRequest) -> anyhow::Result<FleetRecord>;
      pub fn get(&self, name: &str) -> anyhow::Result<Option<FleetRecord>>;    pub fn list(&self) -> anyhow::Result<Vec<FleetSummary>>;
      pub fn down(&self, name: &str, q: &DownQuery) -> anyhow::Result<FleetRecord>;
  }
  pub const NOT_RUNNING: &str = "daemon not running; run `balerix serve -d`";
  pub fn parse_duration(s: &str) -> anyhow::Result<Duration>     // "90", "90s", "5m", "1h"
  pub fn render_status(r: &FleetRecord) -> String;  pub fn render_list(rows: &[FleetSummary]) -> String
  pub fn up_command(&ApplyArgs), update_command(&ApplyArgs), down_command(&DownArgs), status_command(&StatusArgs), list_command(&ListArgs) -> anyhow::Result<String>
  ```

- [ ] **Step 1: CLI surface**

`crates/balerix/src/cli.rs` — add to `Command` (after `Serve`):
```rust
    /// Create a fleet from a YAML file and wait until it is ready.
    Up(ApplyArgs),
    /// Replace a running fleet's spec; only agents whose settings changed restart.
    Update(ApplyArgs),
    /// Stop a fleet; keep repos and/or sessions, or purge everything.
    Down(DownArgs),
    /// Show one fleet.
    Status(StatusArgs),
    /// List fleets.
    List(ListArgs),
    /// Internal: post the hook event on stdin to the daemon (used by generated settings.json).
    HookRelay,
```
and the structs:
```rust
#[derive(Debug, Args)]
pub struct ApplyArgs {
    /// Path to the fleet YAML file.
    pub file: PathBuf,
    /// Fleet name; overrides `name` in the file.
    #[arg(long)]
    pub name: Option<String>,
    /// Do not layer the host's ~/.claude/settings.json or send host credentials.
    #[arg(long)]
    pub no_host_defaults: bool,
    /// How long to wait for Ready (e.g. 90s, 5m, 1h).
    #[arg(long, default_value = "5m")]
    pub timeout: String,
    /// Return right after the request instead of waiting for Ready.
    #[arg(long)]
    pub no_wait: bool,
    /// Daemon URL (default: $BALERIX_API_URL, then the running daemon's endpoint file).
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct DownArgs {
    pub fleet: String,
    #[arg(long)]
    pub keep_repos: bool,
    #[arg(long)]
    pub keep_sessions: bool,
    /// Both --keep-repos and --keep-sessions.
    #[arg(long)]
    pub keep: bool,
    /// Also delete the fleet record and everything under its directory.
    #[arg(long)]
    pub purge: bool,
    #[arg(long, default_value = "5m")]
    pub timeout: String,
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    pub fleet: String,
    /// Print the raw record as JSON.
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub api_url: Option<String>,
}
```
`main.rs` `run()` arms: `Command::Up(a) => commands::fleet::up_command(&a)`, `Update` → `update_command`, `Down` → `down_command`, `Status` → `status_command`, `List` → `list_command`, `Command::HookRelay => commands::relay::hook_relay_command()` (Task 11 — add the arm there). `commands/mod.rs`: `pub mod fleet;`. `main.rs`: `mod client;`.

- [ ] **Step 2: Write the failing unit tests**

`crates/balerix/src/client.rs` test module:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_prefers_flag_then_env_then_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("endpoint");
        assert_eq!(
            resolve_endpoint(None, None, &file).unwrap_err().to_string(),
            NOT_RUNNING
        );
        std::fs::write(&file, "http://127.0.0.1:4000\n").unwrap();
        assert_eq!(resolve_endpoint(None, None, &file).unwrap(), "http://127.0.0.1:4000");
        assert_eq!(
            resolve_endpoint(None, Some("http://127.0.0.1:5000/"), &file).unwrap(),
            "http://127.0.0.1:5000"
        );
        assert_eq!(
            resolve_endpoint(Some("http://127.0.0.1:6000"), Some("http://x"), &file).unwrap(),
            "http://127.0.0.1:6000"
        );
    }

    #[test]
    fn a_refused_connection_reads_as_daemon_not_running() {
        let c = Client::new("http://127.0.0.1:1".into(), "t".into());
        let e = c.list().unwrap_err().to_string();
        assert!(e.starts_with(NOT_RUNNING), "{e}");
    }
}
```
`crates/balerix/src/commands/fleet.rs` test module:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentPhase, FleetPhase, FleetSpec};
    use std::collections::BTreeMap;

    #[test]
    fn durations_parse_with_units() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn status_and_list_render_aligned_tables() {
        let mut r = FleetRecord::new(FleetSpec {
            name: "payments".into(),
            crews: BTreeMap::new(),
        });
        r.generation = 3;
        r.status.generation = 3;
        r.status.observed_generation = 3;
        r.status.phase = FleetPhase::Degraded;
        r.status.entry("payments/backend/alice").phase = AgentPhase::Ready;
        let bob = r.status.entry("payments/backend/bob");
        bob.phase = AgentPhase::Starting;
        bob.restarts = 1;
        bob.message = "exited with status 1".into();
        assert_eq!(
            render_status(&r),
            "payments  degraded  generation 3 (observed 3)\n\
             AGENT                   PHASE     RESTARTS  MESSAGE\n\
             payments/backend/alice  ready     0\n\
             payments/backend/bob    starting  1         exited with status 1\n"
        );
        let rows = vec![r.summary()];
        assert_eq!(
            render_list(&rows),
            "NAME      PHASE     GEN  OBSERVED  AGENTS\n\
             payments  degraded  3    3         2\n"
        );
        assert_eq!(render_list(&[]), "no fleets\n");
    }
}
```

- [ ] **Step 3: Write the failing CLI test**

`crates/balerix/tests/cli_fleet.rs` — the real daemon over the server crate's fakes, driven through the binary:
```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use assert_cmd::Command;
use balerix_api::AgentPhase;
use balerix_core::FleetRecord;
use balerix_core::{AgentId, PassThrough};
use balerix_server::testing::Harness;
use balerix_server::{Daemon, Metrics, Ports, router, serve};
use predicates::prelude::*;

const PAYMENTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/payments.yaml");

/// A daemon on port 0 over the fakes; the binary finds it through the
/// endpoint and token files under this HOME.
struct Stub {
    home: tempfile::TempDir,
    url: String,
    daemon: Arc<Daemon>,
    _stop: tokio::sync::oneshot::Sender<()>,
    _rt: tokio::runtime::Runtime,
}

fn stub() -> Stub {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let h = Harness::new(Duration::from_secs(3600));
    let ports = Ports {
        materializer: h.materializer.clone(),
        runner: h.runner.clone(),
        clock: h.clock.clone(),
        store: h.store.clone(),
        policy: Default::default(),
        hook_url: "http://127.0.0.1:1".into(),
        resync: Duration::from_secs(3600),
    };
    let (daemon, url, stop) = rt.block_on(async {
        let daemon = Daemon::start(ports, Arc::new(PassThrough), Metrics::new().unwrap(), "tok".into(), Vec::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(serve(listener, router(daemon.clone()), async {
            let _ = rx.await;
        }));
        (daemon, url, tx)
    });
    let home = tempfile::tempdir().unwrap();
    let server = home.path().join(".local/state/balerix/server");
    fs::create_dir_all(&server).unwrap();
    fs::write(server.join("endpoint"), &url).unwrap();
    fs::write(server.join("token"), "tok").unwrap();
    Stub { home, url, daemon, _stop: stop, _rt: rt }
}

fn balerix(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_balerix"));
    cmd.env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("BALERIX_API_URL")
        .env_remove("CLAUDE_CONFIG_DIR");
    cmd
}

#[test]
fn up_status_list_update_and_down_through_the_binary() {
    let s = stub();
    let home = s.home.path();

    balerix(home)
        .args(["up", PAYMENTS, "--no-host-defaults", "--no-wait"])
        .assert()
        .success()
        .stdout(predicate::str::contains("payments  pending  generation 1 (observed 0)"));
    balerix(home)
        .args(["up", PAYMENTS, "--no-host-defaults", "--no-wait"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("fleet exists"));

    // the fakes start every window but nothing sends SessionStart: up times out
    balerix(home)
        .args(["update", PAYMENTS, "--no-host-defaults", "--timeout", "2s"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("timed out after 2s waiting for ready"))
        .stderr(predicate::str::contains("payments/backend/alice  starting"));

    let out = balerix(home)
        .args(["status", "payments", "--json"])
        .assert()
        .success();
    let rec: FleetRecord = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(rec.generation, 2);
    assert_eq!(rec.status.agents["payments/backend/bob"].phase, AgentPhase::Starting);

    balerix(home)
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("NAME      PHASE        GEN  OBSERVED  AGENTS"))
        .stdout(predicate::str::contains("payments  reconciling  2    2         2"));

    // ready both agents by hand, then `up`'s wait loop sees Ready
    for a in ["payments/backend/alice", "payments/backend/bob"] {
        let id: AgentId = a.parse().unwrap();
        let secret = s._rt.block_on(s.daemon.hook_secret(&id)).unwrap();
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let resp = agent
            .post(&format!("{}/v1/agents/{a}/events", s.url))
            .header("Authorization", &format!("Bearer {secret}"))
            .send_json(&serde_json::json!({ "hook_event_name": "SessionStart" }))
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }
    balerix(home)
        .args(["update", PAYMENTS, "--no-host-defaults", "--timeout", "30s"])
        .assert()
        .success()
        .stdout(predicate::str::contains("payments  ready  generation 3 (observed 3)"))
        .stderr(predicate::str::contains("payments/backend/alice: ready"));

    balerix(home)
        .args(["down", "payments", "--purge", "--keep"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--purge cannot be combined"));
    balerix(home)
        .args(["down", "payments", "--keep", "--timeout", "30s"])
        .assert()
        .success()
        .stdout(predicate::str::contains("payments  down  generation 3 (observed 3)"));
    balerix(home)
        .args(["down", "payments", "--purge", "--timeout", "30s"])
        .assert()
        .success()
        .stdout(predicate::str::contains("payments: purged"));
    balerix(home)
        .args(["status", "payments"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("fleet payments not found"));
    balerix(home)
        .args(["list"])
        .assert()
        .success()
        .stdout("no fleets\n");
}

#[test]
fn without_a_daemon_the_client_says_how_to_start_one() {
    let home = tempfile::tempdir().unwrap();
    balerix(home.path())
        .args(["list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("daemon not running; run `balerix serve -d`"));
    balerix(home.path())
        .args(["up", PAYMENTS, "--no-host-defaults", "--timeout", "zz"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid duration"));
}
```
- [ ] **Step 4: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix`
Expected: compile errors (`client`, `fleet` modules absent).

- [ ] **Step 5: Implement `client.rs`**

```rust
//! The CLI's view of the daemon (Phase 3 spec §5): endpoint resolution and
//! typed calls over `ureq`. Plain HTTP on loopback (P3-1).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use balerix_api::{DownQuery, ErrorBody, FleetRequest, FleetSummary};
use balerix_core::FleetRecord;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::wiring::{layout_from_env, server_paths};

pub const NOT_RUNNING: &str = "daemon not running; run `balerix serve -d`";

/// `--api-url`, then `$BALERIX_API_URL`, then the running daemon's endpoint file.
pub fn resolve_endpoint(flag: Option<&str>, env: Option<&str>, endpoint_file: &Path) -> Result<String> {
    let url = match (flag, env) {
        (Some(f), _) => f.to_string(),
        (None, Some(e)) if !e.trim().is_empty() => e.to_string(),
        _ => balerix_server::read_endpoint(endpoint_file)?.ok_or_else(|| anyhow!(NOT_RUNNING))?,
    };
    Ok(url.trim().trim_end_matches('/').to_string())
}

pub struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn connect(api_url: Option<&str>) -> Result<Self> {
        let layout = layout_from_env()?;
        let paths = server_paths(&layout);
        let env = std::env::var("BALERIX_API_URL").ok();
        let base = resolve_endpoint(api_url, env.as_deref(), &paths.endpoint())?;
        let token = std::fs::read_to_string(paths.token())
            .map(|t| t.trim().to_string())
            .map_err(|_| anyhow!("{NOT_RUNNING} (no token at {})", paths.token().display()))?;
        Ok(Self::new(base, token))
    }

    pub fn new(base: String, token: String) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token,
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(30)))
                .http_status_as_error(false)
                .build()
                .into(),
        }
    }

    fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Option<T>> {
        let url = format!("{}{path}", self.base);
        let auth = format!("Bearer {}", self.token);
        let sent = match (method, body) {
            ("GET", _) => self.agent.get(&url).header("Authorization", &auth).call(),
            ("DELETE", _) => self.agent.delete(&url).header("Authorization", &auth).call(),
            ("POST", Some(b)) => self.agent.post(&url).header("Authorization", &auth).send_json(b),
            ("PUT", Some(b)) => self.agent.put(&url).header("Authorization", &auth).send_json(b),
            _ => bail!("unsupported request {method} without a body"),
        };
        let mut resp = sent.map_err(|e| anyhow!("{NOT_RUNNING} ({e})"))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .context("cannot read the daemon's response")?;
        match status {
            200..=299 => Ok(Some(serde_json::from_str(&text).with_context(|| {
                format!("unexpected response from the daemon: {text}")
            })?)),
            404 => Ok(None),
            _ => {
                let message = serde_json::from_str::<ErrorBody>(&text)
                    .map(|e| e.error)
                    .unwrap_or(text);
                bail!("{message} (HTTP {status})")
            }
        }
    }

    fn must<T>(r: Result<Option<T>>, what: &str) -> Result<T> {
        r?.ok_or_else(|| anyhow!("{what} not found"))
    }

    pub fn create(&self, req: &FleetRequest) -> Result<FleetRecord> {
        Self::must(
            self.request("POST", "/v1/fleets", Some(&serde_json::to_value(req)?)),
            "fleet",
        )
    }

    pub fn update(&self, req: &FleetRequest) -> Result<FleetRecord> {
        Self::must(
            self.request(
                "PUT",
                &format!("/v1/fleets/{}", req.spec.name),
                Some(&serde_json::to_value(req)?),
            ),
            &format!("fleet {}", req.spec.name),
        )
    }

    pub fn get(&self, name: &str) -> Result<Option<FleetRecord>> {
        self.request("GET", &format!("/v1/fleets/{name}"), None)
    }

    pub fn list(&self) -> Result<Vec<FleetSummary>> {
        Self::must(self.request("GET", "/v1/fleets", None), "fleet list")
    }

    pub fn down(&self, name: &str, q: &DownQuery) -> Result<FleetRecord> {
        Self::must(
            self.request(
                "DELETE",
                &format!("/v1/fleets/{name}?{}", q.to_query_string()),
                None,
            ),
            &format!("fleet {name}"),
        )
    }
}
```

- [ ] **Step 6: Implement `commands/fleet.rs`**

```rust
//! `up`, `update`, `down`, `status`, `list` (Phase 3 spec §5): thin
//! wrappers over `Client` plus one renderer they all share.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use balerix_api::{AgentPhase, CredentialBundle, DownQuery, FleetPhase, FleetRequest, FleetSpec, FleetSummary};
use balerix_config::{HostPaths, ResolveOptions, host, read, resolve};
use balerix_core::{Fleet, FleetRecord};

use crate::cli::{ApplyArgs, DownArgs, ListArgs, StatusArgs};
use crate::client::Client;

const POLL: Duration = Duration::from_secs(1);

/// `90`, `90s`, `5m`, `1h`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (digits, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| anyhow!("invalid duration {s:?} (use 90s, 5m or 1h)"))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => bail!("invalid duration {s:?} (use 90s, 5m or 1h)"),
    };
    Ok(Duration::from_secs(n * mult))
}

fn label<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub fn render_status(r: &FleetRecord) -> String {
    let mut out = format!(
        "{}  {}  generation {} (observed {})\n",
        r.name(),
        label(r.status.phase),
        r.generation,
        r.status.observed_generation
    );
    let rows: Vec<[String; 4]> = r
        .status
        .agents
        .iter()
        .map(|(id, a)| {
            [
                id.clone(),
                label(a.phase),
                a.restarts.to_string(),
                a.message.clone(),
            ]
        })
        .collect();
    out.push_str(&table(&["AGENT", "PHASE", "RESTARTS", "MESSAGE"], &rows));
    out
}

pub fn render_list(rows: &[FleetSummary]) -> String {
    if rows.is_empty() {
        return "no fleets\n".to_string();
    }
    let rows: Vec<[String; 5]> = rows
        .iter()
        .map(|s| {
            [
                s.name.clone(),
                label(s.phase),
                s.generation.to_string(),
                s.observed_generation.to_string(),
                s.agents.to_string(),
            ]
        })
        .collect();
    table(&["NAME", "PHASE", "GEN", "OBSERVED", "AGENTS"], &rows)
}

/// Columns padded to the widest cell, two spaces apart, trailing spaces
/// trimmed, so rows compare byte-for-byte in tests.
fn table<const N: usize>(header: &[&str; N], rows: &[[String; N]]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let line = |cells: &[&str]| -> String {
        let mut s = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                s.push_str("  ");
            }
            s.push_str(&format!("{c:<width$}", width = widths[i]));
        }
        s.trim_end_matches(' ').to_string() + "\n"
    };
    let mut out = line(header);
    for row in rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        out.push_str(&line(&cells));
    }
    out
}
```
(`table` trims trailing spaces, which is why the empty-message row in the unit test ends right after the restart count.)

```rust
fn load_request(args: &ApplyArgs) -> Result<(FleetSpec, CredentialBundle)> {
    let file = read(&args.file)?;
    let defaults = if args.no_host_defaults {
        host::HostDefaults::default()
    } else {
        host::load(&HostPaths::discover()?)?
    };
    let spec = resolve(
        &file,
        &ResolveOptions {
            name_override: args.name.clone(),
            host_claude_settings: defaults.claude_settings,
        },
    )?;
    // client-side validation before any request (architecture spec §9)
    Fleet::try_from(spec.clone())?;
    Ok((spec, defaults.credentials))
}

/// Polls every second, printing one line per agent phase change to
/// stderr, until `done` or the deadline.
fn wait_until(
    client: &Client,
    name: &str,
    timeout: Duration,
    what: &str,
    done: impl Fn(&FleetRecord) -> bool,
) -> Result<String> {
    let start = Instant::now();
    let mut seen: BTreeMap<String, AgentPhase> = BTreeMap::new();
    loop {
        let record = client
            .get(name)?
            .ok_or_else(|| anyhow!("fleet {name} disappeared while waiting"))?;
        for (id, a) in &record.status.agents {
            if seen.get(id) != Some(&a.phase) {
                if a.message.is_empty() {
                    eprintln!("{id}: {}", label(a.phase));
                } else {
                    eprintln!("{id}: {} ({})", label(a.phase), a.message);
                }
                seen.insert(id.clone(), a.phase);
            }
        }
        if done(&record) {
            return Ok(render_status(&record));
        }
        if start.elapsed() >= timeout {
            bail!(
                "timed out after {}s waiting for {what}:\n{}",
                timeout.as_secs(),
                render_status(&record).trim_end()
            );
        }
        std::thread::sleep(POLL);
    }
}

fn apply(args: &ApplyArgs, replace: bool) -> Result<String> {
    let timeout = parse_duration(&args.timeout)?;
    let (spec, credentials) = load_request(args)?;
    let client = Client::connect(args.api_url.as_deref())?;
    let name = spec.name.clone();
    let req = FleetRequest { spec, credentials };
    let record = if replace {
        client.update(&req)?
    } else {
        client.create(&req)?
    };
    if args.no_wait {
        return Ok(render_status(&record));
    }
    wait_until(&client, &name, timeout, "ready", |r| {
        r.status.observed_generation == r.generation && r.status.phase == FleetPhase::Ready
    })
}

pub fn up_command(args: &ApplyArgs) -> Result<String> {
    apply(args, false)
}

pub fn update_command(args: &ApplyArgs) -> Result<String> {
    apply(args, true)
}

pub fn down_command(args: &DownArgs) -> Result<String> {
    let timeout = parse_duration(&args.timeout)?;
    let keep_any = args.keep || args.keep_repos || args.keep_sessions;
    if args.purge && keep_any {
        bail!("--purge cannot be combined with --keep, --keep-repos or --keep-sessions");
    }
    let q = DownQuery {
        keep_repos: args.keep || args.keep_repos,
        keep_sessions: args.keep || args.keep_sessions,
        purge: args.purge,
    };
    let client = Client::connect(args.api_url.as_deref())?;
    client.down(&args.fleet, &q)?;
    if args.purge {
        let start = Instant::now();
        while client.get(&args.fleet)?.is_some() {
            if start.elapsed() >= timeout {
                bail!("timed out after {}s waiting for the purge", timeout.as_secs());
            }
            std::thread::sleep(POLL);
        }
        return Ok(format!("{}: purged\n", args.fleet));
    }
    wait_until(&client, &args.fleet, timeout, "down", |r| {
        r.status.phase == FleetPhase::Down
    })
}

pub fn status_command(args: &StatusArgs) -> Result<String> {
    let client = Client::connect(args.api_url.as_deref())?;
    let record = client
        .get(&args.fleet)?
        .with_context(|| format!("fleet {} not found", args.fleet))?;
    Ok(if args.json {
        serde_json::to_string_pretty(&record)? + "\n"
    } else {
        render_status(&record)
    })
}

pub fn list_command(args: &ListArgs) -> Result<String> {
    let client = Client::connect(args.api_url.as_deref())?;
    let rows = client.list()?;
    Ok(if args.json {
        serde_json::to_string_pretty(&rows)? + "\n"
    } else {
        render_list(&rows)
    })
}
```
`with_context` on an `Option` needs `anyhow::Context` — it is imported. The `up` test expects `"payments  pending  generation 1 (observed 0)"`: after `POST`, the reply carries generation 1 and a default status (`Pending`, observed 0), which is what `--no-wait` prints.

- [ ] **Step 7: Run the tests**

Run: `mise run check`
Expected: PASS. The `cli_fleet` test relies on `balerix_server::testing::Harness` (normal dependency) and on the `update … --timeout 2s` run producing the timeout error text; the fakes never send `SessionStart`, so the first wait cannot succeed.

- [ ] **Step 8: Commit**

```bash
git add crates/balerix
git commit -m "Add the fleet CLI: up, update, down, status, list over a ureq client

The client resolves the endpoint from --api-url, BALERIX_API_URL, then
server/endpoint, reads the token, and maps a refused connection to
\"daemon not running; run balerix serve -d\". up/update resolve the file
like config resolve, validate client-side, then poll once a second
printing phase changes until Ready or the timeout; down waits for Down,
or for the record to vanish when purging (Phase 3 spec §5, P3-7).

Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT"
```

---

### Task 11: `balerix` binary — `hook-relay` and `dev fake-claude`

**Files:**
- Create: `crates/balerix/src/commands/relay.rs`
- Modify: `crates/balerix/src/commands/dev.rs` (`fake-claude`)
- Create: `crates/balerix/src/testutil.rs` (`#[cfg(test)]` HTTP stub)
- Modify: `crates/balerix/src/cli.rs`, `src/main.rs`, `src/commands/mod.rs`
- Create: `crates/balerix/tests/cli_relay.rs`

**Interfaces:**
- Produces:
  ```rust
  pub fn hook_relay_command() -> anyhow::Result<String>                                   // never fails Claude: prints {} on any error
  pub fn relay(input: impl Read, env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<String>
  pub struct FakeClaudeArgs { pub rest: Vec<String> }                                     // trailing, hyphens allowed
  pub fn fake_claude_command(args: &FakeClaudeArgs) -> anyhow::Result<String>             // never returns: sleeps after the hooks
  pub fn fake_claude_once(config_dir: &Path, home: &Path, argv: &[String]) -> anyhow::Result<()>
  // testutil: pub fn stub_server(status_line: &'static str, body: &'static str) -> (String /* http://127.0.0.1:port */, mpsc::Receiver<String> /* the raw request */)
  ```

- [ ] **Step 1: CLI surface**

`cli.rs`: `DevCommand` gains
```rust
    /// Stand-in for `claude` in the e2e: runs the SessionStart command hooks
    /// and one Notification HTTP hook from settings.json, then sleeps.
    FakeClaude(FakeClaudeArgs),
```
```rust
#[derive(Debug, Args)]
pub struct FakeClaudeArgs {
    /// Whatever the fleet passes to claude (`--verbose`, `--continue`, …); recorded, not interpreted.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}
```
`main.rs`: `Command::HookRelay => commands::relay::hook_relay_command(),` and `Command::Dev { command: DevCommand::FakeClaude(args) } => commands::dev::fake_claude_command(&args),`; `mod testutil;` under `#[cfg(test)]`. `commands/mod.rs`: `pub mod relay;`.

- [ ] **Step 2: Write the stub and the failing tests**

`crates/balerix/src/testutil.rs`:
```rust
//! A one-request HTTP stub for unit tests: captures the raw request, answers
//! with a canned status and body.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

pub fn stub_server(status_line: &'static str, body: &'static str) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = sock.read(&mut chunk).unwrap();
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(idx) = text.find("\r\n\r\n") {
                let len: usize = text[..idx]
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                    .unwrap_or(0);
                if buf.len() >= idx + 4 + len {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        tx.send(String::from_utf8_lossy(&buf).to_string()).unwrap();
        write!(
            sock,
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    (format!("http://{addr}"), rx)
}
```
`crates/balerix/src/commands/relay.rs` test module:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::stub_server;
    use std::collections::HashMap;

    fn env(url: &str) -> HashMap<&'static str, String> {
        HashMap::from([
            ("BALERIX_API_URL", url.to_string()),
            ("BALERIX_AGENT_ID", "f/c/a".to_string()),
            ("BALERIX_HOOK_SECRET", "s3".to_string()),
        ])
    }

    #[test]
    fn posts_stdin_to_the_agent_route_with_the_secret_and_prints_the_reply() {
        let (url, seen) = stub_server("200 OK", r#"{"ok":true}"#);
        let vars = env(&format!("{url}/"));
        let out = relay(
            r#"{"hook_event_name":"SessionStart"}"#.as_bytes(),
            &|k| vars.get(k).cloned(),
        )
        .unwrap();
        assert_eq!(out, r#"{"ok":true}"#);
        let req = seen.recv().unwrap().to_ascii_lowercase();
        assert!(req.starts_with("post /v1/agents/f/c/a/events http/1.1"), "{req}");
        assert!(req.contains("authorization: bearer s3"), "{req}");
        assert!(req.contains("content-type: application/json"), "{req}");
        assert!(req.ends_with(r#"{"hook_event_name":"sessionstart"}"#), "{req}");
    }

    #[test]
    fn failures_are_errors_the_command_turns_into_an_empty_object() {
        let (url, _seen) = stub_server("503 Service Unavailable", r#"{"error":"nope"}"#);
        let vars = env(&url);
        let e = relay(b"{}", &|k| vars.get(k).cloned()).unwrap_err().to_string();
        assert!(e.contains("503"), "{e}");
        let vars = env("http://127.0.0.1:1");
        assert!(relay(b"{}", &|k| vars.get(k).cloned()).is_err());
        let e = relay(b"{}", &|_| None).unwrap_err().to_string();
        assert!(e.contains("BALERIX_API_URL"), "{e}");
    }
}
```
`crates/balerix/src/commands/dev.rs` test module (append; `dev.rs` has none yet):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::stub_server;
    use serde_json::json;

    #[test]
    fn fake_claude_runs_command_hooks_posts_one_http_hook_and_records_argv() {
        let (url, seen) = stub_server("200 OK", "{}");
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let config_dir = home.join(".claude");
        std::fs::create_dir_all(&config_dir).unwrap();
        let settings = json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{ "type": "command", "command": "cat > \"$HOME/seen.json\"", "timeout": 10 }] }],
                "Notification": [{ "hooks": [{ "type": "http", "url": format!("{url}/v1/agents/f/c/a/events"), "headers": { "Authorization": "Bearer s3" } }] }],
                "Stop": [{ "hooks": [{ "type": "http", "url": "http://127.0.0.1:1/never" }] }]
            }
        });
        std::fs::write(config_dir.join("settings.json"), settings.to_string()).unwrap();
        fake_claude_once(&config_dir, &home, &["--verbose".into(), "--continue".into()]).unwrap();

        assert_eq!(
            std::fs::read_to_string(home.join("fake-claude.argv")).unwrap(),
            "--verbose\n--continue\n"
        );
        let seen_payload: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join("seen.json")).unwrap()).unwrap();
        assert_eq!(seen_payload["hook_event_name"], "SessionStart");
        assert!(config_dir.join("projects/e2e/session.marker").exists());
        let req = seen.recv().unwrap();
        assert!(req.contains("Authorization: Bearer s3") || req.contains("authorization: Bearer s3"), "{req}");
        assert!(req.contains(r#""hook_event_name":"Notification""#), "{req}");
    }
}
```
`crates/balerix/tests/cli_relay.rs`:
```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn hook_relay_never_fails_the_agent() {
    Command::new(env!("CARGO_BIN_EXE_balerix"))
        .arg("hook-relay")
        .env("BALERIX_API_URL", "http://127.0.0.1:1")
        .env("BALERIX_AGENT_ID", "f/c/a")
        .env("BALERIX_HOOK_SECRET", "s")
        .write_stdin(r#"{"hook_event_name":"SessionStart"}"#)
        .assert()
        .success()
        .stdout("{}\n")
        .stderr(predicate::str::contains("balerix hook-relay:"));
    Command::new(env!("CARGO_BIN_EXE_balerix"))
        .arg("hook-relay")
        .env_remove("BALERIX_API_URL")
        .write_stdin("{}")
        .assert()
        .success()
        .stdout("{}\n")
        .stderr(predicate::str::contains("BALERIX_API_URL"));
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix relay fake_claude`
Expected: compile errors.

- [ ] **Step 4: Implement `commands/relay.rs`**

```rust
//! `balerix hook-relay` (Phase 3 spec §4): Claude runs this as the
//! `SessionStart` command hook. Reads the hook JSON from stdin, posts it to
//! the daemon with the agent's secret, prints the reply. On any failure it
//! prints `{}` and exits 0 — a daemon outage degrades the fleet, it never
//! breaks the agent.

use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, bail};

const MAX_BODY: u64 = 1 << 20;
const TIMEOUT: Duration = Duration::from_secs(5);

pub fn hook_relay_command() -> Result<String> {
    match relay(std::io::stdin().lock(), &|k| std::env::var(k).ok()) {
        Ok(reply) => Ok(reply),
        Err(e) => {
            eprintln!("balerix hook-relay: {e}");
            Ok("{}\n".to_string())
        }
    }
}

pub fn relay(mut input: impl Read, env: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let url = env("BALERIX_API_URL").context("BALERIX_API_URL not set")?;
    let id = env("BALERIX_AGENT_ID").context("BALERIX_AGENT_ID not set")?;
    let secret = env("BALERIX_HOOK_SECRET").context("BALERIX_HOOK_SECRET not set")?;
    let mut body = Vec::new();
    input
        .take(MAX_BODY)
        .read_to_end(&mut body)
        .context("cannot read the hook event from stdin")?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .post(&format!("{}/v1/agents/{id}/events", url.trim_end_matches('/')))
        .header("Authorization", &format!("Bearer {secret}"))
        .header("Content-Type", "application/json")
        .send(&body[..])
        .context("cannot reach the daemon")?;
    let status = resp.status();
    let text = resp
        .body_mut()
        .read_to_string()
        .context("cannot read the daemon's reply")?;
    if !status.is_success() {
        bail!("daemon answered {status}: {}", text.trim());
    }
    Ok(if text.trim().is_empty() {
        "{}\n".to_string()
    } else {
        text
    })
}
```

- [ ] **Step 5: Implement `fake-claude` in `commands/dev.rs`**

Append to `dev.rs` (imports: `std::io::Write`, `std::path::{Path, PathBuf}`, `std::process::{Command, Stdio}`, `std::time::Duration`, `serde_json::{Value, json}`, `crate::cli::FakeClaudeArgs`):
```rust
/// `balerix dev fake-claude` (Phase 3 spec §7): what the e2e launches in
/// place of `claude`. Runs once, then sleeps until the runner kills it.
pub fn fake_claude_command(args: &FakeClaudeArgs) -> Result<String> {
    let config_dir = PathBuf::from(
        std::env::var_os("CLAUDE_CONFIG_DIR").context("CLAUDE_CONFIG_DIR not set")?,
    );
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME not set")?);
    fake_claude_once(&config_dir, &home, &args.rest)?;
    eprintln!("fake-claude: hooks done; sleeping until killed");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Records argv, marks a session so `--continue` triggers next time, runs
/// every `SessionStart` command hook with a payload on stdin (as Claude
/// does), and posts one `Notification` to every HTTP hook for that event.
pub fn fake_claude_once(config_dir: &Path, home: &Path, argv: &[String]) -> Result<()> {
    std::fs::write(home.join("fake-claude.argv"), argv.join("\n") + "\n")?;
    let projects = config_dir.join("projects").join("e2e");
    std::fs::create_dir_all(&projects)?;
    std::fs::write(projects.join("session.marker"), "fake\n")?;
    let settings: Value = serde_json::from_slice(&std::fs::read(config_dir.join("settings.json"))?)?;
    let cwd = std::env::current_dir()?;

    for hook in hooks_of(&settings, "SessionStart") {
        let Some(cmd) = (hook["type"] == "command").then(|| hook["command"].as_str()).flatten() else {
            continue;
        };
        let payload = json!({
            "hook_event_name": "SessionStart", "session_id": "fake-claude",
            "cwd": cwd, "source": "startup"
        });
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .env("HOME", home)
            .env("CLAUDE_CONFIG_DIR", config_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("cannot run SessionStart hook {cmd:?}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(payload.to_string().as_bytes())?;
        }
        let out = child.wait_with_output()?;
        eprintln!(
            "fake-claude: SessionStart hook {cmd:?} exited {} with {}",
            out.status,
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }

    for hook in hooks_of(&settings, "Notification") {
        let Some(url) = (hook["type"] == "http").then(|| hook["url"].as_str()).flatten() else {
            continue;
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut req = agent.post(url);
        if let Some(headers) = hook["headers"].as_object() {
            for (k, v) in headers {
                if let Some(v) = v.as_str() {
                    req = req.header(k, v);
                }
            }
        }
        let payload = json!({
            "hook_event_name": "Notification", "session_id": "fake-claude",
            "message": "fake claude is up"
        });
        match req.send_json(&payload) {
            Ok(r) => eprintln!("fake-claude: Notification hook → {}", r.status()),
            Err(e) => eprintln!("fake-claude: Notification hook failed: {e}"),
        }
    }
    Ok(())
}

fn hooks_of(settings: &Value, event: &str) -> Vec<Value> {
    settings["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["hooks"].as_array().cloned().unwrap_or_default())
        .collect()
}
```

- [ ] **Step 6: Run the tests**

Run: `mise run check`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/balerix
git commit -m "Add balerix hook-relay and the dev fake-claude stand-in

hook-relay posts the SessionStart JSON on stdin to the daemon with the
secret from its environment and prints the reply; on any failure it
prints {} and exits 0 so a daemon outage never breaks the agent (Phase 3
spec §4). dev fake-claude records argv, marks a session, runs the
SessionStart command hooks and one Notification HTTP hook, then sleeps —
the e2e launches it in place of claude (§7).

Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT"
```

---

### Task 12: the e2e journey, mise tasks, CI

**Files:**
- Create: `crates/balerix/tests/e2e.rs`
- Modify: `mise.toml` (tasks `e2e`, `serve`)
- Modify: `.github/workflows/ci.yml` (comment only; `check` already runs the workspace tests with `BALERIX_REQUIRE_TOOLS=1`)

**Interfaces:**
- Consumes: everything. No new library surface.

- [ ] **Step 1: Write the e2e test**

`crates/balerix/tests/e2e.rs`:
```rust
//! The Phase 3 journey (spec §8): a real daemon over git, mise, nono and
//! tmux, with `balerix dev fake-claude` in place of `claude`. Skips without
//! the tools or Landlock; `BALERIX_REQUIRE_TOOLS=1` (CI) fails instead.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use balerix_api::{AgentPhase, FleetPhase};
use balerix_core::FleetRecord;

const BALERIX: &str = env!("CARGO_BIN_EXE_balerix");

fn tool(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn require_or_skip(name: &str, present: bool) -> bool {
    if present {
        return true;
    }
    if std::env::var_os("BALERIX_REQUIRE_TOOLS").is_some_and(|v| v == "1") {
        panic!("{name} is required (BALERIX_REQUIRE_TOOLS=1) but not available");
    }
    eprintln!("skip: {name} not available");
    false
}

/// See `balerix-runtime/tests/support/mod.rs::landlock_works` for why the
/// probe home is a sibling of `root`.
fn landlock_works(nono: &Path, root: &Path) -> bool {
    let home = root.with_file_name(format!(
        "{}-nono-probe-home",
        root.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::create_dir_all(&home).unwrap();
    let ok = Command::new(nono)
        .args(["-s", "run", "--allow-cwd", "--", "/bin/true"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .current_dir(root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = fs::remove_dir_all(&home);
    ok
}

fn git(dir: &Path, args: &[&str]) {
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
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

struct World {
    home: PathBuf,
    socket: String,
    tmux: PathBuf,
}

impl World {
    fn balerix(&self) -> Command {
        let mut c = Command::new(BALERIX);
        c.env("HOME", &self.home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("BALERIX_API_URL")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("GH_CONFIG_DIR");
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        let out = self.balerix().args(args).output().unwrap();
        eprintln!(
            "$ balerix {}\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "balerix {args:?} failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    fn state(&self) -> PathBuf {
        self.home.join(".local").join("state").join("balerix")
    }
    fn agent_dir(&self, agent: &str) -> PathBuf {
        self.state().join("fleets/e2e/crews/c/agents").join(agent)
    }
    fn status(&self) -> FleetRecord {
        serde_json::from_str(&self.ok(&["status", "e2e", "--json"])).unwrap()
    }
    fn window_pids(&self) -> String {
        let out = Command::new(&self.tmux)
            .args(["-L", &self.socket, "list-windows", "-t", "=e2e/c", "-F", "#{window_name} #{pane_pid}"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    fn pid_of(&self, window: &str) -> String {
        self.window_pids()
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{window} ")).map(str::to_string))
            .unwrap_or_else(|| panic!("no window {window} in {:?}", self.window_pids()))
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let pid_file = self.state().join("server").join("balerix.pid");
        if let Ok(pid) = fs::read_to_string(&pid_file) {
            let pid = pid.trim().to_string();
            let _ = Command::new("kill").args(["-TERM", &pid]).status();
            let start = Instant::now();
            while pid_file.exists() && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = Command::new("kill").args(["-KILL", &pid]).status();
        }
        let _ = Command::new(&self.tmux).args(["-L", &self.socket, "kill-server"]).status();
    }
}

fn fleet_yaml(bare: &Path, bob_model: Option<&str>) -> String {
    let bob = match bob_model {
        Some(m) => format!("      bob: {{ claude: {{ resume: true, settings: {{ model: {m} }} }} }}\n"),
        None => "      bob: { claude: { resume: true } }\n".to_string(),
    };
    format!(
        "apiVersion: balerix/v1\nkind: Fleet\nname: e2e\ndefaults:\n  claude:\n    binary: \"{BALERIX}\"\n    args: [dev, fake-claude, \"--verbose\"]\n    settings: {{ model: sonnet }}\n  tools: {{}}\ncrews:\n  c:\n    repo: \"file://{}\"\n    ref: main\n    git: {{ push: false, auth: none }}\n    agents:\n      alice: {{}}\n{bob}",
        bare.display()
    )
}

fn hook_secrets(w: &World) -> Vec<String> {
    let mut out = Vec::new();
    for a in ["alice", "bob"] {
        let settings = fs::read_to_string(w.agent_dir(a).join("home/.claude/settings.json")).unwrap();
        for piece in settings.split("Bearer ").skip(1) {
            out.push(piece.split('"').next().unwrap().to_string());
        }
    }
    assert!(!out.is_empty());
    out
}

#[test]
fn serve_up_update_down_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("balerix-e2e-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    // empty system tool table: nothing to download, `mise exec` still resolves
    let cfg = w.home.join(".config/balerix");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();

    // a repo with one commit and an untrusted mise.toml naming an uninstalled tool
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("README"), "hi\n").unwrap();
    fs::write(work.join("mise.toml"), "[tools]\nnode = \"0.0.1\"\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("repo.git");
    git(&root, &["clone", "-q", "--bare", &work.display().to_string(), &bare.display().to_string()]);
    let v1 = root.join("fleet.yaml");
    let v2 = root.join("fleet2.yaml");
    fs::write(&v1, fleet_yaml(&bare, None)).unwrap();
    fs::write(&v2, fleet_yaml(&bare, Some("opus"))).unwrap();

    // serve -d
    let out = w.ok(&["serve", "-d", "--bind", "127.0.0.1:0", "--tmux-socket", &w.socket]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let url = fs::read_to_string(w.state().join("server/endpoint")).unwrap().trim().to_string();

    // up → both Ready through the relay
    let out = w.ok(&["up", &v1.display().to_string(), "--no-host-defaults", "--timeout", "180s"]);
    assert!(out.contains("e2e  ready"), "{out}");
    let rec = w.status();
    assert_eq!(rec.status.phase, FleetPhase::Ready);
    for a in ["e2e/c/alice", "e2e/c/bob"] {
        assert_eq!(rec.status.agents[a].phase, AgentPhase::Ready, "{a}");
    }
    let alice_hash = rec.status.agents["e2e/c/alice"].applied_hash.clone().unwrap();
    let bob_hash = rec.status.agents["e2e/c/bob"].applied_hash.clone().unwrap();
    let alice_pid = w.pid_of("alice");
    assert!(w.ok(&["list"]).contains("e2e  ready"));
    let argv = fs::read_to_string(w.agent_dir("alice").join("home/fake-claude.argv")).unwrap();
    assert!(argv.contains("--verbose") && !argv.contains("--continue"), "{argv}");
    assert!(w.agent_dir("alice").join("workspace/README").exists(), "worktree checked out");
    assert!(w.agent_dir("alice").join("home/.gitconfig").exists());

    // metrics saw the relay (SessionStart) and the HTTP hook (Notification)
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let metrics = agent.get(&format!("{url}/metrics")).call().unwrap().body_mut().read_to_string().unwrap();
    assert!(metrics.contains("balerix_hook_events_total{agent=\"alice\",crew=\"c\",event=\"SessionStart\",fleet=\"e2e\"} 1"), "{metrics}");
    assert!(metrics.contains("balerix_hook_events_total{agent=\"alice\",crew=\"c\",event=\"Notification\",fleet=\"e2e\"} 1"), "{metrics}");

    // update bob only
    let out = w.ok(&["update", &v2.display().to_string(), "--no-host-defaults", "--timeout", "180s"]);
    assert!(out.contains("e2e  ready"), "{out}");
    let rec = w.status();
    assert_eq!(rec.generation, 2);
    assert_ne!(rec.status.agents["e2e/c/bob"].applied_hash.as_ref().unwrap(), &bob_hash);
    assert_eq!(rec.status.agents["e2e/c/alice"].applied_hash.as_ref().unwrap(), &alice_hash);
    assert_eq!(rec.status.agents["e2e/c/bob"].restarts, 0, "a spec change is not a crash");
    assert_eq!(w.pid_of("alice"), alice_pid, "alice untouched");

    // down --keep
    let out = w.ok(&["down", "e2e", "--keep", "--timeout", "60s"]);
    assert!(out.contains("e2e  down"), "{out}");
    assert!(w.state().join("fleets/e2e/crews/c/repo").exists());
    assert!(w.agent_dir("bob").join("home").exists());
    assert!(w.state().join("fleets/e2e/fleet.json").exists());
    assert!(w.window_pids().is_empty(), "session gone");

    // up again re-applies in place; bob resumes
    let out = w.ok(&["up", &v2.display().to_string(), "--no-host-defaults", "--timeout", "180s"]);
    assert!(out.contains("e2e  ready"), "{out}");
    assert_eq!(w.status().generation, 3);
    let bob_argv = fs::read_to_string(w.agent_dir("bob").join("home/fake-claude.argv")).unwrap();
    assert!(bob_argv.contains("--continue"), "{bob_argv}");
    let alice_argv = fs::read_to_string(w.agent_dir("alice").join("home/fake-claude.argv")).unwrap();
    assert!(!alice_argv.contains("--continue"), "{alice_argv}");

    // no secret leaks into the daemon log or launch scripts
    let secrets = hook_secrets(&w);
    let token = fs::read_to_string(w.state().join("server/token")).unwrap();
    let log = fs::read_to_string(w.state().join("server/server.log")).unwrap();
    for s in secrets.iter().chain(std::iter::once(&token.trim().to_string())) {
        assert!(!log.contains(s), "secret in server.log");
        for a in ["alice", "bob"] {
            let launch = fs::read_to_string(w.agent_dir(a).join("launch.sh")).unwrap();
            assert!(!launch.contains(s), "secret in {a}'s launch.sh");
        }
    }

    // purge
    let out = w.ok(&["down", "e2e", "--purge", "--timeout", "60s"]);
    assert!(out.contains("e2e: purged"), "{out}");
    assert!(!w.state().join("fleets/e2e").exists());
    let out = w.run(&["status", "e2e"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
    drop(w);
    assert!(!root.join("home/.local/state/balerix/server/endpoint").exists(), "SIGTERM cleaned up");
}
```

- [ ] **Step 2: mise tasks and CI**

`mise.toml` — append:
```toml
[tasks.e2e]
description = "End-to-end journey against a real daemon, git, mise, nono and tmux (fails, not skips, without tools)"
env = { BALERIX_REQUIRE_TOOLS = "1" }
run = "cargo nextest run -p balerix --test e2e"

[tasks.serve]
description = "Run a daemon in the foreground under target/tmp/serve (a scratch XDG root)"
env = { HOME = "{{config_root}}/target/tmp/serve/home" }
run = [
  "mkdir -p target/tmp/serve/home",
  "cargo run -q -p balerix -- serve --tmux-socket balerix-dev",
]
```
`.github/workflows/ci.yml` — above the `check` job add the comment `# check runs the whole workspace, e2e included; BALERIX_REQUIRE_TOOLS makes a missing tool fail.` No install changes: the e2e overrides the system tool table with an empty one, so no `claude` download.

- [ ] **Step 3: Run it**

Run: `mise run e2e` then `mise run check`
Expected: PASS; note the wall-clock of the e2e in the commit message (expect well under a minute: local clone, empty tool table). If `up` times out, read `target/tmp/e2e-*/home/.local/state/balerix/fleets/e2e/crews/c/agents/alice/logs/tmux.log` and `nono.log` first — the fake's stderr lands in `tmux.log`.

Record the §8.1 verdicts this run settles (Task 13 writes them into the spec): the relay reached the daemon from inside nono with the profile's `BALERIX_HOOK_SECRET`; the repo's untrusted `mise.toml` was ignored (verified 2026-09-06 by hand as well: `mise exec` with `MISE_QUIET=1` and an untrusted `mise.toml` in cwd runs the command); a single-file read grant works (probed 2026-09-06).

- [ ] **Step 4: Commit**

```bash
git add crates/balerix/tests/e2e.rs mise.toml .github/workflows/ci.yml
git commit -m "Add the end-to-end journey and the e2e and serve tasks

serve -d, up (two agents Ready through the relay and nono), update (only
bob restarts), down --keep, up again (bob resumes with --continue),
down --purge — against real git, mise, nono and tmux with dev fake-claude
in place of claude, an empty system tool table so nothing downloads, and
a private tmux socket. Runs in the PR tier (P3-8).

Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT"
```

---

### Task 13: docs, threat model, spec addenda and verdicts

**Files:**
- Modify: `README.md`, `ARCHITECTURE.md`, `AGENTS.md`, `docs/THREAT-MODEL.md`
- Modify: `docs/superpowers/specs/2026-09-05-balerix-architecture-design.md` (addendum)
- Modify: `docs/superpowers/specs/2026-09-06-balerix-a3-control-plane-design.md` (§8.1 verdicts, refinements)
- Modify: `docs/superpowers/specs/2026-09-06-balerix-a2-runtime-design.md` (§9 deferred item resolved)

- [ ] **Step 1: `README.md`**

Replace the Quickstart list with:
```markdown
## Quickstart
1. `mise trust && mise install` — pinned toolchain (Rust and every tool balerix shells out to).
2. `git config core.hooksPath .githooks` — enables the pre-commit tier.
3. `mise run check` — lint + tests, e2e included; the same gate CI runs.
4. `mise x -- cargo run -q -p balerix -- config resolve examples/payments.yaml --no-host-defaults`
   — resolves the example fleet and prints every agent's merged settings.
5. `mise x -- cargo run -q -p balerix -- serve -d` — starts the daemon on `127.0.0.1:7643`
   (token, vault key and log under `$XDG_STATE_HOME/balerix/server/`).
6. `mise x -- cargo run -q -p balerix -- up my-fleet.yaml` — point `repo:` at a repository you
   can clone; waits until every agent's Claude has started. Then `status <fleet>`, `list`,
   and `down <fleet> --keep` (repos and homes survive; `--purge` removes everything).
7. `mise x -- cargo run -q -p balerix -- dev materialize examples/payments.yaml backend/bob --no-host-defaults`
   — renders bob's generated files into a temp dir without launching anything.
```
Replace the Status section with: `Spec A is complete: configuration (Phase 1), runtime (Phase 2) and the control plane (Phase 3: daemon, API, hook ingress, CLI). Next is Spec B, the \`flow\` state machine over hook events.`

- [ ] **Step 2: `ARCHITECTURE.md`**

"Start here": replace the second sentence with "`config resolve` and `dev materialize` never talk to the daemon; `serve` runs it; `up`, `update`, `down`, `status`, `list` are HTTP clients of it; `hook-relay` is what an agent's `SessionStart` hook runs."

"The pieces": add
```markdown
- `balerix-server` — the daemon: one actor task per fleet over the reconciler,
  `FileFleetStore` with an encrypted `secrets.enc`, axum routes, hook ingress,
  `/metrics`. Depends on `core` + `api` only; the binary hands it the runtime.
```
"How it flows": append
```markdown
**Control plane (Phase 3):** `up` resolves the file exactly like `config resolve`,
validates, and `POST`s `{spec, credentials}` to `http://127.0.0.1:7643`. The
`Daemon` registry spawns (or re-applies) the fleet's actor; the actor bumps the
generation, mints a hook secret per new agent, runs `reconcile_pass` in
`spawn_blocking`, persists `fleet.json` + `secrets.enc`, and publishes a snapshot
that `GET` reads. Agents post hook events to `/v1/agents/{f}/{c}/{a}/events` with
their secret; `SessionStart` arrives via `balerix hook-relay` (a command hook) and
turns the agent `Ready`, which is what `up` waits for. `down` sets `desired: Down`;
a clean terminating pass settles in `Down`, `--purge` deletes the directory.
```
"Non-obvious decisions": append
```markdown
- **Plain HTTP on loopback; TLS deferred.** An unprivileged local process cannot
  read loopback traffic, and TLS would have added cert generation, client pinning
  and a Claude CA-trust knob for nothing this iteration protects (Phase 3 spec P3-1).
- **`SessionStart` is a command hook, everything else HTTP.** Claude 2.1.263 refuses
  HTTP hooks for `SessionStart`/`Setup`; the relay is balerix itself, so the nono
  profile grants the binary read-only and passes `BALERIX_HOOK_SECRET` (P3-2, P3-3).
- **One actor per fleet.** Single writer, passes in `spawn_blocking`, snapshots on a
  `watch`; a Ready that lands mid-pass is applied when the pass ends (P3-4).
- **`Down` is a resting state.** The record and kept directories stay; `up` on a
  `Down` fleet re-applies in place; only `--purge` deletes (P3-5).
- **A failing pass retries at the resync cadence**, not at `next_restart_at`, so a
  flapping clone never spins the daemon.
```

- [ ] **Step 3: `AGENTS.md`**

Tasks: add `- \`e2e\` — the Phase 3 journey against a real daemon; needs the same tools as \`test-it\`.` and `- \`serve\` — a foreground daemon under \`target/tmp/serve\` for poking by hand (\`HOME\` is overridden, so it never touches your real state).`
Conventions: the ports bullet becomes "Ports (`Materializer`, `AgentRunner`, `Clock`, `FleetStore`, `EventHandler`) live in `balerix-core`; adapter crates implement them and never depend on each other. Only the `balerix` binary wires adapters to ports; `balerix-server` receives `Ports` and never imports `balerix-runtime`."
Gotchas: add
```markdown
- The e2e overrides the system tool table with an empty `[tools]` in its scratch
  `$XDG_CONFIG_HOME/balerix/mise.toml` so nothing downloads; the real embedded
  table pins `claude`, and a fresh `up` on a real host installs it.
- `balerix dev fake-claude` is what the e2e runs as `claude.binary`; it reads
  `$CLAUDE_CONFIG_DIR/settings.json` and fires the hooks itself. Change the hooks
  block in `home.rs` and the fake together.
- `serve` writes `server/endpoint` after binding; clients resolve `--api-url`,
  then `BALERIX_API_URL`, then that file. Tests bind port 0 and read it.
- `DELETE …?keep_repos=true`: axum's `Query` rejects bare flags, so every flag is
  `key=true|false` (`DownQuery::to_query_string`).
- Hook secrets live in `secrets.enc` (vault), the agent's `settings.json` (HTTP
  header) and `nono-profile.json` (`BALERIX_HOOK_SECRET` for the relay), all 0600.
```

- [ ] **Step 4: `docs/THREAT-MODEL.md`**

- Trust boundaries: "CLI ↔ daemon" row text becomes "resolved specs and credential bundles cross it over plain HTTP on `127.0.0.1`, authenticated by a 0600 bearer token; the user controls both ends today."
- Out of scope / accepted risks: add `- **No TLS on loopback** — an unprivileged local process cannot read loopback traffic; TLS arrives with the remote control plane (Phase 3 spec P3-1).` and `- **The balerix binary is readable inside the sandbox** (it is the \`SessionStart\` relay); the admin token and the state root are not granted, so an agent cannot drive the fleet API. \`hook-relay\` lets an agent post events as itself, which it could already do over HTTP.`
- Mitigations: replace the three *(planned)* rows:
  - "Credentials at rest": `vault key 0600 created at first serve; secrets.enc holds XChaCha20-Poly1305 ciphertext with the fleet name as AAD; plaintext only while writing an agent's files` — `crates/balerix-server/src/vault.rs`, `store.rs`.
  - "Forged hook events": `per-agent 32-byte secret compared in constant time (unknown agent and bad secret answer alike), 20/s burst 50 per agent, 1 MiB body, JSON-object-with-string-name validation at the edge, 2 s handler timeout` — `crates/balerix-server/src/hooks.rs`, `auth.rs`, `daemon.rs`.
  - "Local process reaching the API": `loopback bind, 0600 admin token compared in constant time on every /v1/fleets route` — `crates/balerix-server/src/api.rs`, `lifecycle.rs`.
  - "Secrets in debug output / logs": append `; hook payloads logged at debug only (daemon.rs); the e2e asserts no hook secret or token appears in server.log or any launch.sh`.
  - Add row "Hook secret at rest on the agent side": `settings.json (HTTP header) and nono-profile.json (BALERIX_HOOK_SECRET) are 0600 inside a 0700 agent dir; the secret authenticates only that agent` — `crates/balerix-runtime/src/home.rs`, `sandbox.rs`.

- [ ] **Step 5: Spec addenda and verdicts**

Architecture spec — append after the Phase 2 addendum:
```markdown
## Addendum 2026-09-06 (Phase 3)
`docs/superpowers/specs/2026-09-06-balerix-a3-control-plane-design.md` §10 lists
the corrections Phase 3 made: D4 (TLS deferred), D5 (`SessionStart` via
`balerix hook-relay`), §3 (`FleetStore` without `get`; `EventHandler` in
`core`), §7 (`FleetPhase::Down`, re-`up` in place, `status`/`list`), §8
(`Outcome` without actions until Spec B), §10 (e2e in the PR tier). Where they
differ, the Phase 3 spec wins. Spec A is complete.
```
Phase 2 spec §9 "Deliberately deferred": append ` — Resolved in Phase 3 (§6.3 of its spec): \`home/.gitconfig\` carries the gh credential helper for crews that may push.`
Phase 3 spec:
- §2: `FleetStore` shows `load_all`/`put`/`purge` only, with the note "`delete` from the brainstorm draft was dropped: nothing calls it; a downed fleet keeps its record until purged."
- §3.2: add "A pass with a failed step retries at the resync cadence, not at `next_restart_at`" and "`Down` requires a clean terminating pass **and** an empty agent map".
- §3.4: note "flags travel as `key=true|false`; axum's `Query` rejects bare keys".
- §5: `serve` gains the hidden `--tmux-socket` and `--detached-child` flags; `up`/`update` client-validate with `Fleet::try_from` before any request.
- §8.1: fill every row with a *Verdict*: single-file read grant — holds (probed 2026-09-06 with nono 0.75.0: validates, enforces, executes the granted binary); `SessionStart` command hook with the profile environment — holds in the e2e via `dev fake-claude`, **by hand with real claude: record the outcome here**; real Claude HTTP hooks to loopback `http://` with the literal header — **by hand: record the outcome**; repo `mise.toml` ignored — holds (probed 2026-09-06: `mise exec` with `MISE_QUIET=1` and an untrusted `mise.toml` in cwd runs the command; the e2e repo carries one); `.claude.json` copy and onboarding seed — **by hand: record which copy Claude read and whether any prompt appeared**; `HOME` relocation under live claude — **by hand: `ls agents/<a>/nono` after a real session**.

The three by-hand rows need one real `claude` session: on this machine, write a fleet file with `repo:` pointing at a small public repository, `git: { auth: none, push: false }`, run `serve -d`, `up`, attach with `tmux -L balerix attach -t <fleet>/<crew>`, confirm Claude reaches its prompt without onboarding, then check `balerix status` shows `Ready` (relay), `/metrics` shows a `UserPromptSubmit` or `Notification` count after typing one prompt (HTTP hooks), `ls agents/<a>/nono` (relocation), and which `.claude.json` has a newer mtime. Write each result into §8.1. If the HTTP-hook row fails, open a follow-up item "move remaining events to the relay" rather than changing code in this task.

- [ ] **Step 6: Verify onboarding, commit**

From a clean clone in `target/tmp/onboard` (`git clone /workspace target/tmp/onboard`), run README lines 1, 3, 5 and `list` (expect `no fleets`), then `down`-nothing and stop the daemon with `kill -TERM $(cat ~/.local/state/balerix/server/balerix.pid)` — or run the whole check with `HOME` pointed at a scratch dir to keep the real state untouched. Then:
```bash
mise run check && mise run test-it && mise run e2e
git add README.md ARCHITECTURE.md AGENTS.md docs
git commit -m "Document Phase 3: control-plane flow, decisions, threat model, spec addenda and verdicts

Claude-Session: https://claude.ai/code/session_01CbQxZqUzjQGWNgexiGTcNT"
```

## Done when

- `mise run check` passes on a fresh clone here and in CI with `BALERIX_REQUIRE_TOOLS=1`, `api_it`, `cli_serve`, `cli_fleet` and `e2e` included, inside the five-minute budget.
- The README quickstart (`serve -d`, `up`, `status`, `list`, `down --keep`) works by hand on this machine against a real repository, and the three by-hand rows of the Phase 3 spec §8.1 carry a verdict.
- `mise run mutants` reports no surviving mutants in `balerix-core/src/reconcile/`.
- Test count: `balerix-api` +4, `balerix-core` +8, `balerix-runtime` +4 unit (+ golden updated), `balerix-server` ~30 unit + 2 property + 1 API integration, `balerix` +6 unit + 6 CLI + 1 e2e.

## Deliberately deferred

- `Action` (`SendText`, `Restart`, `Stop`), the `balerix-events` crate, the `flow` block and `balerix_flow_*` metrics — Spec B.
- TLS on the API and client cert pinning — the remote control plane milestone (P3-1).
- Hook-secret rotation; a per-agent secret lives as long as the agent is in the spec.
- `balerix attach <fleet>/<crew>` — `tmux -L balerix attach -t <fleet>/<crew>` does it today.
- Reading the host's git identity as a host default; `git.identity` is the only source (§6.3).
