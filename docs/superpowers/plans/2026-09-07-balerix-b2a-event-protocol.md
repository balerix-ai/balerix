# Balerix Spec B / Phase 2a — Event Protocol Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give plugins the host protocol: the daemon activates a plugin for the agents whose `plugins:` block names it, runs the interceptor chain on every hook event, batches events to observers, executes the actions a verdict carries, and serves the `fleets`, `actions` and `kv` routes; the SDK gains the `Plugin` half and an async `Host`; `docs/plugin-protocol.md` becomes the contract. Ends with `balerix dev fake-plugin`, rewritten on the full SDK, blocking a `PreToolUse` and sending text to `fake-claude` in the e2e.

**Architecture:** `balerix-api` gains the §4 wire types and per-agent activation rows on `AgentStatus`. `balerix-core`'s `EventHandler` becomes async (a boxed `std::future::Future`, no tokio in core), `Outcome` regains `actions`, and `FleetRecord` gains a daemon-owned `stopped` set the planner honours: a stopped agent is stopped and never restarted, which is how `stop` and `restart` actions work without touching the restart counter. `balerix-server` gains four modules under `plugins/`: `client.rs` (one `reqwest` client for every daemon → plugin call), `registry.rs` (load-list order, listen addresses, subscriptions, `needs`, and the `(agent, plugin) → activation` table), `chain.rs` (`PluginEventHandler`: the interceptor chain with its 1500 ms budget, and the per-plugin observer queues), `kv.rs` (the per-plugin file store, secrets sealed by the vault); plus `activation.rs` (the pure spec diff) and `plugin_api.rs` (the `/v1/plugin-host/*` routes gated by `needs`). Activation runs in `Daemon::apply` *before* the actor sees the spec, so a rejection is a 400 and nothing lands; activation state is a read-time overlay on the record (`Daemon::get`/`snapshots`), so the actor stays the single writer of its record and the registry the single writer of activations. The actor gains one message, `SetStopped`. `balerix-plugin-sdk` becomes async on `reqwest` + `axum`: `Host` with a method per §4.1 route, a `Plugin` trait with default no-op methods, `serve()`, and `testing::FakeHost`. The binary wires `PluginEventHandler` into `serve`, makes `up` wait on activations, rewrites `dev fake-plugin` on the SDK and teaches `fake-claude` to read stdin and fire `PreToolUse`/`Stop` hooks.

**Tech Stack:** Rust 1.98.1 (edition 2024); tokio 1.53.1, axum 0.8.9, prometheus 0.14.0; **new:** reqwest 0.13.4 (`default-features = false`, `features = ["json"]`: no TLS provider, no HTTP/2, no proxy discovery); ureq 3.4.1 stays in the CLI, relay and tests; serde/serde_json/serde_norway; chacha20poly1305 (the vault); thiserror; clap 4; insta, proptest, proptest-state-machine; tempfile, assert_cmd, predicates; real `mise`, `nono 0.75.0`, `tmux 3.7c` for the e2e.

**Spec:** `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md` (the *plugins spec*), §13 item 2a and §16 (the phase 2a decisions), on top of §3, §4, §7, §9, §11, and the Phase 3 spec `docs/superpowers/specs/2026-09-06-balerix-a3-control-plane-design.md` for the daemon it extends. Read plugins spec §4 and §16 before any task; §4.3 before Task 5; §7 before Task 8; §11 "Testing" before Tasks 9–10. Where this plan refines §16 (recorded again in Task 13):

- **Activation state is a read-time overlay, not an actor message.** §16.3 says the daemon publishes activation changes "through the fleet's actor". The registry owns the `(agent, plugin) → PluginActivation` table and `Daemon::get`/`snapshots`/`apply` copy it into `AgentStatus.plugins` on the way out. The actor never sees activations, its persisted record never carries them, and the single-writer rule holds for both: the actor writes the record, the registry writes activations. `up` polls `GET /v1/fleets/{name}`, so it sees the overlay.
- **A plugin is identified by its token alone.** No route under `/v1/plugin-host/` carries the plugin's name (§4.1 specifies only the bearer). `Daemon::plugin_for_token` walks the `balerix` fleet's entries of the secret index with constant-time compares; plugins are few. `hello` still checks that the body's `name` matches.
- **`stopped` is keyed by the agent id's display form**, like `FleetStatus.agents`, so a stored `fleet.json` from phase 1 loads with an empty set (`#[serde(default)]`).
- **Health is polled by a daemon task every 10 s**, and a failure only sets the plugin's status message (`degraded: <reason>`), shown by `plugin list`; the registry keeps that message, the actor's record is untouched.
- **`fleets/watch` is not built** (§16.5); the `fleets` capability gates `GET fleets` and `GET fleets/{f}` only.
- **`balerix_plugin_proxy_requests_total` is not registered**: it has no producer until phase 3.

## Global Constraints

Copied from the specs and the phase 1 plan; every task's requirements include these.

- Rust **1.98.1**, `edition = "2024"`, `rust-version = "1.98"`; every tool in `mise.toml` is an exact version. Run cargo as `mise x -- cargo …` or through `mise run <task>`.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass at every commit. `unsafe_code = "forbid"`. No `unwrap`/`expect` outside tests (clippy warns; `-D warnings` makes it an error; test modules and `tests/*.rs` carry `#![allow(clippy::unwrap_used, clippy::expect_used)]`). `std::env::set_var` is unsafe in edition 2024 — inject environment through parameters.
- Library crates return `thiserror` errors whose `Display` starts with the id (runtime, server) or config path (config, core; `crews.<c>.agents.<a>.plugins.<p>: …` for activation); only the `balerix` binary uses `anyhow`.
- Dependency direction: `api` leaf → `core` → `config` / `runtime` / `server` / `plugin-sdk` (adapters) → binary. `server` never depends on `runtime` or `config`; `plugin-sdk` depends on `api` only among workspace crates; no adapter depends on another adapter. **Exception, dev-dependencies only:** `balerix-server`'s integration tests may depend on `balerix-plugin-sdk` (spec §11: "a fake plugin built on the SDK runs in-process against `PluginHost`").
- New Cargo dependencies go in `[workspace.dependencies]` with an exact version and a reason in the commit message. This plan adds exactly one: `reqwest = { version = "0.13.4", default-features = false, features = ["json"] }` (§16.1), used by `balerix-server`, `balerix-plugin-sdk` and `balerix` (the binary, for `fake-plugin` through the SDK only). `axum` and `tokio` are added to `balerix-plugin-sdk` at the workspace versions; `ureq` leaves it. No `futures`, `async-trait`, `trait-variant`, `tokio-util`: `tokio::task::JoinSet` fans out, `std::future::Future` types the port, and the SDK's `Plugin` trait uses return-position `impl Future + Send`.
- Plain HTTP on `127.0.0.1` only (P3-1). No `rustls`, `rcgen`, `tonic`; `reqwest` without any TLS feature. Every `reqwest::Client` is built with `.no_proxy()` so a `HTTP_PROXY` in the daemon's environment cannot redirect loopback calls.
- Secrets never appear in `Debug` output, logs, argv, the outer environment, or `launch.sh`. The plugin token travels only in `nono-profile.json` (0600) and the `Authorization` header. KV secret entries are sealed by the vault before they touch disk; `kv/` is never granted to the sandbox.
- Subprocesses are argv arrays via `std::process::Command`; never a shell string.
- Names are already validated (`FleetName`, `AgentName`, `AgentId`); the server never builds a path from an unvalidated string. KV keys are validated (`[A-Za-z0-9._/-]{1,200}`, no empty or `..` segment) before any path is formed.
- Plugin responses are untrusted input: a `response` that is not a JSON object is a failure (`reason="body"`), a rejection message is quoted verbatim into the config-path error and never parsed further, bodies are capped at 1 MiB.
- Integration and e2e tests skip with a printed reason when a tool or Landlock is missing; if `BALERIX_REQUIRE_TOOLS=1` is set (CI), a would-be skip panics instead. Temp roots live under `target/tmp` (`CARGO_TARGET_TMPDIR`), never `/tmp`.
- Commit messages: imperative subject, body explains why, and end with the trailer line `Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe`.
- insta: read every `.snap.new`, compare against the expected values listed in the task, then `mise x -- cargo insta accept`. Never blind-accept.
- The pre-commit hook runs `mise run precommit` (gitleaks + `check`, e2e included). Every commit below goes through it. `cargo mutants -p balerix-core` (`mise run mutants`) must still report no surviving mutants in `reconcile` after Task 2.

## File structure

```
Cargo.toml                                        + reqwest 0.13.4 (workspace dep)
crates/balerix-api/src/status.rs                  + ActivationState, PluginActivation; AgentStatus.plugins
crates/balerix-api/src/plugin.rs                  PluginStatus.active_agents
crates/balerix-api/src/record.rs                  NEW: FleetRecord, Desired, Keep (moved from core; core re-exports them)
crates/balerix-api/src/protocol.rs                NEW: PluginAction, ActivateRequest, DeactivateRequest, EventBatch,
                                                  InterceptRequest, InterceptResponse, KvKeys, OBSERVER_BATCH, CHAIN_BUDGET_MS
crates/balerix-api/src/lib.rs                     re-exports
crates/balerix-core/src/events.rs                 Outcome.actions; async EventHandler (HandlerFuture); PassThrough
crates/balerix-api/src/record.rs                  FleetRecord.stopped: BTreeSet<String>
crates/balerix-core/src/store.rs                  re-exports FleetRecord, Desired from api
crates/balerix-core/src/ports.rs                  re-exports Keep from api
crates/balerix-core/src/reconcile/mod.rs          plan(.., stopped, ..): stopped agents are stopped and never restarted
crates/balerix-core/src/reconcile/execute.rs      ReconcileContext.stopped
crates/balerix-core/src/reconcile/status.rs       derive_fleet_phase: Stopped counts as settled
crates/balerix-core/tests/plan_golden.rs          + stopped snapshots
crates/balerix-core/tests/reconcile_model.rs      + Stop/Resume transitions
crates/balerix-server/Cargo.toml                  + reqwest; dev: balerix-plugin-sdk
crates/balerix-server/src/actor.rs                Msg::SetStopped; Apply clears stopped
crates/balerix-server/src/metrics.rs              + six plugin families; hook_action()
crates/balerix-server/src/plugins/client.rs       NEW: PluginClient (activate, deactivate, events, intercept, health, metrics)
crates/balerix-server/src/plugins/registry.rs     NEW: PluginRegistry
crates/balerix-server/src/plugins/activation.rs   NEW: pairs(), diff() — pure
crates/balerix-server/src/plugins/kv.rs           NEW: PluginKv, validate_key
crates/balerix-server/src/plugins/chain.rs        NEW: PluginEventHandler, ObserverQueue
crates/balerix-server/src/plugins/host.rs         registry instead of `listen`; degraded message; active_agents
crates/balerix-server/src/plugins/mod.rs          re-exports; PluginError::{Activation, Capability, Kv}
crates/balerix-server/src/daemon.rs               activation in apply/down/start/hello; execute_action; plugin_for_token; overlay; health task
crates/balerix-server/src/hooks.rs                spawn the outcome's actions after the response
crates/balerix-server/src/plugin_api.rs           NEW: /v1/plugin-host/{fleets,agents/*/actions,kv} handlers
crates/balerix-server/src/api.rs                  mount plugin_api; /metrics re-export
crates/balerix-server/src/testing.rs              Harness gains registry/client helpers
crates/balerix-server/src/lib.rs                  re-exports
crates/balerix-server/tests/support/mod.rs        NEW: shared Api helper + world()
crates/balerix-server/tests/events_it.rs          NEW: activation, chain, observers, actions, kv, stop/restart
crates/balerix-server/tests/protocol_it.rs        NEW: PluginClient conformance against the fixtures
crates/balerix-plugin-sdk/Cargo.toml              + reqwest, tokio, axum, serde; − ureq
crates/balerix-plugin-sdk/src/lib.rs              Env; re-exports
crates/balerix-plugin-sdk/src/host.rs             NEW: async Host
crates/balerix-plugin-sdk/src/plugin.rs           NEW: Plugin trait, router(), serve()
crates/balerix-plugin-sdk/src/testing.rs          NEW: FakeHost
crates/balerix-plugin-sdk/tests/conformance.rs    NEW: router + Host against the fixtures
docs/plugin-protocol.md                           NEW: the wire contract
docs/plugin-protocol/*.json                       NEW: fixtures
crates/balerix/Cargo.toml                         (reqwest via the SDK only; tokio already present)
crates/balerix/src/commands/serve.rs              wire registry, client, PluginEventHandler
crates/balerix/src/commands/fleet.rs              up/update wait on activations; PLUGINS column
crates/balerix/src/commands/plugin.rs             ACTIVE column
crates/balerix/src/commands/dev.rs                fake-plugin on the SDK; fake-claude stdin reader + PreToolUse/Stop hooks
crates/balerix/src/cli.rs                         FakePlugin doc
crates/balerix/tests/e2e.rs                       plugin_protocol_journey
ARCHITECTURE.md, AGENTS.md, README.md, docs/THREAT-MODEL.md, the plugins spec §16
```

---

### Task 1: `balerix-api` — protocol wire types, activation rows, `active_agents`

**Files:**
- Create: `crates/balerix-api/src/protocol.rs`, `crates/balerix-api/src/record.rs`
- Modify: `crates/balerix-core/src/store.rs` (re-export), `crates/balerix-core/src/ports.rs` (re-export), `crates/balerix-api/src/status.rs`, `crates/balerix-api/src/plugin.rs:96-110` (`PluginStatus`), `crates/balerix-api/src/lib.rs`, `crates/balerix-server/src/plugins/host.rs:276` and `crates/balerix/src/commands/plugin.rs:253-268` (struct literals gain the new field)

**Interfaces:**
- Consumes: `HookEvent`, `Timestamp`, `AgentPhase` (existing).
- Produces (used by every later task):
  - `balerix_api::PluginAction` — `SendText { text: String, submit: bool } | Restart | Stop`, tagged `"action"` on the wire (`send_text` / `restart` / `stop`); `fn label(&self) -> &'static str` returns the same three strings.
  - `balerix_api::ActivationState` — `Pending | Active | Rejected`, lowercase on the wire.
  - `balerix_api::PluginActivation { pub state: ActivationState, pub message: String }`, with `PluginActivation::pending()`, `::active()`, `::rejected(message: impl Into<String>)`.
  - `AgentStatus.plugins: BTreeMap<String, PluginActivation>` (default empty, skipped when empty).
  - `PluginStatus.active_agents: u32` (default 0).
  - `balerix_api::{ActivateRequest { agent: String, config: Value }, DeactivateRequest { agent: String }, EventBatch { events: Vec<HookEvent> }, InterceptRequest { event: HookEvent, response_so_far: Value, deadline_ms: u64 }, InterceptResponse { response: Value, actions: Vec<PluginAction> }, KvKeys { keys: Vec<String> }}`.
  - `balerix_api::{OBSERVER_BATCH: usize = 64, OBSERVER_QUEUE: usize = 1024, CHAIN_BUDGET_MS: u64 = 1500}`.
  - `balerix_api::{FleetRecord, Desired, Keep}` — moved from `balerix-core` unchanged (fields, `FleetRecord::{new, name, is_down, summary}`, serde forms); `balerix_core::{FleetRecord, Desired, Keep}` keep working as re-exports, so no other crate changes an import. The SDK (Task 8) needs the record typed and may depend on `balerix-api` only.

- [ ] **Step 1: Write the failing tests**

Create `crates/balerix-api/src/protocol.rs` with only a test module first:

```rust
//! Daemon ↔ plugin bodies (plugins spec §4.2, §4.3) and the constants both
//! sides agree on. Serde DTOs only.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Timestamp;
    use serde_json::json;

    #[test]
    fn actions_are_tagged_by_kind_and_labelled() {
        let a: PluginAction =
            serde_json::from_value(json!({ "action": "send_text", "text": "hi", "submit": true }))
                .unwrap();
        assert_eq!(
            a,
            PluginAction::SendText {
                text: "hi".into(),
                submit: true
            }
        );
        assert_eq!(a.label(), "send_text");
        let r: PluginAction = serde_json::from_value(json!({ "action": "restart" })).unwrap();
        assert_eq!((r.clone(), r.label()), (PluginAction::Restart, "restart"));
        assert_eq!(
            serde_json::to_value(PluginAction::Stop).unwrap(),
            json!({ "action": "stop" })
        );
        let no_submit: PluginAction =
            serde_json::from_value(json!({ "action": "send_text", "text": "x" })).unwrap();
        assert_eq!(
            no_submit,
            PluginAction::SendText {
                text: "x".into(),
                submit: false
            },
            "submit defaults to false"
        );
        assert!(serde_json::from_value::<PluginAction>(json!({ "action": "reboot" })).is_err());
        assert!(
            serde_json::from_value::<PluginAction>(json!({ "action": "stop", "x": 1 })).is_err(),
            "unknown fields rejected"
        );
    }

    #[test]
    fn intercept_bodies_round_trip_and_actions_default_empty() {
        let event = HookEvent {
            agent: "f/c/a".into(),
            name: "PreToolUse".into(),
            session_id: None,
            received_at: Timestamp(5),
            payload: json!({ "tool_name": "Bash" }),
        };
        let req = InterceptRequest {
            event: event.clone(),
            response_so_far: json!({}),
            deadline_ms: 1200,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["deadline_ms"], 1200);
        assert_eq!(v["event"]["name"], "PreToolUse");
        let back: InterceptRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back, req);
        let resp: InterceptResponse =
            serde_json::from_value(json!({ "response": { "decision": "block" } })).unwrap();
        assert_eq!(resp.response["decision"], "block");
        assert!(resp.actions.is_empty());
        let batch = EventBatch {
            events: vec![event],
        };
        let back: EventBatch =
            serde_json::from_str(&serde_json::to_string(&batch).unwrap()).unwrap();
        assert_eq!(back, batch);
        let act: ActivateRequest =
            serde_json::from_value(json!({ "agent": "f/c/a", "config": { "k": 1 } })).unwrap();
        assert_eq!((act.agent.as_str(), act.config["k"].as_i64()), ("f/c/a", Some(1)));
        let de = DeactivateRequest {
            agent: "f/c/a".into(),
        };
        assert_eq!(serde_json::to_value(&de).unwrap(), json!({ "agent": "f/c/a" }));
        let keys: KvKeys = serde_json::from_value(json!({ "keys": ["a", "b/c"] })).unwrap();
        assert_eq!(keys.keys, vec!["a", "b/c"]);
        assert_eq!((OBSERVER_BATCH, OBSERVER_QUEUE, CHAIN_BUDGET_MS), (64, 1024, 1500));
    }
}
```

Append to the test module in `crates/balerix-api/src/status.rs`:

```rust
    #[test]
    fn activation_rows_are_optional_on_the_wire() {
        let s: AgentStatus = serde_json::from_value(json!({ "phase": "ready" })).unwrap();
        assert!(s.plugins.is_empty());
        let v = serde_json::to_value(&s).unwrap();
        assert!(v.get("plugins").is_none(), "empty map is skipped");
        let mut s = s;
        s.plugins
            .insert("flow".into(), PluginActivation::rejected("bad regex"));
        s.plugins.insert("web".into(), PluginActivation::active());
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["plugins"]["flow"]["state"], "rejected");
        assert_eq!(v["plugins"]["flow"]["message"], "bad regex");
        assert_eq!(v["plugins"]["web"]["state"], "active");
        assert_eq!(v["plugins"]["web"]["message"], "");
        let back: AgentStatus = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
        assert_eq!(
            PluginActivation::pending(),
            PluginActivation {
                state: ActivationState::Pending,
                message: String::new()
            }
        );
    }
```

And in `crates/balerix-api/src/plugin.rs`'s `hello_status_and_report_round_trip`, after `let v = serde_json::to_value(&s).unwrap();` add:

```rust
        assert_eq!(v["active_agents"], 0);
        let older: PluginStatus = serde_json::from_value(json!({
            "name": "web", "version": "0.1.0", "phase": "ready", "routes": false
        }))
        .unwrap();
        assert_eq!(older.active_agents, 0, "a phase 1 row still loads");
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-api`
Expected: compile errors — `PluginAction`, `InterceptRequest`, `PluginActivation`, `active_agents` not found.

- [ ] **Step 3: Implement**

Prepend to `crates/balerix-api/src/protocol.rs` (above the test module):

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::HookEvent;

/// Observer batches are cut at this many events (spec §4.3).
pub const OBSERVER_BATCH: usize = 64;
/// Per-plugin observer queue depth; the oldest event is dropped on overflow.
pub const OBSERVER_QUEUE: usize = 1024;
/// The interceptor chain's shared budget inside Claude's 2 s hook timeout.
pub const CHAIN_BUDGET_MS: u64 = 1500;

/// What a verdict may ask the daemon to do (spec §3, §4.3, §8.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginAction {
    SendText {
        text: String,
        #[serde(default)]
        submit: bool,
    },
    Restart,
    Stop,
}

impl PluginAction {
    /// The wire tag; also the metrics label.
    pub fn label(&self) -> &'static str {
        match self {
            PluginAction::SendText { .. } => "send_text",
            PluginAction::Restart => "restart",
            PluginAction::Stop => "stop",
        }
    }
}

/// `POST /v1/activate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivateRequest {
    /// `fleet/crew/agent`.
    pub agent: String,
    /// The agent's resolved config for this plugin.
    pub config: Value,
}

/// `POST /v1/deactivate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeactivateRequest {
    pub agent: String,
}

/// `POST /v1/events`: at most `OBSERVER_BATCH`, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventBatch {
    pub events: Vec<HookEvent>,
}

/// `POST /v1/intercept`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterceptRequest {
    pub event: HookEvent,
    /// The chain's response so far; `{}` for the first plugin.
    pub response_so_far: Value,
    /// What remains of the chain's budget for this call.
    pub deadline_ms: u64,
}

/// The verdict. `response` must be a JSON object; anything else is a
/// failure the daemon skips.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterceptResponse {
    pub response: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<PluginAction>,
}

/// `GET /v1/plugin-host/kv?prefix=`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KvKeys {
    pub keys: Vec<String>,
}
```

In `crates/balerix-api/src/status.rs`, after `AgentPhase`:

```rust
/// Where a plugin stands for one agent (plugins spec §16.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivationState {
    /// Recorded; `activate` goes out at the plugin's next `hello`.
    Pending,
    /// The plugin accepted the agent's config.
    Active,
    /// The plugin refused; `message` is its error.
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginActivation {
    pub state: ActivationState,
    #[serde(default)]
    pub message: String,
}

impl PluginActivation {
    pub fn pending() -> Self {
        Self {
            state: ActivationState::Pending,
            message: String::new(),
        }
    }
    pub fn active() -> Self {
        Self {
            state: ActivationState::Active,
            message: String::new(),
        }
    }
    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            state: ActivationState::Rejected,
            message: message.into(),
        }
    }
}
```

Add to `AgentStatus` (last field) and its `Default`:

```rust
    /// Plugin name → activation state, filled in by the daemon on the way
    /// out (a read-time overlay); never stored.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, PluginActivation>,
```
```rust
            plugins: BTreeMap::new(),
```

In `crates/balerix-api/src/plugin.rs` add to `PluginStatus` after `message`:

```rust
    /// Agents this plugin is currently `Active` for.
    #[serde(default)]
    pub active_agents: u32,
```

Update the two struct literals that construct `PluginStatus` outside this crate — `crates/balerix-server/src/plugins/host.rs:276` (add `active_agents: 0,` for now; Task 7 fills it) and the two in the test at `crates/balerix/src/commands/plugin.rs:253-268` (`active_agents: 0,`), and the one in this crate's test (`active_agents: 0,`).

In `crates/balerix-api/src/lib.rs`: add `pub mod protocol;` and

```rust
pub use protocol::{
    ActivateRequest, CHAIN_BUDGET_MS, DeactivateRequest, EventBatch, InterceptRequest,
    InterceptResponse, KvKeys, OBSERVER_BATCH, OBSERVER_QUEUE, PluginAction,
};
pub use status::{
    ActivationState, AgentPhase, AgentStatus, FleetPhase, FleetStatus, FleetSummary,
    PluginActivation, SpecHash, Timestamp,
};
```

(replace the existing `status` re-export line).

- [ ] **Step 3b: Move `FleetRecord`, `Desired` and `Keep` into `balerix-api`**

Create `crates/balerix-api/src/record.rs` with the `Keep` struct from `crates/balerix-core/src/ports.rs:93-99`, and the `FleetRecord` struct, `Desired` enum and `impl FleetRecord` block from `crates/balerix-core/src/store.rs:15-56`, verbatim (module doc: "One fleet as the daemon stores and returns it (Phase 3 spec §2). A wire type: the CLI, the plugin SDK and the daemon all read it."). Move the four tests that exercise them (`a_new_record_is_up_at_generation_zero`, `desired_serializes_with_a_state_tag`, `is_down_needs_both_the_desire_and_the_phase` from `store.rs`; nothing from `ports.rs` tests `Keep` on its own) into `record.rs`'s test module. In `lib.rs`: `pub mod record;` and `pub use record::{Desired, FleetRecord, Keep};`.

In `balerix-core`: `store.rs` drops the moved items and adds `pub use balerix_api::{Desired, FleetRecord};` (its remaining `use balerix_api::{…}` list loses `FleetPhase, FleetSpec, FleetSummary` if now unused); `ports.rs` drops `Keep` and adds `pub use balerix_api::Keep;`. `lib.rs`'s `pub use store::{Desired, FleetRecord, …}` and `pub use ports::{…, Keep, …}` lines stay as they are. `FleetSecrets`, `FleetStore` and `StoreError` stay in core (not wire types).

Run: `mise x -- cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean; every `balerix_core::FleetRecord` / `Keep` / `Desired` path in the workspace still resolves through the re-exports.

- [ ] **Step 4: Run the tests and the workspace build**

Run: `mise x -- cargo test -p balerix-api && mise x -- cargo clippy --workspace --all-targets -- -D warnings`
Expected: all `balerix-api` tests pass, including the three new ones; the workspace compiles (the only other change is the `active_agents: 0` literals).

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-api crates/balerix-core crates/balerix-server/src/plugins/host.rs crates/balerix/src/commands/plugin.rs
git commit -m "Add the plugin protocol wire types and activation rows

Spec B §4.2/§4.3 bodies, PluginAction with its wire tag, and the
pending/active/rejected activation row the daemon overlays on
AgentStatus (§16.3). PluginStatus gains active_agents. FleetRecord,
Desired and Keep move to balerix-api (re-exported by core) so the SDK,
which depends on api only, can type the fleets route.

Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe"
```

---

### Task 2: `balerix-core` — async `EventHandler`, `Outcome.actions`, `stopped` in the planner

**Files:**
- Modify: `crates/balerix-core/src/events.rs`, `crates/balerix-api/src/record.rs`, `crates/balerix-core/src/reconcile/mod.rs`, `crates/balerix-core/src/reconcile/execute.rs`, `crates/balerix-core/src/reconcile/status.rs`, `crates/balerix-core/src/lib.rs`, `crates/balerix-core/tests/plan_golden.rs`, `crates/balerix-core/tests/reconcile_model.rs`, `crates/balerix-server/src/actor.rs:292-302` and `crates/balerix-server/src/daemon.rs:320` (compile fixes for the new signatures)

**Interfaces:**
- Consumes: Task 1's `PluginAction`.
- Produces:
  - `balerix_core::Outcome { pub response: Value, pub actions: Vec<PluginAction> }`; `Outcome::allow()` has no actions.
  - `balerix_core::HandlerFuture<'a> = Pin<Box<dyn Future<Output = Outcome> + Send + 'a>>`; `trait EventHandler: Send + Sync { fn handle<'a>(&'a self, event: &'a HookEvent) -> HandlerFuture<'a>; }`. `PassThrough` still implements it.
  - `FleetRecord.stopped: BTreeSet<String>` (agent ids in display form; serde default).
  - `reconcile::plan(fleet, desired, keep, stopped: &BTreeSet<AgentId>, status, observed, policy, now)` — the new fourth parameter. A desired agent in `stopped` gets `Stop` if observed and nothing else; `apply(Stop, Ok)` sets phase `Stopped` and clears `next_restart_at`; `derive_fleet_phase` treats `Stopped` as settled (a fleet whose agents are all `Ready` or `Stopped` is `Ready`).
  - `ReconcileContext.stopped: &'a BTreeSet<AgentId>`.

- [ ] **Step 1: Write the failing tests**

In `crates/balerix-core/src/events.rs`, replace the test module with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::Timestamp;
    use serde_json::json;
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    /// Enough of an executor for a handler that is immediately ready:
    /// core has no async runtime and must not grow one for a test.
    fn block_on<F: Future>(f: F) -> F::Output {
        let mut f = pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    #[test]
    fn pass_through_allows_everything_with_no_actions() {
        let e = HookEvent {
            agent: "f/c/a".into(),
            name: "PreToolUse".into(),
            session_id: None,
            received_at: Timestamp(1),
            payload: json!({ "tool_name": "Bash" }),
        };
        let h: &dyn EventHandler = &PassThrough;
        let out = block_on(h.handle(&e));
        assert_eq!(out, Outcome::allow());
        assert_eq!(out.response, json!({}));
        assert!(out.actions.is_empty());
    }
}
```

In `crates/balerix-api/src/record.rs` tests, add:

```rust
    #[test]
    fn stopped_defaults_empty_and_round_trips() {
        let mut r = FleetRecord::new(spec());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["stopped"], serde_json::json!([]));
        let older: FleetRecord = serde_json::from_value(serde_json::json!({
            "spec": { "name": "payments" }, "generation": 1, "desired": { "state": "up" },
            "status": { "generation": 1, "observed_generation": 1, "phase": "ready" }
        }))
        .unwrap();
        assert!(older.stopped.is_empty(), "a phase 1 fleet.json loads");
        r.stopped.insert("payments/backend/bob".into());
        let back: FleetRecord =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back.stopped, r.stopped);
    }
```

In `crates/balerix-core/src/reconcile/mod.rs` tests: change `plan_for` to take `stopped: &[&str]` and add two tests:

```rust
    fn plan_for(
        desired: Option<&Fleet>,
        stopped: &[&str],
        status: &FleetStatus,
        observed: &ObservedState,
        now: u64,
    ) -> Vec<String> {
        let stopped: BTreeSet<AgentId> = stopped.iter().map(|s| id(s)).collect();
        render(&plan(
            &fleet_name(),
            desired,
            Keep::default(),
            &stopped,
            status,
            observed,
            &ReconcilePolicy::default(),
            Timestamp(now),
        ))
    }
```

(every existing call gains `&[]` as its second argument; the three direct `plan(` calls gain `&BTreeSet::new(),` after `Keep {..}`.)

```rust
    #[test]
    fn a_stopped_agent_is_stopped_and_never_restarted() {
        let f = fleet(&[("a", 1), ("b", 1)]);
        let mut st = FleetStatus::default();
        for a in ["a", "b"] {
            let e = st.entry(&format!("f/c/{a}"));
            e.applied_hash = Some(hash_of(&f, a));
            e.phase = AgentPhase::Ready;
        }
        let mut obs = ObservedState::default();
        obs.set(&id("f/c/a"), ProcessState::Running { pid: 1 });
        obs.set(&id("f/c/b"), ProcessState::Running { pid: 2 });
        assert_eq!(
            plan_for(Some(&f), &["f/c/b"], &st, &obs, 0),
            vec!["stop f/c/b", "ensure-crew f/c"],
            "running: stopped, a untouched"
        );
        // exited with a due restart: still nothing but the stop
        st.entry("f/c/b").next_restart_at = Some(Timestamp(0));
        obs.set(&id("f/c/b"), ProcessState::Exited { code: Some(1) });
        assert_eq!(
            plan_for(Some(&f), &["f/c/b"], &st, &obs, 5),
            vec!["stop f/c/b", "ensure-crew f/c"]
        );
        // window gone, phase Stopped: nothing at all
        st.entry("f/c/b").phase = AgentPhase::Stopped;
        st.entry("f/c/b").next_restart_at = None;
        obs.remove(&id("f/c/b"));
        assert_eq!(
            plan_for(Some(&f), &["f/c/b"], &st, &obs, 5),
            vec!["ensure-crew f/c"]
        );
        // a changed hash while stopped: still nothing (the stop wins)
        let f2 = fleet(&[("a", 1), ("b", 2)]);
        assert_eq!(
            plan_for(Some(&f2), &["f/c/b"], &st, &obs, 5),
            vec!["ensure-crew f/c"]
        );
    }

    #[test]
    fn resuming_a_stopped_agent_restarts_it() {
        let f = fleet(&[("a", 1)]);
        let mut st = FleetStatus::default();
        *st.entry("f/c/a") = AgentStatus {
            phase: AgentPhase::Stopped,
            applied_hash: Some(hash_of(&f, "a")),
            restarts: 3,
            ..AgentStatus::default()
        };
        let got = plan_for(Some(&f), &[], &st, &ObservedState::default(), 0);
        assert_eq!(
            got,
            vec![
                "ensure-crew f/c".to_string(),
                "materialize f/c/a".into(),
                format!("start f/c/a {}", short(&hash_of(&f, "a")))
            ]
        );
    }
```

In `crates/balerix-core/src/reconcile/status.rs` tests, add:

```rust
    #[test]
    fn a_stop_clears_the_pending_restart_and_counts_as_settled() {
        let mut s = FleetStatus::default();
        apply(
            &mut s,
            &Step::Start(id("f/c/a"), h("x")),
            &Ok(()),
            &policy(),
            Timestamp(0),
        );
        apply(
            &mut s,
            &Step::NoteExit(id("f/c/a"), Some(1)),
            &Ok(()),
            &policy(),
            Timestamp(10),
        );
        assert!(s.agents["f/c/a"].next_restart_at.is_some());
        apply(
            &mut s,
            &Step::Stop(id("f/c/a")),
            &Ok(()),
            &policy(),
            Timestamp(11),
        );
        let a = &s.agents["f/c/a"];
        assert_eq!((a.phase, a.next_restart_at, a.restarts), (AgentPhase::Stopped, None, 1));
        s.entry("f/c/b").phase = AgentPhase::Ready;
        finish_pass(&mut s, false, true);
        assert_eq!(s.phase, FleetPhase::Ready, "stopped agents do not hold the fleet");
        s.entry("f/c/b").phase = AgentPhase::Starting;
        finish_pass(&mut s, false, true);
        assert_eq!(s.phase, FleetPhase::Reconciling);
    }
```

In `crates/balerix-core/src/reconcile/execute.rs`: add `stopped: BTreeSet<AgentId>` to the test `Harness` (default empty), pass `stopped: &self.stopped` in `ctx()`, and add:

```rust
    #[test]
    fn stop_then_resume_keeps_the_restart_count() {
        let mut h = Harness::new();
        let f = fleet(&["a"]);
        let mut st = FleetStatus::default();
        reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        st.agents.get_mut("f/c/a").unwrap().restarts = 2;
        h.stopped.insert("f/c/a".parse().unwrap());
        let (p, rep) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert!(rep.all_ok());
        assert_eq!(
            p.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["stop f/c/a", "ensure-crew f/c"]
        );
        assert_eq!(st.agents["f/c/a"].phase, AgentPhase::Stopped);
        assert_eq!(h.r.observed().get(&"f/c/a".parse().unwrap()), None);
        // idle while stopped
        let (p, _) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(p.len(), 1);
        h.stopped.clear();
        let (p, _) = reconcile_pass(&mut st, &h.ctx(Some(&f))).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(st.agents["f/c/a"].phase, AgentPhase::Starting);
        assert_eq!(st.agents["f/c/a"].restarts, 2, "a deliberate stop is not an exit");
    }
```

In `crates/balerix-core/tests/plan_golden.rs`: give `go` a `stopped: &[&str]` parameter (every existing call passes `&[]`), building `BTreeSet<AgentId>` from `format!("payments/backend/{s}")`, and add:

```rust
#[test]
fn stopped_agent() {
    let f = fleet(&[("alice", "sonnet"), ("bob", "opus")]);
    let mut st = FleetStatus::default();
    applied(&f, &mut st, AgentPhase::Ready);
    insta::assert_snapshot!(
        "stopped_agent_running",
        go(Some(&f), Keep::default(), &["bob"], &st, &running(&f))
    );
    let mut obs = running(&f);
    obs.remove(&"payments/backend/bob".parse().unwrap());
    st.entry("payments/backend/bob").phase = AgentPhase::Stopped;
    insta::assert_snapshot!(
        "stopped_agent_resumed",
        go(Some(&f), Keep::default(), &[], &st, &obs)
    );
}
```

Expected snapshots: `stopped_agent_running` is exactly `stop payments/backend/bob\nensure-crew payments/backend\n`; `stopped_agent_resumed` is `ensure-crew payments/backend\nmaterialize payments/backend/bob\nstart payments/backend/bob <8 hex chars>\n`.

In `crates/balerix-core/tests/reconcile_model.rs`: add `Stop(String)` and `Resume(String)` transitions.

- `RefState` gains `stopped: BTreeSet<String>`; `Sut` gains `stopped: BTreeSet<AgentId>` (pass `stopped: &self.stopped` in `pass()`; `init_test` starts it empty).
- `transitions`: add `2 => pick.clone().prop_map(Transition::Stop)` and `2 => pick.clone().prop_map(Transition::Resume)` to the non-empty branch.
- `preconditions`: `Transition::Stop(a) => state.desired.as_ref().is_some_and(|d| d.contains_key(a)) && !state.stopped.contains(a)`; `Transition::Resume(a) => state.stopped.contains(a)`.
- Reference `apply`:

```rust
            Transition::Stop(name) => {
                s.stopped.insert(name.clone());
                if let Some(a) = s.agents.get_mut(name) {
                    a.phase = AgentPhase::Stopped;
                    a.next_restart_at = None;
                }
            }
            Transition::Resume(name) => {
                s.stopped.remove(name);
                if s.desired.as_ref().is_some_and(|d| d.contains_key(name))
                    && let Some(a) = s.agents.get_mut(name)
                {
                    a.phase = AgentPhase::Starting;
                    a.next_restart_at = None;
                }
            }
```

  and `Up(map)` / `Update(..)` clear the set for every declared agent (an `Apply` always does): in `Up`, before the version loop, `let resumed: Vec<String> = s.stopped.iter().filter(|n| map.contains_key(*n)).cloned().collect(); s.stopped.retain(|n| !map.contains_key(n));` and inside the loop, `else if resumed.contains(name) { let a = s.agents.get_mut(name).unwrap(); a.phase = AgentPhase::Starting; a.next_restart_at = None; }` after the `if changed { … }`. In `Update`, `let resumed = std::mem::take(&mut s.stopped);` first, then for every `n` in `resumed` that is in `s.agents` and whose version is unchanged by this update, set `phase = Starting`, `next_restart_at = None` (a changed version already becomes a fresh `Starting` entry).
- SUT `apply`: `Transition::Stop(name) => { sut.stopped.insert(id(&name)); }`, `Transition::Resume(name) => { sut.stopped.remove(&id(&name)); }`, and `Up(map)` / `Update(..)` do `sut.stopped.retain(|a| !map.contains_key(a.agent.as_str()))` / `sut.stopped.clear()` respectively.
- The `expected_fleet` derivation is unchanged (`Stopped` is neither `Dead` nor `Starting`, so a fleet of `Ready` and `Stopped` agents is `Ready`, which is what `derive_fleet_phase` now says).
- The `again` SUT in `check_invariants` copies `stopped: sut.stopped.clone()`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-api -p balerix-core`
Expected: compile errors (`actions` missing on `Outcome`, `handle` is not async, `plan` arity, `stopped` unknown).

- [ ] **Step 3: Implement**

`crates/balerix-core/src/events.rs` (whole file above the tests):

```rust
//! The hook-event port (architecture spec §8; plugins spec §4.3, §16.1).
//! The handler is async because Spec B's chain calls plugins over HTTP;
//! it returns a boxed `std::future::Future` so `dyn EventHandler` works and
//! this crate needs no runtime.

use std::future::Future;
use std::pin::Pin;

use balerix_api::{HookEvent, PluginAction};
use serde_json::{Value, json};

/// What the daemon answers Claude with, and what it does afterwards.
/// `{}` with no actions means allow / no-op.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub response: Value,
    pub actions: Vec<PluginAction>,
}

impl Outcome {
    pub fn allow() -> Self {
        Self {
            response: json!({}),
            actions: Vec::new(),
        }
    }
}

pub type HandlerFuture<'a> = Pin<Box<dyn Future<Output = Outcome> + Send + 'a>>;

pub trait EventHandler: Send + Sync {
    fn handle<'a>(&'a self, event: &'a HookEvent) -> HandlerFuture<'a>;
}

/// Allow everything, do nothing: the zero-plugin behaviour and the test handler.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassThrough;

impl EventHandler for PassThrough {
    fn handle<'a>(&'a self, _: &'a HookEvent) -> HandlerFuture<'a> {
        Box::pin(async { Outcome::allow() })
    }
}
```

`crates/balerix-api/src/record.rs`: add to `FleetRecord` after `desired`:

```rust
    /// Agents held stopped by a plugin action (plugins spec §16.4): stopped
    /// if observed, never restarted, restart counter untouched. Keyed like
    /// `status.agents`. Cleared for every agent an `Apply` declares.
    #[serde(default)]
    pub stopped: BTreeSet<String>,
```

(`use std::collections::{BTreeMap, BTreeSet};`; `FleetRecord::new` sets `stopped: BTreeSet::new()`.)

`crates/balerix-core/src/reconcile/mod.rs`: add the parameter and the branch.

```rust
/// … (existing doc) …
/// A desired agent in `stopped` (plugins spec §16.4) gets `Stop` if it is
/// observed at all and nothing else: no restart, no `NoteExit`, even when
/// its hash changed. Leaving the set is an ordinary "absent → restart".
pub fn plan(
    fleet: &FleetName,
    desired: Option<&Fleet>,
    keep: Keep,
    stopped: &BTreeSet<AgentId>,
    status: &FleetStatus,
    observed: &ObservedState,
    _policy: &ReconcilePolicy,
    now: Timestamp,
) -> Plan {
```

and at the top of the per-agent loop:

```rust
    for (id, agent) in &desired_agents {
        if stopped.contains(id) {
            if observed.get(id).is_some() {
                stops.push(Step::Stop(id.clone()));
            }
            continue;
        }
```

Update the doc table with a first row: `| any | in \`stopped\` | \`Stop\` if observed, else — |`.

`crates/balerix-core/src/reconcile/execute.rs`: `pub stopped: &'a BTreeSet<AgentId>,` on `ReconcileContext` (after `keep`), `use std::collections::{BTreeMap, BTreeSet};`, and `reconcile_pass` passes `ctx.stopped` as the fourth argument to `plan`.

`crates/balerix-core/src/reconcile/status.rs`:

```rust
        (Step::Stop(id), Ok(())) => {
            if let Some(a) = status.agents.get_mut(&id.to_string()) {
                a.phase = AgentPhase::Stopped;
                a.next_restart_at = None;
            }
        }
```

and in `derive_fleet_phase` replace the `Ready` arm's condition with
`!status.agents.is_empty() && phases().all(|p| matches!(p, AgentPhase::Ready | AgentPhase::Stopped))`.

`crates/balerix-core/src/lib.rs`: `pub use events::{EventHandler, HandlerFuture, Outcome, PassThrough};`.

Compile fixes outside core, no behaviour change yet:
- `crates/balerix-server/src/actor.rs` `pass()`: before `spawn_blocking`, `let stopped: BTreeSet<AgentId> = self.record.stopped.iter().filter_map(|s| s.parse().ok()).collect();`, move it into the closure and set `stopped: &stopped,` on the `ReconcileContext` (add `BTreeSet` to the `std::collections` import).
- `crates/balerix-server/src/daemon.rs:320`: `let outcome = self.handler.handle(&hook_event).await;`.

- [ ] **Step 4: Run the tests, accept the two new snapshots, run mutants**

Run: `mise x -- cargo test -p balerix-core`
Expected: two `.snap.new` files under `crates/balerix-core/tests/snapshots/` with exactly the content listed in Step 1; every other test passes.

Run: `mise x -- cargo insta accept` (after reading both), then `mise x -- cargo test -p balerix-core` again, then `mise run check`.
Expected: green.

Run: `mise run mutants`
Expected: no surviving mutants in `reconcile`. If `stopped.contains(id)` → `true`/`false` or the `observed.get(id).is_some()` guard survives, the two new tests in `mod.rs` are not covering it — fix the test, not the code.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-api/src/record.rs crates/balerix-core crates/balerix-server/src/actor.rs crates/balerix-server/src/daemon.rs
git commit -m "Make the event handler async and add stopped agents to the planner

Spec B §16.1: the chain calls plugins over HTTP, so EventHandler returns
a boxed future; core stays runtime-free. §16.4: FleetRecord.stopped is a
daemon-owned set the planner honours — stopped if observed, never
restarted, restart counter untouched — which is what the stop and
restart actions are built on. Outcome regains actions (P3-6 reversed).

Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe"
```

---

### Task 3: `balerix-server` — `SetStopped`, plugin metrics, `reqwest`, `PluginClient`

**Files:**
- Create: `crates/balerix-server/src/plugins/client.rs`
- Modify: `Cargo.toml` (workspace deps), `crates/balerix-server/Cargo.toml`, `crates/balerix-server/src/actor.rs`, `crates/balerix-server/src/metrics.rs`, `crates/balerix-server/src/plugins/mod.rs`, `crates/balerix-server/src/lib.rs`

**Interfaces:**
- Consumes: Task 1's bodies; Task 2's `FleetRecord.stopped`.
- Produces:
  - `actor::Msg::SetStopped { agent: AgentId, stopped: bool, reply: oneshot::Sender<FleetRecord> }` — the actor edits `record.stopped`, persists, runs a pass, then replies. `Apply` removes every agent the new spec declares from `stopped`.
  - `Metrics::plugin_event(&self, plugin: &str, event: &str, mode: &str)`, `Metrics::intercept(&self, plugin: &str, event: &str, secs: f64, failure: Option<&str>)`, `Metrics::events_dropped(&self, plugin: &str, n: u64)`, `Metrics::plugin_action(&self, plugin: &str, action: &str)`, `Metrics::hook_action(&self, id: &AgentId, action: &str)`, `Metrics::scrape_failure(&self, plugin: &str)`. Families: `balerix_plugin_events_total{plugin,event,mode}`, `balerix_plugin_intercept_duration_seconds{plugin,event}`, `balerix_plugin_intercept_failures_total{plugin,reason}`, `balerix_plugin_events_dropped_total{plugin}`, `balerix_plugin_actions_total{plugin,action}`, `balerix_plugin_metrics_scrape_failures_total{plugin}`.
  - `plugins::client::{PluginClient, CallFailure}`: `PluginClient::new() -> Result<Self, PluginError>`; `async fn activate(&self, listen: &str, req: &ActivateRequest) -> Result<(), CallFailure>`; `async fn deactivate(&self, listen: &str, req: &DeactivateRequest) -> Result<(), CallFailure>`; `async fn events(&self, listen: &str, batch: &EventBatch) -> Result<(), CallFailure>`; `async fn intercept(&self, listen: &str, req: &InterceptRequest, timeout: Duration) -> Result<InterceptResponse, CallFailure>`; `async fn health(&self, listen: &str) -> Result<(), CallFailure>`; `async fn metrics(&self, listen: &str, timeout: Duration) -> Result<String, CallFailure>`. `CallFailure::{Timeout, Connect, Status { status: u16, message: String }, Body(String)}` with `fn reason(&self) -> &'static str` (`timeout` / `connect` / `status` / `body`) and `Display` = `<reason>: <detail>` (`Status` displays `HTTP <status>: <message>`). Default timeout 5 s (§4.2). `listen` is `host:port`; the client speaks `http://<listen><path>`.

- [ ] **Step 1: Add the dependency**

`Cargo.toml` `[workspace.dependencies]`, after `ureq`:

```toml
# Async loopback HTTP for the daemon → plugin calls and the plugin SDK
# (plugins spec §16.1). No TLS provider, no HTTP/2, no proxy discovery.
reqwest = { version = "0.13.4", default-features = false, features = ["json"] }
```

`crates/balerix-server/Cargo.toml` `[dependencies]`: `reqwest = { workspace = true }`.

- [ ] **Step 2: Write the failing tests**

`crates/balerix-server/src/actor.rs` tests:

```rust
    async fn set_stopped(h: &FleetHandle, agent: &str, stopped: bool) -> FleetRecord {
        let (tx, rx) = oneshot::channel();
        h.tx.send(Msg::SetStopped {
            agent: id(agent),
            stopped,
            reply: tx,
        })
        .await
        .unwrap();
        rx.await.unwrap()
    }

    #[tokio::test]
    async fn set_stopped_stops_resumes_and_apply_clears_it() {
        let h = Harness::new(Duration::from_secs(3600));
        let (handle, _shared, _purged) = start(&h);
        apply(&handle, spec(&["a", "b"])).await;
        let mut rx = handle.status.clone();
        wait(&mut rx, |r| r.status.observed_generation == 1).await;

        let rec = set_stopped(&handle, "f/c/a", true).await;
        assert_eq!(rec.stopped, BTreeSet::from(["f/c/a".to_string()]));
        assert_eq!(rec.status.agents["f/c/a"].phase, AgentPhase::Stopped);
        assert_eq!(rec.status.agents["f/c/b"].phase, AgentPhase::Starting);
        assert!(h.runner.calls().contains(&"stop_agent f/c/a".to_string()));
        assert_eq!(h.store.get("f").unwrap().0.stopped.len(), 1, "persisted");
        let ensure_a = || {
            h.runner
                .calls()
                .iter()
                .filter(|c| *c == "ensure_agent f/c/a")
                .count()
        };
        assert_eq!(ensure_a(), 1);

        let rec = set_stopped(&handle, "f/c/a", false).await;
        assert!(rec.stopped.is_empty());
        assert_eq!(rec.status.agents["f/c/a"].phase, AgentPhase::Starting);
        assert_eq!(rec.status.agents["f/c/a"].restarts, 0);
        assert_eq!(ensure_a(), 2, "resumed in the same message's pass");

        set_stopped(&handle, "f/c/b", true).await;
        let rec = apply(&handle, spec(&["a", "b"])).await;
        assert!(rec.stopped.is_empty(), "an Apply clears every declared agent");
        wait(&mut rx, |r| r.status.observed_generation == 2).await;
        assert!(
            h.runner
                .calls()
                .iter()
                .filter(|c| *c == "ensure_agent f/c/b")
                .count()
                >= 2
        );
    }
```

(add `use std::collections::BTreeSet;` to the test module.)

`crates/balerix-server/src/metrics.rs` test, appended to `gauges_follow_the_snapshots_and_counters_accumulate` before the `for name in [...]` loop:

```rust
        m.plugin_event("flow", "PreToolUse", "intercept");
        m.intercept("flow", "PreToolUse", 0.01, None);
        m.intercept("flow", "PreToolUse", 1.5, Some("timeout"));
        m.events_dropped("web", 3);
        m.plugin_action("flow", "send_text");
        m.hook_action(&id, "send_text");
        m.scrape_failure("web");
        let text = m.encode();
        assert!(text.contains(
            "balerix_plugin_events_total{event=\"PreToolUse\",mode=\"intercept\",plugin=\"flow\"} 1"
        ));
        assert!(text.contains(
            "balerix_plugin_intercept_duration_seconds_count{event=\"PreToolUse\",plugin=\"flow\"} 2"
        ));
        assert!(text.contains(
            "balerix_plugin_intercept_failures_total{plugin=\"flow\",reason=\"timeout\"} 1"
        ));
        assert!(text.contains("balerix_plugin_events_dropped_total{plugin=\"web\"} 3"));
        assert!(text.contains("balerix_plugin_actions_total{action=\"send_text\",plugin=\"flow\"} 1"));
        assert!(text.contains(
            "balerix_hook_actions_total{action=\"send_text\",agent=\"a\",crew=\"c\",fleet=\"f\"} 1"
        ));
        assert!(text.contains("balerix_plugin_metrics_scrape_failures_total{plugin=\"web\"} 1"));
```

and extend that loop's list with the six new family names.

`crates/balerix-server/src/plugins/client.rs` — write the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use balerix_api::{HookEvent, Timestamp};
    use serde_json::{Value, json};

    /// A plugin stub: activate rejects agent "bad", intercept echoes the
    /// event name (or returns a non-object for "PreCompact"), health is
    /// fine, metrics is one family, `/slow` never answers in time.
    async fn stub() -> String {
        let app = Router::new()
            .route(
                "/v1/activate",
                post(|Json(v): Json<Value>| async move {
                    if v["agent"] == "f/c/bad" {
                        (
                            axum::http::StatusCode::BAD_REQUEST,
                            Json(json!({ "error": "states.working: unknown event" })),
                        )
                    } else {
                        (axum::http::StatusCode::OK, Json(json!({})))
                    }
                }),
            )
            .route("/v1/deactivate", post(|| async { Json(json!({})) }))
            .route("/v1/events", post(|| async { Json(json!({})) }))
            .route(
                "/v1/intercept",
                post(|Json(v): Json<Value>| async move {
                    if v["event"]["name"] == "PreCompact" {
                        Json(json!({ "response": 7 }))
                    } else if v["event"]["name"] == "Stop" {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        Json(json!({ "response": {} }))
                    } else {
                        Json(json!({
                            "response": { "seen": v["event"]["name"], "so_far": v["response_so_far"] },
                            "actions": [{ "action": "stop" }]
                        }))
                    }
                }),
            )
            .route("/v1/health", get(|| async { "ok" }))
            .route(
                "/v1/metrics",
                get(|| async { "# TYPE balerix_plugin_x_up gauge\nbalerix_plugin_x_up 1\n" }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    fn event(name: &str) -> HookEvent {
        HookEvent {
            agent: "f/c/a".into(),
            name: name.into(),
            session_id: None,
            received_at: Timestamp(1),
            payload: json!({}),
        }
    }

    #[tokio::test]
    async fn calls_reach_the_plugin_and_classify_failures() {
        let listen = stub().await;
        let c = PluginClient::new().unwrap();
        c.activate(
            &listen,
            &ActivateRequest {
                agent: "f/c/a".into(),
                config: json!({}),
            },
        )
        .await
        .unwrap();
        let e = c
            .activate(
                &listen,
                &ActivateRequest {
                    agent: "f/c/bad".into(),
                    config: json!({}),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            e,
            CallFailure::Status {
                status: 400,
                message: "states.working: unknown event".into()
            }
        );
        assert_eq!(e.reason(), "status");
        assert_eq!(e.to_string(), "HTTP 400: states.working: unknown event");
        c.deactivate(
            &listen,
            &DeactivateRequest {
                agent: "f/c/a".into(),
            },
        )
        .await
        .unwrap();
        c.events(
            &listen,
            &EventBatch {
                events: vec![event("Notification")],
            },
        )
        .await
        .unwrap();

        let v = c
            .intercept(
                &listen,
                &InterceptRequest {
                    event: event("PreToolUse"),
                    response_so_far: json!({ "a": 1 }),
                    deadline_ms: 1000,
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(v.response["seen"], "PreToolUse");
        assert_eq!(v.response["so_far"]["a"], 1);
        assert_eq!(v.actions, vec![balerix_api::PluginAction::Stop]);
        let e = c
            .intercept(
                &listen,
                &InterceptRequest {
                    event: event("PreCompact"),
                    response_so_far: json!({}),
                    deadline_ms: 1000,
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert_eq!(e.reason(), "body");
        assert!(matches!(e, CallFailure::Body(_)), "{e}");
        let e = c
            .intercept(
                &listen,
                &InterceptRequest {
                    event: event("Stop"),
                    response_so_far: json!({}),
                    deadline_ms: 100,
                },
                Duration::from_millis(100),
            )
            .await
            .unwrap_err();
        assert_eq!(e, CallFailure::Timeout);
        let e = c
            .health("127.0.0.1:1")
            .await
            .unwrap_err();
        assert_eq!(e, CallFailure::Connect);
        assert_eq!(e.reason(), "connect");
        c.health(&listen).await.unwrap();
        let text = c.metrics(&listen, Duration::from_millis(500)).await.unwrap();
        assert!(text.starts_with("# TYPE balerix_plugin_x_up"));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-server`
Expected: compile errors — `Msg::SetStopped`, `plugin_event`, `PluginClient` not found.

- [ ] **Step 4: Implement**

`crates/balerix-server/src/actor.rs`:

```rust
    /// Hold (or release) one agent in the record's `stopped` set (plugins
    /// spec §16.4). Replied to after the pass, so `restart` (stop, then
    /// resume) is two ordered round trips.
    SetStopped {
        agent: AgentId,
        stopped: bool,
        reply: oneshot::Sender<FleetRecord>,
    },
```

in `run()`:

```rust
                Some(Msg::SetStopped {
                    agent,
                    stopped,
                    reply,
                }) => {
                    let key = agent.to_string();
                    if stopped {
                        self.record.stopped.insert(key);
                    } else {
                        self.record.stopped.remove(&key);
                    }
                    self.persist().await;
                    self.publish();
                    self.pass().await;
                    let _ = reply.send(self.record.clone());
                }
```

in `apply()`, right after `let wanted = self.wanted_agents();`:

```rust
        // an Apply always wins over a plugin's stop (plugins spec §16.4)
        self.record
            .stopped
            .retain(|k| !wanted.iter().any(|id| id.to_string() == *k));
```

`crates/balerix-server/src/metrics.rs`: six new fields on `Inner`, registered like the others; drop `#[allow(dead_code)]` on `hook_actions` and the placeholder `with_label_values(&["", "", "", ""])` line (the family stays in the output because the test's `# TYPE` loop is what dashboards need; if the encoder omits an empty family, keep the placeholder line — check the test). Methods:

```rust
    pub fn plugin_event(&self, plugin: &str, event: &str, mode: &str) {
        self.inner
            .plugin_events
            .with_label_values(&[plugin, event, mode])
            .inc();
    }

    /// One interceptor call; `failure` is the `reason` label when it failed.
    pub fn intercept(&self, plugin: &str, event: &str, secs: f64, failure: Option<&str>) {
        self.inner
            .plugin_intercept_duration
            .with_label_values(&[plugin, event])
            .observe(secs);
        if let Some(reason) = failure {
            self.inner
                .plugin_intercept_failures
                .with_label_values(&[plugin, reason])
                .inc();
        }
    }

    pub fn events_dropped(&self, plugin: &str, n: u64) {
        self.inner
            .plugin_events_dropped
            .with_label_values(&[plugin])
            .inc_by(n);
    }

    pub fn plugin_action(&self, plugin: &str, action: &str) {
        self.inner
            .plugin_actions
            .with_label_values(&[plugin, action])
            .inc();
    }

    pub fn hook_action(&self, id: &AgentId, action: &str) {
        self.inner
            .hook_actions
            .with_label_values(&[id.fleet.as_str(), id.crew.as_str(), id.agent.as_str(), action])
            .inc();
    }

    pub fn scrape_failure(&self, plugin: &str) {
        self.inner
            .plugin_metrics_scrape_failures
            .with_label_values(&[plugin])
            .inc();
    }
```

Opts: `("balerix_plugin_events_total", "Hook events handed to plugins", ["plugin","event","mode"])`, `HistogramOpts("balerix_plugin_intercept_duration_seconds", "Interceptor call latency", ["plugin","event"])`, `("balerix_plugin_intercept_failures_total", "Interceptor calls skipped", ["plugin","reason"])`, `("balerix_plugin_events_dropped_total", "Observer events dropped on overflow", ["plugin"])`, `("balerix_plugin_actions_total", "Actions requested by plugins", ["plugin","action"])`, `("balerix_plugin_metrics_scrape_failures_total", "Plugin /v1/metrics bodies dropped", ["plugin"])`.

`crates/balerix-server/src/plugins/client.rs`:

```rust
//! Every daemon → plugin call (plugins spec §4.2) on one `reqwest` client:
//! loopback, plain HTTP/1.1, no proxy, 5 s unless the caller says otherwise.
//! Failures are classified into the four `reason` labels of §4.3.

use std::fmt;
use std::time::Duration;

use balerix_api::{
    ActivateRequest, DeactivateRequest, ErrorBody, EventBatch, InterceptRequest,
    InterceptResponse,
};
use serde::Serialize;
use serde_json::Value;

use super::PluginError;

/// Default per-call timeout (§4.2 "5 s unless stated").
pub const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Plugin response bodies are capped like every other body.
const MAX_BODY: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallFailure {
    Timeout,
    Connect,
    Status { status: u16, message: String },
    Body(String),
}

impl CallFailure {
    /// The metrics `reason` label.
    pub fn reason(&self) -> &'static str {
        match self {
            CallFailure::Timeout => "timeout",
            CallFailure::Connect => "connect",
            CallFailure::Status { .. } => "status",
            CallFailure::Body(_) => "body",
        }
    }
}

impl fmt::Display for CallFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallFailure::Timeout => f.write_str("timeout"),
            CallFailure::Connect => f.write_str("connection refused"),
            CallFailure::Status { status, message } => write!(f, "HTTP {status}: {message}"),
            CallFailure::Body(m) => write!(f, "bad response body: {m}"),
        }
    }
}

impl From<reqwest::Error> for CallFailure {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            CallFailure::Timeout
        } else if e.is_connect() || e.is_request() {
            CallFailure::Connect
        } else {
            CallFailure::Body(e.to_string())
        }
    }
}

#[derive(Clone)]
pub struct PluginClient {
    http: reqwest::Client,
}

impl fmt::Debug for PluginClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PluginClient")
    }
}

impl PluginClient {
    pub fn new() -> Result<Self, PluginError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(CALL_TIMEOUT)
            .build()
            .map_err(|e| PluginError::Internal(format!("http client: {e}")))?;
        Ok(Self { http })
    }

    fn url(listen: &str, path: &str) -> String {
        format!("http://{listen}{path}")
    }

    /// Reads at most `MAX_BODY` bytes; a non-2xx becomes `Status` with
    /// the `{ "error" }` message or the raw text.
    async fn body(resp: reqwest::Response) -> Result<Vec<u8>, CallFailure> {
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await?;
        if bytes.len() > MAX_BODY {
            return Err(CallFailure::Body(format!("{} bytes exceeds 1 MiB", bytes.len())));
        }
        if !(200..300).contains(&status) {
            let text = String::from_utf8_lossy(&bytes).trim().to_string();
            let message = serde_json::from_slice::<ErrorBody>(&bytes)
                .map(|e| e.error)
                .unwrap_or(text);
            return Err(CallFailure::Status { status, message });
        }
        Ok(bytes.to_vec())
    }

    async fn post<T: Serialize + ?Sized>(
        &self,
        listen: &str,
        path: &str,
        body: &T,
        timeout: Duration,
    ) -> Result<Vec<u8>, CallFailure> {
        let resp = self
            .http
            .post(Self::url(listen, path))
            .timeout(timeout)
            .json(body)
            .send()
            .await?;
        Self::body(resp).await
    }

    pub async fn activate(&self, listen: &str, req: &ActivateRequest) -> Result<(), CallFailure> {
        self.post(listen, "/v1/activate", req, CALL_TIMEOUT)
            .await
            .map(|_| ())
    }

    pub async fn deactivate(
        &self,
        listen: &str,
        req: &DeactivateRequest,
    ) -> Result<(), CallFailure> {
        self.post(listen, "/v1/deactivate", req, CALL_TIMEOUT)
            .await
            .map(|_| ())
    }

    pub async fn events(&self, listen: &str, batch: &EventBatch) -> Result<(), CallFailure> {
        self.post(listen, "/v1/events", batch, CALL_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// `timeout` is what remains of the chain's budget. A `response` that
    /// is not a JSON object is a `Body` failure (§4.3).
    pub async fn intercept(
        &self,
        listen: &str,
        req: &InterceptRequest,
        timeout: Duration,
    ) -> Result<InterceptResponse, CallFailure> {
        let bytes = self.post(listen, "/v1/intercept", req, timeout).await?;
        let verdict: InterceptResponse =
            serde_json::from_slice(&bytes).map_err(|e| CallFailure::Body(e.to_string()))?;
        if !matches!(verdict.response, Value::Object(_)) {
            return Err(CallFailure::Body("response is not a JSON object".into()));
        }
        Ok(verdict)
    }

    pub async fn health(&self, listen: &str) -> Result<(), CallFailure> {
        let resp = self
            .http
            .get(Self::url(listen, "/v1/health"))
            .send()
            .await?;
        Self::body(resp).await.map(|_| ())
    }

    pub async fn metrics(&self, listen: &str, timeout: Duration) -> Result<String, CallFailure> {
        let resp = self
            .http
            .get(Self::url(listen, "/v1/metrics"))
            .timeout(timeout)
            .send()
            .await?;
        let bytes = Self::body(resp).await?;
        String::from_utf8(bytes).map_err(|e| CallFailure::Body(e.to_string()))
    }
}
```

`crates/balerix-server/src/plugins/mod.rs`: `pub mod client;` and `pub use client::{CallFailure, PluginClient};`. `crates/balerix-server/src/lib.rs`: add `PluginClient` to the `plugins` re-export.

- [ ] **Step 5: Run the tests**

Run: `mise x -- cargo test -p balerix-server && mise run check`
Expected: green, including the new actor, metrics and client tests. If `is_request()` does not exist on this `reqwest` (check `mise x -- cargo doc -p reqwest --no-deps` or the docs.rs page for 0.13.4), classify with `is_connect()` alone and map the rest to `Connect` when `e.status().is_none()`; the client test's `127.0.0.1:1` case pins the behaviour.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/balerix-server
git commit -m "Add SetStopped, the plugin metric families and the reqwest plugin client

reqwest 0.13.4 (default-features = false, json): the daemon calls plugins
on every hook event under a deadline, so the client is async end to end
(plugins spec §16.1); no TLS provider, no proxy discovery, loopback only.
The actor's SetStopped is what stop and restart actions use (§16.4).

Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe"
```

---

### Task 4: `balerix-server` — `PluginRegistry`, the activation diff, `PluginKv`

**Files:**
- Create: `crates/balerix-server/src/plugins/registry.rs`, `crates/balerix-server/src/plugins/activation.rs`, `crates/balerix-server/src/plugins/kv.rs`
- Modify: `crates/balerix-server/src/plugins/mod.rs` (modules, `PluginError` variants), `crates/balerix-server/src/api.rs:74-88` (`ApiError` mapping), `crates/balerix-server/src/lib.rs`

**Interfaces:**
- Consumes: `ResolvedPlugin`, `PluginManifest`, `Capability`, `PluginActivation`, `Vault`, `fsutil::write_private`.
- Produces:
  - `PluginError::Activation { path: String, message: String }` (`{path}: {message}`, 400); `PluginError::Capability(Capability)` (`capability "kv" not declared in balerix-plugin.yaml`, 403); `PluginError::KvKey(String)` (`kv: invalid key: {0}`, 400); `PluginError::NotActive(String)` (`plugin is not active for agent {0}`, 404); `PluginError::Kv { path: PathBuf, message: String }` (`{path}: {message}`, 500).
  - `plugins::registry::{PluginRegistry, PluginInfo, ActivationRow}`:
    `PluginRegistry::new() -> Arc<Self>`; `replace_plugins(&self, plugins: &[ResolvedPlugin], keep_listen: &[String])`; `set_listen(&self, name: &AgentName, listen: String)` (also `ready = true`, clears `degraded`); `set_ready(&self, name: &AgentName, ready: bool)`; `set_degraded(&self, name: &AgentName, reason: Option<String>)`; `plugin(&self, name: &AgentName) -> Option<PluginInfo>`; `names(&self) -> Vec<AgentName>` (load-list order); `is_installed(&self, name: &str) -> bool`; `has(&self, name: &AgentName, cap: Capability) -> bool`; `ready_listen(&self, name: &AgentName) -> Option<String>`; `set_row(&self, agent: &AgentId, plugin: &AgentName, row: ActivationRow)`; `set_state(&self, agent: &AgentId, plugin: &AgentName, activation: PluginActivation) -> bool`; `remove_row(&self, agent: &AgentId, plugin: &AgentName) -> Option<ActivationRow>`; `remove_fleet(&self, fleet: &FleetName) -> Vec<(AgentId, AgentName)>`; `row(&self, agent: &AgentId, plugin: &AgentName) -> Option<ActivationRow>`; `rows_for_plugin(&self, name: &AgentName) -> Vec<(AgentId, ActivationRow)>`; `is_active(&self, agent: &AgentId, plugin: &AgentName) -> bool`; `active_agents(&self, name: &AgentName) -> u32`; `interceptors(&self, agent: &AgentId, event: &str) -> Vec<(AgentName, String)>` (load order; ready, intercepting `event`, active for `agent`; the `String` is `listen`); `observers(&self, agent: &AgentId, event: &str) -> Vec<(AgentName, String)>`; `overlay(&self, record: &mut FleetRecord)`.
    `PluginInfo { pub manifest: PluginManifest, pub listen: Option<String>, pub ready: bool, pub degraded: Option<String> }`; `ActivationRow { pub config: Value, pub activation: PluginActivation }`.
  - `plugins::activation::{Pair, ActivationDiff, pairs, diff, config_path}`: `Pair { pub agent: AgentId, pub plugin: AgentName, pub config: Value }`; `pairs(fleet: &FleetName, spec: &FleetSpec) -> Result<Vec<Pair>, PluginError>` (sorted by agent then plugin; an unparsable plugin name is `PluginError::Activation`); `ActivationDiff { pub activate: Vec<Pair>, pub deactivate: Vec<(AgentId, AgentName)> }`; `diff(old: &[Pair], new: &[Pair]) -> ActivationDiff` (a changed config appears in both lists); `config_path(agent: &AgentId, plugin: &AgentName) -> String` = `crews.<crew>.agents.<agent>.plugins.<plugin>`.
  - `plugins::kv::{PluginKv, validate_key}`: `PluginKv::new(state_root: PathBuf, vault: Vault) -> Self` (`state_root` is `$XDG_STATE_HOME/balerix/plugins`, so a plugin's store is `<state_root>/<name>/kv/`, the same directory `balerix_runtime::StateLayout::plugin(name).kv` names); `get(&self, name: &AgentName, key: &str) -> Result<Option<Vec<u8>>, PluginError>`; `put(&self, name: &AgentName, key: &str, bytes: &[u8], secret: bool) -> Result<(), PluginError>`; `delete(&self, name: &AgentName, key: &str) -> Result<bool, PluginError>`; `list(&self, name: &AgentName, prefix: &str) -> Result<Vec<String>, PluginError>` (sorted); `validate_key(key: &str) -> Result<(), String>`. All blocking; callers use `spawn_blocking`.

- [ ] **Step 1: Write the failing tests**

`registry.rs` test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{ActivationState, FleetSpec};
    use balerix_core::FleetRecord;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn plugin(name: &str, intercept: &[&str], observe: &[&str], needs: &[&str]) -> ResolvedPlugin {
        ResolvedPlugin {
            name: name.parse().unwrap(),
            package: format!("/pkg/{name}").into(),
            manifest: serde_json::from_value(json!({
                "apiVersion": "balerix/v1", "kind": "Plugin", "name": name,
                "version": "0.1.0", "protocol": 1, "start": "serve",
                "hooks": { "intercept": intercept, "observe": observe }, "needs": needs
            }))
            .unwrap(),
            config: json!({}),
            digest: None,
        }
    }

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }
    fn name(s: &str) -> AgentName {
        s.parse().unwrap()
    }

    #[test]
    fn plugins_keep_load_order_readiness_and_capabilities() {
        let r = PluginRegistry::new();
        r.replace_plugins(
            &[
                plugin("web", &[], &["SessionStart"], &["fleets"]),
                plugin("flow", &["PreToolUse", "Stop"], &[], &["actions", "kv"]),
            ],
            &[],
        );
        assert_eq!(
            r.names().iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["web", "flow"]
        );
        assert!(r.is_installed("flow") && !r.is_installed("nope"));
        assert!(r.has(&name("flow"), Capability::Kv));
        assert!(!r.has(&name("flow"), Capability::Fleets));
        assert!(!r.has(&name("nope"), Capability::Fleets));
        assert_eq!(r.ready_listen(&name("flow")), None);
        r.set_listen(&name("flow"), "127.0.0.1:4000".into());
        assert_eq!(r.ready_listen(&name("flow")).as_deref(), Some("127.0.0.1:4000"));
        r.set_degraded(&name("flow"), Some("HTTP 500".into()));
        assert_eq!(r.plugin(&name("flow")).unwrap().degraded.as_deref(), Some("HTTP 500"));
        r.set_ready(&name("flow"), false);
        assert_eq!(r.ready_listen(&name("flow")), None, "not ready: no listen");
        assert_eq!(
            r.plugin(&name("flow")).unwrap().listen.as_deref(),
            Some("127.0.0.1:4000"),
            "the address itself is kept"
        );
        r.set_listen(&name("flow"), "127.0.0.1:4001".into());
        let p = r.plugin(&name("flow")).unwrap();
        assert!(p.ready && p.degraded.is_none(), "hello clears degraded");
        // a re-sync keeps listen only for the names the caller says
        r.replace_plugins(
            &[plugin("flow", &["PreToolUse"], &[], &[]), plugin("x", &[], &[], &[])],
            &["flow".into()],
        );
        assert_eq!(r.ready_listen(&name("flow")).as_deref(), Some("127.0.0.1:4001"));
        assert!(r.plugin(&name("web")).is_none());
        r.replace_plugins(&[plugin("flow", &[], &[], &[])], &[]);
        assert_eq!(r.ready_listen(&name("flow")), None, "changed plugin: forgotten");
    }

    #[test]
    fn rows_drive_interceptors_observers_counts_and_the_overlay() {
        let r = PluginRegistry::new();
        r.replace_plugins(
            &[
                plugin("flow", &["PreToolUse", "Stop"], &["Stop"], &[]),
                plugin("web", &["PreToolUse"], &["SessionStart", "PreToolUse"], &[]),
            ],
            &[],
        );
        r.set_listen(&name("flow"), "127.0.0.1:1".into());
        r.set_listen(&name("web"), "127.0.0.1:2".into());
        let a = id("f/c/a");
        let b = id("f/c/b");
        let row = |state: ActivationState| ActivationRow {
            config: json!({ "k": 1 }),
            activation: PluginActivation {
                state,
                message: String::new(),
            },
        };
        r.set_row(&a, &name("flow"), row(ActivationState::Active));
        r.set_row(&a, &name("web"), row(ActivationState::Pending));
        r.set_row(&b, &name("web"), row(ActivationState::Active));
        assert_eq!(
            r.interceptors(&a, "PreToolUse"),
            vec![(name("flow"), "127.0.0.1:1".to_string())],
            "web is pending for a"
        );
        assert_eq!(r.interceptors(&a, "Stop").len(), 1);
        assert_eq!(r.interceptors(&a, "Notification").len(), 0);
        assert_eq!(
            r.interceptors(&b, "PreToolUse"),
            vec![(name("web"), "127.0.0.1:2".to_string())]
        );
        assert_eq!(r.observers(&b, "SessionStart").len(), 1);
        assert_eq!(r.observers(&a, "Stop"), vec![(name("flow"), "127.0.0.1:1".to_string())]);
        r.set_ready(&name("flow"), false);
        assert!(r.interceptors(&a, "PreToolUse").is_empty(), "not ready: skipped");
        r.set_ready(&name("flow"), true);
        assert!(r.is_active(&a, &name("flow")));
        assert!(!r.is_active(&a, &name("web")));
        assert_eq!((r.active_agents(&name("flow")), r.active_agents(&name("web"))), (1, 1));
        assert!(r.set_state(&a, &name("web"), PluginActivation::active()));
        assert!(!r.set_state(&id("f/c/z"), &name("web"), PluginActivation::active()));
        assert_eq!(r.active_agents(&name("web")), 2);
        assert_eq!(r.rows_for_plugin(&name("web")).len(), 2);
        assert_eq!(r.row(&a, &name("flow")).unwrap().config["k"], 1);

        let mut record = FleetRecord::new(FleetSpec {
            name: "f".into(),
            crews: BTreeMap::new(),
        });
        record.status.entry("f/c/a");
        record.status.entry("f/c/b");
        r.overlay(&mut record);
        assert_eq!(record.status.agents["f/c/a"].plugins.len(), 2);
        assert_eq!(
            record.status.agents["f/c/a"].plugins["flow"].state,
            ActivationState::Active
        );
        assert_eq!(record.status.agents["f/c/b"].plugins.len(), 1);

        assert_eq!(r.remove_row(&a, &name("web")).unwrap().config["k"], 1);
        assert!(r.remove_row(&a, &name("web")).is_none());
        let removed = r.remove_fleet(&"f".parse().unwrap());
        assert_eq!(removed.len(), 2);
        assert!(r.rows_for_plugin(&name("flow")).is_empty());
        assert_eq!(r.active_agents(&name("web")), 0);
    }
}
```

`activation.rs` test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentSettings, CrewSpec, GitSettings};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn spec(agents: &[(&str, &[(&str, serde_json::Value)])]) -> FleetSpec {
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
                        .map(|(n, plugins)| {
                            let mut s = AgentSettings::default();
                            s.plugins = plugins
                                .iter()
                                .map(|(p, c)| (p.to_string(), c.clone()))
                                .collect();
                            (n.to_string(), s)
                        })
                        .collect(),
                },
            )]),
        }
    }

    fn render(pairs: &[Pair]) -> Vec<String> {
        pairs
            .iter()
            .map(|p| format!("{} {} {}", p.agent, p.plugin, p.config))
            .collect()
    }

    #[test]
    fn pairs_are_sorted_and_named_by_config_path() {
        let fleet: FleetName = "f".parse().unwrap();
        let s = spec(&[
            ("b", &[("web", json!({ "enabled": true }))]),
            ("a", &[("web", json!({})), ("flow", json!({ "initial": "x" }))]),
        ]);
        assert_eq!(
            render(&pairs(&fleet, &s).unwrap()),
            vec![
                "f/c/a flow {\"initial\":\"x\"}",
                "f/c/a web {}",
                "f/c/b web {\"enabled\":true}"
            ]
        );
        let bad = spec(&[("a", &[("Bad Name", json!({}))])]);
        assert_eq!(
            pairs(&fleet, &bad).unwrap_err().to_string(),
            "crews.c.agents.a.plugins.Bad Name: invalid plugin name: contains characters other than a-z, 0-9 and '-'"
        );
        assert_eq!(
            config_path(&"f/c/a".parse().unwrap(), &"flow".parse().unwrap()),
            "crews.c.agents.a.plugins.flow"
        );
    }

    #[test]
    fn diff_activates_new_and_changed_and_deactivates_dropped_and_changed() {
        let fleet: FleetName = "f".parse().unwrap();
        let old = pairs(
            &fleet,
            &spec(&[
                ("a", &[("flow", json!({ "v": 1 })), ("web", json!({}))]),
                ("b", &[("web", json!({}))]),
            ]),
        )
        .unwrap();
        let new = pairs(
            &fleet,
            &spec(&[
                ("a", &[("flow", json!({ "v": 2 })), ("web", json!({}))]),
                ("c", &[("web", json!({}))]),
            ]),
        )
        .unwrap();
        let d = diff(&old, &new);
        assert_eq!(
            render(&d.activate),
            vec!["f/c/a flow {\"v\":2}", "f/c/c web {}"]
        );
        assert_eq!(
            d.deactivate
                .iter()
                .map(|(a, p)| format!("{a} {p}"))
                .collect::<Vec<_>>(),
            vec!["f/c/a flow", "f/c/b web"]
        );
        let none = diff(&new, &new);
        assert!(none.activate.is_empty() && none.deactivate.is_empty());
        let fresh = diff(&[], &new);
        assert_eq!(fresh.activate.len(), 3);
        assert!(fresh.deactivate.is_empty());
    }
}
```

`kv.rs` test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::Vault;
    use proptest::prelude::*;
    use std::os::unix::fs::PermissionsExt;

    fn kv(dir: &std::path::Path, key: u8) -> PluginKv {
        PluginKv::new(dir.join("plugins"), Vault::from_key([key; 32]))
    }
    fn flow() -> AgentName {
        "flow".parse().unwrap()
    }

    #[test]
    fn keys_are_validated_before_any_path_is_built() {
        for ok in ["a", "state/f/c/a", "x.y-z_1", "A/B", &"k".repeat(200)] {
            assert_eq!(validate_key(ok), Ok(()), "{ok}");
        }
        for bad in ["", "/a", "a/", "a//b", "..", "a/../b", "a b", "ä", &"k".repeat(201)] {
            assert!(validate_key(bad).is_err(), "{bad:?} accepted");
        }
        let dir = tempfile::tempdir().unwrap();
        let e = kv(dir.path(), 1).get(&flow(), "../x").unwrap_err();
        assert!(e.to_string().starts_with("kv: invalid key:"), "{e}");
        assert!(!dir.path().join("plugins").exists(), "nothing touched");
    }

    #[test]
    fn plain_and_secret_entries_round_trip_and_secrets_are_sealed_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = kv(dir.path(), 1);
        assert_eq!(store.get(&flow(), "state/f/c/a").unwrap(), None);
        store.put(&flow(), "state/f/c/a", b"working", false).unwrap();
        store.put(&flow(), "token", b"hunter2-SECRET", true).unwrap();
        assert_eq!(store.get(&flow(), "state/f/c/a").unwrap().as_deref(), Some(&b"working"[..]));
        assert_eq!(store.get(&flow(), "token").unwrap().as_deref(), Some(&b"hunter2-SECRET"[..]));
        let file = dir.path().join("plugins/flow/kv/token");
        let raw = std::fs::read(&file).unwrap();
        assert!(!raw.windows(6).any(|w| w == b"SECRET"), "sealed");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        // another vault cannot open it; another plugin's key is another aad
        let e = kv(dir.path(), 2).get(&flow(), "token").unwrap_err();
        assert!(e.to_string().contains("ciphertext rejected"), "{e}");
        assert_eq!(
            store.list(&flow(), "").unwrap(),
            vec!["state/f/c/a".to_string(), "token".to_string()]
        );
        assert_eq!(store.list(&flow(), "state/").unwrap(), vec!["state/f/c/a".to_string()]);
        assert!(store.list(&"web".parse().unwrap(), "").unwrap().is_empty());
        store.put(&flow(), "state/f/c/a", b"review", false).unwrap();
        assert_eq!(store.get(&flow(), "state/f/c/a").unwrap().as_deref(), Some(&b"review"[..]));
        assert!(store.delete(&flow(), "token").unwrap());
        assert!(!store.delete(&flow(), "token").unwrap());
        assert_eq!(store.get(&flow(), "token").unwrap(), None);
        assert_eq!(
            std::fs::read_dir(dir.path().join("plugins/flow/kv")).unwrap().count(),
            1,
            "no temp files left"
        );
    }

    proptest! {
        #[test]
        fn any_valid_key_and_bytes_round_trip(
            key in "[A-Za-z0-9._-]{1,12}(/[A-Za-z0-9._-]{1,12}){0,3}",
            bytes in proptest::collection::vec(any::<u8>(), 0..2048),
            secret in any::<bool>(),
        ) {
            prop_assume!(validate_key(&key).is_ok());
            let dir = tempfile::tempdir().unwrap();
            let store = kv(dir.path(), 3);
            store.put(&flow(), &key, &bytes, secret).unwrap();
            prop_assert_eq!(store.get(&flow(), &key).unwrap(), Some(bytes));
            prop_assert_eq!(store.list(&flow(), "").unwrap(), vec![key]);
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-server registry activation kv`
Expected: compile errors, the modules do not exist.

- [ ] **Step 3: Implement**

`crates/balerix-server/src/plugins/mod.rs`: add `pub mod activation; pub mod kv; pub mod registry;`, re-export `pub use kv::{PluginKv, validate_key}; pub use registry::{ActivationRow, PluginInfo, PluginRegistry};` and the variants:

```rust
    /// An agent's `plugins.<name>` could not be activated; `path` is the
    /// config path (`crews.c.agents.a.plugins.flow`).
    #[error("{path}: {message}")]
    Activation { path: String, message: String },
    #[error("capability {0:?} not declared in balerix-plugin.yaml")]
    Capability(String),
    #[error("kv: invalid key: {0}")]
    KvKey(String),
    #[error("plugin is not active for agent {0}")]
    NotActive(String),
    #[error("{path}: {message}")]
    Kv { path: PathBuf, message: String },
```

(`Capability` carries the lowercase wire label: `Capability::Kv` → `"kv"`; build it with the `label` helper from `metrics.rs` moved into `plugins/mod.rs` as `pub(crate) fn wire_label<T: serde::Serialize>(v: T) -> String`, and have `metrics.rs` use that one.) `api.rs`: `Activation | KvKey` → 400, `Capability` → 403, `NotActive` → 404, `Kv` → 500.

`registry.rs`:

```rust
//! The daemon's view of the installed plugins (plugins spec §4, §16.2,
//! §16.3): load-list order, where each one listens, what it subscribes to
//! and may call, and the `(agent, plugin) → activation` table. The single
//! writer of activation state; the fleet actor never sees it.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use balerix_api::{ActivationState, Capability, PluginActivation, PluginManifest};
use balerix_core::{AgentId, AgentName, FleetName, FleetRecord, ResolvedPlugin};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct PluginInfo {
    pub manifest: PluginManifest,
    /// From the last `hello`; kept while the plugin restarts.
    pub listen: Option<String>,
    /// `Ready` per the plugin fleet's record. Only ready plugins are called.
    pub ready: bool,
    /// The health poller's verdict; `hello` clears it.
    pub degraded: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActivationRow {
    pub config: Value,
    pub activation: PluginActivation,
}

#[derive(Default)]
struct Inner {
    order: Vec<AgentName>,
    plugins: BTreeMap<AgentName, PluginInfo>,
    rows: BTreeMap<(AgentId, AgentName), ActivationRow>,
}

#[derive(Default)]
pub struct PluginRegistry {
    inner: RwLock<Inner>,
}

impl PluginRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// The synced set. `keep_listen` names the plugins whose package,
    /// manifest and config did not change; every other name starts over
    /// with no listen address and not ready.
    pub fn replace_plugins(&self, plugins: &[ResolvedPlugin], keep_listen: &[String]) {
        let mut w = self.write();
        let old = std::mem::take(&mut w.plugins);
        w.order = plugins.iter().map(|p| p.name.clone()).collect();
        for p in plugins {
            let kept = keep_listen.iter().any(|k| k == p.name.as_str());
            let prev = old.get(&p.name).filter(|_| kept);
            w.plugins.insert(
                p.name.clone(),
                PluginInfo {
                    manifest: p.manifest.clone(),
                    listen: prev.and_then(|i| i.listen.clone()),
                    ready: prev.is_some_and(|i| i.ready),
                    degraded: prev.and_then(|i| i.degraded.clone()),
                },
            );
        }
        let names: Vec<AgentName> = w.plugins.keys().cloned().collect();
        w.rows.retain(|(_, p), _| names.contains(p));
    }

    pub fn set_listen(&self, name: &AgentName, listen: String) {
        if let Some(p) = self.write().plugins.get_mut(name) {
            p.listen = Some(listen);
            p.ready = true;
            p.degraded = None;
        }
    }

    pub fn set_ready(&self, name: &AgentName, ready: bool) {
        if let Some(p) = self.write().plugins.get_mut(name) {
            p.ready = ready;
        }
    }

    pub fn set_degraded(&self, name: &AgentName, reason: Option<String>) {
        if let Some(p) = self.write().plugins.get_mut(name) {
            p.degraded = reason;
        }
    }

    pub fn plugin(&self, name: &AgentName) -> Option<PluginInfo> {
        self.read().plugins.get(name).cloned()
    }

    /// Load-list order (`plugins.yaml` order): the interceptor order.
    pub fn names(&self) -> Vec<AgentName> {
        self.read().order.clone()
    }

    pub fn is_installed(&self, name: &str) -> bool {
        self.read().plugins.keys().any(|n| n.as_str() == name)
    }

    pub fn has(&self, name: &AgentName, cap: Capability) -> bool {
        self.read()
            .plugins
            .get(name)
            .is_some_and(|p| p.manifest.needs.contains(&cap))
    }

    /// `Some(listen)` only while the plugin is ready.
    pub fn ready_listen(&self, name: &AgentName) -> Option<String> {
        let r = self.read();
        let p = r.plugins.get(name)?;
        p.ready.then(|| p.listen.clone()).flatten()
    }

    pub fn set_row(&self, agent: &AgentId, plugin: &AgentName, row: ActivationRow) {
        self.write()
            .rows
            .insert((agent.clone(), plugin.clone()), row);
    }

    /// `false` when there is no such row.
    pub fn set_state(&self, agent: &AgentId, plugin: &AgentName, activation: PluginActivation) -> bool {
        match self
            .write()
            .rows
            .get_mut(&(agent.clone(), plugin.clone()))
        {
            Some(row) => {
                row.activation = activation;
                true
            }
            None => false,
        }
    }

    pub fn remove_row(&self, agent: &AgentId, plugin: &AgentName) -> Option<ActivationRow> {
        self.write().rows.remove(&(agent.clone(), plugin.clone()))
    }

    /// Every pair of the fleet, removed and returned (for `deactivate`).
    pub fn remove_fleet(&self, fleet: &FleetName) -> Vec<(AgentId, AgentName)> {
        let mut w = self.write();
        let gone: Vec<(AgentId, AgentName)> = w
            .rows
            .keys()
            .filter(|(a, _)| &a.fleet == fleet)
            .cloned()
            .collect();
        for k in &gone {
            w.rows.remove(k);
        }
        gone
    }

    pub fn row(&self, agent: &AgentId, plugin: &AgentName) -> Option<ActivationRow> {
        self.read()
            .rows
            .get(&(agent.clone(), plugin.clone()))
            .cloned()
    }

    pub fn rows_for_plugin(&self, name: &AgentName) -> Vec<(AgentId, ActivationRow)> {
        self.read()
            .rows
            .iter()
            .filter(|((_, p), _)| p == name)
            .map(|((a, _), row)| (a.clone(), row.clone()))
            .collect()
    }

    pub fn is_active(&self, agent: &AgentId, plugin: &AgentName) -> bool {
        self.read()
            .rows
            .get(&(agent.clone(), plugin.clone()))
            .is_some_and(|r| r.activation.state == ActivationState::Active)
    }

    pub fn active_agents(&self, name: &AgentName) -> u32 {
        u32::try_from(
            self.read()
                .rows
                .iter()
                .filter(|((_, p), r)| p == name && r.activation.state == ActivationState::Active)
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    fn subscribed(
        &self,
        agent: &AgentId,
        event: &str,
        pick: impl Fn(&PluginManifest) -> bool,
    ) -> Vec<(AgentName, String)> {
        let r = self.read();
        r.order
            .iter()
            .filter_map(|name| {
                let p = r.plugins.get(name)?;
                let listen = p.listen.clone().filter(|_| p.ready)?;
                let active = r
                    .rows
                    .get(&(agent.clone(), name.clone()))
                    .is_some_and(|row| row.activation.state == ActivationState::Active);
                (active && pick(&p.manifest)).then(|| (name.clone(), listen))
            })
            .collect()
    }

    /// Ready plugins intercepting `event` and active for `agent`, in
    /// load-list order, with their listen addresses.
    pub fn interceptors(&self, agent: &AgentId, event: &str) -> Vec<(AgentName, String)> {
        self.subscribed(agent, event, |m| m.hooks.intercept.contains(event))
    }

    pub fn observers(&self, agent: &AgentId, event: &str) -> Vec<(AgentName, String)> {
        self.subscribed(agent, event, |m| m.hooks.observe.contains(event))
    }

    /// Copies the fleet's rows into `status.agents[*].plugins` (§16.3);
    /// agents the record does not know yet are skipped.
    pub fn overlay(&self, record: &mut FleetRecord) {
        let r = self.read();
        for (id, status) in &mut record.status.agents {
            status.plugins = r
                .rows
                .iter()
                .filter(|((a, _), _)| a.to_string() == *id)
                .map(|((_, p), row)| (p.to_string(), row.activation.clone()))
                .collect();
        }
    }
}
```

`activation.rs`:

```rust
//! Which `(agent, plugin)` pairs a spec wants, and what changes between
//! two specs (plugins spec §16.2). Pure.

use balerix_api::FleetSpec;
use balerix_core::{AgentId, AgentName, FleetName};
use serde_json::Value;

use super::PluginError;

#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    pub agent: AgentId,
    pub plugin: AgentName,
    pub config: Value,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ActivationDiff {
    /// New pairs, and pairs whose config changed.
    pub activate: Vec<Pair>,
    /// Dropped pairs, and pairs whose config changed (before their activate).
    pub deactivate: Vec<(AgentId, AgentName)>,
}

/// `crews.<crew>.agents.<agent>.plugins.<plugin>`: the config path every
/// activation error starts with.
pub fn config_path(agent: &AgentId, plugin: &AgentName) -> String {
    format!(
        "crews.{}.agents.{}.plugins.{}",
        agent.crew, agent.agent, plugin
    )
}

/// Every `(agent, plugin, config)` of the spec, sorted by agent then
/// plugin. The spec was resolved client-side, but plugin names are
/// validated here again: the daemon builds ids from them.
pub fn pairs(fleet: &FleetName, spec: &FleetSpec) -> Result<Vec<Pair>, PluginError> {
    let mut out = Vec::new();
    for (crew, c) in &spec.crews {
        for (agent, settings) in &c.agents {
            let id: AgentId = format!("{fleet}/{crew}/{agent}")
                .parse()
                .map_err(|e: balerix_core::NameError| PluginError::Activation {
                    path: format!("crews.{crew}.agents.{agent}"),
                    message: e.to_string(),
                })?;
            for (name, config) in &settings.plugins {
                if let Err(reason) = balerix_core::name::validate_name(name) {
                    return Err(PluginError::Activation {
                        path: format!("crews.{crew}.agents.{agent}.plugins.{name}"),
                        message: format!("invalid plugin name: {reason}"),
                    });
                }
                let plugin: AgentName = name
                    .parse()
                    .map_err(|e: balerix_core::NameError| PluginError::Activation {
                        path: format!("crews.{crew}.agents.{agent}.plugins.{name}"),
                        message: e.to_string(),
                    })?;
                out.push(Pair {
                    agent: id.clone(),
                    plugin,
                    config: config.clone(),
                });
            }
        }
    }
    out.sort_by(|x, y| (&x.agent, &x.plugin).cmp(&(&y.agent, &y.plugin)));
    Ok(out)
}

pub fn diff(old: &[Pair], new: &[Pair]) -> ActivationDiff {
    let find = |set: &[Pair], p: &Pair| {
        set.iter()
            .find(|q| q.agent == p.agent && q.plugin == p.plugin)
            .cloned()
    };
    let mut d = ActivationDiff::default();
    for p in new {
        match find(old, p) {
            Some(prev) if prev.config == p.config => {}
            Some(_) => {
                d.deactivate.push((p.agent.clone(), p.plugin.clone()));
                d.activate.push(p.clone());
            }
            None => d.activate.push(p.clone()),
        }
    }
    for p in old {
        if find(new, p).is_none() {
            d.deactivate.push((p.agent.clone(), p.plugin.clone()));
        }
    }
    d.deactivate.sort();
    d
}
```

`kv.rs`:

```rust
//! The per-plugin key/value store (plugins spec §4.1 `kv`): one 0600 file
//! per key under `plugins/<name>/kv/`, written atomically; secret entries
//! sealed by the vault with `<plugin>/<key>` as associated data. The
//! sandbox never sees this directory — the route is the only way in.

use std::fs;
use std::path::{Path, PathBuf};

use balerix_core::AgentName;

use super::PluginError;
use crate::fsutil::write_private;
use crate::vault::Vault;

const PLAIN: u8 = b'p';
const SEALED: u8 = b's';
pub const MAX_KEY: usize = 200;

/// `[A-Za-z0-9._/-]{1,200}`, no empty segment, no `.` or `..` segment.
pub fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("empty".into());
    }
    if key.len() > MAX_KEY {
        return Err(format!("longer than {MAX_KEY} bytes"));
    }
    if let Some(c) = key
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-')))
    {
        return Err(format!("character {c:?} is not in [A-Za-z0-9._/-]"));
    }
    for seg in key.split('/') {
        match seg {
            "" => return Err("empty path segment".into()),
            "." | ".." => return Err(format!("segment {seg:?} is not allowed")),
            _ => {}
        }
    }
    Ok(())
}

pub struct PluginKv {
    state_root: PathBuf,
    vault: Vault,
}

impl PluginKv {
    /// `state_root` is `$XDG_STATE_HOME/balerix/plugins`.
    pub fn new(state_root: PathBuf, vault: Vault) -> Self {
        Self { state_root, vault }
    }

    fn dir(&self, name: &AgentName) -> PathBuf {
        self.state_root.join(name.as_str()).join("kv")
    }

    fn path(&self, name: &AgentName, key: &str) -> Result<PathBuf, PluginError> {
        validate_key(key).map_err(PluginError::KvKey)?;
        Ok(self.dir(name).join(key))
    }

    fn io(path: &Path, e: std::io::Error) -> PluginError {
        PluginError::Kv {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }

    fn aad(name: &AgentName, key: &str) -> String {
        format!("{name}/{key}")
    }

    pub fn get(&self, name: &AgentName, key: &str) -> Result<Option<Vec<u8>>, PluginError> {
        let path = self.path(name, key)?;
        let raw = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Self::io(&path, e)),
        };
        match raw.split_first() {
            Some((&PLAIN, rest)) => Ok(Some(rest.to_vec())),
            Some((&SEALED, rest)) => self
                .vault
                .open(&Self::aad(name, key), rest)
                .map(Some)
                .map_err(|e| PluginError::Kv {
                    path,
                    message: e.to_string(),
                }),
            _ => Err(PluginError::Kv {
                path,
                message: "unknown entry format".into(),
            }),
        }
    }

    pub fn put(&self, name: &AgentName, key: &str, bytes: &[u8], secret: bool) -> Result<(), PluginError> {
        let path = self.path(name, key)?;
        let mut out = Vec::with_capacity(bytes.len() + 1);
        if secret {
            out.push(SEALED);
            let sealed = self
                .vault
                .seal(&Self::aad(name, key), bytes)
                .map_err(|e| PluginError::Kv {
                    path: path.clone(),
                    message: e.to_string(),
                })?;
            out.extend_from_slice(&sealed);
        } else {
            out.push(PLAIN);
            out.extend_from_slice(bytes);
        }
        write_private(&path, &out).map_err(|e| Self::io(&path, e))
    }

    /// `Ok(false)` when there was nothing to delete.
    pub fn delete(&self, name: &AgentName, key: &str) -> Result<bool, PluginError> {
        let path = self.path(name, key)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Self::io(&path, e)),
        }
    }

    /// Every key under the prefix, sorted. Temp files (`.` + name) are
    /// never keys, so they are skipped.
    pub fn list(&self, name: &AgentName, prefix: &str) -> Result<Vec<String>, PluginError> {
        let dir = self.dir(name);
        let mut keys = Vec::new();
        if dir.exists() {
            Self::walk(&dir, &dir, &mut keys)?;
        }
        keys.retain(|k| k.starts_with(prefix));
        keys.sort();
        Ok(keys)
    }

    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), PluginError> {
        for entry in fs::read_dir(dir).map_err(|e| Self::io(dir, e))? {
            let entry = entry.map_err(|e| Self::io(dir, e))?;
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                Self::walk(root, &path, out)?;
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
        Ok(())
    }
}
```

`write_private` creates parent directories (`fsutil.rs`), so nested keys work. `crates/balerix-server/src/lib.rs`: extend the `plugins` re-export with `ActivationRow, PluginInfo, PluginKv, PluginRegistry`.

- [ ] **Step 4: Run the tests**

Run: `mise x -- cargo test -p balerix-server && mise run check`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-server
git commit -m "Add the plugin registry, the activation diff and the KV store

The registry is the single writer of activation state (plugins spec
§16.2/§16.3) and the source of the interceptor order; the diff is pure
so Daemon::apply can be tested by its inputs; KV entries are one 0600
file per validated key, secrets sealed by the vault with the plugin and
key as associated data.

Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe"
```

---

### Task 5: `balerix-server` — `PluginEventHandler`: the interceptor chain and observer queues

**Files:**
- Create: `crates/balerix-server/src/plugins/chain.rs`
- Modify: `crates/balerix-server/src/plugins/mod.rs`, `crates/balerix-server/src/lib.rs`

**Interfaces:**
- Consumes: Task 2's `EventHandler`/`HandlerFuture`/`Outcome`; Task 3's `PluginClient`, `CallFailure`, `Metrics`; Task 4's `PluginRegistry`.
- Produces:
  - `plugins::chain::PluginEventHandler::new(registry: Arc<PluginRegistry>, client: PluginClient, metrics: Metrics) -> Arc<Self>`; implements `EventHandler` (spec §4.3): walks `registry.interceptors(agent, event)` under a `CHAIN_BUDGET_MS` budget, skips failures (counted by reason), collects actions, then pushes the event to every `registry.observers(agent, event)` queue. `fn on_hello(&self, name: &AgentName)` clears that plugin's queue. `fn queue_len(&self, name: &AgentName) -> usize` (tests).
  - `plugins::chain::ObserverQueue`: `push(&self, event: HookEvent) -> bool` (`true` when an older event was dropped to make room), `clear(&self)`, `len(&self)`, `drain(&self, max: usize) -> Vec<HookEvent>`; capacity `OBSERVER_QUEUE`. One delivery task per plugin: batches of at most `OBSERVER_BATCH`, cut after `BATCH_WINDOW` (100 ms), posted with `PluginClient::events`; a non-2xx is logged and the batch counted as dropped.

- [ ] **Step 1: Write the failing tests**

Test module of `chain.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use balerix_api::{ActivationState, HookEvent, PluginAction, Timestamp};
    use balerix_core::ResolvedPlugin;
    use proptest::prelude::*;
    use serde_json::{Value, json};
    use std::sync::Mutex as StdMutex;

    fn plugin(name: &str, intercept: &[&str], observe: &[&str]) -> ResolvedPlugin {
        ResolvedPlugin {
            name: name.parse().unwrap(),
            package: format!("/pkg/{name}").into(),
            manifest: serde_json::from_value(json!({
                "apiVersion": "balerix/v1", "kind": "Plugin", "name": name,
                "version": "0.1.0", "protocol": 1, "start": "serve",
                "hooks": { "intercept": intercept, "observe": observe }
            }))
            .unwrap(),
            config: json!({}),
            digest: None,
        }
    }

    fn event(name: &str) -> HookEvent {
        HookEvent {
            agent: "f/c/a".into(),
            name: name.into(),
            session_id: None,
            received_at: Timestamp(1),
            payload: json!({ "tool_input": { "command": "rm -rf /" } }),
        }
    }

    /// What one stub plugin does with an intercept.
    #[derive(Clone, Debug)]
    enum Behaviour {
        /// Merge `{ key: value }` over `response_so_far`, plus these actions.
        Merge(String, i64, Vec<PluginAction>),
        Status500,
        NotAnObject,
        Sleep(u64),
    }

    #[derive(Clone)]
    struct StubState {
        behaviour: Behaviour,
        batches: Arc<StdMutex<Vec<Vec<HookEvent>>>>,
        events_status: u16,
    }

    async fn stub(behaviour: Behaviour, events_status: u16) -> (String, Arc<StdMutex<Vec<Vec<HookEvent>>>>) {
        let batches = Arc::new(StdMutex::new(Vec::new()));
        let state = StubState {
            behaviour,
            batches: batches.clone(),
            events_status,
        };
        let app = Router::new()
            .route(
                "/v1/intercept",
                post(|State(s): State<StubState>, Json(v): Json<Value>| async move {
                    match s.behaviour {
                        Behaviour::Merge(k, n, actions) => {
                            let mut r = v["response_so_far"].clone();
                            r[k] = json!(n);
                            (axum::http::StatusCode::OK, Json(json!({ "response": r, "actions": actions })))
                        }
                        Behaviour::Status500 => (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": "boom" })),
                        ),
                        Behaviour::NotAnObject => (axum::http::StatusCode::OK, Json(json!({ "response": [1] }))),
                        Behaviour::Sleep(ms) => {
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                            (axum::http::StatusCode::OK, Json(json!({ "response": { "late": true } })))
                        }
                    }
                }),
            )
            .route(
                "/v1/events",
                post(|State(s): State<StubState>, Json(b): Json<balerix_api::EventBatch>| async move {
                    s.batches.lock().unwrap().push(b.events);
                    axum::http::StatusCode::from_u16(s.events_status).unwrap()
                }),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, batches)
    }

    fn active(r: &PluginRegistry, plugin: &str) {
        r.set_row(
            &"f/c/a".parse().unwrap(),
            &plugin.parse().unwrap(),
            ActivationRow {
                config: json!({}),
                activation: balerix_api::PluginActivation::active(),
            },
        );
    }

    fn handler(r: &Arc<PluginRegistry>) -> (Arc<PluginEventHandler>, Metrics) {
        let m = Metrics::new().unwrap();
        (
            PluginEventHandler::new(r.clone(), PluginClient::new().unwrap(), m.clone()),
            m,
        )
    }

    #[tokio::test]
    async fn the_chain_folds_verdicts_in_load_order_and_skips_failures() {
        let r = PluginRegistry::new();
        r.replace_plugins(
            &[
                plugin("first", &["PreToolUse"], &[]),
                plugin("broken", &["PreToolUse"], &[]),
                plugin("odd", &["PreToolUse"], &[]),
                plugin("last", &["PreToolUse"], &[]),
                plugin("bystander", &["Stop"], &[]),
            ],
            &[],
        );
        let (first, _) = stub(
            Behaviour::Merge("a".into(), 1, vec![PluginAction::Stop]),
            200,
        )
        .await;
        let (broken, _) = stub(Behaviour::Status500, 200).await;
        let (odd, _) = stub(Behaviour::NotAnObject, 200).await;
        let (last, _) = stub(
            Behaviour::Merge(
                "b".into(),
                2,
                vec![PluginAction::SendText {
                    text: "hi".into(),
                    submit: true,
                }],
            ),
            200,
        )
        .await;
        for (n, l) in [("first", first), ("broken", broken), ("odd", odd), ("last", last)] {
            r.set_listen(&n.parse().unwrap(), l);
            active(&r, n);
        }
        let (h, m) = handler(&r);
        let out = h.handle(&event("PreToolUse")).await;
        assert_eq!(out.response, json!({ "a": 1, "b": 2 }));
        assert_eq!(
            out.actions,
            vec![
                PluginAction::Stop,
                PluginAction::SendText {
                    text: "hi".into(),
                    submit: true
                }
            ]
        );
        let text = m.encode();
        assert!(text.contains("balerix_plugin_intercept_failures_total{plugin=\"broken\",reason=\"status\"} 1"), "{text}");
        assert!(text.contains("balerix_plugin_intercept_failures_total{plugin=\"odd\",reason=\"body\"} 1"));
        assert!(text.contains("balerix_plugin_events_total{event=\"PreToolUse\",mode=\"intercept\",plugin=\"first\"} 1"));
        assert!(!text.contains("plugin=\"bystander\""), "not subscribed to this event");
        // nobody intercepts Notification: allow, and cheap
        assert_eq!(h.handle(&event("Notification")).await, Outcome::allow());
    }

    #[tokio::test]
    async fn a_slow_plugin_is_skipped_within_the_budget_and_a_dead_one_counts_as_connect() {
        let r = PluginRegistry::new();
        r.replace_plugins(
            &[plugin("slow", &["PreToolUse"], &[]), plugin("dead", &["PreToolUse"], &[]), plugin("ok", &["PreToolUse"], &[])],
            &[],
        );
        let (slow, _) = stub(Behaviour::Sleep(5_000), 200).await;
        let (ok, _) = stub(Behaviour::Merge("k".into(), 9, vec![]), 200).await;
        r.set_listen(&"slow".parse().unwrap(), slow);
        r.set_listen(&"dead".parse().unwrap(), "127.0.0.1:1".into());
        r.set_listen(&"ok".parse().unwrap(), ok);
        for n in ["slow", "dead", "ok"] {
            active(&r, n);
        }
        let (h, m) = handler(&r);
        let started = Instant::now();
        let out = h.handle(&event("PreToolUse")).await;
        let took = started.elapsed();
        assert!(took < Duration::from_millis(1900), "chain took {took:?}");
        assert!(took >= Duration::from_millis(1400), "the slow plugin got the whole budget: {took:?}");
        assert_eq!(out.response, json!({}), "the budget was spent before ok ran");
        let text = m.encode();
        assert!(text.contains("balerix_plugin_intercept_failures_total{plugin=\"slow\",reason=\"timeout\"} 1"), "{text}");
        assert!(text.contains("balerix_plugin_intercept_failures_total{plugin=\"dead\",reason=\"connect\"} 1"));
        assert!(text.contains("balerix_plugin_intercept_failures_total{plugin=\"ok\",reason=\"timeout\"} 1"), "no budget left: skipped as a timeout");
    }

    #[tokio::test]
    async fn observers_get_batches_in_order_and_hello_clears_the_queue() {
        let r = PluginRegistry::new();
        r.replace_plugins(&[plugin("web", &[], &["Stop", "Notification"])], &[]);
        let (listen, batches) = stub(Behaviour::Status500, 200).await;
        r.set_listen(&"web".parse().unwrap(), listen);
        active(&r, "web");
        let (h, _m) = handler(&r);
        for i in 0..70 {
            let mut e = event("Stop");
            e.payload = json!({ "i": i });
            assert_eq!(h.handle(&e).await, Outcome::allow(), "observers never block or answer");
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let got: usize = batches.lock().unwrap().iter().map(Vec::len).sum();
                if got == 70 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("all 70 delivered");
        let b = batches.lock().unwrap();
        assert!(b[0].len() <= OBSERVER_BATCH);
        let order: Vec<i64> = b.iter().flatten().map(|e| e.payload["i"].as_i64().unwrap()).collect();
        assert_eq!(order, (0..70).collect::<Vec<_>>(), "order kept across batches");
        drop(b);
        // not ready: nothing queued; hello clears whatever was
        r.set_ready(&"web".parse().unwrap(), false);
        h.handle(&event("Notification")).await;
        assert_eq!(h.queue_len(&"web".parse().unwrap()), 0);
        h.on_hello(&"web".parse().unwrap());
        assert_eq!(h.queue_len(&"web".parse().unwrap()), 0);
    }

    #[test]
    fn the_queue_drops_the_oldest_on_overflow() {
        let q = ObserverQueue::new("web".parse().unwrap());
        for i in 0..(OBSERVER_QUEUE as i64 + 5) {
            let mut e = event("Stop");
            e.payload = json!({ "i": i });
            let dropped = q.push(e);
            assert_eq!(dropped, i >= OBSERVER_QUEUE as i64, "i={i}");
        }
        assert_eq!(q.len(), OBSERVER_QUEUE);
        let first = q.drain(1);
        assert_eq!(first[0].payload["i"], 5, "the five oldest went");
        assert_eq!(q.drain(OBSERVER_BATCH).len(), OBSERVER_BATCH);
        q.clear();
        assert_eq!(q.len(), 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]
        /// Spec §11 property: the final response is the fold over the
        /// plugins that did not fail, whichever ones do.
        #[test]
        fn the_final_response_is_the_fold_over_the_non_failing_plugins(
            fails in proptest::collection::vec(any::<bool>(), 1..5)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let r = PluginRegistry::new();
                let names: Vec<String> = (0..fails.len()).map(|i| format!("p{i}")).collect();
                let plugins: Vec<ResolvedPlugin> = names.iter().map(|n| plugin(n, &["PreToolUse"], &[])).collect();
                r.replace_plugins(&plugins, &[]);
                let mut expected = serde_json::Map::new();
                for (i, (name, fail)) in names.iter().zip(&fails).enumerate() {
                    let behaviour = if *fail { Behaviour::Status500 } else { Behaviour::Merge(name.clone(), i as i64, vec![]) };
                    let (listen, _) = stub(behaviour, 200).await;
                    r.set_listen(&name.parse().unwrap(), listen);
                    active(&r, name);
                    if !fail {
                        expected.insert(name.clone(), json!(i));
                    }
                }
                let (h, _) = handler(&r);
                let out = h.handle(&event("PreToolUse")).await;
                assert_eq!(out.response, Value::Object(expected));
            });
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-server chain`
Expected: compile error, no `chain` module.

- [ ] **Step 3: Implement**

```rust
//! The interceptor chain and observer delivery (plugins spec §4.3). One
//! `EventHandler` for the daemon: interceptors run in load-list order under
//! a shared budget and fail open; observers get batches from a bounded
//! per-plugin queue and never touch the response.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use balerix_api::{
    CHAIN_BUDGET_MS, EventBatch, HookEvent, InterceptRequest, OBSERVER_BATCH, OBSERVER_QUEUE,
};
use balerix_core::{AgentId, AgentName, EventHandler, HandlerFuture, Outcome};
use tokio::sync::Notify;

use super::client::PluginClient;
use super::registry::PluginRegistry;
use crate::metrics::Metrics;

/// How long a delivery task waits for a batch to fill before sending.
pub const BATCH_WINDOW: Duration = Duration::from_millis(100);

pub struct ObserverQueue {
    plugin: AgentName,
    buf: Mutex<VecDeque<HookEvent>>,
    notify: Notify,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ObserverQueue {
    pub fn new(plugin: AgentName) -> Arc<Self> {
        Arc::new(Self {
            plugin,
            buf: Mutex::new(VecDeque::with_capacity(OBSERVER_QUEUE)),
            notify: Notify::new(),
        })
    }

    /// Appends; on overflow the oldest event goes and `true` comes back.
    pub fn push(&self, event: HookEvent) -> bool {
        let dropped = {
            let mut b = lock(&self.buf);
            let dropped = if b.len() >= OBSERVER_QUEUE {
                b.pop_front();
                true
            } else {
                false
            };
            b.push_back(event);
            dropped
        };
        self.notify.notify_one();
        dropped
    }

    pub fn clear(&self) {
        lock(&self.buf).clear();
    }

    pub fn len(&self) -> usize {
        lock(&self.buf).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn drain(&self, max: usize) -> Vec<HookEvent> {
        let mut b = lock(&self.buf);
        let n = max.min(b.len());
        b.drain(..n).collect()
    }

    /// The delivery loop: wait for something, give the batch `BATCH_WINDOW`
    /// to fill (or `OBSERVER_BATCH`), post it. A plugin that stopped being
    /// ready loses the batch (§4.3: no catch-up).
    async fn deliver(self: Arc<Self>, registry: Arc<PluginRegistry>, client: PluginClient, metrics: Metrics) {
        loop {
            if self.is_empty() {
                self.notify.notified().await;
            }
            let deadline = tokio::time::Instant::now() + BATCH_WINDOW;
            while self.len() < OBSERVER_BATCH {
                tokio::select! {
                    () = self.notify.notified() => {}
                    () = tokio::time::sleep_until(deadline) => break,
                }
            }
            let batch = self.drain(OBSERVER_BATCH);
            if batch.is_empty() {
                continue;
            }
            let Some(listen) = registry.ready_listen(&self.plugin) else {
                metrics.events_dropped(self.plugin.as_str(), batch.len() as u64);
                continue;
            };
            let n = batch.len();
            if let Err(e) = client.events(&listen, &EventBatch { events: batch }).await {
                tracing::warn!(plugin = %self.plugin, events = n, "observer batch not acknowledged: {e}");
                metrics.events_dropped(self.plugin.as_str(), n as u64);
            }
        }
    }
}

pub struct PluginEventHandler {
    registry: Arc<PluginRegistry>,
    client: PluginClient,
    metrics: Metrics,
    observers: Mutex<BTreeMap<AgentName, Arc<ObserverQueue>>>,
}

impl PluginEventHandler {
    pub fn new(registry: Arc<PluginRegistry>, client: PluginClient, metrics: Metrics) -> Arc<Self> {
        Arc::new(Self {
            registry,
            client,
            metrics,
            observers: Mutex::new(BTreeMap::new()),
        })
    }

    /// The plugin's queue, its delivery task spawned on first use. Needs
    /// a tokio runtime, which every caller (a request handler) has.
    fn queue_for(&self, name: &AgentName) -> Arc<ObserverQueue> {
        let mut map = lock(&self.observers);
        if let Some(q) = map.get(name) {
            return q.clone();
        }
        let q = ObserverQueue::new(name.clone());
        tokio::spawn(q.clone().deliver(
            self.registry.clone(),
            self.client.clone(),
            self.metrics.clone(),
        ));
        map.insert(name.clone(), q.clone());
        q
    }

    /// A plugin that just said hello starts from an empty queue (§4.3).
    pub fn on_hello(&self, name: &AgentName) {
        if let Some(q) = lock(&self.observers).get(name) {
            q.clear();
        }
    }

    pub fn queue_len(&self, name: &AgentName) -> usize {
        lock(&self.observers).get(name).map_or(0, |q| q.len())
    }

    async fn run(&self, event: &HookEvent) -> Outcome {
        let Ok(agent) = event.agent.parse::<AgentId>() else {
            return Outcome::allow();
        };
        let mut outcome = Outcome::allow();
        let deadline = Instant::now() + Duration::from_millis(CHAIN_BUDGET_MS);
        for (name, listen) in self.registry.interceptors(&agent, &event.name) {
            self.metrics
                .plugin_event(name.as_str(), &event.name, "intercept");
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.metrics
                    .intercept(name.as_str(), &event.name, 0.0, Some("timeout"));
                continue;
            }
            let req = InterceptRequest {
                event: event.clone(),
                response_so_far: outcome.response.clone(),
                deadline_ms: u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
            };
            let started = Instant::now();
            match self.client.intercept(&listen, &req, remaining).await {
                Ok(verdict) => {
                    outcome.response = verdict.response;
                    outcome.actions.extend(verdict.actions);
                    self.metrics.intercept(
                        name.as_str(),
                        &event.name,
                        started.elapsed().as_secs_f64(),
                        None,
                    );
                }
                Err(e) => {
                    tracing::warn!(plugin = %name, agent = %agent, event = %event.name, "interceptor skipped: {e}");
                    self.metrics.intercept(
                        name.as_str(),
                        &event.name,
                        started.elapsed().as_secs_f64(),
                        Some(e.reason()),
                    );
                }
            }
        }
        for (name, _) in self.registry.observers(&agent, &event.name) {
            self.metrics
                .plugin_event(name.as_str(), &event.name, "observe");
            if self.queue_for(&name).push(event.clone()) {
                self.metrics.events_dropped(name.as_str(), 1);
            }
        }
        outcome
    }
}

impl EventHandler for PluginEventHandler {
    fn handle<'a>(&'a self, event: &'a HookEvent) -> HandlerFuture<'a> {
        Box::pin(self.run(event))
    }
}
```

`plugins/mod.rs`: `pub mod chain;` and `pub use chain::{ObserverQueue, PluginEventHandler};`; `lib.rs` re-exports `PluginEventHandler`.

- [ ] **Step 4: Run the tests**

Run: `mise x -- cargo test -p balerix-server chain && mise run check`
Expected: green. The budget test's timing bounds are wide (1.4–1.9 s); if it flakes on a slow CI box, widen the upper bound, never the lower one.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-server
git commit -m "Add the interceptor chain and observer delivery

Plugins spec §4.3: interceptors run in plugins.yaml order under a shared
1500 ms budget and fail open, with each skip counted by reason; observers
get batches of 64 or 100 ms from a per-plugin queue of 1024 that drops the
oldest on overflow and starts empty at hello.

Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe"
```

---

### Task 6: `balerix-server` — the daemon: activation, actions, token lookup, overlay, health

**Files:**
- Modify: `crates/balerix-server/src/daemon.rs`, `crates/balerix-server/src/hooks.rs:59-66`, `crates/balerix-server/src/plugins/host.rs`, `crates/balerix-server/src/testing.rs`, `crates/balerix-server/src/lib.rs`, `crates/balerix-server/tests/api_it.rs:106-113`, `crates/balerix-server/tests/plugins_it.rs:94-101,377-384`, `crates/balerix/tests/cli_fleet.rs:44-51`, `crates/balerix/src/commands/serve.rs:171-181`

**Interfaces:**
- Consumes: Tasks 3–5.
- Produces:
  - `Daemon::start(ports, handler, metrics, token, existing, plugin_config, registry: Arc<PluginRegistry>, client: PluginClient, kv: PluginKv) -> Arc<Self>` (three new trailing parameters). `PluginHostConfig` unchanged.
  - `Daemon::registry(&self) -> &Arc<PluginRegistry>`, `Daemon::client(&self) -> &PluginClient`, `Daemon::kv(&self) -> &Arc<PluginKv>`.
  - `Daemon::apply` now activates before the actor (§16.2) and returns the overlaid record; `Daemon::get`/`snapshots` overlay; `Daemon::down` deactivates the fleet's pairs after the actor replies.
  - `Daemon::plugin_hello` re-sends `activate` for the plugin's rows after `hello` and clears its observer queue. The handler parameter stays a single `Arc`: `pub trait HelloObserver: Send + Sync { fn on_hello(&self, name: &AgentName); }` (implemented by `PluginEventHandler`, and as a no-op for `PassThrough`) and `pub trait DaemonHandler: EventHandler + HelloObserver {}` with a blanket impl; `Daemon::start` takes `handler: Arc<dyn DaemonHandler>`. Both traits live in `balerix-server/src/daemon.rs`.
  - `Daemon::execute_action(&self, agent: &AgentId, action: &PluginAction, plugin: Option<&str>) -> Result<(), DaemonError>`; `Daemon::run_actions(self: Arc<Self>, agent: AgentId, actions: Vec<PluginAction>)` (spawned by ingress after the response is written).
  - `Daemon::plugin_for_token(&self, token: &str) -> Option<AgentName>`.
  - `Daemon::plugin_fleets(&self) -> Vec<FleetRecord>` (user fleets only, overlaid; what the `fleets` route returns).
  - `PluginHost::start(config, agent_ports, shared, registry: Arc<PluginRegistry>)`; `PluginHost::hello` records the listen in the registry (no `listen` map any more) and `list()` fills `active_agents` and the degraded message.
  - `testing::Harness` gains `pub registry: Arc<PluginRegistry>`, `pub client: PluginClient`, `pub kv: Arc<PluginKv>` (kv under a `tempfile::TempDir` the harness owns as `pub kv_dir`), and `pub fn daemon(&self, handler: Arc<dyn DaemonHandler>, plugin_dir: &Path) -> Arc<Daemon>` that builds `Ports` over the fakes and calls `Daemon::start` — the four existing call sites switch to it.
  - `HEALTH_INTERVAL: Duration = 10 s`; `Daemon::start` spawns the health poller.

- [ ] **Step 1: Write the failing tests**

Add to `crates/balerix-server/src/daemon.rs` a test module (the daemon had none; the API tests covered it). These tests use the fakes and a stub plugin server built with the same `stub` helper pattern as Task 5, extracted into `crates/balerix-server/src/testing.rs` as `pub async fn stub_plugin(script: StubScript) -> StubPlugin` so Task 10's integration tests can use it too:

```rust
// testing.rs additions
use std::sync::Mutex as StdMutex;

/// A scripted plugin endpoint for daemon tests: records every call,
/// rejects activation for agents named in `reject` with that message,
/// answers intercepts with `verdict` (merged over `response_so_far`).
#[derive(Clone, Default)]
pub struct StubScript {
    pub reject: BTreeMap<String, String>,
    pub verdict: serde_json::Value,
    pub actions: Vec<balerix_api::PluginAction>,
    pub health_ok: bool,
    pub metrics_body: String,
}

#[derive(Clone)]
pub struct StubPlugin {
    pub listen: String,
    pub calls: Arc<StdMutex<Vec<(String, serde_json::Value)>>>,
}

impl StubPlugin {
    pub fn calls(&self) -> Vec<(String, serde_json::Value)> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn calls_named(&self, route: &str) -> Vec<serde_json::Value> {
        self.calls()
            .into_iter()
            .filter(|(r, _)| r == route)
            .map(|(_, v)| v)
            .collect()
    }
}

pub async fn stub_plugin(script: StubScript) -> StubPlugin {
    use axum::extract::State;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::{Value, json};
    let calls = Arc::new(StdMutex::new(Vec::new()));
    #[derive(Clone)]
    struct S {
        script: StubScript,
        calls: Arc<StdMutex<Vec<(String, Value)>>>,
    }
    let record = |s: &S, route: &str, v: Value| {
        s.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((route.to_string(), v));
    };
    let app = Router::new()
        .route(
            "/v1/activate",
            post(move |State(s): State<S>, Json(v): Json<Value>| async move {
                record(&s, "activate", v.clone());
                let agent = v["agent"].as_str().unwrap_or_default();
                match s.script.reject.get(agent) {
                    Some(msg) => (axum::http::StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))),
                    None => (axum::http::StatusCode::OK, Json(json!({}))),
                }
            }),
        )
        .route(
            "/v1/deactivate",
            post(move |State(s): State<S>, Json(v): Json<Value>| async move {
                record(&s, "deactivate", v);
                Json(json!({}))
            }),
        )
        .route(
            "/v1/events",
            post(move |State(s): State<S>, Json(v): Json<Value>| async move {
                record(&s, "events", v);
                Json(json!({}))
            }),
        )
        .route(
            "/v1/intercept",
            post(move |State(s): State<S>, Json(v): Json<Value>| async move {
                record(&s, "intercept", v.clone());
                let mut r = v["response_so_far"].clone();
                if let (Some(dst), Some(src)) = (r.as_object_mut(), s.script.verdict.as_object()) {
                    for (k, val) in src {
                        dst.insert(k.clone(), val.clone());
                    }
                }
                Json(json!({ "response": r, "actions": s.script.actions }))
            }),
        )
        .route(
            "/v1/health",
            get(move |State(s): State<S>| async move {
                record(&s, "health", json!({}));
                if s.script.health_ok {
                    axum::http::StatusCode::OK
                } else {
                    axum::http::StatusCode::SERVICE_UNAVAILABLE
                }
            }),
        )
        .route(
            "/v1/metrics",
            get(move |State(s): State<S>| async move { s.script.metrics_body.clone() }),
        )
        .with_state(S {
            script,
            calls: calls.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("bind: {e}"));
    let listen = listener
        .local_addr()
        .unwrap_or_else(|e| panic!("addr: {e}"))
        .to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    StubPlugin { listen, calls }
}
```

(`testing.rs` is a non-test module of the library, so it may not use `unwrap`; the `unwrap_or_else(panic!)` form is what keeps clippy quiet, as `Harness` already does with `Mutex` poisoning.) Because `PluginHost` needs a package on disk to declare a plugin, tests that want a *registered* plugin write a package and `plugins.yaml` into `plugin_dir` and call `daemon.sync_plugins()`, exactly as `plugins_it.rs` does today; add `pub fn write_plugin_package(dir: &Path, name: &str, manifest_extra: &str)` to `testing.rs`:

```rust
/// A package directory for tests: `balerix-plugin.yaml` with the given
/// extra lines (hooks, needs) and a `mise.toml` whose start task is `true`.
pub fn write_plugin_package(dir: &Path, name: &str, manifest_extra: &str) {
    let _ = std::fs::create_dir_all(dir);
    let manifest = format!(
        "apiVersion: balerix/v1\nkind: Plugin\nname: {name}\nversion: 0.1.0\nprotocol: 1\nstart: serve\n{manifest_extra}"
    );
    let _ = std::fs::write(dir.join("balerix-plugin.yaml"), manifest);
    let _ = std::fs::write(dir.join("mise.toml"), "[tools]\n[tasks.serve]\nrun = \"true\"\n");
}
```

The daemon tests (`crates/balerix-server/src/daemon.rs`, `#[cfg(test)] mod tests`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::PluginEventHandler;
    use crate::testing::{Harness, StubScript, plugin_config_in, stub_plugin, write_plugin_package};
    use balerix_api::{ActivationState, AgentPhase, AgentSettings, CrewSpec, GitSettings, PluginAction};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn spec(agents: &[(&str, &[(&str, serde_json::Value)])]) -> FleetSpec {
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
                        .map(|(n, plugins)| {
                            let mut s = AgentSettings::default();
                            s.plugins = plugins.iter().map(|(p, c)| (p.to_string(), c.clone())).collect();
                            (n.to_string(), s)
                        })
                        .collect(),
                },
            )]),
        }
    }

    struct World {
        h: Harness,
        daemon: Arc<Daemon>,
        _dir: tempfile::TempDir,
        dir: std::path::PathBuf,
    }

    /// A daemon with the chain handler and one declared plugin `flow`
    /// (intercepts PreToolUse and Stop, observes Stop, needs actions+kv)
    /// that has not said hello yet.
    async fn world() -> World {
        let h = Harness::new(Duration::from_secs(3600));
        let dir = tempfile::tempdir().unwrap();
        write_plugin_package(
            &dir.path().join("flow-pkg"),
            "flow",
            "hooks: { intercept: [PreToolUse, Stop], observe: [Stop] }\nneeds: [actions, kv]\n",
        );
        std::fs::write(
            dir.path().join("plugins.yaml"),
            "plugins:\n  - name: flow\n    source: ./flow-pkg\n",
        )
        .unwrap();
        let handler = PluginEventHandler::new(h.registry.clone(), h.client.clone(), Metrics::new().unwrap());
        let daemon = h.daemon(handler, dir.path());
        daemon.sync_plugins().await.unwrap();
        World {
            h,
            daemon,
            dir: dir.path().to_path_buf(),
            _dir: dir,
        }
    }

    async fn hello(w: &World, listen: &str) {
        let name: AgentName = "flow".parse().unwrap();
        let token = w.daemon.hook_secret(&plugin_id(&name)).await.unwrap();
        w.daemon
            .plugin_hello(
                &name,
                &token,
                HelloRequest {
                    name: "flow".into(),
                    version: "0.1.0".into(),
                    protocol: balerix_api::PLUGIN_PROTOCOL,
                    listen: listen.into(),
                },
            )
            .await
            .unwrap();
    }

    async fn wait_gen(daemon: &Daemon, g: u64) {
        let name: FleetName = "f".parse().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if daemon.get(&name).await.is_some_and(|r| r.status.observed_generation == g) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unknown_plugin_fails_the_apply_before_the_actor_sees_it() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        let e = w
            .daemon
            .apply(&name, spec(&[("a", &[("nope", json!({}))])]), Default::default(), false)
            .await
            .unwrap_err();
        assert_eq!(
            e,
            DaemonError::Invalid("crews.c.agents.a.plugins.nope: no plugin \"nope\" is installed".into())
        );
        assert!(w.daemon.get(&name).await.is_none(), "no actor was spawned");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pending_pair_is_activated_at_hello_and_a_rejection_is_recorded() {
        let w = world().await;
        let name: FleetName = "f".parse().unwrap();
        let rec = w
            .daemon
            .apply(
                &name,
                spec(&[("a", &[("flow", json!({ "v": 1 }))]), ("b", &[("flow", json!({ "v": 2 }))])]),
                Default::default(),
                false,
            )
            .await
            .unwrap();
        wait_gen(&w.daemon, 1).await;
        let rec = w.daemon.get(&name).await.unwrap_or(rec);
        assert_eq!(rec.status.agents["f/c/a"].plugins["flow"].state, ActivationState::Pending);
        assert_eq!(rec.status.agents["f/c/b"].plugins["flow"].state, ActivationState::Pending);

        let stub = stub_plugin(StubScript {
            reject: BTreeMap::from([("f/c/b".to_string(), "states.x: unknown".to_string())]),
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        let rec = w.daemon.get(&name).await.unwrap();
        assert_eq!(rec.status.agents["f/c/a"].plugins["flow"].state, ActivationState::Active);
        let b = &rec.status.agents["f/c/b"].plugins["flow"];
        assert_eq!((b.state, b.message.as_str()), (ActivationState::Rejected, "states.x: unknown"));
        let activates = stub.calls_named("activate");
        assert_eq!(activates.len(), 2);
        assert_eq!(activates[0]["agent"], "f/c/a");
        assert_eq!(activates[0]["config"]["v"], 1);
        assert_eq!(w.daemon.plugins().list().await[0].active_agents, 1);
        // a second hello re-activates everything again (restart recovery)
        hello(&w, &stub.listen).await;
        assert_eq!(stub.calls_named("activate").len(), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_ready_plugin_is_activated_during_apply_and_a_rejection_fails_it() {
        let w = world().await;
        let stub = stub_plugin(StubScript {
            reject: BTreeMap::from([("f/c/bad".to_string(), "no".to_string())]),
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        let name: FleetName = "f".parse().unwrap();
        let e = w
            .daemon
            .apply(
                &name,
                spec(&[("a", &[("flow", json!({}))]), ("bad", &[("flow", json!({}))])]),
                Default::default(),
                false,
            )
            .await
            .unwrap_err();
        assert_eq!(e, DaemonError::Invalid("crews.c.agents.bad.plugins.flow: no".into()));
        assert!(w.daemon.get(&name).await.is_none(), "nothing landed");
        assert_eq!(
            stub.calls_named("deactivate").len(),
            1,
            "the pair that had been activated is rolled back"
        );
        assert!(w.daemon.registry().row(&"f/c/a".parse().unwrap(), &"flow".parse().unwrap()).is_none());

        // a good spec: active at once; a changed config deactivates then activates;
        // a dropped agent deactivates; down deactivates the rest
        let rec = w
            .daemon
            .apply(&name, spec(&[("a", &[("flow", json!({ "v": 1 }))]), ("c", &[("flow", json!({}))])]), Default::default(), false)
            .await
            .unwrap();
        assert_eq!(rec.status.agents.get("f/c/a").map(|a| a.plugins["flow"].state), None, "the actor's first pass has not created the entry yet; the overlay skips unknown agents");
        wait_gen(&w.daemon, 1).await;
        let rec = w.daemon.get(&name).await.unwrap();
        assert_eq!(rec.status.agents["f/c/a"].plugins["flow"].state, ActivationState::Active);
        let before = stub.calls().len();
        w.daemon
            .apply(&name, spec(&[("a", &[("flow", json!({ "v": 2 }))])]), Default::default(), true)
            .await
            .unwrap();
        let after: Vec<String> = stub.calls()[before..]
            .iter()
            .map(|(r, v)| format!("{r} {}", v["agent"].as_str().unwrap_or_default()))
            .collect();
        assert_eq!(after, vec!["deactivate f/c/a", "activate f/c/a", "deactivate f/c/c"]);
        w.daemon.down(&name, Keep::default(), false).await.unwrap();
        let last = stub.calls().last().unwrap().clone();
        assert_eq!((last.0.as_str(), last.1["agent"].as_str()), ("deactivate", Some("f/c/a")));
        assert!(w.daemon.registry().rows_for_plugin(&"flow".parse().unwrap()).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_run_the_chain_and_actions_reach_the_runner_and_the_actor() {
        let w = world().await;
        let stub = stub_plugin(StubScript {
            verdict: json!({ "decision": "block", "reason": "no" }),
            actions: vec![
                PluginAction::SendText { text: "fix it".into(), submit: true },
                PluginAction::Restart,
            ],
            health_ok: true,
            ..StubScript::default()
        })
        .await;
        hello(&w, &stub.listen).await;
        let name: FleetName = "f".parse().unwrap();
        w.daemon
            .apply(&name, spec(&[("a", &[("flow", json!({}))])]), Default::default(), false)
            .await
            .unwrap();
        wait_gen(&w.daemon, 1).await;
        let agent: AgentId = "f/c/a".parse().unwrap();
        let secret = w.daemon.hook_secret(&agent).await.unwrap();
        let out = w
            .daemon
            .event(
                &agent,
                &secret,
                ParsedEvent {
                    name: "PreToolUse".into(),
                    session_id: None,
                    payload: json!({ "hook_event_name": "PreToolUse" }),
                },
            )
            .await
            .unwrap();
        assert_eq!(out.response, json!({ "decision": "block", "reason": "no" }));
        assert_eq!(out.actions.len(), 2);
        w.daemon.clone().run_actions(agent.clone(), out.actions).await;
        let calls = w.h.runner.calls();
        assert!(calls.contains(&"send_text f/c/a \"fix it\" submit=true".to_string()), "{calls:?}");
        assert!(calls.contains(&"stop_agent f/c/a".to_string()));
        assert_eq!(calls.iter().filter(|c| *c == "ensure_agent f/c/a").count(), 2, "restart = stop + start");
        let rec = w.daemon.get(&name).await.unwrap();
        assert!(rec.stopped.is_empty(), "restart leaves nothing stopped");
        assert_eq!(rec.status.agents["f/c/a"].restarts, 0);
        w.daemon
            .execute_action(&agent, &PluginAction::Stop, Some("flow"))
            .await
            .unwrap();
        let rec = w.daemon.get(&name).await.unwrap();
        assert_eq!(rec.status.agents["f/c/a"].phase, AgentPhase::Stopped);
        assert!(rec.stopped.contains("f/c/a"));
        let text = w.daemon.metrics().encode();
        assert!(text.contains("balerix_plugin_actions_total{action=\"stop\",plugin=\"flow\"} 1"), "{text}");
        assert!(text.contains("balerix_hook_actions_total{action=\"restart\",agent=\"a\",crew=\"c\",fleet=\"f\"} 1"));
        // an unknown event on a non-intercepting plugin: observers only
        w.daemon
            .event(&agent, &secret, ParsedEvent { name: "Stop".into(), session_id: None, payload: json!({}) })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while stub.calls_named("events").is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the observer batch arrived");
        assert_eq!(stub.calls_named("events")[0]["events"][0]["name"], "Stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokens_identify_plugins_and_the_health_poller_marks_degraded() {
        let w = world().await;
        let name: AgentName = "flow".parse().unwrap();
        let token = w.daemon.hook_secret(&plugin_id(&name)).await.unwrap();
        assert_eq!(w.daemon.plugin_for_token(&token).await, Some(name.clone()));
        assert_eq!(w.daemon.plugin_for_token("nope").await, None);
        let stub = stub_plugin(StubScript { health_ok: false, ..StubScript::default() }).await;
        hello(&w, &stub.listen).await;
        w.daemon.poll_health().await;
        let rows = w.daemon.plugins().list().await;
        assert_eq!(rows[0].message, "degraded: HTTP 503: ");
        assert_eq!(rows[0].phase, AgentPhase::Ready, "never restarted for it");
        hello(&w, &stub.listen).await;
        assert_eq!(w.daemon.plugins().list().await[0].message, "", "hello clears it");
        let _ = &w.dir;
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-server daemon`
Expected: compile errors (`Harness::daemon`, `registry()`, `run_actions`, `plugin_for_token`, `poll_health`, `stub_plugin` missing).

- [ ] **Step 3: Implement**

`daemon.rs` — new fields and constructor:

```rust
use crate::plugins::activation::{self, Pair};
use crate::plugins::{PluginClient, PluginKv, PluginRegistry, ActivationRow};
use balerix_api::{ActivationState, PluginAction, PluginActivation, ActivateRequest, DeactivateRequest};

/// The chain handler's hello hook; `PassThrough` has nothing to clear.
pub trait HelloObserver: Send + Sync {
    fn on_hello(&self, name: &AgentName);
}
impl HelloObserver for balerix_core::PassThrough {
    fn on_hello(&self, _: &AgentName) {}
}
impl HelloObserver for crate::plugins::PluginEventHandler {
    fn on_hello(&self, name: &AgentName) {
        crate::plugins::PluginEventHandler::on_hello(self, name);
    }
}
pub trait DaemonHandler: EventHandler + HelloObserver {}
impl<T: EventHandler + HelloObserver> DaemonHandler for T {}

/// How often every ready plugin's `GET /v1/health` is polled (§16.5).
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(10);

pub struct Daemon {
    fleets: RwLock<BTreeMap<FleetName, FleetHandle>>,
    ports: Arc<Ports>,
    shared: Shared,
    handler: Arc<dyn DaemonHandler>,
    token: String,
    plugins: Arc<PluginHost>,
    registry: Arc<PluginRegistry>,
    client: PluginClient,
    kv: Arc<PluginKv>,
    /// One apply at a time per daemon: activation and the actor message
    /// must not interleave with another apply of the same fleet.
    applying: tokio::sync::Mutex<()>,
}
```

`Daemon::start(ports, handler: Arc<dyn DaemonHandler>, metrics, token, existing, plugin_config, registry, client, kv)`: pass `registry.clone()` to `PluginHost::start`; after spawning the stored fleets, seed pending rows: for every `(record, _)` with `Desired::Up`, `for p in activation::pairs(&name, &record.spec).unwrap_or_default() { registry.set_row(&p.agent, &p.plugin, ActivationRow { config: p.config, activation: PluginActivation::pending() }) }`; spawn `Self::health_loop(Arc::downgrade(&daemon))`.

```rust
    async fn health_loop(daemon: std::sync::Weak<Self>) {
        loop {
            tokio::time::sleep(HEALTH_INTERVAL).await;
            let Some(d) = daemon.upgrade() else { return };
            d.poll_health().await;
        }
    }

    /// One round: every ready plugin's `/v1/health`; a failure sets its
    /// degraded message, a success clears it. Never restarts anything.
    pub async fn poll_health(&self) {
        for name in self.registry.names() {
            let Some(listen) = self.registry.ready_listen(&name) else { continue };
            match self.client.health(&listen).await {
                Ok(()) => self.registry.set_degraded(&name, None),
                Err(e) => {
                    tracing::warn!(plugin = %name, "health check failed: {e}");
                    self.registry.set_degraded(&name, Some(e.to_string()));
                }
            }
        }
    }

    pub fn registry(&self) -> &Arc<PluginRegistry> { &self.registry }
    pub fn client(&self) -> &PluginClient { &self.client }
    pub fn kv(&self) -> &Arc<PluginKv> { &self.kv }

    fn overlay(&self, mut record: FleetRecord) -> FleetRecord {
        self.registry.overlay(&mut record);
        record
    }
```

`get`, `snapshots`, `apply`'s return and `down`'s return all pass through `overlay`. `plugin_fleets`:

```rust
    /// User fleets with their activation rows: what the `fleets` route
    /// serves. Secrets never live in a record.
    pub async fn plugin_fleets(&self) -> Vec<FleetRecord> {
        self.fleets
            .read()
            .await
            .values()
            .map(|h| self.overlay(h.status.borrow().clone()))
            .collect()
    }
```

Activation inside `apply` — after `Fleet::try_from` and before the `handle` block, holding `let _guard = self.applying.lock().await;` for the whole method:

```rust
        let previous: Option<FleetSpec> = {
            let fleets = self.fleets.read().await;
            fleets.get(name).and_then(|h| {
                let r = h.status.borrow();
                (!r.is_down()).then(|| r.spec.clone())
            })
        };
        let old = match &previous {
            Some(s) => activation::pairs(name, s).unwrap_or_default(),
            None => Vec::new(),
        };
        let new = activation::pairs(name, &spec).map_err(|e| DaemonError::Invalid(e.to_string()))?;
        for p in &new {
            if !self.registry.is_installed(p.plugin.as_str()) {
                return Err(DaemonError::Invalid(format!(
                    "{}: no plugin {:?} is installed",
                    activation::config_path(&p.agent, &p.plugin),
                    p.plugin.as_str()
                )));
            }
        }
        let d = activation::diff(&old, &new);
        // deactivate changed pairs first (§16.2), then activate every new or
        // changed pair on a ready plugin; the first rejection rolls back the
        // ones already accepted and nothing reaches the actor
        for (agent, plugin) in &d.deactivate {
            if d.activate.iter().any(|p| &p.agent == agent && &p.plugin == plugin) {
                self.deactivate_pair(agent, plugin).await;
            }
        }
        let mut accepted: Vec<Pair> = Vec::new();
        let mut rows: Vec<(Pair, PluginActivation)> = Vec::new();
        for p in &d.activate {
            match self.registry.ready_listen(&p.plugin) {
                Some(listen) => {
                    let req = ActivateRequest { agent: p.agent.to_string(), config: p.config.clone() };
                    match self.client.activate(&listen, &req).await {
                        Ok(()) => {
                            accepted.push(p.clone());
                            rows.push((p.clone(), PluginActivation::active()));
                        }
                        Err(e) => {
                            for a in &accepted {
                                self.deactivate_pair(&a.agent, &a.plugin).await;
                            }
                            return Err(DaemonError::Invalid(format!(
                                "{}: {}",
                                activation::config_path(&p.agent, &p.plugin),
                                Self::activation_message(&e)
                            )));
                        }
                    }
                }
                None => rows.push((p.clone(), PluginActivation::pending())),
            }
        }
```

then the existing actor handling (`handle`, `Msg::Apply`, `rx.await`), and after the reply:

```rust
        for (agent, plugin) in &d.deactivate {
            if !d.activate.iter().any(|p| &p.agent == agent && &p.plugin == plugin) {
                self.deactivate_pair(agent, plugin).await;
            }
            self.registry.remove_row(agent, plugin);
        }
        for (p, activation) in rows {
            self.registry.set_row(&p.agent, &p.plugin, ActivationRow { config: p.config, activation });
        }
        Ok(self.overlay(record))
```

with:

```rust
    /// `CallFailure::Status` carries the plugin's own error verbatim; every
    /// other failure is described, never trusted as a message.
    fn activation_message(e: &CallFailure) -> String {
        match e {
            CallFailure::Status { message, .. } => message.clone(),
            other => format!("plugin unreachable ({other})"),
        }
    }

    /// Best effort: a plugin that cannot be told is logged, not an error.
    async fn deactivate_pair(&self, agent: &AgentId, plugin: &AgentName) {
        let Some(listen) = self.registry.ready_listen(plugin) else { return };
        let req = DeactivateRequest { agent: agent.to_string() };
        if let Err(e) = self.client.deactivate(&listen, &req).await {
            tracing::warn!(plugin = %plugin, agent = %agent, "deactivate failed: {e}");
        }
    }
```

`down`: after the actor's reply, `for (agent, plugin) in self.registry.remove_fleet(name) { self.deactivate_pair(&agent, &plugin).await; }` and return `self.overlay(record)`.

`plugin_hello`: after `self.plugins.hello(name, req).await?` (which now calls `registry.set_listen`), do `self.handler.on_hello(name);` then re-activate:

```rust
        if let Some(listen) = self.registry.ready_listen(name) {
            for (agent, row) in self.registry.rows_for_plugin(name) {
                let req = ActivateRequest { agent: agent.to_string(), config: row.config.clone() };
                let activation = match self.client.activate(&listen, &req).await {
                    Ok(()) => PluginActivation::active(),
                    Err(e) => {
                        tracing::warn!(plugin = %name, agent = %agent, "activation rejected at hello: {e}");
                        PluginActivation::rejected(Self::activation_message(&e))
                    }
                };
                self.registry.set_state(&agent, name, activation);
            }
        }
```

`plugin_for_token`:

```rust
    /// Which plugin presents this token: the `balerix` fleet's entries of
    /// the secret index, each compared in constant time. Plugins are few.
    pub async fn plugin_for_token(&self, token: &str) -> Option<AgentName> {
        let idx = self.shared.hook_secrets.read().await;
        idx.iter()
            .filter(|(id, _)| is_reserved_fleet(id.fleet.as_str()))
            .find(|(_, s)| constant_time_eq(s.as_bytes(), token.as_bytes()))
            .map(|(id, _)| id.agent.clone())
    }
```

Actions:

```rust
    /// One action from a verdict or from the `actions` route. `plugin` is
    /// the metrics label when a plugin asked for it.
    pub async fn execute_action(
        &self,
        agent: &AgentId,
        action: &PluginAction,
        plugin: Option<&str>,
    ) -> Result<(), DaemonError> {
        self.shared.metrics.hook_action(agent, action.label());
        if let Some(p) = plugin {
            self.shared.metrics.plugin_action(p, action.label());
        }
        match action {
            PluginAction::SendText { text, submit } => {
                let runner = self.ports.runner.clone();
                let (id, text, submit) = (agent.clone(), text.clone(), *submit);
                tokio::task::spawn_blocking(move || runner.send_text(&id, &text, submit))
                    .await
                    .map_err(|e| DaemonError::Internal(e.to_string()))?
                    .map_err(|e| DaemonError::Internal(e.to_string()))
            }
            PluginAction::Stop => self.set_stopped(agent, true).await.map(|_| ()),
            PluginAction::Restart => {
                self.set_stopped(agent, true).await?;
                self.set_stopped(agent, false).await.map(|_| ())
            }
        }
    }

    async fn set_stopped(&self, agent: &AgentId, stopped: bool) -> Result<FleetRecord, DaemonError> {
        let handle = self.fleets.read().await.get(&agent.fleet).cloned().ok_or(DaemonError::NotFound)?;
        let (reply, rx) = oneshot::channel();
        handle
            .tx
            .send(Msg::SetStopped { agent: agent.clone(), stopped, reply })
            .await
            .map_err(|_| DaemonError::Internal("fleet task is gone".into()))?;
        rx.await.map_err(|_| DaemonError::Internal("fleet task dropped the request".into()))
    }

    /// Runs a verdict's actions in order after the response was written
    /// (architecture spec §8). Failures are logged; Claude already moved on.
    pub async fn run_actions(self: Arc<Self>, agent: AgentId, actions: Vec<PluginAction>) {
        for a in &actions {
            if let Err(e) = self.execute_action(&agent, a, None).await {
                tracing::warn!(agent = %agent, action = a.label(), "action failed: {e}");
            }
        }
    }
```

`run_actions` counts once per action through `execute_action`; the `plugin` label is `None` for chain verdicts because a merged chain does not attribute actions per plugin — Task 7's `actions` route passes `Some(name)`. In Task 5's test the verdict's actions are attributed by the chain: change `run` in `chain.rs` to remember `(AgentName, PluginAction)`? **No** — `Outcome.actions` is `Vec<PluginAction>` by spec §3; the chain counts `balerix_plugin_actions_total{plugin,action}` itself when it collects a verdict's actions (`self.metrics.plugin_action(name, a.label())` inside the `Ok(verdict)` arm). Add that line to `chain.rs::run` in this task and the assertion `balerix_plugin_actions_total{action="stop",plugin="first"} 1` to the chain test.

`hooks.rs` `events`: on `Ok(Ok(outcome))`, before returning the JSON, `if !outcome.actions.is_empty() { tokio::spawn(state.daemon.clone().run_actions(id.clone(), outcome.actions.clone())); }`.

`host.rs`: replace the `listen: RwLock<BTreeMap<..>>` field with `registry: Arc<PluginRegistry>` (constructor parameter); `sync` calls `self.registry.replace_plugins(&resolved, &report.unchanged)` where it used to prune `listen`; `hello` calls `self.registry.set_listen(name.clone(), req.listen.clone())` (validation unchanged); `list` reads `listen`, `active_agents` and the degraded message from the registry:

```rust
                let info = self.registry.plugin(&p.name);
                let message = match st.map(|s| s.message.clone()).filter(|m| !m.is_empty()) {
                    Some(m) => m,
                    None => info
                        .as_ref()
                        .and_then(|i| i.degraded.as_ref())
                        .map(|d| format!("degraded: {d}"))
                        .unwrap_or_default(),
                };
                PluginStatus {
                    …
                    listen: info.as_ref().and_then(|i| i.listen.clone()),
                    active_agents: self.registry.active_agents(&p.name),
                    message,
                }
```

Plus a watcher task started in `PluginHost::start` that mirrors the plugin fleet's phases into the registry: 

```rust
        let mut rx = handle.status.clone();
        let reg = registry.clone();
        tokio::spawn(async move {
            loop {
                {
                    let record = rx.borrow_and_update();
                    for (id, st) in &record.status.agents {
                        if let Ok(id) = id.parse::<AgentId>() {
                            reg.set_ready(&id.agent, st.phase == AgentPhase::Ready);
                        }
                    }
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        });
```

(`hello` marks ready synchronously through `set_listen`, so re-activation at hello does not race the watcher; the watcher's job is the way *down*: a plugin that exits or restarts stops being called.)

`testing.rs`: `Harness` gains the three fields, built in `with_policy` (`registry: PluginRegistry::new()`, `client: PluginClient::new().unwrap_or_else(|e| panic!("{e}"))`, `kv_dir: tempfile::tempdir()…`, `kv: Arc::new(PluginKv::new(kv_dir.path().join("plugins"), Vault::from_key([7u8; 32])))`), and:

```rust
    /// A daemon over these fakes with an empty fleet set.
    pub fn daemon(&self, handler: Arc<dyn DaemonHandler>, plugin_dir: &Path) -> Arc<Daemon> {
        let ports = Ports {
            materializer: self.materializer.clone(),
            runner: self.runner.clone(),
            clock: self.clock.clone(),
            store: self.store.clone(),
            policy: self.ports.policy.clone(),
            hook_url: self.ports.hook_url.clone(),
            resync: self.ports.resync,
        };
        Daemon::start(
            ports,
            handler,
            Metrics::new().unwrap_or_else(|e| panic!("{e}")),
            "admin-tok".into(),
            Vec::new(),
            plugin_config_in(plugin_dir),
            self.registry.clone(),
            self.client.clone(),
            self.kv.clone(),
        )
    }
```

Switch `api_it.rs`, `plugins_it.rs` (both tests), `cli_fleet.rs` to `h.daemon(Arc::new(PassThrough), dir)` (the `cli_fleet` stub uses the token `"tok"`: keep `Daemon::start` there but pass `h.registry.clone(), h.client.clone(), h.kv.clone()`; simplest is a `Harness::daemon_with_token(&self, handler, plugin_dir, token: &str)` that `daemon` delegates to). `serve.rs` gets the three new arguments in Task 11; for now make it compile with `PluginRegistry::new()`, `PluginClient::new()?`, and `PluginKv::new(layout.plugins_state_dir(), vault.clone())` (build the `Vault` once, clone it into the store), still passing `Arc::new(PassThrough)`.

`lib.rs`: `pub use daemon::{Daemon, DaemonError, DaemonHandler, HelloObserver, HEALTH_INTERVAL};` and `testing::{StubPlugin, StubScript, stub_plugin, write_plugin_package}` are reachable as `balerix_server::testing::*`.

- [ ] **Step 4: Run everything**

Run: `mise run check`
Expected: green across the workspace, including the five new daemon tests, the amended chain test, and the four call sites.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-server crates/balerix/src/commands/serve.rs crates/balerix/tests/cli_fleet.rs
git commit -m "Activate plugins in Daemon::apply and execute verdict actions

Plugins spec §16.2/§16.3: activation runs before the actor sees the
spec, so a rejection is a 400 and nothing lands; a plugin that is not
ready gets a pending row that its next hello activates; activation state
is overlaid on the record at read time. §16.4: stop and restart go
through the actor's SetStopped, send_text through the runner. The token
identifies the plugin for the host routes; health is polled every 10 s.

Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe"
```

---

### Task 7: `balerix-server` — host routes (`fleets`, `actions`, `kv`) and the metrics re-export

**Files:**
- Create: `crates/balerix-server/src/plugin_api.rs`
- Modify: `crates/balerix-server/src/api.rs` (mount; `/metrics`), `crates/balerix-server/src/lib.rs`

**Interfaces:**
- Consumes: Task 6's `Daemon::{plugin_for_token, plugin_fleets, get, registry, kv, execute_action}`; Task 4's `PluginKv`, `PluginError::{Capability, KvKey, NotActive}`.
- Produces routes (plugin bearer on every one; 401 `unknown plugin or bad token` for a missing or unknown token; 403 `capability "…" not declared in balerix-plugin.yaml` when the manifest's `needs` lacks it):
  - `GET /v1/plugin-host/fleets` → `Vec<FleetRecord>` (`fleets`).
  - `GET /v1/plugin-host/fleets/{name}` → `FleetRecord`, 404 for unknown and for `balerix` (`fleets`).
  - `POST /v1/plugin-host/agents/{fleet}/{crew}/{agent}/actions`, body `PluginAction` → `{}`; 404 `plugin is not active for agent f/c/a` unless the pair is `Active` (`actions`).
  - `GET /v1/plugin-host/kv/{*key}` → raw bytes, `application/octet-stream`, 404 when absent; `PUT /v1/plugin-host/kv/{*key}[?secret=true]`, raw body ≤ 1 MiB → `{}`; `DELETE /v1/plugin-host/kv/{*key}` → `{}` (200 even when absent); `GET /v1/plugin-host/kv?prefix=` → `KvKeys` (`kv`). Invalid keys are 400.
  - `GET /metrics` appends each ready plugin's `/v1/metrics` body whose every family name starts with `balerix_plugin_<name>_`, scraped in parallel with a 500 ms timeout; anything else is dropped whole and counted in `balerix_plugin_metrics_scrape_failures_total{plugin}`.
  - `plugin_api::families_ok(body: &str, plugin: &str) -> bool` — the prefix rule, pure.

- [ ] **Step 1: Write the failing tests**

Unit test for the prefix rule in `plugin_api.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_passes_only_if_every_family_carries_the_plugin_prefix() {
        let ok = "# HELP balerix_plugin_flow_state Current state\n# TYPE balerix_plugin_flow_state gauge\nbalerix_plugin_flow_state{agent=\"a\"} 1\n\nbalerix_plugin_flow_transitions_total{from=\"a\",to=\"b\"} 2\n";
        assert!(families_ok(ok, "flow"));
        assert!(!families_ok(ok, "web"), "another plugin's prefix");
        assert!(!families_ok("balerix_plugin_flow_x 1\nprocess_cpu_seconds_total 3\n", "flow"));
        assert!(!families_ok("balerix_agents{fleet=\"f\"} 1\n", "flow"), "daemon families cannot be spoofed");
        assert!(families_ok("", "flow"), "empty is fine");
        assert!(families_ok("# just a comment\n", "flow"));
        assert!(!families_ok("balerix_plugin_flow 1\n", "flow"), "the prefix needs the trailing underscore");
    }
}
```

Integration coverage lives in Task 10 (`events_it.rs`), which drives every route over HTTP with a real token. The unit tests here only cover the pure rule; the handlers are exercised end to end.

- [ ] **Step 2: Run the test to verify it fails**

Run: `mise x -- cargo test -p balerix-server plugin_api`
Expected: compile error, no module.

- [ ] **Step 3: Implement**

`crates/balerix-server/src/plugin_api.rs`:

```rust
//! `/v1/plugin-host/*` beyond `hello` (plugins spec §4.1): the plugin's
//! bearer identifies it, the manifest's `needs` gates every route.

use axum::body::Bytes;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use axum::routing::get;
use balerix_api::{Capability, KvKeys, PluginAction};
use balerix_core::{AgentId, AgentName, FleetName, FleetRecord, is_reserved_fleet};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{ApiError, AppState};
use crate::auth::bearer;
use crate::daemon::DaemonError;
use crate::plugins::PluginError;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/plugin-host/fleets", get(list_fleets))
        .route("/v1/plugin-host/fleets/{name}", get(get_fleet))
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/actions",
            axum::routing::post(post_action),
        )
        .route("/v1/plugin-host/kv", get(list_keys))
        .route(
            "/v1/plugin-host/kv/{*key}",
            get(get_key).put(put_key).delete(delete_key),
        )
}

/// The calling plugin, from its token, with the capability check.
async fn caller(
    state: &AppState,
    headers: &HeaderMap,
    cap: Capability,
) -> Result<AgentName, ApiError> {
    let unauthorized = || ApiError::new(StatusCode::UNAUTHORIZED, "unknown plugin or bad token");
    let token = bearer(headers).ok_or_else(unauthorized)?;
    let name = state
        .daemon
        .plugin_for_token(token)
        .await
        .ok_or_else(unauthorized)?;
    if !state.daemon.registry().has(&name, cap) {
        return Err(PluginError::Capability(crate::plugins::wire_label(cap)).into());
    }
    Ok(name)
}

async fn list_fleets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<FleetRecord>>, ApiError> {
    caller(&state, &headers, Capability::Fleets).await?;
    Ok(Json(state.daemon.plugin_fleets().await))
}

async fn get_fleet(
    State(state): State<AppState>,
    headers: HeaderMap,
    name: Result<Path<String>, PathRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    caller(&state, &headers, Capability::Fleets).await?;
    let Path(name) = name.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    let name: FleetName = name
        .parse()
        .map_err(|_: balerix_core::NameError| ApiError::from(DaemonError::NotFound))?;
    if is_reserved_fleet(name.as_str()) {
        return Err(DaemonError::NotFound.into());
    }
    state
        .daemon
        .get(&name)
        .await
        .map(Json)
        .ok_or_else(|| DaemonError::NotFound.into())
}

async fn post_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
    body: Result<Json<PluginAction>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Actions).await?;
    let Path((f, c, a)) = path.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    let agent: AgentId = format!("{f}/{c}/{a}")
        .parse()
        .map_err(|_: balerix_core::NameError| ApiError::from(DaemonError::NotFound))?;
    let Json(action) = body.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    if !state.daemon.registry().is_active(&agent, &plugin) {
        return Err(PluginError::NotActive(agent.to_string()).into());
    }
    state
        .daemon
        .execute_action(&agent, &action, Some(plugin.as_str()))
        .await?;
    Ok(Json(json!({})))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct KvQuery {
    prefix: String,
    secret: bool,
}

fn kv_query(q: Result<Query<KvQuery>, QueryRejection>) -> Result<KvQuery, ApiError> {
    q.map(|Query(q)| q)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))
}

fn key_of(p: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    p.map(|Path(k)| k)
        .map_err(|e| ApiError::new(e.status(), e.body_text()))
}

async fn list_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
    q: Result<Query<KvQuery>, QueryRejection>,
) -> Result<Json<KvKeys>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Kv).await?;
    let q = kv_query(q)?;
    let kv = state.daemon.kv().clone();
    let keys = tokio::task::spawn_blocking(move || kv.list(&plugin, &q.prefix))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(KvKeys { keys }))
}

async fn get_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let plugin = caller(&state, &headers, Capability::Kv).await?;
    let key = key_of(key)?;
    let kv = state.daemon.kv().clone();
    let value = tokio::task::spawn_blocking(move || kv.get(&plugin, &key))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    match value {
        Some(bytes) => Ok(([(CONTENT_TYPE, "application/octet-stream")], bytes).into_response()),
        None => Err(ApiError::new(StatusCode::NOT_FOUND, "no such key")),
    }
}

async fn put_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Result<Path<String>, PathRejection>,
    q: Result<Query<KvQuery>, QueryRejection>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Kv).await?;
    let key = key_of(key)?;
    let q = kv_query(q)?;
    let kv = state.daemon.kv().clone();
    tokio::task::spawn_blocking(move || kv.put(&plugin, &key, &body, q.secret))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(json!({})))
}

async fn delete_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Kv).await?;
    let key = key_of(key)?;
    let kv = state.daemon.kv().clone();
    tokio::task::spawn_blocking(move || kv.delete(&plugin, &key))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(json!({})))
}

/// The metrics prefix rule (plugins spec §9): every family name in the
/// body starts with `balerix_plugin_<name>_`. Comment lines are checked
/// through their `# TYPE`/`# HELP` family, sample lines through the name
/// before `{` or the first space.
pub fn families_ok(body: &str, plugin: &str) -> bool {
    let prefix = format!("balerix_plugin_{plugin}_");
    body.lines().all(|line| {
        let line = line.trim();
        if line.is_empty() {
            return true;
        }
        let family = if let Some(rest) = line.strip_prefix('#') {
            let mut words = rest.split_whitespace();
            match words.next() {
                Some("TYPE" | "HELP") => match words.next() {
                    Some(f) => f,
                    None => return false,
                },
                _ => return true,
            }
        } else {
            line.split(|c: char| c == '{' || c.is_whitespace())
                .next()
                .unwrap_or("")
        };
        family.starts_with(&prefix)
    })
}
```

`api.rs`: `.merge(crate::plugin_api::router().layer(DefaultBodyLimit::max(1 << 20)))` next to the other merges (the `plugins` router keeps the 64 KiB `hello` limit), and the `/metrics` handler becomes:

```rust
async fn metrics(State(state): State<AppState>) -> Response {
    let snapshots = state.daemon.snapshots().await;
    state.daemon.metrics().set_gauges(&snapshots);
    let mut body = state.daemon.metrics().encode();
    let registry = state.daemon.registry().clone();
    let mut scrapes = tokio::task::JoinSet::new();
    for name in registry.names() {
        let Some(listen) = registry.ready_listen(&name) else { continue };
        let client = state.daemon.client().clone();
        scrapes.spawn(async move {
            let r = client.metrics(&listen, SCRAPE_TIMEOUT).await;
            (name, r)
        });
    }
    while let Some(joined) = scrapes.join_next().await {
        let Ok((name, result)) = joined else { continue };
        match result {
            Ok(text) if crate::plugin_api::families_ok(&text, name.as_str()) => {
                body.push_str(&text);
                if !text.ends_with('\n') {
                    body.push('\n');
                }
            }
            Ok(_) => state.daemon.metrics().scrape_failure(name.as_str()),
            Err(e) => {
                tracing::debug!(plugin = %name, "metrics scrape failed: {e}");
                state.daemon.metrics().scrape_failure(name.as_str());
            }
        }
    }
    ([(CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}
```

with `const SCRAPE_TIMEOUT: Duration = Duration::from_millis(500);`. The daemon-side counter bumps after `encode()` ran, so a failure shows on the *next* scrape; that is fine and is what the Task 10 test asserts. `lib.rs`: `pub mod plugin_api;`.

- [ ] **Step 4: Run the tests**

Run: `mise x -- cargo test -p balerix-server && mise run check`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-server
git commit -m "Serve the fleets, actions and kv host routes and re-export plugin metrics

Plugins spec §4.1: every route is gated by the manifest's needs and
identifies the plugin by its token alone; kv bodies are raw bytes with
secrets sealed by the vault. §9: /metrics appends each ready plugin's
families under the balerix_plugin_<name>_ prefix and drops the rest.

Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe"
```

---

### Task 8: `balerix-plugin-sdk` — async `Host`, the `Plugin` trait, `serve`, `testing::FakeHost`

**Files:**
- Create: `crates/balerix-plugin-sdk/src/host.rs`, `crates/balerix-plugin-sdk/src/plugin.rs`, `crates/balerix-plugin-sdk/src/testing.rs`
- Modify: `crates/balerix-plugin-sdk/Cargo.toml`, `crates/balerix-plugin-sdk/src/lib.rs`, `crates/balerix/src/commands/dev.rs:231-260` (compile fix: `fake_plugin_command` uses the blocking `Host::hello`; Task 11 rewrites it — for this task, make it build a runtime with `tokio::runtime::Runtime::new()?.block_on(host.hello(..))`)

**Interfaces:**
- Consumes: `balerix_api::{FleetRecord, PluginAction, ActivateRequest, DeactivateRequest, EventBatch, InterceptRequest, InterceptResponse, KvKeys, HelloRequest, HelloResponse, ErrorBody, HookEvent, PLUGIN_PROTOCOL}` (Task 1, with `FleetRecord` in `balerix-api` per Task 1 Step 3b).
- Produces:
  - `balerix_plugin_sdk::Env` unchanged. `SdkError` gains `Bind(String)` (`listen: {0}`).
  - `Host::new(env: Env) -> Result<Host, SdkError>`; `fn env(&self) -> &Env`; `async fn hello(&self, version: &str, listen: &str) -> Result<HelloResponse, SdkError>`; `async fn fleets(&self) -> Result<Vec<FleetRecord>, SdkError>`; `async fn fleet(&self, name: &str) -> Result<Option<FleetRecord>, SdkError>` (404 → `None`); `async fn action(&self, agent: &str, action: &PluginAction) -> Result<(), SdkError>`; `async fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>, SdkError>`; `async fn kv_put(&self, key: &str, bytes: &[u8], secret: bool) -> Result<(), SdkError>`; `async fn kv_delete(&self, key: &str) -> Result<(), SdkError>`; `async fn kv_list(&self, prefix: &str) -> Result<Vec<String>, SdkError>`. Non-2xx (other than the 404s named) is `SdkError::Status { status, message }`.
  - `trait Plugin: Send + Sync + 'static` with default methods `activate(&self, agent: &str, config: Value) -> impl Future<Output = Result<(), String>> + Send`, `deactivate(&self, agent: &str) -> impl Future<Output = ()> + Send`, `observe(&self, events: Vec<HookEvent>) -> impl Future<Output = ()> + Send`, `intercept(&self, event: HookEvent, response_so_far: Value, deadline_ms: u64) -> impl Future<Output = InterceptResponse> + Send` (default: `response_so_far`, no actions), `health(&self) -> impl Future<Output = Result<(), String>> + Send`, `metrics(&self) -> impl Future<Output = String> + Send`.
  - `fn router<P: Plugin>(plugin: Arc<P>) -> axum::Router` (the §4.2 routes); `async fn bind() -> Result<(tokio::net::TcpListener, String), SdkError>` (127.0.0.1:0 and its `host:port`); `async fn run<P: Plugin>(listener, plugin: Arc<P>) -> Result<(), SdkError>` (serves until the task is dropped); `async fn serve<P: Plugin>(host: &Host, version: &str, plugin: P) -> Result<(), SdkError>` = bind, hello, run.
  - `testing::FakeHost`: `async fn start(token: &str, config: Value, fleets: Vec<FleetRecord>) -> FakeHost`; `fn env(&self, name: &str, scratch: &Path) -> Env`; `fn hellos(&self) -> Vec<HelloRequest>`; `fn actions(&self) -> Vec<(String, PluginAction)>`; `fn kv(&self) -> BTreeMap<String, (Vec<u8>, bool)>`; `pub url: String`. Speaks exactly §4.1 (bearer check, `{ "error" }` bodies, raw kv bytes, `KvKeys`).

- [ ] **Step 1: Dependencies**

`crates/balerix-plugin-sdk/Cargo.toml`:

```toml
[dependencies]
balerix-api = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
reqwest = { workspace = true }
tokio = { workspace = true }
axum = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

- [ ] **Step 2: Write the failing tests**

`host.rs` tests (through `FakeHost`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeHost;
    use balerix_api::{FleetRecord, FleetSpec, PluginAction};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn record(name: &str) -> FleetRecord {
        FleetRecord::new(FleetSpec {
            name: name.into(),
            crews: BTreeMap::new(),
        })
    }

    #[tokio::test]
    async fn every_route_round_trips_through_the_fake_host() {
        let fake = FakeHost::start("tok", json!({ "greeting": "hi" }), vec![record("payments")]).await;
        let host = Host::new(fake.env("flow", std::path::Path::new("/s"))).unwrap();
        let hello = host.hello("0.1.0", "127.0.0.1:4321").await.unwrap();
        assert_eq!(hello.config["greeting"], "hi");
        let seen = fake.hellos();
        assert_eq!(seen.len(), 1);
        assert_eq!((seen[0].name.as_str(), seen[0].protocol, seen[0].listen.as_str()), ("flow", 1, "127.0.0.1:4321"));

        let fleets = host.fleets().await.unwrap();
        assert_eq!(fleets.len(), 1);
        assert_eq!(fleets[0].name(), "payments");
        assert_eq!(host.fleet("payments").await.unwrap().unwrap().name(), "payments");
        assert_eq!(host.fleet("nope").await.unwrap(), None);

        host.action("payments/backend/bob", &PluginAction::Restart).await.unwrap();
        assert_eq!(fake.actions(), vec![("payments/backend/bob".to_string(), PluginAction::Restart)]);

        assert_eq!(host.kv_get("state/x").await.unwrap(), None);
        host.kv_put("state/x", b"working", false).await.unwrap();
        host.kv_put("secret/t", b"\x00\x01", true).await.unwrap();
        assert_eq!(host.kv_get("state/x").await.unwrap().as_deref(), Some(&b"working"[..]));
        assert_eq!(host.kv_get("secret/t").await.unwrap().as_deref(), Some(&b"\x00\x01"[..]));
        assert_eq!(fake.kv()["secret/t"].1, true, "the secret flag was sent");
        assert_eq!(host.kv_list("state/").await.unwrap(), vec!["state/x".to_string()]);
        assert_eq!(host.kv_list("").await.unwrap().len(), 2);
        host.kv_delete("state/x").await.unwrap();
        assert_eq!(host.kv_get("state/x").await.unwrap(), None);
    }

    #[tokio::test]
    async fn statuses_and_transport_failures_are_reported() {
        let fake = FakeHost::start("tok", json!({}), vec![]).await;
        let mut env = fake.env("flow", std::path::Path::new("/s"));
        env.token = "wrong".into();
        let e = Host::new(env).unwrap().hello("0.1.0", "127.0.0.1:1").await.unwrap_err();
        assert_eq!(e.to_string(), "daemon: HTTP 401: unknown plugin or bad token");
        let mut env = fake.env("flow", std::path::Path::new("/s"));
        env.api_url = "http://127.0.0.1:1".into();
        let e = Host::new(env).unwrap().hello("0.1.0", "127.0.0.1:1").await.unwrap_err();
        assert!(matches!(e, SdkError::Transport(_)), "{e}");
        let dbg = format!("{:?}", Host::new(fake.env("flow", std::path::Path::new("/s"))).unwrap());
        assert!(!dbg.contains("tok"), "{dbg}");
    }
}
```

`plugin.rs` tests (the router through `reqwest` on a port):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{HookEvent, PluginAction, Timestamp};
    use serde_json::{Value, json};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        activated: Mutex<Vec<(String, Value)>>,
        deactivated: Mutex<Vec<String>>,
        observed: Mutex<Vec<HookEvent>>,
        healthy: Mutex<bool>,
    }

    impl Plugin for Recorder {
        async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
            if agent.ends_with("/bad") {
                return Err("states.working.on[1].match: bad regex".into());
            }
            self.activated.lock().unwrap().push((agent.to_string(), config));
            Ok(())
        }
        async fn deactivate(&self, agent: &str) {
            self.deactivated.lock().unwrap().push(agent.to_string());
        }
        async fn observe(&self, events: Vec<HookEvent>) {
            self.observed.lock().unwrap().extend(events);
        }
        async fn intercept(&self, event: HookEvent, mut response_so_far: Value, deadline_ms: u64) -> InterceptResponse {
            response_so_far["seen"] = json!(event.name);
            response_so_far["deadline"] = json!(deadline_ms);
            InterceptResponse {
                response: response_so_far,
                actions: vec![PluginAction::Stop],
            }
        }
        async fn health(&self) -> Result<(), String> {
            if *self.healthy.lock().unwrap() { Ok(()) } else { Err("warming up".into()) }
        }
        async fn metrics(&self) -> String {
            "balerix_plugin_rec_up 1\n".into()
        }
    }

    /// A plugin with every method left at its default.
    struct Silent;
    impl Plugin for Silent {}

    async fn post(url: &str, body: Value) -> (u16, Value) {
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        let r = c.post(url).json(&body).send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    #[tokio::test]
    async fn the_router_speaks_section_4_2() {
        let plugin = Arc::new(Recorder::default());
        let (listener, listen) = bind().await.unwrap();
        tokio::spawn(run(listener, plugin.clone()));
        let base = format!("http://{listen}");
        let (s, v) = post(&format!("{base}/v1/activate"), json!({ "agent": "f/c/a", "config": { "k": 1 } })).await;
        assert_eq!((s, v), (200, json!({})));
        let (s, v) = post(&format!("{base}/v1/activate"), json!({ "agent": "f/c/bad", "config": {} })).await;
        assert_eq!(s, 400);
        assert_eq!(v["error"], "states.working.on[1].match: bad regex");
        assert_eq!(plugin.activated.lock().unwrap()[0].0, "f/c/a");
        let (s, _) = post(&format!("{base}/v1/deactivate"), json!({ "agent": "f/c/a" })).await;
        assert_eq!(s, 200);
        let event = HookEvent { agent: "f/c/a".into(), name: "PreToolUse".into(), session_id: None, received_at: Timestamp(1), payload: json!({}) };
        let (s, _) = post(&format!("{base}/v1/events"), json!({ "events": [event] })).await;
        assert_eq!(s, 200);
        assert_eq!(plugin.observed.lock().unwrap().len(), 1);
        let (s, v) = post(
            &format!("{base}/v1/intercept"),
            json!({ "event": event, "response_so_far": { "a": 1 }, "deadline_ms": 900 }),
        )
        .await;
        assert_eq!(s, 200);
        assert_eq!(v["response"], json!({ "a": 1, "seen": "PreToolUse", "deadline": 900 }));
        assert_eq!(v["actions"], json!([{ "action": "stop" }]));
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(c.get(format!("{base}/v1/health")).send().await.unwrap().status().as_u16(), 503);
        *plugin.healthy.lock().unwrap() = true;
        assert_eq!(c.get(format!("{base}/v1/health")).send().await.unwrap().status().as_u16(), 200);
        let m = c.get(format!("{base}/v1/metrics")).send().await.unwrap();
        assert!(m.headers()["content-type"].to_str().unwrap().starts_with("text/plain"));
        assert_eq!(m.text().await.unwrap(), "balerix_plugin_rec_up 1\n");
        let (s, v) = post(&format!("{base}/v1/activate"), json!({ "agent": "f/c/a" })).await;
        assert_eq!(s, 400, "{v}");
    }

    #[tokio::test]
    async fn defaults_accept_everything_and_pass_the_response_through() {
        let (listener, listen) = bind().await.unwrap();
        tokio::spawn(run(listener, Arc::new(Silent)));
        let base = format!("http://{listen}");
        let (s, _) = post(&format!("{base}/v1/activate"), json!({ "agent": "f/c/a", "config": {} })).await;
        assert_eq!(s, 200);
        let event = HookEvent { agent: "f/c/a".into(), name: "Stop".into(), session_id: None, received_at: Timestamp(1), payload: json!({}) };
        let (s, v) = post(&format!("{base}/v1/intercept"), json!({ "event": event, "response_so_far": { "x": 2 }, "deadline_ms": 5 })).await;
        assert_eq!((s, v), (200, json!({ "response": { "x": 2 } })));
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(c.get(format!("{base}/v1/health")).send().await.unwrap().status().as_u16(), 200);
        assert_eq!(c.get(format!("{base}/v1/metrics")).send().await.unwrap().text().await.unwrap(), "");
    }

    #[tokio::test]
    async fn serve_binds_says_hello_and_runs() {
        let fake = crate::testing::FakeHost::start("tok", json!({}), vec![]).await;
        let host = Host::new(fake.env("rec", std::path::Path::new("/s"))).unwrap();
        let handle = tokio::spawn(async move { serve(&host, "0.1.0", Silent).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while fake.hellos().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let listen = fake.hellos()[0].listen.clone();
        assert!(listen.starts_with("127.0.0.1:"));
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(c.get(format!("http://{listen}/v1/health")).send().await.unwrap().status().as_u16(), 200);
        handle.abort();
    }
}
```

`lib.rs`: keep the existing `Env` tests; the `Host` construction there becomes `Host::new(e).unwrap()` and the two ureq-stub hello tests move to `host.rs` above (delete them from `lib.rs`).

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-plugin-sdk`
Expected: compile errors.

- [ ] **Step 4: Implement**

`lib.rs`: keep `Env`, `SdkError` (+ `#[error("listen: {0}")] Bind(String)`), and

```rust
pub mod host;
pub mod plugin;
pub mod testing;

pub use host::Host;
pub use plugin::{Plugin, bind, router, run, serve};
```

`host.rs`:

```rust
//! The plugin → daemon half (plugins spec §4.1): one method per route,
//! bearer from `Env`, loopback only.

use std::fmt;
use std::time::Duration;

use balerix_api::{
    ErrorBody, FleetRecord, HelloRequest, HelloResponse, KvKeys, PLUGIN_PROTOCOL, PluginAction,
};
use serde::de::DeserializeOwned;

use crate::{Env, SdkError};

const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Host {
    env: Env,
    http: reqwest::Client,
}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host").field("env", &self.env).finish()
    }
}

impl Host {
    pub fn new(env: Env) -> Result<Self, SdkError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        Ok(Self { env, http })
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1/plugin-host/{path}", self.env.api_url)
    }

    /// Sends with the bearer; `Ok((status, bytes))` for any status.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<(u16, Vec<u8>), SdkError> {
        let resp = req
            .bearer_auth(&self.env.token)
            .send()
            .await
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        Ok((status, bytes.to_vec()))
    }

    fn status_error(status: u16, bytes: &[u8]) -> SdkError {
        let text = String::from_utf8_lossy(bytes).trim().to_string();
        let message = serde_json::from_slice::<ErrorBody>(bytes)
            .map(|e| e.error)
            .unwrap_or(text);
        SdkError::Status { status, message }
    }

    /// 2xx → parsed body; anything else → `Status`.
    async fn json<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, SdkError> {
        let (status, bytes) = self.send(req).await?;
        if !(200..300).contains(&status) {
            return Err(Self::status_error(status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|e| SdkError::Transport(format!("bad reply: {e}")))
    }

    pub async fn hello(&self, version: &str, listen: &str) -> Result<HelloResponse, SdkError> {
        let req = HelloRequest {
            name: self.env.name.clone(),
            version: version.to_string(),
            protocol: PLUGIN_PROTOCOL,
            listen: listen.to_string(),
        };
        self.json(self.http.post(self.url("hello")).json(&req)).await
    }

    pub async fn fleets(&self) -> Result<Vec<FleetRecord>, SdkError> {
        self.json(self.http.get(self.url("fleets"))).await
    }

    pub async fn fleet(&self, name: &str) -> Result<Option<FleetRecord>, SdkError> {
        let (status, bytes) = self.send(self.http.get(self.url(&format!("fleets/{name}")))).await?;
        match status {
            200..=299 => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| SdkError::Transport(format!("bad reply: {e}"))),
            404 => Ok(None),
            _ => Err(Self::status_error(status, &bytes)),
        }
    }

    pub async fn action(&self, agent: &str, action: &PluginAction) -> Result<(), SdkError> {
        self.json::<serde_json::Value>(
            self.http
                .post(self.url(&format!("agents/{agent}/actions")))
                .json(action),
        )
        .await
        .map(|_| ())
    }

    pub async fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>, SdkError> {
        let (status, bytes) = self.send(self.http.get(self.url(&format!("kv/{key}")))).await?;
        match status {
            200..=299 => Ok(Some(bytes)),
            404 => Ok(None),
            _ => Err(Self::status_error(status, &bytes)),
        }
    }

    pub async fn kv_put(&self, key: &str, bytes: &[u8], secret: bool) -> Result<(), SdkError> {
        let req = self
            .http
            .put(self.url(&format!("kv/{key}")))
            .query(&[("secret", if secret { "true" } else { "false" })])
            .header("content-type", "application/octet-stream")
            .body(bytes.to_vec());
        self.json::<serde_json::Value>(req).await.map(|_| ())
    }

    pub async fn kv_delete(&self, key: &str) -> Result<(), SdkError> {
        self.json::<serde_json::Value>(self.http.delete(self.url(&format!("kv/{key}"))))
            .await
            .map(|_| ())
    }

    pub async fn kv_list(&self, prefix: &str) -> Result<Vec<String>, SdkError> {
        let keys: KvKeys = self
            .json(self.http.get(self.url("kv")).query(&[("prefix", prefix)]))
            .await?;
        Ok(keys.keys)
    }
}
```

`reqwest`'s `.query()` needs its `query` feature? No: `RequestBuilder::query` requires the `query` feature in 0.13 (see the feature list: `query = [dep:serde, dep:serde_urlencoded]`). Do not enable it; build the query string by hand: `self.url(&format!("kv?prefix={}", urlencode(prefix)))` where `fn urlencode(s: &str) -> String` percent-encodes everything outside `[A-Za-z0-9._~/-]`, and `kv/{key}?secret=true`. Keys are already restricted to `[A-Za-z0-9._/-]`, so they pass through unchanged. Put `urlencode` in `host.rs` with a unit test (`"a b/c" → "a%20b/c"`).

`plugin.rs`:

```rust
//! The daemon → plugin half (plugins spec §4.2, §7): implement `Plugin`,
//! hand it to `serve`. Every method has a no-op default so a plugin
//! implements only what it subscribes to.

use std::future::{Future, IntoFuture};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use balerix_api::{
    ActivateRequest, DeactivateRequest, ErrorBody, EventBatch, HookEvent, InterceptRequest,
    InterceptResponse,
};
use serde_json::{Value, json};

use crate::{Host, SdkError};

pub trait Plugin: Send + Sync + 'static {
    /// `activate`: `Err(message)` rejects the agent's config; the daemon
    /// reports it as `crews.<c>.agents.<a>.plugins.<name>: <message>`.
    fn activate(&self, agent: &str, config: Value) -> impl Future<Output = Result<(), String>> + Send {
        let _ = (agent, config);
        async { Ok(()) }
    }
    fn deactivate(&self, agent: &str) -> impl Future<Output = ()> + Send {
        let _ = agent;
        async {}
    }
    fn observe(&self, events: Vec<HookEvent>) -> impl Future<Output = ()> + Send {
        let _ = events;
        async {}
    }
    /// The verdict; default passes `response_so_far` through untouched.
    fn intercept(
        &self,
        event: HookEvent,
        response_so_far: Value,
        deadline_ms: u64,
    ) -> impl Future<Output = InterceptResponse> + Send {
        let _ = (event, deadline_ms);
        async move {
            InterceptResponse {
                response: response_so_far,
                actions: Vec::new(),
            }
        }
    }
    fn health(&self) -> impl Future<Output = Result<(), String>> + Send {
        async { Ok(()) }
    }
    /// Prometheus text; every family must start with `balerix_plugin_<name>_`.
    fn metrics(&self) -> impl Future<Output = String> + Send {
        async { String::new() }
    }
}

pub fn router<P: Plugin>(plugin: Arc<P>) -> Router {
    Router::new()
        .route("/v1/activate", post(activate::<P>))
        .route("/v1/deactivate", post(deactivate::<P>))
        .route("/v1/events", post(events::<P>))
        .route("/v1/intercept", post(intercept::<P>))
        .route("/v1/health", get(health::<P>))
        .route("/v1/metrics", get(metrics::<P>))
        .with_state(plugin)
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(ErrorBody { error: message.into() })).into_response()
}

async fn activate<P: Plugin>(State(p): State<Arc<P>>, body: Result<Json<ActivateRequest>, axum::extract::rejection::JsonRejection>) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    match p.activate(&req.agent, req.config).await {
        Ok(()) => Json(json!({})).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn deactivate<P: Plugin>(State(p): State<Arc<P>>, body: Result<Json<DeactivateRequest>, axum::extract::rejection::JsonRejection>) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    p.deactivate(&req.agent).await;
    Json(json!({})).into_response()
}

async fn events<P: Plugin>(State(p): State<Arc<P>>, body: Result<Json<EventBatch>, axum::extract::rejection::JsonRejection>) -> Response {
    let Json(batch) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    p.observe(batch.events).await;
    Json(json!({})).into_response()
}

async fn intercept<P: Plugin>(State(p): State<Arc<P>>, body: Result<Json<InterceptRequest>, axum::extract::rejection::JsonRejection>) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    let verdict = p.intercept(req.event, req.response_so_far, req.deadline_ms).await;
    Json(verdict).into_response()
}

async fn health<P: Plugin>(State(p): State<Arc<P>>) -> Response {
    match p.health().await {
        Ok(()) => (StatusCode::OK, "ok").into_response(),
        Err(message) => error(StatusCode::SERVICE_UNAVAILABLE, message),
    }
}

async fn metrics<P: Plugin>(State(p): State<Arc<P>>) -> Response {
    ([(CONTENT_TYPE, "text/plain; version=0.0.4")], p.metrics().await).into_response()
}

/// A loopback listener on an ephemeral port and its `host:port`.
pub async fn bind() -> Result<(tokio::net::TcpListener, String), SdkError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| SdkError::Bind(e.to_string()))?;
    let listen = listener
        .local_addr()
        .map_err(|e| SdkError::Bind(e.to_string()))?
        .to_string();
    Ok((listener, listen))
}

/// Serves the router until the future is dropped.
pub async fn run<P: Plugin>(listener: tokio::net::TcpListener, plugin: Arc<P>) -> Result<(), SdkError> {
    axum::serve(listener, router(plugin))
        .into_future()
        .await
        .map_err(|e| SdkError::Bind(e.to_string()))
}

/// Bind, say hello, serve. Returns only on a bind or hello failure, or
/// when the server stops.
pub async fn serve<P: Plugin>(host: &Host, version: &str, plugin: P) -> Result<(), SdkError> {
    let (listener, listen) = bind().await?;
    let plugin = Arc::new(plugin);
    let server = tokio::spawn(run(listener, plugin));
    host.hello(version, &listen).await?;
    server
        .await
        .map_err(|e| SdkError::Bind(e.to_string()))?
}
```

`testing.rs` — `FakeHost` is an axum app holding `token`, `config`, `fleets`, and three `Mutex`es (`hellos`, `actions`, `kv: BTreeMap<String, (Vec<u8>, bool)>`), started on `127.0.0.1:0`; routes exactly as Task 7's, without capability gating: bearer must equal `token` else 401 `{ "error": "unknown plugin or bad token" }`; `hello` records and answers `{ config }`; `fleets` and `fleets/{name}` (404 `{ "error": "fleet not found" }`); `agents/{f}/{c}/{a}/actions` records `(f/c/a, action)` and answers `{}`; `kv` with `?prefix=`, `kv/{*key}` GET (raw bytes or 404 `{ "error": "no such key" }`), PUT (`?secret=true|false`, body raw) → `{}`, DELETE → `{}`. `fn env(&self, name, scratch) -> Env { api_url: self.url.clone(), name: name.into(), token: self.token.clone(), scratch: scratch.into() }`. Being a library module (not `cfg(test)`), it uses no `unwrap`; `Mutex` locks go through `unwrap_or_else(|e| e.into_inner())`.

`crates/balerix/src/commands/dev.rs` `fake_plugin_command`: replace the ureq-era `Host::new(env)` + `host.hello(..)?` with a `tokio::runtime::Runtime::new()?` and `rt.block_on(Host::new(env)?.hello(env!("CARGO_PKG_VERSION"), &listen))?` (Task 11 rewrites the whole command; this only keeps the workspace green).

- [ ] **Step 5: Run the tests**

Run: `mise x -- cargo test -p balerix-plugin-sdk && mise run check`
Expected: green. `cargo deny` / `mise run audit` still passes (reqwest's tree without TLS adds no new licence class; if `deny.toml` flags a crate, add it to the allow list in the same commit and say why).

- [ ] **Step 6: Commit**

```bash
git add crates/balerix-plugin-sdk crates/balerix/src/commands/dev.rs Cargo.lock
git commit -m "Give the plugin SDK its async Host and the Plugin half

Plugins spec §7: Host has a method per §4.1 route on reqwest; Plugin is
a trait with default no-op methods behind an axum router; serve binds,
says hello and runs. testing::FakeHost speaks the same wire format for
plugin unit tests. ureq leaves the SDK (§16.1).

Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe"
```

---

### Task 9: `docs/plugin-protocol.md`, fixtures, and the conformance tests

**Files:**
- Create: `docs/plugin-protocol.md`, `docs/plugin-protocol/*.json` (one file per fixture, listed below), `crates/balerix-plugin-sdk/tests/conformance.rs`, `crates/balerix-server/tests/protocol_it.rs`

**Interfaces:**
- Consumes: Task 8's `router`, `Host`, `FakeHost`; Task 3's `PluginClient`; Task 6's `testing::stub_plugin`.
- Produces: the fixture format every conformance test reads:

```json
{ "route": "POST /v1/intercept", "direction": "daemon-to-plugin",
  "request": { …body… }, "status": 200, "response": { …body… } }
```

  For `plugin-to-daemon` fixtures `request` is the body the plugin sends (or `null`) and `response` what the daemon answers; `raw` (base64) replaces `request`/`response` for the kv byte bodies.

- [ ] **Step 1: Write the fixtures**

`docs/plugin-protocol/` — twelve files, each exactly one JSON object:

| File | route | direction | request | status | response |
|---|---|---|---|---|---|
| `hello.json` | `POST /v1/plugin-host/hello` | plugin-to-daemon | `{"name":"flow","version":"0.1.0","protocol":1,"listen":"127.0.0.1:4000"}` | 200 | `{"config":{"greeting":"hi"}}` |
| `hello-bad-token.json` | same | plugin-to-daemon | same body, header `Authorization: Bearer wrong` (`"token":"wrong"` at top level) | 401 | `{"error":"unknown plugin or bad token"}` |
| `fleets.json` | `GET /v1/plugin-host/fleets` | plugin-to-daemon | `null` | 200 | `[{"spec":{"name":"payments","crews":{}},"generation":1,"desired":{"state":"up"},"stopped":[],"status":{"generation":1,"observed_generation":1,"phase":"ready","agents":{}}}]` |
| `fleet-missing.json` | `GET /v1/plugin-host/fleets/nope` | plugin-to-daemon | `null` | 404 | `{"error":"fleet not found"}` |
| `action.json` | `POST /v1/plugin-host/agents/payments/backend/bob/actions` | plugin-to-daemon | `{"action":"send_text","text":"Run the tests.","submit":true}` | 200 | `{}` |
| `kv-put.json` | `PUT /v1/plugin-host/kv/state/payments/backend/bob?secret=false` | plugin-to-daemon | `"raw":"d29ya2luZw=="` | 200 | `{}` |
| `kv-get.json` | `GET /v1/plugin-host/kv/state/payments/backend/bob` | plugin-to-daemon | `null` | 200 | `"raw":"d29ya2luZw=="` |
| `kv-list.json` | `GET /v1/plugin-host/kv?prefix=state/` | plugin-to-daemon | `null` | 200 | `{"keys":["state/payments/backend/bob"]}` |
| `activate.json` | `POST /v1/activate` | daemon-to-plugin | `{"agent":"payments/backend/bob","config":{"initial":"working"}}` | 200 | `{}` |
| `activate-rejected.json` | `POST /v1/activate` | daemon-to-plugin | `{"agent":"payments/backend/bad","config":{"initial":"nope"}}` | 400 | `{"error":"initial: unknown state \"nope\""}` |
| `events.json` | `POST /v1/events` | daemon-to-plugin | `{"events":[{"agent":"payments/backend/bob","name":"Stop","received_at":1757000000,"payload":{"hook_event_name":"Stop"}}]}` | 200 | `{}` |
| `intercept.json` | `POST /v1/intercept` | daemon-to-plugin | `{"event":{"agent":"payments/backend/bob","name":"PreToolUse","session_id":"s1","received_at":1757000000,"payload":{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /"}}},"response_so_far":{},"deadline_ms":1500}` | 200 | `{"response":{"decision":"block","reason":"no recursive deletes"},"actions":[{"action":"send_text","text":"Use trash instead.","submit":true}]}` |

Also `health.json` (`GET /v1/health`, `null`, 200, `"raw":"b2s="`) and `metrics.json` (`GET /v1/metrics`, `null`, 200, `"raw"` of `# TYPE balerix_plugin_flow_state gauge\nbalerix_plugin_flow_state{agent="bob",state="working"} 1\n`).

- [ ] **Step 2: Write the failing conformance tests**

`crates/balerix-plugin-sdk/tests/conformance.rs`:

```rust
//! The SDK against `docs/plugin-protocol/` (plugins spec §4, §10):
//! every daemon-to-plugin fixture through `router`, every plugin-to-daemon
//! fixture through `Host` against `FakeHost`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use balerix_api::{FleetRecord, HookEvent, InterceptResponse, PluginAction};
use balerix_plugin_sdk::testing::FakeHost;
use balerix_plugin_sdk::{Host, Plugin, bind, run};
use serde_json::{Value, json};

fn fixtures() -> BTreeMap<String, Value> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/plugin-protocol");
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let p: PathBuf = entry.unwrap().path();
        if p.extension().is_some_and(|e| e == "json") {
            let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
            out.insert(p.file_stem().unwrap().to_string_lossy().into_owned(), v);
        }
    }
    assert_eq!(out.len(), 14, "every fixture accounted for: {:?}", out.keys());
    out
}

fn b64(s: &str) -> Vec<u8> {
    // tiny base64 decoder: fixtures only carry short ASCII payloads
    let table: Vec<u8> = (b'A'..=b'Z').chain(b'a'..=b'z').chain(b'0'..=b'9').chain([b'+', b'/']).collect();
    let mut bits = 0u32;
    let mut n = 0;
    let mut out = Vec::new();
    for c in s.bytes().filter(|c| *c != b'=') {
        bits = (bits << 6) | table.iter().position(|t| *t == c).unwrap() as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
            bits &= (1 << n) - 1;
        }
    }
    out
}

/// The reference plugin the daemon-to-plugin fixtures describe.
struct Reference;
impl Plugin for Reference {
    async fn activate(&self, _agent: &str, config: Value) -> Result<(), String> {
        match config["initial"].as_str() {
            Some("working") => Ok(()),
            Some(other) => Err(format!("initial: unknown state {other:?}")),
            None => Err("initial: missing".into()),
        }
    }
    async fn intercept(&self, event: HookEvent, _so_far: Value, _deadline: u64) -> InterceptResponse {
        assert_eq!(event.payload["tool_input"]["command"], "rm -rf /");
        InterceptResponse {
            response: json!({ "decision": "block", "reason": "no recursive deletes" }),
            actions: vec![PluginAction::SendText { text: "Use trash instead.".into(), submit: true }],
        }
    }
    async fn metrics(&self) -> String {
        "# TYPE balerix_plugin_flow_state gauge\nbalerix_plugin_flow_state{agent=\"bob\",state=\"working\"} 1\n".into()
    }
}

#[tokio::test]
async fn the_router_answers_every_daemon_to_plugin_fixture() {
    let (listener, listen) = bind().await.unwrap();
    tokio::spawn(run(listener, Arc::new(Reference)));
    let c = reqwest::Client::builder().no_proxy().build().unwrap();
    for (name, f) in fixtures().iter().filter(|(_, f)| f["direction"] == "daemon-to-plugin") {
        let (method, path) = f["route"].as_str().unwrap().split_once(' ').unwrap();
        let url = format!("http://{listen}{path}");
        let req = match method {
            "POST" => c.post(&url).json(&f["request"]),
            "GET" => c.get(&url),
            _ => unreachable!(),
        };
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status().as_u16(), f["status"].as_u64().unwrap() as u16, "{name}");
        let bytes = resp.bytes().await.unwrap();
        match f.get("raw") {
            Some(raw) => assert_eq!(bytes.to_vec(), b64(raw.as_str().unwrap()), "{name}"),
            None => assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), f["response"], "{name}"),
        }
    }
}

#[tokio::test]
async fn the_host_sends_every_plugin_to_daemon_fixture_and_reads_the_answer() {
    let fx = fixtures();
    let record: FleetRecord = serde_json::from_value(fx["fleets"]["response"][0].clone()).unwrap();
    let fake = FakeHost::start("tok", json!({ "greeting": "hi" }), vec![record]).await;
    let host = Host::new(fake.env("flow", Path::new("/s"))).unwrap();

    let r = host.hello("0.1.0", "127.0.0.1:4000").await.unwrap();
    assert_eq!(serde_json::to_value(&r).unwrap(), fx["hello"]["response"]);
    assert_eq!(serde_json::to_value(&fake.hellos()[0]).unwrap(), fx["hello"]["request"]);
    let mut env = fake.env("flow", Path::new("/s"));
    env.token = fx["hello-bad-token"]["token"].as_str().unwrap().into();
    let e = Host::new(env).unwrap().hello("0.1.0", "127.0.0.1:4000").await.unwrap_err();
    assert_eq!(e.to_string(), format!("daemon: HTTP 401: {}", fx["hello-bad-token"]["response"]["error"].as_str().unwrap()));

    let fleets = host.fleets().await.unwrap();
    assert_eq!(serde_json::to_value(&fleets).unwrap(), fx["fleets"]["response"]);
    assert_eq!(host.fleet("nope").await.unwrap(), None);

    let action: PluginAction = serde_json::from_value(fx["action"]["request"].clone()).unwrap();
    host.action("payments/backend/bob", &action).await.unwrap();
    assert_eq!(fake.actions()[0], ("payments/backend/bob".to_string(), action));

    let bytes = b64(fx["kv-put"]["raw"].as_str().unwrap());
    host.kv_put("state/payments/backend/bob", &bytes, false).await.unwrap();
    assert_eq!(host.kv_get("state/payments/backend/bob").await.unwrap(), Some(b64(fx["kv-get"]["raw"].as_str().unwrap())));
    assert_eq!(serde_json::to_value(host.kv_list("state/").await.unwrap()).unwrap(), fx["kv-list"]["response"]["keys"]);
}
```

`crates/balerix-server/tests/protocol_it.rs`:

```rust
//! The daemon's client against the daemon-to-plugin fixtures: what
//! `PluginClient` puts on the wire is byte-for-byte the documented body.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use balerix_api::{ActivateRequest, EventBatch, InterceptRequest};
use balerix_server::PluginClient;
use balerix_server::testing::{StubScript, stub_plugin};
use serde_json::{Value, json};

fn fixture(name: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/plugin-protocol").join(format!("{name}.json"));
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

#[tokio::test]
async fn the_client_sends_the_documented_bodies() {
    let stub = stub_plugin(StubScript {
        reject: BTreeMap::from([("payments/backend/bad".to_string(), "initial: unknown state \"nope\"".to_string())]),
        verdict: json!({ "decision": "block", "reason": "no recursive deletes" }),
        actions: vec![balerix_api::PluginAction::SendText { text: "Use trash instead.".into(), submit: true }],
        health_ok: true,
        ..StubScript::default()
    })
    .await;
    let c = PluginClient::new().unwrap();

    let f = fixture("activate");
    let req: ActivateRequest = serde_json::from_value(f["request"].clone()).unwrap();
    c.activate(&stub.listen, &req).await.unwrap();
    assert_eq!(stub.calls_named("activate")[0], f["request"]);

    let f = fixture("activate-rejected");
    let req: ActivateRequest = serde_json::from_value(f["request"].clone()).unwrap();
    let e = c.activate(&stub.listen, &req).await.unwrap_err();
    assert_eq!(e.to_string(), format!("HTTP 400: {}", f["response"]["error"].as_str().unwrap()));

    let f = fixture("events");
    let batch: EventBatch = serde_json::from_value(f["request"].clone()).unwrap();
    c.events(&stub.listen, &batch).await.unwrap();
    assert_eq!(stub.calls_named("events")[0], f["request"]);

    let f = fixture("intercept");
    let req: InterceptRequest = serde_json::from_value(f["request"].clone()).unwrap();
    let v = c.intercept(&stub.listen, &req, Duration::from_secs(1)).await.unwrap();
    assert_eq!(stub.calls_named("intercept")[0], f["request"]);
    assert_eq!(serde_json::to_value(&v).unwrap(), f["response"]);
}
```

`crates/balerix-server/Cargo.toml` `[dev-dependencies]`: `balerix-plugin-sdk = { workspace = true }` (used by Task 10; add it here so both test files build), and `reqwest` is needed by the SDK conformance test's `Cargo.toml` `[dev-dependencies]`: `reqwest = { workspace = true }` is already a normal dependency of the SDK, nothing to add.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-plugin-sdk --test conformance && mise x -- cargo test -p balerix-server --test protocol_it`
Expected: the fixture directory is missing → the first assertion (count) fails; write the fixtures, then run again and fix whatever body differs — a difference here is a bug in the type, the doc, or the fixture, and the fixture wins only when the spec agrees with it.

- [ ] **Step 4: Write `docs/plugin-protocol.md`**

Sections, in this order, each stating the exact JSON fields with types and the status codes; the fixture file names are cited beside each route:

1. **Scope and versioning** — protocol `1`; `hello.protocol` must match; both directions are JSON over HTTP/1.1 on loopback; every body is an object except where a raw body is stated; every non-2xx is `{ "error": "<message>" }`; bodies are capped at 1 MiB.
2. **Environment** — the four `BALERIX_*` variables (§5.1), what `BALERIX_API_URL` looks like, that the token is the bearer for every plugin → daemon call.
3. **Plugin → daemon** (`/v1/plugin-host/`): a table of route, capability, request, response, statuses, fixture. `hello` (always), `fleets` and `fleets/{name}` (`fleets`), `agents/{fleet}/{crew}/{agent}/actions` with the three action shapes (`actions`; 404 when not active), `kv` (`kv`; key grammar, `?secret=true`, raw bodies, `?prefix=`), 401 and 403 semantics.
4. **Daemon → plugin** (at the `listen` address): `activate` (a non-2xx rejects the agent, the message is shown to the operator as `crews.<c>.agents.<a>.plugins.<name>: <message>`), `deactivate`, `events` (batch bounds, ordering, no catch-up), `intercept` (`response_so_far`, `deadline_ms`, the verdict shape, that the response must be an object, the four failure classes and fail-open), `health`, `metrics` (the prefix rule).
5. **Activation lifecycle** — pending / active / rejected as the operator sees them, re-activation after every `hello`, deactivation on `down`, removal and config change.
6. **Conformance** — how `docs/plugin-protocol/*.json` is laid out and which tests replay it (`crates/balerix-plugin-sdk/tests/conformance.rs`, `crates/balerix-server/tests/protocol_it.rs`).

Under 300 lines. No prose that the tests do not enforce: if a sentence names a status code or a field, a fixture carries it.

- [ ] **Step 5: Run everything**

Run: `mise run check`
Expected: green.

- [ ] **Step 6: Commit**

```bash
git add docs/plugin-protocol.md docs/plugin-protocol crates/balerix-plugin-sdk/tests crates/balerix-server/tests/protocol_it.rs crates/balerix-server/Cargo.toml Cargo.lock
git commit -m "Document the plugin protocol and pin it with conformance fixtures

docs/plugin-protocol.md is the contract for plugins in any language
(plugins spec §4, §11); the JSON fixtures under docs/plugin-protocol/ are
replayed through the SDK router and Host and through the daemon's client,
so the doc, the SDK and the server cannot drift apart.

Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe"
```

---

### Task 10: `balerix-server` — integration: an SDK plugin against the daemon over HTTP

**Files:**
- Create: `crates/balerix-server/tests/support/mod.rs`, `crates/balerix-server/tests/events_it.rs`

**Interfaces:**
- Consumes: everything above. `support::Api` is the `Api` helper of `api_it.rs` (same `call` signature) plus `fn plugin(&self, token: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, Value)` and `fn raw_put(&self, token: &str, path: &str, bytes: &[u8]) -> (u16, Value)` / `fn raw_get(&self, token: &str, path: &str) -> (u16, Vec<u8>)`; `support::world(handler_kind: HandlerKind) -> World` builds a daemon with `PluginEventHandler` over the harness and serves it on a port, returning `World { api, daemon, h: Harness, dir: TempDir, stop }`.
- Produces: the spec §11 "server integration" rows for phase 2a, as one test file with four tests.

- [ ] **Step 1: Write the tests**

`support/mod.rs` extracts the `Api` struct from `api_it.rs` (the old files keep their private copies; do not touch them) with `pub base: String`, `pub fn new(base: String, token: &str) -> Api` (a `ureq::Agent` with `http_status_as_error(false)` and a 5 s global timeout), the existing `call`/`admin`, plus `plugin`, `raw_put`, `raw_get` as listed above, and adds:

```rust
pub struct World {
    pub api: Api,
    pub daemon: Arc<Daemon>,
    pub h: Harness,
    pub dir: tempfile::TempDir,
    pub stop: Option<tokio::sync::oneshot::Sender<()>>,
}

/// A daemon with the chain handler, served on a port, with one package
/// `flow` declared (intercepts PreToolUse+Stop, observes Stop, needs
/// actions+kv) and one package `web` (observes SessionStart, needs fleets).
pub async fn world() -> World {
    let h = Harness::new(Duration::from_secs(3600));
    let dir = tempfile::tempdir().unwrap();
    write_plugin_package(&dir.path().join("flow-pkg"), "flow", "hooks: { intercept: [PreToolUse, Stop], observe: [Stop] }\nneeds: [actions, kv]\n");
    write_plugin_package(&dir.path().join("web-pkg"), "web", "hooks: { observe: [SessionStart] }\nneeds: [fleets]\n");
    std::fs::write(dir.path().join("plugins.yaml"), "plugins:\n  - name: flow\n    source: ./flow-pkg\n  - name: web\n    source: ./web-pkg\n").unwrap();
    let handler = PluginEventHandler::new(h.registry.clone(), h.client.clone(), Metrics::new().unwrap());
    let daemon = h.daemon(handler, dir.path());
    daemon.sync_plugins().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(listener, router(daemon.clone()), async { let _ = rx.await; }));
    World { api: Api::new(base, "admin-tok"), daemon, h, dir, stop: Some(stop) }
}
```

`events_it.rs` — an SDK plugin implemented in the test:

```rust
//! Plugins spec §11 "server integration", phase 2a: a plugin built on the
//! SDK, in-process, against the daemon over a real listener with the fakes
//! behind it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use balerix_api::{ActivationState, AgentPhase, AgentSettings, CrewSpec, FleetRequest, FleetSpec, GitSettings, HookEvent, InterceptResponse, PluginAction};
use balerix_core::plugin_id;
use balerix_plugin_sdk::{Env, Host, Plugin, bind, run};
use serde_json::{Value, json};
use support::{World, world};

#[derive(Default)]
struct FlowLike {
    activations: Mutex<Vec<(String, Value)>>,
    deactivations: Mutex<Vec<String>>,
    observed: Mutex<Vec<HookEvent>>,
    slow: Mutex<bool>,
}

impl Plugin for FlowLike {
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        if config.get("reject").is_some() {
            return Err(format!("states: rejected for {agent}"));
        }
        self.activations.lock().unwrap().push((agent.to_string(), config));
        Ok(())
    }
    async fn deactivate(&self, agent: &str) {
        self.deactivations.lock().unwrap().push(agent.to_string());
    }
    async fn observe(&self, events: Vec<HookEvent>) {
        self.observed.lock().unwrap().extend(events);
    }
    async fn intercept(&self, event: HookEvent, mut so_far: Value, _deadline: u64) -> InterceptResponse {
        if *self.slow.lock().unwrap() {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        let mut actions = Vec::new();
        if event.name == "PreToolUse" && event.payload["tool_input"]["command"].as_str().is_some_and(|c| c.starts_with("rm -rf")) {
            so_far["decision"] = json!("block");
            so_far["reason"] = json!("no recursive deletes");
        }
        if event.name == "Stop" {
            actions.push(PluginAction::SendText { text: "Run the tests.".into(), submit: true });
        }
        InterceptResponse { response: so_far, actions }
    }
}

fn spec(agents: &[(&str, Value)]) -> FleetSpec {
    FleetSpec {
        name: "f".into(),
        crews: BTreeMap::from([("c".to_string(), CrewSpec {
            repo: "acme/x".into(), git_ref: "main".into(), git: GitSettings::default(),
            agents: agents.iter().map(|(n, cfg)| {
                let mut s = AgentSettings::default();
                if !cfg.is_null() { s.plugins.insert("flow".into(), cfg.clone()); }
                (n.to_string(), s)
            }).collect(),
        })]),
    }
}

async fn token(w: &World, plugin: &str) -> String {
    w.daemon.hook_secret(&plugin_id(&plugin.parse().unwrap())).await.unwrap()
}

/// Starts the SDK plugin on a port and says hello with its real token.
async fn start_flow(w: &World) -> (Arc<FlowLike>, Host) {
    let plugin = Arc::new(FlowLike::default());
    let (listener, listen) = bind().await.unwrap();
    tokio::spawn(run(listener, plugin.clone()));
    let env = Env { api_url: w.api.base.clone(), name: "flow".into(), token: token(w, "flow").await, scratch: w.dir.path().join("scratch") };
    let host = Host::new(env).unwrap();
    host.hello("0.1.0", &listen).await.unwrap();
    (plugin, host)
}

async fn wait_for(w: &World, pred: impl Fn(&balerix_api::FleetRecord) -> bool) -> balerix_api::FleetRecord {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(r) = w.daemon.get(&"f".parse().unwrap()).await && pred(&r) { return r; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("condition not reached")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activation_pending_then_active_rejection_fails_up_and_down_deactivates() {
    let w = world().await;
    // pending: the plugin has not said hello
    let req = json!(FleetRequest { spec: spec(&[("a", json!({ "v": 1 }))]), credentials: Default::default() });
    let (s, v) = w.api.admin("POST", "/v1/fleets", Some(&req));
    assert_eq!(s, 200, "{v}");
    let rec = wait_for(&w, |r| r.status.agents.contains_key("f/c/a")).await;
    assert_eq!(rec.status.agents["f/c/a"].plugins["flow"].state, ActivationState::Pending);
    let (_, rows) = w.api.admin("GET", "/v1/plugins", None);
    assert_eq!(rows[0]["active_agents"], 0);

    let (plugin, _host) = start_flow(&w).await;
    let rec = wait_for(&w, |r| r.status.agents["f/c/a"].plugins["flow"].state == ActivationState::Active).await;
    assert_eq!(rec.status.agents["f/c/a"].plugins["flow"].message, "");
    assert_eq!(plugin.activations.lock().unwrap()[0], ("f/c/a".to_string(), json!({ "v": 1 })));
    let (_, rows) = w.api.admin("GET", "/v1/plugins", None);
    assert_eq!(rows[0]["active_agents"], 1);

    // a rejected activation fails the update with the config path
    let bad = json!(FleetRequest { spec: spec(&[("a", json!({ "v": 1 })), ("b", json!({ "reject": true }))]), credentials: Default::default() });
    let (s, v) = w.api.admin("PUT", "/v1/fleets/f", Some(&bad));
    assert_eq!(s, 400);
    assert_eq!(v["error"], "crews.c.agents.b.plugins.flow: states: rejected for f/c/b");
    assert_eq!(w.daemon.get(&"f".parse().unwrap()).await.unwrap().generation, 1, "nothing landed");

    // an unknown plugin is refused before anything
    let mut s2 = spec(&[("a", Value::Null)]);
    s2.crews.get_mut("c").unwrap().agents.get_mut("a").unwrap().plugins.insert("nope".into(), json!({}));
    let (s, v) = w.api.admin("PUT", "/v1/fleets/f", Some(&json!(FleetRequest { spec: s2, credentials: Default::default() })));
    assert_eq!((s, v["error"].as_str().unwrap()), (400, "crews.c.agents.a.plugins.nope: no plugin \"nope\" is installed"));

    // down deactivates
    let (s, _) = w.api.admin("DELETE", "/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=false", None);
    assert_eq!(s, 200);
    tokio::time::timeout(Duration::from_secs(2), async {
        while plugin.deactivations.lock().unwrap().is_empty() { tokio::time::sleep(Duration::from_millis(10)).await; }
    }).await.unwrap();
    assert_eq!(plugin.deactivations.lock().unwrap()[0], "f/c/a");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_chain_blocks_observers_see_and_actions_reach_the_runner() {
    let w = world().await;
    let (plugin, _host) = start_flow(&w).await;
    let req = json!(FleetRequest { spec: spec(&[("a", json!({}))]), credentials: Default::default() });
    let (s, _) = w.api.admin("POST", "/v1/fleets", Some(&req));
    assert_eq!(s, 200);
    wait_for(&w, |r| r.status.observed_generation == 1).await;
    let secret = w.daemon.hook_secret(&"f/c/a".parse().unwrap()).await.unwrap();
    let hook = |name: &str, extra: Value| {
        let mut body = json!({ "hook_event_name": name, "session_id": "s1" });
        if let (Some(dst), Some(src)) = (body.as_object_mut(), extra.as_object()) { for (k, v) in src { dst.insert(k.clone(), v.clone()); } }
        body
    };
    let (s, v) = w.api.call("POST", "/v1/agents/f/c/a/events", Some(&secret), Some(&hook("PreToolUse", json!({ "tool_name": "Bash", "tool_input": { "command": "rm -rf /" } }))));
    assert_eq!((s, v), (200, json!({ "decision": "block", "reason": "no recursive deletes" })));
    let (s, v) = w.api.call("POST", "/v1/agents/f/c/a/events", Some(&secret), Some(&hook("PreToolUse", json!({ "tool_input": { "command": "ls" } }))));
    assert_eq!((s, v), (200, json!({})));
    let (s, v) = w.api.call("POST", "/v1/agents/f/c/a/events", Some(&secret), Some(&hook("Stop", json!({}))));
    assert_eq!((s, v), (200, json!({})), "the response is written before the action runs");
    tokio::time::timeout(Duration::from_secs(2), async {
        while !w.h.runner.calls().contains(&"send_text f/c/a \"Run the tests.\" submit=true".to_string()) { tokio::time::sleep(Duration::from_millis(10)).await; }
    }).await.expect("send_text reached the runner");
    tokio::time::timeout(Duration::from_secs(2), async {
        while plugin.observed.lock().unwrap().is_empty() { tokio::time::sleep(Duration::from_millis(10)).await; }
    }).await.expect("the observer batch arrived");
    assert_eq!(plugin.observed.lock().unwrap()[0].name, "Stop");
    // timeout fail-open
    *plugin.slow.lock().unwrap() = true;
    let started = std::time::Instant::now();
    let (s, v) = w.api.call("POST", "/v1/agents/f/c/a/events", Some(&secret), Some(&hook("PreToolUse", json!({ "tool_input": { "command": "rm -rf /" } }))));
    assert_eq!((s, v), (200, json!({})), "slow plugin: allowed");
    assert!(started.elapsed() < Duration::from_secs(2));
    let (_, m) = w.api.call("GET", "/metrics", None, None);
    let m = m.as_str().unwrap();
    assert!(m.contains("balerix_plugin_intercept_failures_total{plugin=\"flow\",reason=\"timeout\"} 1"), "{m}");
    assert!(m.contains("balerix_plugin_events_total{event=\"PreToolUse\",mode=\"intercept\",plugin=\"flow\"} 3"));
    assert!(m.contains("balerix_hook_actions_total{action=\"send_text\",agent=\"a\",crew=\"c\",fleet=\"f\"} 1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_routes_are_gated_by_needs_and_kv_and_actions_work() {
    let w = world().await;
    let (_plugin, host) = start_flow(&w).await;
    let (s, _) = w.api.admin("POST", "/v1/fleets", Some(&json!(FleetRequest { spec: spec(&[("a", json!({}))]), credentials: Default::default() })));
    assert_eq!(s, 200);
    wait_for(&w, |r| r.status.observed_generation == 1).await;
    // flow has kv + actions, not fleets
    let e = host.fleets().await.unwrap_err();
    assert_eq!(e.to_string(), "daemon: HTTP 403: capability \"fleets\" not declared in balerix-plugin.yaml");
    host.kv_put("state/f/c/a", b"working", false).await.unwrap();
    host.kv_put("tok", b"s3cret", true).await.unwrap();
    assert_eq!(host.kv_get("state/f/c/a").await.unwrap().as_deref(), Some(&b"working"[..]));
    assert_eq!(host.kv_get("tok").await.unwrap().as_deref(), Some(&b"s3cret"[..]));
    assert_eq!(host.kv_list("state/").await.unwrap(), vec!["state/f/c/a".to_string()]);
    let on_disk = std::fs::read(w.h.kv_dir.path().join("plugins/flow/kv/tok")).unwrap();
    assert!(!on_disk.windows(6).any(|x| x == b"s3cret"), "sealed on disk");
    let e = host.kv_get("../x").await.unwrap_err();
    assert!(e.to_string().starts_with("daemon: HTTP 400: kv: invalid key"), "{e}");
    host.kv_delete("tok").await.unwrap();
    assert_eq!(host.kv_get("tok").await.unwrap(), None);
    host.action("f/c/a", &PluginAction::Stop).await.unwrap();
    let rec = wait_for(&w, |r| r.status.agents["f/c/a"].phase == AgentPhase::Stopped).await;
    assert!(rec.stopped.contains("f/c/a"));
    host.action("f/c/a", &PluginAction::Restart).await.unwrap();
    wait_for(&w, |r| r.stopped.is_empty() && r.status.agents["f/c/a"].phase == AgentPhase::Starting).await;
    let e = host.action("f/c/b", &PluginAction::Stop).await.unwrap_err();
    assert_eq!(e.to_string(), "daemon: HTTP 404: plugin is not active for agent f/c/b");
    // web has fleets, and sees the overlay
    let web_token = token(&w, "web").await;
    let (s, v) = w.api.plugin(&web_token, "GET", "/v1/plugin-host/fleets", None);
    assert_eq!(s, 200);
    assert_eq!(v[0]["status"]["agents"]["f/c/a"]["plugins"]["flow"]["state"], "active");
    let (s, _) = w.api.plugin(&web_token, "GET", "/v1/plugin-host/fleets/balerix", None);
    assert_eq!(s, 404);
    let (s, _) = w.api.plugin(&web_token, "GET", "/v1/plugin-host/kv?prefix=", None);
    assert_eq!(s, 403);
    let (s, _) = w.api.plugin("nope", "GET", "/v1/plugin-host/fleets", None);
    assert_eq!(s, 401);
    let (_, m) = w.api.call("GET", "/metrics", None, None);
    assert!(m.as_str().unwrap().contains("balerix_plugin_actions_total{action=\"stop\",plugin=\"flow\"} 1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_metrics_are_re_exported_under_the_prefix_rule() {
    let w = world().await;
    struct Good;
    impl Plugin for Good { async fn metrics(&self) -> String { "balerix_plugin_flow_state{agent=\"a\"} 1\n".into() } }
    struct Bad;
    impl Plugin for Bad { async fn metrics(&self) -> String { "balerix_agents{fleet=\"spoof\"} 9\n".into() } }
    let (l1, listen1) = bind().await.unwrap();
    tokio::spawn(run(l1, Arc::new(Good)));
    let (l2, listen2) = bind().await.unwrap();
    tokio::spawn(run(l2, Arc::new(Bad)));
    for (name, listen) in [("flow", listen1), ("web", listen2)] {
        let env = Env { api_url: w.api.base.clone(), name: name.into(), token: token(&w, name).await, scratch: w.dir.path().join("s") };
        Host::new(env).unwrap().hello("0.1.0", &listen).await.unwrap();
    }
    let (_, m) = w.api.call("GET", "/metrics", None, None);
    let m = m.as_str().unwrap();
    assert!(m.contains("balerix_plugin_flow_state{agent=\"a\"} 1"), "{m}");
    assert!(!m.contains("spoof"));
    let (_, m) = w.api.call("GET", "/metrics", None, None);
    assert!(m.as_str().unwrap().contains("balerix_plugin_metrics_scrape_failures_total{plugin=\"web\"} 1"));
}
```

- [ ] **Step 2: Run the tests**

Run: `mise x -- cargo test -p balerix-server --test events_it`
Expected: green on the first run if Tasks 1–9 are right. Each failure names the task to fix: an activation status → Task 6, a response body → Task 5, a route status → Task 7, an SDK error string → Task 8. Do not adjust these assertions to the code; they are the spec's rows.

- [ ] **Step 3: Full check and commit**

Run: `mise run check`
Expected: green.

```bash
git add crates/balerix-server/tests
git commit -m "Test the event protocol end to end with an SDK plugin in-process

Plugins spec §11 server-integration rows for phase 2a: pending then
active at hello, a rejection failing the update, the chain blocking,
observers, actions through the runner and the actor, kv with a sealed
secret, needs gating, and the metrics prefix rule.

Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe"
```

---

### Task 11: `balerix` binary — `serve` wiring, `up` waits on activations, columns, `dev fake-plugin`, `fake-claude`

**Files:**
- Modify: `crates/balerix/src/commands/serve.rs:150-181`, `crates/balerix/src/commands/fleet.rs` (`render_status`, `wait_until`, `apply`), `crates/balerix/src/commands/plugin.rs` (`render_plugins` and its test), `crates/balerix/src/commands/dev.rs` (`fake_plugin_command`, `fake_claude_once`), `crates/balerix/src/cli.rs:236-241` (doc comments), `crates/balerix/tests/cli_fleet.rs` (rendered table expectations, if any include the header)

**Interfaces:**
- Consumes: Tasks 6–8.
- Produces:
  - `serve` builds `PluginRegistry::new()`, `PluginClient::new()`, `PluginKv::new(layout.plugins_state_dir(), vault.clone())`, and `PluginEventHandler::new(registry.clone(), client.clone(), metrics.clone())` as the daemon's handler.
  - `balerix up`/`update` wait until the fleet is `Ready` **and** every `plugins` row of every agent is `active`; a `rejected` row fails at once with `crews.<c>.agents.<a>.plugins.<p>: <message>`; the timeout message lists the pending rows through the table.
  - `render_status` gains a `PLUGINS` column: `flow=active,web=pending` (comma-joined, sorted by name; empty when none). `render_plugins` gains `ACTIVE` (the `active_agents` count) between `ROUTES` and `MESSAGE`.
  - `balerix dev fake-plugin`: an SDK `Plugin` that intercepts `PreToolUse` (block when `tool_input.command` starts with `rm -rf`, reason `fake-plugin: no recursive deletes`) and `Stop` (action `send_text` `"fake-plugin says hi"`, `submit: true`), observes everything into `$BALERIX_PLUGIN_SCRATCH/events.jsonl` (one `HookEvent` per line), records activations into `$BALERIX_PLUGIN_SCRATCH/activations.jsonl` (`{ "agent", "config" }` per line), rejects a config with `"reject": true`, and still writes `fake-plugin.hello` after `hello`. Its manifest needs are the e2e's business (Task 12); the binary just implements the trait.
  - `balerix dev fake-claude`: after the `SessionStart` relay and the `Notification` hook, posts a `PreToolUse` (`tool_name: Bash`, `tool_input.command: "rm -rf /tmp/x"`) and a `Stop` to their HTTP hooks and writes each reply body to `$HOME/fake-claude.<event>.reply`; a thread reads stdin line by line for the rest of the process's life and appends every line to `$HOME/fake-claude.stdin`.

- [ ] **Step 1: Write the failing tests**

`fleet.rs` tests: extend `status_and_list_render_aligned_tables` — give alice `plugins: { flow: active }` and bob `{ flow: rejected("bad regex"), web: pending }` (through `PluginActivation::active()` etc.) and expect:

```text
payments  degraded  generation 3 (observed 3)
AGENT                   PHASE     RESTARTS  PLUGINS                    MESSAGE
payments/backend/alice  ready     0         flow=active
payments/backend/bob    starting  1         flow=rejected,web=pending  exited with status 1
```

and a new test for the wait predicate, extracted as `pub(crate) fn ready_check(r: &FleetRecord) -> Result<bool>`:

```rust
    #[test]
    fn ready_needs_every_activation_active_and_a_rejection_fails() {
        let mut r = FleetRecord::new(FleetSpec { name: "p".into(), crews: BTreeMap::new() });
        r.generation = 1;
        r.status.generation = 1;
        r.status.observed_generation = 1;
        r.status.phase = FleetPhase::Ready;
        r.status.entry("p/c/a").phase = AgentPhase::Ready;
        assert!(ready_check(&r).unwrap());
        r.status.entry("p/c/a").plugins.insert("flow".into(), PluginActivation::pending());
        assert!(!ready_check(&r).unwrap(), "pending holds");
        r.status.entry("p/c/a").plugins.insert("flow".into(), PluginActivation::active());
        assert!(ready_check(&r).unwrap());
        r.status.entry("p/c/a").plugins.insert("web".into(), PluginActivation::rejected("initial: unknown state \"x\""));
        assert_eq!(
            ready_check(&r).unwrap_err().to_string(),
            "crews.c.agents.a.plugins.web: initial: unknown state \"x\""
        );
        r.status.phase = FleetPhase::Reconciling;
        r.status.entry("p/c/a").plugins.clear();
        assert!(!ready_check(&r).unwrap());
    }
```

`plugin.rs` test `renders_the_plugin_table_and_sync_report`: give `flow` `active_agents: 2` and expect

```text
NAME  VERSION  PHASE     LISTEN          ROUTES  ACTIVE  MESSAGE
flow  0.1.0    ready     127.0.0.1:4000  no      2
web   0.2.0    starting  -               yes     0       exited with status 1
```

`dev.rs` test `fake_claude_runs_command_hooks_posts_one_http_hook_and_records_argv` becomes `…_posts_three_http_hooks_records_replies_and_reads_stdin`: the stub answers three requests (extend `crate::testutil::stub_server` with `stub_server_n(n, status, body)` that accepts `n` connections on one listener and sends each request over the channel); settings carry HTTP hooks for `Notification`, `PreToolUse` and `Stop`; assert the three requests arrived in that order with the right `hook_event_name`, that `PreToolUse`'s body has `tool_input.command` starting with `rm -rf`, that `$HOME/fake-claude.PreToolUse.reply` and `fake-claude.Stop.reply` contain the stub's body, and — with `fake_claude_once` given an `impl Read` for stdin (new parameter; the command passes `std::io::stdin()`) — that `fake-claude.stdin` holds the lines fed in. Make the stdin reader a function `pub fn pump_stdin(input: impl Read + Send + 'static, out: PathBuf) -> std::thread::JoinHandle<()>` so the test can feed a `Cursor` and join the thread.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix --lib`
Expected: the three render/check tests fail on the missing column and function; the fake-claude test fails to compile.

- [ ] **Step 3: Implement**

`serve.rs` `run()`:

```rust
    let vault = Vault::load_or_create(&paths.vault_key())?;
    let store = FileFleetStore::new(layout.fleets_dir(), vault.clone());
    …
        let metrics = Metrics::new()?;
        let registry = PluginRegistry::new();
        let client = PluginClient::new().map_err(|e| anyhow!("plugins: {e}"))?;
        let kv = Arc::new(PluginKv::new(layout.plugins_state_dir(), vault.clone()));
        let handler = PluginEventHandler::new(registry.clone(), client.clone(), metrics.clone());
        let daemon = Daemon::start(
            ports,
            handler,
            metrics,
            token,
            existing,
            PluginHostConfig { … },
            registry,
            client,
            kv,
        );
```

(imports: `balerix_server::{PluginClient, PluginEventHandler, PluginKv, PluginRegistry}`; drop `PassThrough`.)

`fleet.rs`:

```rust
fn plugins_cell(a: &AgentStatus) -> String {
    a.plugins
        .iter()
        .map(|(name, p)| format!("{name}={}", label(p.state)))
        .collect::<Vec<_>>()
        .join(",")
}
```

`render_status` rows become `[id, phase, restarts, plugins_cell(a), message]` with header `["AGENT", "PHASE", "RESTARTS", "PLUGINS", "MESSAGE"]`.

```rust
/// `up`/`update` are done when the fleet is Ready and every plugin
/// activation is active (plugins spec §16.3). A rejected activation is an
/// error right away, in the same config-path style the daemon uses.
pub(crate) fn ready_check(r: &FleetRecord) -> Result<bool> {
    for (id, a) in &r.status.agents {
        for (name, p) in &a.plugins {
            if p.state == ActivationState::Rejected {
                let (crew, agent) = id
                    .rsplit_once('/')
                    .and_then(|(rest, agent)| rest.rsplit_once('/').map(|(_, crew)| (crew, agent)))
                    .unwrap_or(("?", id.as_str()));
                bail!("crews.{crew}.agents.{agent}.plugins.{name}: {}", p.message);
            }
        }
    }
    Ok(r.status.observed_generation == r.generation
        && r.status.phase == FleetPhase::Ready
        && r
            .status
            .agents
            .values()
            .all(|a| a.plugins.values().all(|p| p.state == ActivationState::Active)))
}
```

`wait_until`'s `done` becomes `impl Fn(&FleetRecord) -> Result<bool>`; inside the loop `if done(&record)? { … }` (so a rejection propagates as the command's error, after the phase-change lines were printed). `apply` passes `ready_check`; `down_command` passes `|r| Ok(r.status.phase == FleetPhase::Down)`. The "timed out" message already renders the table, and the table now carries the `PLUGINS` column, so pending rows are visible.

`plugin.rs` `render_plugins`: rows become 7-wide with `p.active_agents.to_string()` before the message; header `["NAME", "VERSION", "PHASE", "LISTEN", "ROUTES", "ACTIVE", "MESSAGE"]`.

`dev.rs` `fake_plugin_command`:

```rust
/// `balerix dev fake-plugin`: the e2e's plugin, on the full SDK. Blocks
/// `rm -rf` at PreToolUse, answers Stop with a send_text, observes
/// everything into scratch, records activations and the hello reply.
pub fn fake_plugin_command() -> Result<String> {
    use balerix_plugin_sdk::{Env, Host, bind, run};
    let env = Env::from_process()?;
    let scratch = env.scratch.clone();
    std::fs::create_dir_all(&scratch)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let host = Host::new(env)?;
        let (listener, listen) = match bind().await {
            Ok(b) => b,
            Err(e) => {
                // spec §11.1 row 1 stays observable: record the verdict and
                // still say hello so the e2e reports it instead of hanging
                std::fs::write(scratch.join("fake-plugin.bind-failed"), e.to_string())?;
                anyhow::bail!("fake-plugin: cannot bind a loopback listener: {e}");
            }
        };
        let plugin = Arc::new(FakePlugin { scratch: scratch.clone() });
        let server = tokio::spawn(run(listener, plugin));
        let resp = host.hello(env!("CARGO_PKG_VERSION"), &listen).await?;
        std::fs::write(scratch.join("fake-plugin.hello"), serde_json::to_string_pretty(&resp.config)?)?;
        eprintln!("fake-plugin: hello acknowledged; listening on {listen}");
        server.await??;
        Ok::<String, anyhow::Error>(String::new())
    })
}

struct FakePlugin {
    scratch: PathBuf,
}

impl FakePlugin {
    fn append(&self, file: &str, line: &Value) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.scratch.join(file)) {
            let _ = writeln!(f, "{line}");
        }
    }
}

impl balerix_plugin_sdk::Plugin for FakePlugin {
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        if config.get("reject").is_some() {
            return Err(format!("fake-plugin: rejected by config for {agent}"));
        }
        self.append("activations.jsonl", &json!({ "agent": agent, "config": config }));
        Ok(())
    }
    async fn observe(&self, events: Vec<balerix_api::HookEvent>) {
        for e in events {
            if let Ok(v) = serde_json::to_value(&e) {
                self.append("events.jsonl", &v);
            }
        }
    }
    async fn intercept(&self, event: balerix_api::HookEvent, mut so_far: Value, _deadline_ms: u64) -> balerix_api::InterceptResponse {
        let mut actions = Vec::new();
        match event.name.as_str() {
            "PreToolUse" if event.payload["tool_input"]["command"].as_str().is_some_and(|c| c.starts_with("rm -rf")) => {
                so_far["decision"] = json!("block");
                so_far["reason"] = json!("fake-plugin: no recursive deletes");
            }
            "Stop" => actions.push(balerix_api::PluginAction::SendText { text: "fake-plugin says hi".into(), submit: true }),
            _ => {}
        }
        balerix_api::InterceptResponse { response: so_far, actions }
    }
}
```

`fake_claude_once(config_dir, home, argv, stdin: impl Read + Send + 'static)`: after the `SessionStart` loop, start `pump_stdin(stdin, home.join("fake-claude.stdin"))`, then post the HTTP hooks for `Notification`, `PreToolUse` and `Stop` in that order through one helper `post_hook(settings, event, payload) -> Option<String>` (the existing ureq code, returning the reply body), writing `home.join(format!("fake-claude.{event}.reply"))` for `PreToolUse` and `Stop`. Payloads: `PreToolUse` → `{ "hook_event_name": "PreToolUse", "session_id": "fake-claude", "tool_name": "Bash", "tool_input": { "command": "rm -rf /tmp/x" } }`; `Stop` → `{ "hook_event_name": "Stop", "session_id": "fake-claude", "stop_hook_active": false }`. `fake_claude_command` keeps the pump's `JoinHandle` alive by never returning (it sleeps forever, as today).

```rust
/// Reads `input` line by line until EOF, appending each line to `out`.
/// This is how the e2e sees what tmux `send-keys` delivered.
pub fn pump_stdin(input: impl Read + Send + 'static, out: PathBuf) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Write};
        let reader = BufReader::new(input);
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&out) {
                let _ = writeln!(f, "{line}");
            }
        }
    })
}
```

`cli.rs` doc comments: `FakePlugin` — "Stand-in plugin for the e2e on the SDK: blocks `rm -rf`, answers Stop with send_text, observes into scratch."; `FakeClaude` — "…runs the SessionStart command hooks, posts Notification, PreToolUse and Stop to their HTTP hooks, records the replies, echoes stdin to $HOME/fake-claude.stdin, then sleeps."

- [ ] **Step 4: Run the tests**

Run: `mise x -- cargo test -p balerix && mise run check`
Expected: green. `cli_fleet.rs` and `cli_plugin.rs` may assert full tables; update those expectations to the new columns only where they render `status` or `plugin list`.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix
git commit -m "Wire the plugin event handler into serve and make up wait on activations

Plugins spec §16.3: up and update wait until every activation row is
active, fail at once on a rejected one in config-path style, and show
the rows in status. dev fake-plugin moves onto the full SDK and
fake-claude fires PreToolUse and Stop and echoes stdin, which is what the
phase 2a e2e asserts on.

Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe"
```

---

### Task 12: the e2e — the protocol under nono with `fake-plugin` and `fake-claude`

**Files:**
- Modify: `crates/balerix/tests/e2e.rs` (extend `plugin_package`, add `plugin_protocol_journey`)

**Interfaces:**
- Consumes: Task 11's `dev fake-plugin` / `dev fake-claude`; the existing `World`, `git`, `fleet_yaml`, `plugin_package`, `landlock_works` helpers.
- Produces: spec §13 item 2a "done when": `dev fake-plugin` blocks a `PreToolUse` and sends text to `fake-claude` through a real daemon, real nono, real tmux.

- [ ] **Step 1: Write the test**

`plugin_package(dir)` gains a `manifest_extra: &str` parameter (the phase 1 journey passes `""`). `fleet_yaml` gains a `plugins: Option<&str>` parameter rendered under `alice:` only (`alice: { plugins: { fake: {} } }`); the existing journey passes `None`.

```rust
/// Plugins spec §13 item 2a, "done when": through a real daemon, nono and
/// tmux, the SDK plugin blocks a PreToolUse and sends text to fake-claude;
/// activation is pending until the plugin says hello, then active.
#[test]
fn plugin_protocol_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-protocol-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("balerix-e2e-protocol-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    let cfg = w.home.join(".config/balerix");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();
    let pkg = root.join("fake-pkg");
    plugin_package(
        &pkg,
        "hooks:\n  intercept: [PreToolUse, Stop]\n  observe: [SessionStart, Notification, PreToolUse, Stop]\nneeds: [actions, kv]\n",
    );
    // the package's manifest names the plugin `fake`
    let manifest = fs::read_to_string(pkg.join("balerix-plugin.yaml")).unwrap().replace("name: hello", "name: fake");
    fs::write(pkg.join("balerix-plugin.yaml"), manifest).unwrap();
    fs::write(
        cfg.join("plugins.yaml"),
        format!("plugins:\n  - name: fake\n    source: \"{}\"\n", pkg.display()),
    )
    .unwrap();

    // the same bare repo recipe as the first journey
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("repo.git");
    git(&root, &["clone", "-q", "--bare", &work.display().to_string(), &bare.display().to_string()]);
    let fleet = root.join("fleet.yaml");
    fs::write(&fleet, fleet_yaml(&bare, None, Some("{ fake: {} }"))).unwrap();

    let out = w.ok(&["serve", "-d", "--bind", "127.0.0.1:0", "--tmux-socket", &w.socket]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let plugin_dir = w.state().join("plugins/fake");

    // up waits for the plugin's activation too: the plugin must reach
    // hello (under nono, via mise run) for alice's row to turn active
    let out = w.ok(&["up", &fleet.display().to_string(), "--no-host-defaults", "--timeout", "180s"]);
    assert!(out.contains("e2e  ready"), "{out}");
    assert!(out.contains("fake=active"), "{out}");
    let rec = w.status();
    assert_eq!(rec.status.agents["e2e/c/alice"].plugins["fake"].state, balerix_api::ActivationState::Active);
    assert!(rec.status.agents["e2e/c/bob"].plugins.is_empty());
    let list = w.ok(&["plugin", "list"]);
    assert!(list.contains("fake  ") && list.contains("ready"), "{list}");
    assert!(list.lines().nth(1).is_some_and(|l| l.split_whitespace().nth(5) == Some("1")), "ACTIVE column: {list}");
    let activations = fs::read_to_string(plugin_dir.join("scratch/activations.jsonl")).unwrap();
    assert!(activations.contains("\"agent\":\"e2e/c/alice\""), "{activations}");

    // the PreToolUse block came back to fake-claude through the HTTP hook
    let reply = wait_file(&w.agent_dir("alice").join("home/fake-claude.PreToolUse.reply"));
    assert!(reply.contains("\"decision\":\"block\""), "{reply}");
    assert!(reply.contains("fake-plugin: no recursive deletes"), "{reply}");
    let bob_reply = wait_file(&w.agent_dir("bob").join("home/fake-claude.PreToolUse.reply"));
    assert_eq!(bob_reply.trim(), "{}", "bob has no plugin: pass-through");

    // the Stop verdict's send_text reached alice's stdin through tmux
    let stdin = wait_file(&w.agent_dir("alice").join("home/fake-claude.stdin"));
    assert!(stdin.contains("fake-plugin says hi"), "{stdin}");

    // observers saw alice's events, in order, and none of bob's
    let events = wait_file(&plugin_dir.join("scratch/events.jsonl"));
    let names: Vec<String> = events
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .map(|v| format!("{} {}", v["agent"].as_str().unwrap(), v["name"].as_str().unwrap()))
        .collect();
    assert!(names.iter().all(|n| n.starts_with("e2e/c/alice ")), "{names:?}");
    let idx = |name: &str| names.iter().position(|n| n.ends_with(name)).unwrap_or_else(|| panic!("{name} missing in {names:?}"));
    assert!(idx(" SessionStart") < idx(" PreToolUse") && idx(" PreToolUse") < idx(" Stop"));

    // metrics: the chain ran, the action ran, nothing failed
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let url = fs::read_to_string(w.state().join("server/endpoint")).unwrap().trim().to_string();
    let metrics = agent.get(format!("{url}/metrics")).call().unwrap().body_mut().read_to_string().unwrap();
    assert!(metrics.contains("balerix_plugin_events_total{event=\"PreToolUse\",mode=\"intercept\",plugin=\"fake\"} 1"), "{metrics}");
    assert!(metrics.contains("balerix_plugin_actions_total{action=\"send_text\",plugin=\"fake\"} 1"), "{metrics}");
    assert!(!metrics.contains("balerix_plugin_intercept_failures_total{plugin=\"fake\""), "{metrics}");

    // no token leaks (the plugin's, the agents', the admin's)
    let profile = fs::read_to_string(plugin_dir.join("nono-profile.json")).unwrap();
    let token = profile.split("\"BALERIX_PLUGIN_TOKEN\": \"").nth(1).unwrap().split('"').next().unwrap().to_string();
    let log = fs::read_to_string(w.state().join("server/server.log")).unwrap();
    assert!(!log.contains(&token));
    for s in hook_secrets(&w) {
        assert!(!log.contains(&s), "hook secret in server.log");
    }

    // removing the plugin while the fleet runs: alice's row disappears with the plugin
    w.ok(&["plugin", "remove", "fake"]);
    let start = Instant::now();
    loop {
        let rec = w.status();
        if rec.status.agents["e2e/c/alice"].plugins.is_empty() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "row still there: {rec:?}");
        std::thread::sleep(Duration::from_millis(250));
    }
    w.ok(&["down", "e2e", "--purge", "--timeout", "60s"]);
    drop(w);
}

/// Polls for a file to exist and be non-empty, up to 30 s.
fn wait_file(path: &Path) -> String {
    let start = Instant::now();
    loop {
        if let Ok(s) = fs::read_to_string(path)
            && !s.trim().is_empty()
        {
            return s;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(200));
    }
}
```

The "removing the plugin" step relies on `PluginRegistry::replace_plugins` pruning rows of plugins no longer declared (Task 4) and the overlay following. `balerix_api` is already a dev-dependency of the binary crate's tests (it is a normal dependency).

- [ ] **Step 2: Run it**

Run: `mise run e2e` (or `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix --test e2e`)
Expected: all three journeys pass in under two minutes. Failure diagnosis, in order: `up` times out with `fake=pending` in the table → the plugin never said hello: read `plugins/fake/logs/nono.log` and `mise.toolchain.log` (a `reqwest` or `tokio` runtime failure inside the sandbox shows there; the profile grants nothing new, and a loopback *connect* from the plugin to the daemon's `open_port` is what phase 1 verified) ; `fake-claude.PreToolUse.reply` is `{}` for alice → the chain did not run: check `balerix_plugin_intercept_failures_total` in the metrics and the daemon log for `interceptor skipped`; `fake-claude.stdin` never appears → tmux `send-keys` went to the wrong window or fake-claude's stdin is not the pane's tty: run `tmux -L <socket> send-keys -t =e2e/c:alice -l hello Enter` by hand against the leftover session; `events.jsonl` missing → the observer queue's delivery task never posted: the plugin must be `Ready` (its phase in `plugin list`) and the batch window is 100 ms.

- [ ] **Step 3: Commit**

```bash
git add crates/balerix/tests/e2e.rs
git commit -m "Add the plugin protocol e2e: block, send_text and observe under nono

Plugins spec §13 item 2a, done when: dev fake-plugin, on the full SDK,
blocks fake-claude's PreToolUse, its Stop verdict's send_text arrives on
alice's stdin through tmux, observers see her events in order, and up
waits for the activation to turn active.

Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe"
```

---

### Task 13: docs, threat model, spec refinements

**Files:**
- Modify: `ARCHITECTURE.md`, `AGENTS.md`, `README.md`, `docs/THREAT-MODEL.md`, `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md` (§16)

- [ ] **Step 1: `ARCHITECTURE.md`**

In "The pieces": `balerix-server` line gains "`plugins/`: … `PluginRegistry` (activations, interceptor order), `PluginEventHandler` (the chain), `PluginKv`"; `balerix-plugin-sdk` becomes "the plugin side of the host protocol; depends on `api` only. `Host` (async, one method per route), the `Plugin` trait and `serve`, `testing::FakeHost`."

In "How it flows", after the Spec B phase 1 paragraph:

> **Event protocol (Spec B, phase 2a):** `up` resolves the spec and the daemon, before its actor sees it, activates every `(agent, plugin)` pair on a `Ready` plugin (`POST /v1/activate` at the address the plugin gave in `hello`); a rejection is a 400 `crews.<c>.agents.<a>.plugins.<p>: <message>` and nothing lands, a plugin that is not ready leaves the pair `pending` until its next `hello`. Activation rows are the `PluginRegistry`'s and are overlaid on the record at read time, so `up` waits on them and `status` shows them. Every hook event runs `PluginEventHandler`: the interceptors that subscribe to it and are active for the agent, in `plugins.yaml` order, each given what remains of a 1500 ms budget, failures skipped and counted; the last response goes to Claude; the verdict's actions run afterwards (`send_text` through the runner, `stop`/`restart` through the actor's `SetStopped`). Observers get batches from a per-plugin queue. Plugins call back through `/v1/plugin-host/{fleets,agents/*/actions,kv}` with their token, gated by the manifest's `needs`. `docs/plugin-protocol.md` is the contract.

In "Non-obvious decisions", add:

- **Activation runs before the actor, and its state is a read-time overlay.** `Daemon::apply` activates first so a rejection can be a 400 with nothing landed; the registry owns the rows and `Daemon::get`/`snapshots` copy them into `AgentStatus.plugins`. Two writers, two records: the actor's is persisted, the registry's is rebuilt from the fleet records at start (§16.2, §16.3).
- **`stop` and `restart` are per-agent desired state.** `FleetRecord.stopped` is honoured by the planner: stopped if observed, never restarted, counter untouched. A `restart` is stop then resume, two passes; an `Apply` clears the set for every declared agent (§16.4).
- **The chain fails open.** A dead or slow interceptor is skipped and counted; a dead `flow` plugin stops blocking, which the threat model accepts.
- **The daemon speaks `reqwest` to plugins, the CLI still speaks `ureq`.** The chain runs on every hook event under a deadline; a blocking client would cost a thread per event (§16.1). No TLS feature on either.
- **Plugin metrics are re-exported only under `balerix_plugin_<name>_`.** A body with any other family is dropped whole, so a plugin cannot spoof the daemon's own series.

- [ ] **Step 2: `AGENTS.md`**

Gotchas:

- `up` waits for plugin activations as well as `Ready`; a `fake=pending` in the timeout table means the plugin never said `hello` (look at `plugins/<name>/logs/`), a `rejected` row fails `up` at once with the plugin's message.
- The activation table is not persisted: after a daemon restart every pair is `pending` until the plugin's next `hello`, which re-activates all of them.
- `plugin remove` prunes the activation rows of the removed plugin; `plugin remove --purge` also deletes `plugins/<name>/kv/` — a plugin's KV state survives a plain remove.
- A plugin's `stop` action leaves the agent `Stopped` until a `restart` action or the next `up`/`update`; the reconciler will not restart it and `status` shows `stopped`.
- The `PluginClient` is built with `.no_proxy()`; do not remove it — a `HTTP_PROXY` in the daemon's environment would otherwise capture loopback calls.
- Plugin `/v1/metrics` bodies must carry only `balerix_plugin_<name>_` families or the whole body is dropped (counted in `balerix_plugin_metrics_scrape_failures_total`).
- The SDK's `Plugin` trait uses return-position `impl Future + Send`; implement methods as `async fn` in the impl block (the compiler accepts that), and keep `Send` state (`Mutex`, not `RefCell`).

Conventions: extend the dependency-direction line with "`balerix-server`'s *dev*-dependencies may include `balerix-plugin-sdk` (in-process plugin tests)".

- [ ] **Step 3: `README.md`**

Status:

> Spec A is complete. Spec B (plugins) is in progress: phase 1 (plugin workloads) and phase 2a (the event protocol: activation, the interceptor chain, observers, actions, the `fleets`/`actions`/`kv` routes, the full SDK, `docs/plugin-protocol.md`) are done; phase 2b, the `flow` plugin, is next, then the proxy and `web`.

Under "Where to look" add `docs/plugin-protocol.md` — "the wire contract for plugins in any language". Step 7's paragraph gains: "`plugin list` shows each plugin's phase and how many agents it is active for; an agent opts into a plugin with `plugins: { <name>: { …config… } }` in its settings block and `up` waits until the plugin has accepted it."

Add "### Upgrading to Spec B phase 2a": `status` gains a `PLUGINS` column and `plugin list` an `ACTIVE` column; `fleet.json` gains an empty `stopped` list; an agent naming a plugin that is not in `plugins.yaml` now fails `up` with `crews.<c>.agents.<a>.plugins.<p>: no plugin "<p>" is installed` (phase 1 ignored the block).

- [ ] **Step 4: `docs/THREAT-MODEL.md`**

Trust-boundary line for *plugin ↔ daemon*: replace "`hello` and (later) the host protocol" with "`hello`, the host routes (`fleets`, `actions`, `kv`) and, the other way, `activate`/`deactivate`/`events`/`intercept`/`health`/`metrics` at the address the plugin gave in `hello`".

Controls table rows:

| Plugin host routes | plugin identified by its token alone (constant-time compare against the plugin fleet's index); every route gated by the manifest's `needs` (403); bodies ≤ 1 MiB; `fleets` never returns the `balerix` fleet; `actions` only for agents the plugin is `Active` for | `plugin_api.rs`, `registry.rs` |
| Plugin KV | keys validated (`[A-Za-z0-9._/-]{1,200}`, no `..`) before any path; one 0600 file per key under `plugins/<name>/kv/`, never granted to the sandbox; `?secret=true` sealed by the vault with `<plugin>/<key>` as associated data | `plugins/kv.rs` |
| Plugin responses | `intercept` bodies must be JSON objects, else skipped; rejection messages quoted verbatim into the config-path error and never parsed; metrics bodies dropped whole unless every family carries the plugin's prefix; a plugin's `activate` can only reject its own pair | `plugins/client.rs`, `plugin_api.rs::families_ok` |
| Loopback only, both ways | the daemon calls a plugin only at a loopback `listen` it validated at `hello`; the client is built with `no_proxy()`; no TLS feature in `reqwest` (P3-1) | `host.rs::hello`, `plugins/client.rs` |

Accepted risks: add "**Interceptors fail open**: a dead, slow or misbehaving `flow` plugin stops blocking — hook events are allowed and counted, never held. A plugin with `actions` can stop, restart or type into any agent it is active for; with `fleets` it sees every user fleet's resolved spec (never credentials or hook secrets)."

- [ ] **Step 5: the spec, §16**

Append to §16 a "Refinements from the phase 2a plan (2026-09-07)" list: the six bullets from this plan's header (overlay instead of an actor message — amend the §16.3 sentence in place; token-only identification; `stopped` keyed by display form; health poller shape; no `fleets/watch`; no proxy counter), plus "`FleetRecord`, `Desired` and `Keep` moved to `balerix-api` (re-exported by `balerix-core`) so the SDK's `fleets()` is typed" and "the `balerix` binary depends on `reqwest` only through the SDK".

- [ ] **Step 6: Commit**

```bash
git add ARCHITECTURE.md AGENTS.md README.md docs/THREAT-MODEL.md docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md
git commit -m "Document Spec B phase 2a: the event protocol, its decisions and risks

Claude-Session: https://claude.ai/code/session_01Tg8fptEA1duAdb9Mr2CrQe"
```

---

## Done when

- `mise run check` and `mise run e2e` pass here and in CI with `BALERIX_REQUIRE_TOOLS=1`; `mise run mutants` reports no surviving mutants in `reconcile`.
- `plugin_protocol_journey`: `up` prints `fake=active`, alice's `fake-claude.PreToolUse.reply` carries the block, her `fake-claude.stdin` carries `fake-plugin says hi`, `events.jsonl` has her events in order.
- Every `docs/plugin-protocol/*.json` fixture is replayed by both conformance tests.
- `docs/plugin-protocol.md`, `ARCHITECTURE.md`, `AGENTS.md`, `README.md`, `docs/THREAT-MODEL.md` and the spec's §16 say what the code does.

## Deliberately deferred (phase 2b and 3 of the plugins spec)

- `balerix-plugin-flow`, `mise run package-plugins`, `GET /v1/plugin-host/fleets/watch` (2b).
- `/v1/plugins/<name>/*` reverse proxy with WebSocket passthrough, `AgentRunner::attach`, `balerix-plugin-web`, `balerix_plugin_proxy_requests_total` (3).
- A `balerix stop <agent>` / `balerix restart <agent>` command on top of `SetStopped`: not asked for; the actor message and the planner are ready for it.
